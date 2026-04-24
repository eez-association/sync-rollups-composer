//! Test harness for constructing a minimal [`crate::driver::Driver`] in tests.
//!
//! Feature-gated on `test-utils` (see `Cargo.toml`) so this module — and the
//! paired `_for_test` accessors on `Driver` — NEVER ship in the production
//! binary. Under `cargo test`, the `#[cfg(test)]` arm also pulls it in.
//!
//! # What this exists for
//!
//! `Driver::new` requires reth's full storage stack (seven provider traits), a
//! `ConsensusEngineHandle`, a `RootProvider` for L1, a `RollupEvmConfig` with a
//! chainspec, and a `TransactionPool`. Wiring all of these up from scratch is a
//! lot of scaffolding. This module does it once so tests can drive real
//! production methods (`clear_internal_state`, `apply_sibling_reorg_plan`,
//! `verify_local_block_matches_l1` for specific paths) instead of re-testing
//! the pure helpers those methods delegate to.
//!
//! # What this CANNOT reach
//!
//! Production methods that exercise the engine API or
//! `Driver::build_derived_block` are NOT reachable via this harness:
//!
//! - `rebuild_block_as_sibling` calls `build_derived_block` (which opens a
//!   real `StateProviderDatabase` and runs the EVM) and submits to the engine
//!   via `self.engine`, a concrete `ConsensusEngineHandle<EthEngineTypes>`.
//!   To mock the engine in-place we'd have to generify `Driver` over
//!   [`crate::driver::EngineClient`] — that's a production refactor, not a
//!   test-only change. For sibling-reorg coverage the existing mock-engine
//!   tests in `driver_tests.rs::sibling_reorg_mock_engine` exercise the same
//!   submit-path via `submit_sibling_after_guard`.
//! - `step_sync`'s happy path requires the engine. The clear-on-success
//!   branch is covered via `clear_fields_on_sibling_reorg_success` (a pure
//!   helper the production method calls) plus the wire-through test below
//!   that drives the real field state.
//!
//! The upshot: this harness covers state-mutation methods, not methods that
//! also do engine I/O.

#![cfg(any(test, feature = "test-utils"))]

use crate::config::RollupConfig;
use crate::driver::Driver;
use crate::evm_config::RollupEvmConfig;
use crate::health::HealthStatus;
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::RootProvider;
use reth_chainspec::{ChainSpec, MAINNET};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_provider::{
    ProviderFactory,
    providers::BlockchainProvider,
    test_utils::{MockNodeTypesWithDB, create_test_provider_factory},
};
// BlockchainProvider wraps ProviderFactory and is what satisfies
// `StateProviderFactory`. ProviderFactory alone is missing that impl.

use reth_transaction_pool::test_utils::{TestPool, testing_pool};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::watch;

/// Provider type used by the harness. Reth's `BlockchainProvider` wraps a
/// `ProviderFactory<MockNodeTypesWithDB>` and implements every trait on
/// `Driver`'s `P` bound including `StateProviderFactory` (which
/// `ProviderFactory` alone does not). The underlying factory is backed by a
/// temp directory that is cleaned up on drop.
pub type HarnessL2Provider = BlockchainProvider<MockNodeTypesWithDB>;

