//! Rollup configuration parsed from CLI flags and environment variables.
//!
//! Includes deterministic timestamp helpers and contract address computation.

use alloy_primitives::Address;
use clap::Parser;
use serde::{Deserialize, Serialize};

/// A bootstrap transfer: send `amount_wei` to `address` at block 1.
#[derive(Clone, Debug)]
pub struct BootstrapAccount {
    pub address: Address,
    pub amount_wei: u128,
}

/// Default block time in seconds (matches Ethereum L1).
pub const DEFAULT_BLOCK_TIME: u64 = 12;

/// Configuration for the based rollup.
///
/// All fields can be set via CLI flags or environment variables.
/// CLI flags take precedence over env vars.
#[derive(Clone, Serialize, Deserialize, Parser)]
#[command(
    name = "based-rollup",
    about = "Based rollup configuration",
    // IMPORTANT: ignore_errors MUST stay true.  RollupConfig::parse() sees the
    // same argv as reth's own CLI parser.  Without ignore_errors, any reth flag
    // (e.g. --datadir, --ws, --http.port) that RollupConfig doesn't declare
    // would cause a hard parse error at startup.  The trade-off is that
    // mistyped rollup flags (e.g. --rolups-address) are silently ignored
    // instead of producing an error.  This is acceptable because rollup fields
    // have env-var fallbacks and are validated by RollupConfig::validate().
    // See issue #166.
    ignore_errors = true
)]
pub struct RollupConfig {
    /// L1 RPC endpoint URL (http or ws).
    #[arg(long, env = "L1_RPC_URL", default_value = "http://localhost:8545")]
    pub l1_rpc_url: String,

    /// Address of the L2Context contract (predeployed on L2).
    #[arg(
        long,
        env = "L2_CONTEXT_ADDRESS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub l2_context_address: Address,

    /// The L1 block number at which the Rollups contract was deployed.
    #[arg(long, env = "DEPLOYMENT_L1_BLOCK", default_value = "0")]
    pub deployment_l1_block: u64,

    /// The genesis timestamp for the L2 chain.
    /// L2 block timestamps are computed as: deployment_timestamp + ((l2_block_number + 1) * block_time)
    #[arg(long, env = "DEPLOYMENT_TIMESTAMP", default_value = "0")]
    pub deployment_timestamp: u64,

    /// Block time in seconds.
    #[arg(long, env = "BLOCK_TIME", default_value = "12")]
    #[serde(default = "default_block_time")]
    pub block_time: u64,

    /// Fraction of the L1 block time during which the composer accepts new user
    /// txs into the CURRENT bundle. After this fraction elapses, new txs land
    /// in the next bundle. Default 0.9 (10.8s window out of 12s block time),
    /// leaving ~1.2s slack for finalize + driver commit + L1 inclusion.
    ///
    /// Higher values maximize bundle overlap (sim==runtime for more races) at
    /// the cost of less finalize slack.
    ///
    /// See docs/DERIVATION.md §15 (Composer Bundling) for the full model.
    #[arg(long, env = "COMPOSER_BUNDLE_CLOSE_FRACTION", default_value = "0.9")]
    #[serde(default = "default_composer_bundle_close_fraction")]
    pub composer_bundle_close_fraction: f64,

    /// Whether this node should run in builder mode (build + propose blocks).
    /// If false, runs as a fullnode/verifier (sync only).
    #[arg(long, env = "BUILDER_MODE", default_value = "false")]
    #[serde(default)]
    pub builder_mode: bool,

    /// Hex-encoded builder private key for signing L1 transactions.
    /// Required if builder_mode is true.
    #[arg(long, env = "BUILDER_PRIVATE_KEY")]
    pub builder_private_key: Option<String>,

    /// Optional fallback L1 RPC URL. Used when primary L1_RPC_URL fails.
    #[arg(long, env = "L1_RPC_URL_FALLBACK")]
    pub l1_rpc_url_fallback: Option<String>,

