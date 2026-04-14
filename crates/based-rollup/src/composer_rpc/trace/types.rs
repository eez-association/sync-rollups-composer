//! Types and ABI bindings for trace-based cross-chain call detection.
//!
//! Contains the typed `callTracer` node (`CallTraceNode`), internal parsing
//! helpers, the `DetectedCall` / `ProxyInfo` output types, and the
//! `ProxyLookup` trait for on-chain proxy identity resolution.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, sol};
use serde::Deserialize;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

// ──────────────────────────────────────────────────────────────────────────────
//  Typed callTracer output — replaces ad-hoc serde_json::Value parsing
// ──────────────────────────────────────────────────────────────────────────────

/// A single node in a `callTracer` trace tree.
///
/// Typed deserialization replaces the `.get("field").and_then(|v| v.as_str())`
/// pattern — missing fields are caught at parse time, not scattered across
/// the walk logic.
///
/// Fields are `Option` where the trace may omit them (e.g., `error` is only
/// present on reverted calls, `calls` is absent on leaf nodes).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    dead_code,
    reason = "fields used incrementally as callers migrate from Value"
)]
pub(crate) struct CallTraceNode {
    /// Target address of the call.
    #[serde(default)]
    pub to: Option<String>,
    /// Sender address.
    #[serde(default)]
    pub from: Option<String>,
    /// Hex-encoded calldata (0x-prefixed).
    #[serde(default)]
    pub input: Option<String>,
    /// Hex-encoded ETH value (0x-prefixed).
    #[serde(default)]
    pub value: Option<String>,
    /// Hex-encoded return data (0x-prefixed).
    #[serde(default)]
    pub output: Option<String>,
    /// Error message if the call reverted.
    #[serde(default)]
    pub error: Option<String>,
    /// Revert reason (decoded by the tracer).
    #[serde(default)]
    pub revert_reason: Option<String>,
    /// Child calls.
    #[serde(default)]
    pub calls: Option<Vec<CallTraceNode>>,
}

#[allow(dead_code, reason = "methods used incrementally as callers migrate")]
impl CallTraceNode {
    /// Parse `to` as an Address.
    pub fn to_address(&self) -> Option<Address> {
        self.to.as_deref()?.parse::<Address>().ok()
    }

    /// Parse `from` (sender) as an Address.
    pub fn sender_address(&self) -> Option<Address> {
        self.from.as_deref()?.parse::<Address>().ok()
    }

    /// Decode `input` from hex to bytes.
    pub fn input_bytes(&self) -> Vec<u8> {
        let hex_str = self.input.as_deref().unwrap_or("0x");
        let clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
        hex::decode(clean).unwrap_or_default()
    }

    /// Parse `value` as U256.
    pub fn value_u256(&self) -> U256 {
        self.value
            .as_deref()
            .and_then(|s| U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok())
            .unwrap_or(U256::ZERO)
    }

    /// Decode `output` from hex to bytes.
    pub fn output_bytes(&self) -> Vec<u8> {
        let hex_str = match self.output.as_deref() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
        hex::decode(clean).unwrap_or_default()
    }

    /// Whether this node has an error (reverted).
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// Whether this is a top-level reverted call (error or revert_reason present).
    pub fn is_top_level_error(&self) -> bool {
        self.error.is_some() || self.revert_reason.is_some()
    }

    /// Get child calls (empty slice if none).
    pub fn children(&self) -> &[CallTraceNode] {
        self.calls.as_deref().unwrap_or(&[])
    }

