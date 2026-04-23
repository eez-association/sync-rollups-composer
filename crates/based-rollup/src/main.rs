//! Binary entrypoint for the based rollup node.
//!
//! Launches a reth node with custom consensus, EVM config, and the rollup driver.

use based_rollup::RollupConsensusBuilder;
use based_rollup::config::RollupConfig;
use based_rollup::driver::Driver;
use based_rollup::evm_config::RollupExecutorBuilder;
use based_rollup::health;
use based_rollup::rpc::{
    QueuedCrossChainCall, QueuedL2ToL1Call, SyncRollupsApiServer, SyncRollupsRpc,
};
use clap::Parser;
use eyre::Result;
use reth_node_ethereum::EthereumNode;
use reth_provider::BlockHashReader;
use std::sync::Arc;

/// Compute and print the genesis state root from a genesis.json file, then exit.
/// Usage: based-rollup genesis-state-root --chain /path/to/genesis.json
fn print_genesis_state_root() -> Result<()> {
    // Find --chain arg (reth convention)
    let args: Vec<String> = std::env::args().collect();
    let chain_path = args
        .iter()
        .position(|a| a == "--chain")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "/etc/based-rollup/genesis.json".to_string());

    let raw = std::fs::read_to_string(&chain_path)
        .map_err(|e| eyre::eyre!("failed to read {chain_path}: {e}"))?;
    let genesis: alloy_genesis::Genesis =
        serde_json::from_str(&raw).map_err(|e| eyre::eyre!("failed to parse genesis: {e}"))?;

    // Convert alloc entries to (Address, TrieAccount) for state root computation.
    // GenesisAccount implements Into<TrieAccount> via alloy-genesis.
    let state_root = alloy_trie::root::state_root_unhashed(genesis.alloc);
    println!("{state_root}");
    Ok(())
}