    /// WebSocket URL of the builder node for preconfirmation sync.
    /// Fullnodes connect here to receive blocks before L1 confirmation.
    /// Example: ws://builder:8546
    #[arg(long, env = "BUILDER_WS_URL")]
    pub builder_ws_url: Option<String>,

    /// Port for the health HTTP endpoint (0 = disabled).
    #[arg(long, env = "HEALTH_PORT", default_value = "9100")]
    pub health_port: u16,

    /// Address of the Rollups contract on L1.
    /// All block submission and cross-chain operations go through this contract.
    #[arg(
        long,
        env = "ROLLUPS_ADDRESS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub rollups_address: Address,

    /// Address of the CrossChainManagerL2 contract (predeployed on L2).
    /// Required when cross-chain mode is enabled (rollups_address is non-zero).
    #[arg(
        long,
        env = "CROSS_CHAIN_MANAGER_ADDRESS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub cross_chain_manager_address: Address,

    /// Rollup ID assigned by the L1 Rollups contract.
    /// Used for cross-chain action routing and state delta matching.
    #[arg(long, env = "ROLLUP_ID", default_value = "0")]
    pub rollup_id: u64,

    /// Port for the JSON-RPC proxy (0 = disabled).
    /// When enabled, runs a reverse proxy that intercepts eth_sendRawTransaction
    /// to trigger syncrollups_simulateTransaction for execution planning.
    #[arg(long, env = "PROXY_PORT", default_value = "0")]
    pub proxy_port: u16,

    /// Port for the L1 RPC proxy (0 = disabled).
    /// When enabled, proxies L1 RPC traffic and intercepts eth_sendRawTransaction
    /// to detect cross-chain calls via trace analysis. Users point MetaMask here
    /// for transparent cross-chain execution without custom tooling.
    #[arg(long, env = "L1_PROXY_PORT", default_value = "0")]
    pub l1_proxy_port: u16,

    /// Gas price overbid percentage for postBatch transactions relative to
    /// queued user L1 transactions. Ensures the builder's postBatch lands
    /// BEFORE the user's cross-chain tx in the same L1 block (miners order
    /// by priority fee). Can be negative for testing. Default: 10 (10%).
    #[arg(long, env = "L1_GAS_OVERBID_PCT", default_value = "10")]
    pub l1_gas_overbid_pct: i64,

    /// Builder address on L2 (derived from BUILDER_PRIVATE_KEY, or set
    /// explicitly for fullnodes). Required for all nodes — determines
    /// coinbase, contract deployment addresses, and protocol tx signer.
    #[arg(
        long,
        env = "BUILDER_ADDRESS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub builder_address: Address,

    /// Address of the Bridge contract on L2 (deployed at block 1, nonce=2).
    /// Used for detecting L2→L1 withdrawal transactions.
    #[arg(
        long,
        env = "BRIDGE_L2_ADDRESS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub bridge_l2_address: Address,

    /// Address of the Bridge contract on L1.
    /// Used for canonical bridge resolution (setCanonicalBridgeAddress protocol tx).
    #[arg(
        long,
        env = "BRIDGE_L1_ADDRESS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub bridge_l1_address: Address,

    /// Comma-separated bootstrap transfers for block 1: "addr1:eth1,addr2:eth2".
    /// Each pair funds the given address with the specified ETH amount.
    /// Example: "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc:10"
    #[arg(long, env = "BOOTSTRAP_ACCOUNTS", default_value = "")]
    pub bootstrap_accounts_raw: String,

    /// Parsed bootstrap accounts (populated by validate()).
    #[arg(skip)]
    #[serde(skip)]
    pub bootstrap_accounts: Vec<BootstrapAccount>,
}

/// Compute the CREATE address for a given deployer and nonce.
/// Uses the standard RLP(sender, nonce) scheme.
pub fn compute_create_address(deployer: Address, nonce: u64) -> Address {
    use alloy_primitives::keccak256;
    use alloy_rlp::Encodable;

    let mut buf = Vec::new();
    // RLP list: [deployer, nonce]
    let header = alloy_rlp::Header {
        list: true,
        payload_length: deployer.length() + nonce.length(),
    };
    header.encode(&mut buf);
    deployer.encode(&mut buf);
    nonce.encode(&mut buf);

    let hash = keccak256(&buf);
    Address::from_slice(&hash[12..])
}