    /// Try to deserialize from a serde_json::Value.
    ///
    /// Fallback for code that still passes `&Value` — allows incremental
    /// migration without changing all function signatures at once.
    pub fn try_parse(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
//  ABI bindings via sol! macro — selectors derived at compile time
// ──────────────────────────────────────────────────────────────────────────────

sol! {
    /// Subset of ICrossChainManager used for trace-based detection.
    interface ICrossChainManager {
        function executeCrossChainCall(address sourceAddress, bytes calldata callData)
            external
            payable
            returns (bytes memory result);

        function createCrossChainProxy(address originalAddress, uint256 originalRollupId)
            external
            returns (address proxy);
    }
}

/// 4-byte selector for `executeCrossChainCall(address,bytes)`.
pub(crate) fn execute_cross_chain_call_selector() -> [u8; 4] {
    // SolCall::SELECTOR is a const [u8; 4]
    ICrossChainManager::executeCrossChainCallCall::SELECTOR
}

/// 4-byte selector for `createCrossChainProxy(address,uint256)`.
pub(crate) fn create_cross_chain_proxy_selector() -> [u8; 4] {
    ICrossChainManager::createCrossChainProxyCall::SELECTOR
}

// ──────────────────────────────────────────────────────────────────────────────
//  Public types
// ──────────────────────────────────────────────────────────────────────────────

/// Information about a detected cross-chain proxy call.
#[derive(Debug, Clone)]
pub struct DetectedCall {
    /// Target address on the other chain (`proxy.originalAddress`).
    pub destination: Address,
    /// Calldata passed to the proxy (from the trace node's `input` field).
    pub calldata: Vec<u8>,
    /// ETH value sent with the call.
    pub value: U256,
    /// Who called the proxy (`from` field of the trace node) — used as
    /// `sourceAddress` in the cross-chain action.
    pub source_address: Address,
    /// Depth of the proxy node in the trace tree (root = 0).
    /// Used to compute scope arrays: `scope_depth = trace_depth`.
    /// Each CALL/DELEGATECALL/STATICCALL frame increments depth by 1.
    pub trace_depth: usize,
    /// Raw output bytes from the proxy call's trace node. For reentrant
    /// patterns, this captures the return value of the proxy's
    /// `executeOnBehalf` call (which includes scope-resolved return data).
    /// Used by post-convergence enrichment to extract delivery return data
    /// from intermediate hops without additional simulation.
    pub output: Vec<u8>,
    /// Whether this proxy call is inside a reverted frame (ancestor has "error").
    /// Used for partial revert patterns (revertContinueL2): calls inside a
    /// try/catch that reverts need REVERT/REVERT_CONTINUE on L1, while calls
    /// outside the reverted frame continue normally.
    pub in_reverted_frame: bool,
}

/// Identity of a cross-chain proxy.
#[derive(Debug, Clone, Copy)]
pub struct ProxyInfo {
    /// The address this proxy represents on its home rollup.
    pub original_address: Address,
    /// The home rollup ID.
    pub original_rollup_id: u64,
}

/// Trait for looking up proxy identity from on-chain state.
///
/// Implementations typically query `authorizedProxies(address)` on the
/// appropriate manager contract (Rollups.sol on L1, CrossChainManagerL2 on L2).
///
/// The returned future is boxed because async trait methods require dynamic
/// dispatch; this avoids a dependency on the `async_trait` proc-macro.
pub trait ProxyLookup: Send + Sync {
    /// Look up the identity of `address`.
    ///
    /// Returns `Some(ProxyInfo)` if the address is a registered cross-chain
    /// proxy, `None` otherwise.
    fn lookup_proxy(
        &self,
        address: Address,
    ) -> Pin<Box<dyn Future<Output = Option<ProxyInfo>> + Send + '_>>;
}

// ──────────────────────────────────────────────────────────────────────────────
//  Helper: parse basic fields from a callTracer JSON trace node
// ──────────────────────────────────────────────────────────────────────────────

/// Parsed fields from a single callTracer trace node.
pub(crate) struct TraceNode {
    pub to: Address,
    pub from: Address,
    pub input: Vec<u8>,
    pub value: U256,
}

/// Extract `(to, from, input_bytes, value)` from a typed [`CallTraceNode`].
///
/// Returns `None` if any required field is missing or unparseable.
pub(crate) fn trace_node_from_typed(typed: &CallTraceNode) -> Option<TraceNode> {
    let to = typed.to_address()?;
    let from = typed.sender_address().unwrap_or(Address::ZERO);

    Some(TraceNode {
        to,
        from,
        input: typed.input_bytes(),
        value: typed.value_u256(),
    })
}

/// Extract `(to, from, input_bytes, value)` from a JSON trace node.
///
/// Returns `None` if any required field is missing or unparseable.
/// Accepts raw `serde_json::Value` for callers that haven't migrated
/// to [`CallTraceNode`] yet.
#[allow(
    dead_code,
    reason = "kept for callers outside trace/ that still use &Value"
)]
pub(crate) fn parse_trace_node(node: &Value) -> Option<TraceNode> {
    let typed = CallTraceNode::try_parse(node)?;
    trace_node_from_typed(&typed)
}

// ──────────────────────────────────────────────────────────────────────────────
//  Helper: selector matching
// ──────────────────────────────────────────────────────────────────────────────

/// Check if `input` starts with the given 4-byte function selector.
#[inline]
pub(crate) fn has_selector(input: &[u8], selector: &[u8; 4]) -> bool {
    input.len() >= 4 && input[..4] == *selector
}
