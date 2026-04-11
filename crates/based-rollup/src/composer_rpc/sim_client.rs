//! Simulation client abstraction for composer RPC modules.
//!
//! [`SimulationClient`] is the single IO seam between the composer's
//! cross-chain detection logic and the JSON-RPC network. Both L1→L2 and
//! L2→L1 directions take `Arc<dyn SimulationClient>` so:
//!
//! - Production uses [`HttpSimClient`] (wraps `reqwest::Client`).
//! - Tests use `InMemorySimClient` (fixture-backed, no HTTP).
//!
//! Introduced in refactor step 3.0 (PLAN.md §Phase 3).

use alloy_primitives::Address;
use eyre::Result;
use serde_json::Value;

/// Target chain for a simulation call.
///
/// The composer talks to both L1 and L2 during cross-chain detection.
/// This discriminator lets the [`SimulationClient`] route the call to
/// the correct upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainTarget {
    L1,
    L2,
}

/// Abstraction over JSON-RPC simulation calls used by the composer.
///
/// Covers the two call patterns used across both directions:
///
/// - **`debug_traceCallMany`**: multi-tx bundle simulation with
///   `callTracer` for cross-chain proxy detection and delivery
///   simulation. Returns a JSON trace tree per transaction.
///
/// - **`eth_call`**: read-only view calls for proxy lookups,
///   verification keys, state root queries, etc.
///
/// The trait is object-safe (`dyn SimulationClient`) so directions
/// can hold `Arc<dyn SimulationClient>`. Rust 2024 edition supports
/// native `async fn` in traits — no `#[async_trait]` needed.
pub trait SimulationClient: Send + Sync {
    /// Execute a `debug_traceCallMany` bundle against the given chain.
    ///
    /// `txs` is the `transactions` array (each element is a `[{tx}, {stateOverrides}]`
    /// pair). `block_override` is the block specifier (e.g., `"latest"` or a hex
    /// block number). Returns the raw JSON-RPC `result` field (array of trace objects).
    fn trace_call_many(
        &self,
        chain: ChainTarget,
        txs: &[Value],
        block_override: Option<&str>,
    ) -> impl std::future::Future<Output = Result<Value>> + Send;

    /// Execute a read-only `eth_call` against the given chain.
    ///
    /// Returns the hex-encoded result string (without `0x` prefix), or
    /// `None` if the call reverted or the RPC returned an error.
    fn eth_call_view(
        &self,
        chain: ChainTarget,
        to: Address,
        data: &str,
    ) -> impl std::future::Future<Output = Option<String>> + Send;

    /// Fetch the latest block context from the given chain.
    ///
    /// Returns `(block_number, block_hash, parent_hash)`.
    fn get_block_context(
        &self,
        chain: ChainTarget,
    ) -> impl std::future::Future<Output = Result<(u64, alloy_primitives::B256, alloy_primitives::B256)>> + Send;
}

// ---------------------------------------------------------------------------
// HttpSimClient — production implementation over reqwest
// ---------------------------------------------------------------------------

/// Production [`SimulationClient`] backed by `reqwest::Client`.
///
/// Routes [`ChainTarget::L1`] and [`ChainTarget::L2`] to the
/// corresponding RPC URLs provided at construction time.
pub struct HttpSimClient {
    client: reqwest::Client,
    l1_url: String,
    l2_url: String,
}

impl HttpSimClient {
    /// Create a new HTTP simulation client.
    pub fn new(client: reqwest::Client, l1_url: String, l2_url: String) -> Self {
        Self {
            client,
            l1_url,
            l2_url,
        }
    }

    fn url(&self, chain: ChainTarget) -> &str {
        match chain {
            ChainTarget::L1 => &self.l1_url,
            ChainTarget::L2 => &self.l2_url,
        }
    }
}

impl SimulationClient for HttpSimClient {
    async fn trace_call_many(
        &self,
        chain: ChainTarget,
        txs: &[Value],
        block_override: Option<&str>,
    ) -> Result<Value> {
        let url = self.url(chain);
        let block_param: Value = match block_override {
            Some(b) => serde_json::json!(b),
            None => Value::Null,
        };
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCallMany",
            "params": [txs, block_param, {"tracer": "callTracer"}],
            "id": 1
        });

        let resp = self
            .client
            .post(url)
            .json(&req)
            .send()
            .await?
            .json::<Value>()
            .await?;

        if let Some(error) = resp.get("error") {
            eyre::bail!(
                "debug_traceCallMany RPC error: {}",
                serde_json::to_string(error).unwrap_or_default()
            );
        }

        resp.get("result")
            .cloned()
            .ok_or_else(|| eyre::eyre!("debug_traceCallMany: missing result field"))
    }

    async fn eth_call_view(
        &self,
        chain: ChainTarget,
        to: Address,
        data: &str,
    ) -> Option<String> {
        let url = self.url(chain);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": format!("{to}"), "data": data}, "latest"],
            "id": 99995
        });

        let resp = self.client.post(url).json(&req).send().await.ok()?;
        let body: Value = resp.json().await.ok()?;
        if body.get("error").is_some() {
            return None;
        }
        body.get("result")?.as_str().map(|s| s.to_string())
    }

    async fn get_block_context(
        &self,
        chain: ChainTarget,
    ) -> Result<(u64, alloy_primitives::B256, alloy_primitives::B256)> {
        let url = self.url(chain);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockByNumber",
            "params": ["latest", false],
            "id": 99997
        });

        let resp = self
            .client
            .post(url)
            .json(&req)
            .send()
            .await?
            .json::<Value>()
            .await?;

        let block = resp
            .get("result")
            .ok_or_else(|| eyre::eyre!("get_block_context: no result"))?;

        let number_hex = block
            .get("number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("get_block_context: missing block number"))?;
        let number =
            u64::from_str_radix(number_hex.trim_start_matches("0x"), 16)?;

        let hash_str = block
            .get("hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("get_block_context: missing block hash"))?;
        let hash: alloy_primitives::B256 = hash_str.parse()?;

        let parent_str = block
            .get("parentHash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("get_block_context: missing parent hash"))?;
        let parent: alloy_primitives::B256 = parent_str.parse()?;

        Ok((number, hash, parent))
    }
}