impl std::fmt::Debug for RollupConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RollupConfig")
            .field("l1_rpc_url", &self.l1_rpc_url)
            .field("l2_context_address", &self.l2_context_address)
            .field("deployment_l1_block", &self.deployment_l1_block)
            .field("deployment_timestamp", &self.deployment_timestamp)
            .field("block_time", &self.block_time)
            .field(
                "composer_bundle_close_fraction",
                &self.composer_bundle_close_fraction,
            )
            .field("builder_mode", &self.builder_mode)
            .field(
                "builder_private_key",
                &self.builder_private_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("l1_rpc_url_fallback", &self.l1_rpc_url_fallback)
            .field("builder_ws_url", &self.builder_ws_url)
            .field("health_port", &self.health_port)
            .field("rollups_address", &self.rollups_address)
            .field(
                "cross_chain_manager_address",
                &self.cross_chain_manager_address,
            )
            .field("rollup_id", &self.rollup_id)
            .field("proxy_port", &self.proxy_port)
            .field("l1_proxy_port", &self.l1_proxy_port)
            .field("l1_gas_overbid_pct", &self.l1_gas_overbid_pct)
            .field("builder_address", &self.builder_address)
            .field("bridge_l2_address", &self.bridge_l2_address)
            .field("bridge_l1_address", &self.bridge_l1_address)
            .field("bootstrap_accounts_raw", &self.bootstrap_accounts_raw)
            .field("bootstrap_accounts", &self.bootstrap_accounts)
            .finish()
    }
}

fn default_block_time() -> u64 {
    DEFAULT_BLOCK_TIME
}

fn default_composer_bundle_close_fraction() -> f64 {
    0.9
}

/// Parse the BOOTSTRAP_ACCOUNTS string into a list of (address, wei) pairs.
///
/// Format: "addr1:eth1,addr2:eth2" where ethN is a decimal ETH amount.
/// Returns an empty vec for empty/whitespace-only input.
pub fn parse_bootstrap_accounts(raw: &str) -> eyre::Result<Vec<BootstrapAccount>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut accounts = Vec::new();
    for pair in trimmed.split(',') {
        let pair = pair.trim();
        let (addr_str, eth_str) = pair
            .split_once(':')
            .ok_or_else(|| eyre::eyre!("invalid bootstrap pair (expected addr:eth): {pair}"))?;
        let address: Address = addr_str
            .trim()
            .parse()
            .map_err(|e| eyre::eyre!("invalid address in bootstrap pair '{pair}': {e}"))?;
        let eth: f64 = eth_str
            .trim()
            .parse()
            .map_err(|e| eyre::eyre!("invalid ETH amount in bootstrap pair '{pair}': {e}"))?;
        if eth < 0.0 {
            return Err(eyre::eyre!(
                "negative ETH amount in bootstrap pair '{pair}'"
            ));
        }
        let amount_wei = (eth * 1e18) as u128;
        accounts.push(BootstrapAccount {
            address,
            amount_wei,
        });
    }
    Ok(accounts)
}