fn main() -> Result<()> {
    // If the first CLI arg is "genesis-state-root", compute and print it.
    // Usage: based-rollup genesis-state-root --chain /path/to/genesis.json
    // This avoids starting the full node.
    if std::env::args().nth(1).as_deref() == Some("genesis-state-root") {
        return print_genesis_state_root();
    }

    reth::cli::Cli::parse_args().run(|builder, _| async move {
        // Load rollup config from CLI args / env vars
        let mut rollup_config = RollupConfig::parse();
        rollup_config.validate()?;
        let rollup_config = Arc::new(rollup_config);

        // Shared sync status flag for the RPC namespace
        let synced = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let synced_for_rpc = synced.clone();
        let config_for_rpc = rollup_config.clone();

        // Unified queue for cross-chain calls (entry pairs + gas price + raw L1 tx).
        // The RPC pushes calls here; the driver drains, sorts by gas price, then submits.
        let queued_cross_chain_calls: Arc<std::sync::Mutex<Vec<QueuedCrossChainCall>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_for_rpc = queued_cross_chain_calls.clone();
        let composer_bundle_materialization_lock = Arc::new(tokio::sync::Mutex::new(()));
        let composer_lock_for_proxy = composer_bundle_materialization_lock.clone();

        // Shared queue for raw signed L1 txs to forward after postBatch.
        // The L1 proxy queues user txs here; the driver forwards them to L1.
        let pending_l1_forward_txs: Arc<std::sync::Mutex<Vec<alloy_primitives::Bytes>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let forward_txs_for_rpc = pending_l1_forward_txs.clone();
        // Separate clone for the L1 composer's BundleManager so it can read
        // pending user txs when constructing debug_traceCallMany bundles.
        // See docs/DERIVATION.md §15 (Composer Bundling).
        let forward_txs_for_composer = pending_l1_forward_txs.clone();
        // Separate clone of the unified cross-chain queue so the bundler's
        // finalizer can snapshot entries produced by each user tx and use
        // them as priors for subsequent txs in the same bundle (§15.1).
        let queued_calls_for_composer = queued_cross_chain_calls.clone();

        // Shared queue for L2→L1 calls.
        // The L2 composer RPC detects cross-chain calls and queues here;
        // the driver drains alongside L1→L2 entries (unified intermediate roots).
        let queued_l2_to_l1_calls: Arc<std::sync::Mutex<Vec<QueuedL2ToL1Call>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let l2_to_l1_for_rpc = queued_l2_to_l1_calls.clone();

        // Build the node with Ethereum components + our custom consensus
        let handle = builder
            .with_types::<EthereumNode>()
            .with_components(
                EthereumNode::components()
                    .consensus(RollupConsensusBuilder::new(rollup_config.clone()))
                    .executor(RollupExecutorBuilder::new(rollup_config.clone())),
            )
            .with_add_ons(reth_node_ethereum::EthereumAddOns::default())
            .extend_rpc_modules(move |ctx| {
                let provider = ctx.provider().clone();
                let evm_config = ctx.node().components.evm_config.clone();
                let rpc = SyncRollupsRpc::new(
                    provider,
                    evm_config,
                    config_for_rpc,
                    synced_for_rpc,
                    calls_for_rpc,
                    forward_txs_for_rpc,
                    l2_to_l1_for_rpc,
                );
                ctx.modules.merge_configured(rpc.into_rpc())?;
                tracing::info!(
                    target: "based_rollup",
                    "syncrollups RPC namespace registered"
                );
                Ok(())
            })
            .launch()
            .await?;

        // Spawn RPC proxy if configured (intercepts eth_sendRawTransaction)
        let proxy_port = rollup_config.proxy_port;
        if proxy_port > 0 {
            // The upstream is reth's RPC on 127.0.0.1:8545 (default).
            // TODO: read actual --http.port from reth config if non-default.
            let upstream_port = 8545u16;
            let bridge_l2_addr = rollup_config.bridge_l2_address;
            let ccm_addr = rollup_config.cross_chain_manager_address;
            let rid = rollup_config.rollup_id;
            let l1_rpc = rollup_config.l1_rpc_url.clone();
            let rollups_addr = rollup_config.rollups_address;
            let builder_addr = rollup_config
                .builder_private_key
                .as_ref()
                .and_then(|key| {
                    let key_hex = key.strip_prefix("0x").unwrap_or(key);
                    key_hex.parse::<alloy_signer_local::PrivateKeySigner>().ok()
                })
                .map(|signer| signer.address())
                .unwrap_or(alloy_primitives::Address::ZERO);
            let builder_key = rollup_config.builder_private_key.clone();
            tokio::spawn(async move {
                if let Err(e) = based_rollup::composer_rpc::l2_to_l1::run_rpc_proxy(
                    proxy_port,
                    upstream_port,
                    bridge_l2_addr,
                    ccm_addr,
                    rid,
                    l1_rpc,
                    rollups_addr,
                    builder_addr,
                    builder_key,
                )
                .await
                {
                    tracing::error!(
                        target: "based_rollup",
                        %e,
                        "RPC proxy exited with error"
                    );
                }
            });
        }

        // Spawn L1 RPC proxy if configured (detects cross-chain calls in L1 traces)
        if rollup_config.l1_proxy_port > 0 && !rollup_config.rollups_address.is_zero() {
            let l1_proxy_port = rollup_config.l1_proxy_port;
            let l1_rpc_url = rollup_config.l1_rpc_url.clone();
            // L2 RPC is on 127.0.0.1:8545 (reth default)
            let l2_rpc_url = "http://127.0.0.1:8545".to_string();
            let rollups_address = rollup_config.rollups_address;
            // Derive builder address from private key for postBatch submissions
            let builder_address = rollup_config
                .builder_private_key
                .as_ref()
                .and_then(|key| {
                    let key_hex = key.strip_prefix("0x").unwrap_or(key);
                    key_hex.parse::<alloy_signer_local::PrivateKeySigner>().ok()
                })
                .map(|signer| signer.address())
                .unwrap_or(alloy_primitives::Address::ZERO);
            let builder_private_key = rollup_config.builder_private_key.clone();
            let rollup_id = rollup_config.rollup_id;
            let cross_chain_manager_address = rollup_config.cross_chain_manager_address;
            let l1_block_time_ms = rollup_config.block_time.saturating_mul(1000);
            let bundle_close_fraction = rollup_config.composer_bundle_close_fraction;
            tokio::spawn(async move {
                if let Err(e) = based_rollup::composer_rpc::l1_to_l2::run_l1_rpc_proxy(
                    l1_proxy_port,
                    l1_rpc_url,
                    l2_rpc_url,
                    rollups_address,
                    builder_address,
                    builder_private_key,
                    rollup_id,
                    cross_chain_manager_address,
                    forward_txs_for_composer,
                    queued_calls_for_composer,
                    composer_lock_for_proxy,
                    l1_block_time_ms,
                    bundle_close_fraction,
                )
                .await
                {
                    tracing::error!(
                        target: "based_rollup",
                        %e,
                        "L1 RPC proxy exited with error"
                    );
                }
            });
        }

        // Get handles for the driver
        let engine_handle = handle.node.add_ons_handle.beacon_engine_handle.clone();
        let evm_config = handle.node.evm_config.clone();
        let genesis_hash = handle.node.provider.block_hash(0)?.ok_or_else(|| {
            eyre::eyre!("genesis block hash not found — database may be corrupted")
        })?;
        let l2_provider = handle.node.provider.clone();
        let pool = handle.node.pool.clone();

        // Create a single L1 provider to reuse across all cycles.
        // We only read from L1 (getLogs, getBlock), so no fillers needed.
        let l1_provider = alloy_provider::RootProvider::new_http(rollup_config.l1_rpc_url.parse()?);

        // Spawn the rollup driver as a critical background task
        let config = rollup_config.clone();
        let health_port = config.health_port;
        handle
            .node
            .task_executor
            .spawn_critical_task("based-rollup-driver", async move {
                let (mut driver, health_rx) = Driver::new(
                    config,
                    engine_handle,
                    evm_config,
                    genesis_hash,
                    l1_provider,
                    l2_provider,
                    pool,
                    synced,
                    queued_cross_chain_calls,
                    composer_bundle_materialization_lock,
                    pending_l1_forward_txs,
                    queued_l2_to_l1_calls,
                );

                // Spawn the health HTTP server if a port is configured (0 = disabled).
                // We wrap in a catch_unwind so panics are logged rather than
                // silently swallowed by tokio::spawn.
                if health_port > 0 {
                    tokio::spawn(async move {
                        let result = std::panic::AssertUnwindSafe(
                            health::run_health_server(health_port, health_rx),
                        );
                        match futures::FutureExt::catch_unwind(result).await {
                            Ok(Ok(())) => {
                                tracing::error!("health server exited unexpectedly — server should run indefinitely");
                            }
                            Ok(Err(err)) => {
                                tracing::error!(%err, "health server exited with error");
                            }
                            Err(panic_err) => {
                                let msg = panic_err
                                    .downcast_ref::<String>()
                                    .map(|s| s.as_str())
                                    .or_else(|| panic_err.downcast_ref::<&str>().copied())
                                    .unwrap_or("unknown panic");
                                tracing::error!(panic = msg, "health server panicked");
                            }
                        }
                    });
                }

                if let Err(err) = driver.run().await {
                    tracing::error!(%err, "based rollup driver exited with error");
                }
            });

        handle.node_exit_future.await
    })
}
