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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    ) -> impl std::future::Future<
        Output = Result<(u64, alloy_primitives::B256, alloy_primitives::B256)>,
    > + Send;
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

        let rpc_resp: super::common::JsonRpcResponse = self
            .client
            .post(url)
            .json(&req)
            .send()
            .await?
            .json()
            .await?;

        rpc_resp
            .into_result()
            .map_err(|e| eyre::eyre!("debug_traceCallMany RPC error: {e}"))
    }

    async fn eth_call_view(&self, chain: ChainTarget, to: Address, data: &str) -> Option<String> {
        let url = self.url(chain);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": format!("{to}"), "data": data}, "latest"],
            "id": 99995
        });

        let resp = self.client.post(url).json(&req).send().await.ok()?;
        let body: super::common::JsonRpcResponse = resp.json().await.ok()?;
        if body.error.is_some() {
            return None;
        }
        body.result_str().map(|s| s.to_string())
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

        let rpc_resp: super::common::JsonRpcResponse = self
            .client
            .post(url)
            .json(&req)
            .send()
            .await?
            .json()
            .await?;

        let block = rpc_resp
            .into_result()
            .map_err(|e| eyre::eyre!("get_block_context: {e}"))?;

        let number_hex = block
            .get("number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("get_block_context: missing block number"))?;
        let number = u64::from_str_radix(number_hex.trim_start_matches("0x"), 16)?;

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

// ---------------------------------------------------------------------------
// InMemorySimClient — fixture-backed test double
// ---------------------------------------------------------------------------

/// Fixture-backed [`SimulationClient`] for unit tests.
///
/// Pre-loaded with canned responses for each method. No HTTP, no network.
/// `trace_call_many` returns responses in round-robin order per chain.
#[cfg(any(test, feature = "test-utils"))]
pub struct InMemorySimClient {
    /// Canned responses for `trace_call_many`, keyed by chain target.
    /// Consumed round-robin via `trace_call_count`.
    trace_responses: std::collections::HashMap<ChainTarget, Vec<Value>>,
    /// Canned responses for `eth_call_view`, keyed by `(chain, to, data)`.
    call_responses: std::collections::HashMap<(ChainTarget, Address, String), String>,
    /// Canned block context per chain: `(number, hash, parent_hash)`.
    block_contexts: std::collections::HashMap<
        ChainTarget,
        (u64, alloy_primitives::B256, alloy_primitives::B256),
    >,
    /// Global counter for round-robin `trace_call_many` indexing.
    trace_call_count: std::sync::atomic::AtomicUsize,
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for InMemorySimClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl InMemorySimClient {
    /// Create an empty client with no canned responses.
    pub fn new() -> Self {
        Self {
            trace_responses: std::collections::HashMap::new(),
            call_responses: std::collections::HashMap::new(),
            block_contexts: std::collections::HashMap::new(),
            trace_call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Register a canned `trace_call_many` response for the given chain.
    ///
    /// Multiple calls append to an ordered list; responses are returned
    /// round-robin.
    pub fn with_trace_response(mut self, chain: ChainTarget, response: Value) -> Self {
        self.trace_responses
            .entry(chain)
            .or_default()
            .push(response);
        self
    }

    /// Register a canned `eth_call_view` response for the `(chain, to, data)` key.
    pub fn with_call_response(
        mut self,
        chain: ChainTarget,
        to: Address,
        data: &str,
        result: &str,
    ) -> Self {
        self.call_responses
            .insert((chain, to, data.to_string()), result.to_string());
        self
    }

    /// Register a canned block context for the given chain.
    pub fn with_block_context(
        mut self,
        chain: ChainTarget,
        number: u64,
        hash: alloy_primitives::B256,
        parent: alloy_primitives::B256,
    ) -> Self {
        self.block_contexts.insert(chain, (number, hash, parent));
        self
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl SimulationClient for InMemorySimClient {
    async fn trace_call_many(
        &self,
        chain: ChainTarget,
        _txs: &[Value],
        _block_override: Option<&str>,
    ) -> Result<Value> {
        let responses = self
            .trace_responses
            .get(&chain)
            .ok_or_else(|| eyre::eyre!("InMemorySimClient: no trace responses for {chain:?}"))?;
        if responses.is_empty() {
            eyre::bail!("InMemorySimClient: trace response list empty for {chain:?}");
        }
        let idx = self
            .trace_call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(responses[idx % responses.len()].clone())
    }

    async fn eth_call_view(&self, chain: ChainTarget, to: Address, data: &str) -> Option<String> {
        self.call_responses
            .get(&(chain, to, data.to_string()))
            .cloned()
    }

    async fn get_block_context(
        &self,
        chain: ChainTarget,
    ) -> Result<(u64, alloy_primitives::B256, alloy_primitives::B256)> {
        self.block_contexts
            .get(&chain)
            .copied()
            .ok_or_else(|| eyre::eyre!("InMemorySimClient: no block context for {chain:?}"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, b256};

    #[tokio::test]
    async fn in_memory_sim_client_roundtrip() {
        let hash = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
        let parent = b256!("0x2222222222222222222222222222222222222222222222222222222222222222");
        let to = Address::ZERO;

        let trace_val = serde_json::json!([{"type": "CALL", "from": "0x00", "to": "0x01"}]);

        let client = InMemorySimClient::new()
            .with_trace_response(ChainTarget::L1, trace_val.clone())
            .with_call_response(ChainTarget::L2, to, "0xdeadbeef", "0x0000abc")
            .with_block_context(ChainTarget::L1, 42, hash, parent);

        // trace_call_many returns the canned response
        let result = client
            .trace_call_many(ChainTarget::L1, &[], None)
            .await
            .expect("trace should succeed");
        assert_eq!(result, trace_val);

        // round-robin: same response on second call (single entry wraps)
        let result2 = client
            .trace_call_many(ChainTarget::L1, &[], None)
            .await
            .expect("trace should succeed on repeat");
        assert_eq!(result2, trace_val);

        // eth_call_view returns the canned response
        let call_result = client
            .eth_call_view(ChainTarget::L2, to, "0xdeadbeef")
            .await;
        assert_eq!(call_result, Some("0x0000abc".to_string()));

        // eth_call_view returns None for unknown keys
        let missing = client
            .eth_call_view(ChainTarget::L1, to, "0xdeadbeef")
            .await;
        assert!(missing.is_none());

        // get_block_context returns the canned context
        let (num, h, p) = client
            .get_block_context(ChainTarget::L1)
            .await
            .expect("block context should exist");
        assert_eq!(num, 42);
        assert_eq!(h, hash);
        assert_eq!(p, parent);

        // get_block_context errors for unconfigured chain
        let err = client.get_block_context(ChainTarget::L2).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn in_memory_sim_client_trace_round_robin_multiple() {
        let r1 = serde_json::json!({"round": 1});
        let r2 = serde_json::json!({"round": 2});

        let client = InMemorySimClient::new()
            .with_trace_response(ChainTarget::L1, r1.clone())
            .with_trace_response(ChainTarget::L1, r2.clone());

        let a = client
            .trace_call_many(ChainTarget::L1, &[], None)
            .await
            .expect("first call");
        assert_eq!(a, r1);

        let b = client
            .trace_call_many(ChainTarget::L1, &[], None)
            .await
            .expect("second call");
        assert_eq!(b, r2);

        // wraps back to first
        let c = client
            .trace_call_many(ChainTarget::L1, &[], None)
            .await
            .expect("third call wraps");
        assert_eq!(c, r1);
    }

    #[tokio::test]
    async fn in_memory_sim_client_trace_missing_chain() {
        let client = InMemorySimClient::new();
        let err = client.trace_call_many(ChainTarget::L1, &[], None).await;
        assert!(err.is_err());
    }
}