/// Minimal wiring around `Driver` so unit tests can exercise real production
/// methods without spinning up a reth node.
///
/// Everything needed to keep the Driver alive is held on the harness, including
/// the engine-message receiver (dropped-but-alive so `Driver::engine.send(...)`
/// doesn't silently succeed). Tests that actually invoke engine methods MUST
/// drive the receiver or assert the call path bails before reaching the engine.
pub struct DriverTestHarness {
    /// The real Driver under test.
    pub driver: Driver<HarnessL2Provider, TestPool>,
    /// Health receiver (retained so drops do not close the sender side
    /// `Driver::health_status_tx` would otherwise error on send).
    #[allow(dead_code)]
    pub health_rx: watch::Receiver<HealthStatus>,
    /// Engine message receiver. Dropped senders would cause
    /// `ConsensusEngineHandle` calls to fail; tests that don't care just hold
    /// it alive.
    #[allow(dead_code)]
    engine_rx: Option<
        mpsc::UnboundedReceiver<
            reth_engine_primitives::BeaconEngineMessage<
                reth_ethereum_engine_primitives::EthEngineTypes,
            >,
        >,
    >,
    /// Underlying provider factory (holds the temp dir alive).
    pub provider_factory: ProviderFactory<MockNodeTypesWithDB>,
    /// Clone of the `BlockchainProvider` given to the Driver. Tests that need
    /// to insert headers / state for `l2_provider.sealed_header()` use this.
    pub blockchain_provider: HarnessL2Provider,
    /// Keep a handle to the shared `synced` AtomicBool so tests can assert on
    /// it. (The same Arc is held by the Driver internally.)
    pub synced: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for DriverTestHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl DriverTestHarness {
    /// Build a Driver with permissive defaults. Fields that gate production
    /// behavior can be tuned via the returned harness's public fields or the
    /// `_for_test` accessors on [`Driver`].
    pub fn new() -> Self {
        let provider_factory = create_test_provider_factory();
        // `BlockchainProvider::new` requires the database to contain at least
        // a genesis header. `with_latest` skips that requirement by letting
        // us pass the head directly. We don't actually use the header's
        // contents in any of the tests — its only purpose is to satisfy the
        // BlockchainProvider invariant.
        let dummy_header = alloy_consensus::Header::default();
        let dummy_hash = B256::with_last_byte(0x42);
        let sealed = reth_primitives_traits::SealedHeader::new(dummy_header, dummy_hash);
        let blockchain_provider = BlockchainProvider::with_latest(provider_factory.clone(), sealed)
            .expect("BlockchainProvider::with_latest on fresh factory");
        let chain_spec: Arc<ChainSpec> = MAINNET.clone();
        let config = Self::default_config();
        let evm_config = RollupEvmConfig::new(chain_spec, config.clone());

        // Minimal L1 provider. No actual RPC is reachable at this URL but the
        // field has to be populated; nothing we test calls through it.
        let l1_provider =
            RootProvider::new_http("http://127.0.0.1:1/".parse().expect("valid URL literal"));

        let pool = testing_pool();
        let synced = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Empty queues. The Driver only drains from these on user-tx paths
        // we don't exercise.
        let queued_cross_chain_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pending_l1_forward_txs: Arc<std::sync::Mutex<Vec<Bytes>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let queued_l2_to_l1_calls = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Engine handle: construct from a fresh channel. The receiver is held
        // on the harness so any `self.engine.send(...)` call lands on a live
        // channel (rather than erroring synchronously).
        let (engine_tx, engine_rx) = mpsc::unbounded_channel();
        let engine = ConsensusEngineHandle::new(engine_tx);

        let genesis_hash = B256::with_last_byte(0x42);

        let (driver, health_rx) = Driver::new(
            config,
            engine,
            evm_config,
            genesis_hash,
            l1_provider,
            blockchain_provider.clone(),
            pool,
            synced.clone(),
            queued_cross_chain_calls,
            pending_l1_forward_txs,
            queued_l2_to_l1_calls,
        );

        Self {
            driver,
            health_rx,
            engine_rx: Some(engine_rx),
            provider_factory,
            blockchain_provider,
            synced,
        }
    }

    /// Default test config — no L1 RPC, no builder, no proposer, no WS.
    pub fn default_config() -> Arc<RollupConfig> {
        Arc::new(RollupConfig {
            l1_rpc_url: "http://127.0.0.1:1/".to_string(),
            l2_context_address: Address::ZERO,
            deployment_l1_block: 1000,
            deployment_timestamp: 1_700_000_000,
            block_time: 12,
            builder_mode: false,
            builder_private_key: None,
            l1_rpc_url_fallback: None,
            l1_builder_rpc_url: None,
            builder_ws_url: None,
            health_port: 0,
            rollups_address: Address::ZERO,
            cross_chain_manager_address: Address::ZERO,
            rollup_id: 0,
            proxy_port: 0,
            l1_proxy_port: 0,
            l1_gas_overbid_pct: 10,
            builder_address: Address::ZERO,
            bridge_l2_address: Address::ZERO,
            bridge_l1_address: Address::ZERO,
            bootstrap_accounts_raw: String::new(),
            bootstrap_accounts: Vec::new(),
        })
    }

    /// Quick assertion helper — the common shape after `clear_internal_state`.
    pub fn assert_recovery_state_cleared(&self) {
        assert_eq!(self.driver.pending_sibling_reorg_for_test(), None);
        assert!(!self.driver.hold_for_test().is_armed());
    }
}