impl RollupConfig {
    /// Validate config after parsing. Also derives builder_address from
    /// builder_private_key and computes contract addresses if needed.
    /// Returns an error for invalid values (e.g., block_time=0 which causes
    /// division by zero). Warns on suspicious but potentially intentional values.
    pub fn validate(&mut self) -> eyre::Result<()> {
        // Derive builder_address from private key if not explicitly set
        if self.builder_address.is_zero() {
            if let Some(ref key) = self.builder_private_key {
                let key_hex = key.strip_prefix("0x").unwrap_or(key);
                if let Ok(signer) = key_hex.parse::<alloy_signer_local::PrivateKeySigner>() {
                    self.builder_address = signer.address();
                    tracing::info!(
                        target: "based_rollup::config",
                        builder_address = %self.builder_address,
                        "derived builder address from private key"
                    );
                }
            }
        }

        // Compute deterministic contract addresses from builder address if not explicitly set
        if !self.builder_address.is_zero() {
            if self.l2_context_address.is_zero() {
                self.l2_context_address = compute_create_address(self.builder_address, 0);
                tracing::info!(
                    target: "based_rollup::config",
                    l2_context_address = %self.l2_context_address,
                    "computed L2Context address from builder address (nonce=0)"
                );
            }
            if self.cross_chain_manager_address.is_zero() && !self.rollups_address.is_zero() {
                self.cross_chain_manager_address = compute_create_address(self.builder_address, 1);
                tracing::info!(
                    target: "based_rollup::config",
                    cross_chain_manager_address = %self.cross_chain_manager_address,
                    "computed CCM address from builder address (nonce=1)"
                );
            }
            if self.bridge_l2_address.is_zero() && !self.rollups_address.is_zero() {
                self.bridge_l2_address = compute_create_address(self.builder_address, 2);
                tracing::info!(
                    target: "based_rollup::config",
                    bridge_l2_address = %self.bridge_l2_address,
                    "computed Bridge L2 address from builder address (nonce=2)"
                );
            }
        }
        if self.block_time == 0 {
            return Err(eyre::eyre!(
                "BLOCK_TIME must be > 0 (got 0), which would cause division by zero"
            ));
        }
        if !(0.0 < self.composer_bundle_close_fraction
            && self.composer_bundle_close_fraction < 1.0)
        {
            return Err(eyre::eyre!(
                "COMPOSER_BUNDLE_CLOSE_FRACTION must be in (0, 1), got {}",
                self.composer_bundle_close_fraction
            ));
        }
        if self.deployment_timestamp == 0 {
            tracing::warn!(
                target: "based_rollup::config",
                "DEPLOYMENT_TIMESTAMP is 0 — builder mode will try to produce blocks from Unix epoch. \
                 Ensure this is intentional (e.g., test environment)"
            );
        }
        if self.builder_mode && self.rollups_address.is_zero() {
            return Err(eyre::eyre!(
                "BUILDER_MODE is enabled but ROLLUPS_ADDRESS is zero — builder cannot submit blocks"
            ));
        }
        if self.rollups_address.is_zero() {
            tracing::warn!(
                target: "based_rollup::config",
                "ROLLUPS_ADDRESS is zero — no events will be derived from L1"
            );
        }
        if self.builder_mode && self.builder_private_key.is_none() {
            tracing::warn!(
                target: "based_rollup::config",
                "BUILDER_MODE enabled but BUILDER_PRIVATE_KEY not set — \
                 blocks will be built locally but not submitted to L1"
            );
        }
        // Cross-chain validation
        if !self.rollups_address.is_zero() && self.cross_chain_manager_address.is_zero() {
            return Err(eyre::eyre!(
                "ROLLUPS_ADDRESS is set but CROSS_CHAIN_MANAGER_ADDRESS is zero — \
                 cross-chain mode requires a CrossChainManagerL2 address (computed from BUILDER_ADDRESS or set explicitly)"
            ));
        }
        if self.rollups_address.is_zero() && !self.cross_chain_manager_address.is_zero() {
            tracing::warn!(
                target: "based_rollup::config",
                "CROSS_CHAIN_MANAGER_ADDRESS is set but ROLLUPS_ADDRESS is zero — \
                 cross-chain mode is disabled; the manager address will be unused"
            );
        }
        if !self.rollups_address.is_zero() && self.rollup_id == 0 {
            tracing::warn!(
                target: "based_rollup::config",
                "ROLLUPS_ADDRESS is set but ROLLUP_ID is 0 — \
                 ensure this is intentional (rollup ID 0 is typically reserved for L1)"
            );
        }
        if self.rollups_address.is_zero() && self.rollup_id != 0 {
            tracing::warn!(
                target: "based_rollup::config",
                rollup_id = self.rollup_id,
                "ROLLUP_ID is set but ROLLUPS_ADDRESS is zero — \
                 ROLLUP_ID will be unused"
            );
        }
        if self.l1_proxy_port > 0 && self.rollups_address.is_zero() {
            tracing::warn!(
                target: "based_rollup::config",
                l1_proxy_port = self.l1_proxy_port,
                "L1_PROXY_PORT is set but ROLLUPS_ADDRESS is zero — \
                 the L1 RPC proxy will NOT start"
            );
        }
        // Parse bootstrap accounts
        self.bootstrap_accounts = parse_bootstrap_accounts(&self.bootstrap_accounts_raw)?;
        if !self.bootstrap_accounts.is_empty() {
            tracing::info!(
                target: "based_rollup::config",
                count = self.bootstrap_accounts.len(),
                "parsed bootstrap accounts for block 1"
            );
        }

        // Log full config so operators can verify correct parsing.
        // (clap ignore_errors=true means typos in flag names are silently ignored)
        tracing::info!(
            target: "based_rollup::config",
            rollups_address = %self.rollups_address,
            l2_context = %self.l2_context_address,
            deployment_l1_block = self.deployment_l1_block,
            deployment_timestamp = self.deployment_timestamp,
            block_time = self.block_time,
            builder_mode = self.builder_mode,
            has_builder_key = self.builder_private_key.is_some(),
            l1_rpc_url_fallback = ?self.l1_rpc_url_fallback,
            builder_ws_url = ?self.builder_ws_url,
            cross_chain_manager = %self.cross_chain_manager_address,
            rollup_id = self.rollup_id,
            l1_gas_overbid_pct = self.l1_gas_overbid_pct,
            builder_address = %self.builder_address,
            bridge_l1_address = %self.bridge_l1_address,
            "rollup config loaded — verify all values are correct"
        );
        Ok(())
    }

    /// Compute the L2 block number for a given L1 block number.
    pub fn l2_block_number(&self, l1_block_number: u64) -> u64 {
        l1_block_number.saturating_sub(self.deployment_l1_block)
    }

    /// Compute the deterministic timestamp for a given L2 block number.
    ///
    /// Formula: `deployment_timestamp + (block_number + 1) * block_time`
    ///
    /// The `+1` offset aligns L2 block K with L1 block `deployment_l1_block + K + 1`.
    /// The builder sees L1 head = N and builds up to L2 block `l2_block_number(N)` =
    /// `N - deployment_l1_block`.  The resulting L2 block has timestamp
    /// `deployment_timestamp + (N - deployment_l1_block + 1) * block_time`, which
    /// equals the timestamp of L1 block N + 1 — exactly where `postBatch` lands.
    /// The L2 block therefore carries a timestamp 12 s ahead of the builder's wall
    /// clock at construction time; this is correct because it matches the L1 block
    /// that will include the batch.
    ///
    /// Uses saturating arithmetic to prevent silent wrapping in release builds.
    /// For untrusted inputs (e.g., from L1 events), prefer `l2_timestamp_checked`.
    pub fn l2_timestamp(&self, l2_block_number: u64) -> u64 {
        self.deployment_timestamp.saturating_add(
            l2_block_number
                .saturating_add(1)
                .saturating_mul(self.block_time),
        )
    }

    /// Checked version of `l2_timestamp` that returns None on overflow.
    /// Use this for untrusted inputs from L1 events.
    pub fn l2_timestamp_checked(&self, l2_block_number: u64) -> Option<u64> {
        l2_block_number
            .checked_add(1)
            .and_then(|n| n.checked_mul(self.block_time))
            .and_then(|product| self.deployment_timestamp.checked_add(product))
    }

    /// Compute the L2 block number from a timestamp.
    ///
    /// Inverse of `l2_timestamp`: given `ts = dep + (n+1)*bt`, returns `n`.
    ///
    /// Requires `block_time > 0` (enforced by `validate()`). Returns 0 if
    /// `block_time` is somehow zero to avoid a division-by-zero panic.
    pub fn l2_block_number_from_timestamp(&self, timestamp: u64) -> u64 {
        if self.block_time == 0 {
            return 0;
        }
        (timestamp.saturating_sub(self.deployment_timestamp) / self.block_time).saturating_sub(1)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
