//! Generic trace-based cross-chain call detection.
//!
//! Walks a `callTracer` trace tree to find cross-chain proxy calls using
//! protocol-level detection only (`ICrossChainManager` interface).
//!
//! Two detection mechanisms:
//! 1. **Persistent proxies**: looked up via [`ProxyLookup`] (typically
//!    `authorizedProxies(address)` on the manager contract).
//! 2. **Ephemeral proxies**: `createCrossChainProxy(address, uint256)` calls
//!    that appear within the same trace. The created proxy address is decoded
//!    from the call's output, and the identity is stored in an in-memory map
//!    so that a subsequent proxy call in the same trace can be detected without
//!    an on-chain query.
//!
//! The single entry point is [`walk_trace_tree`], which recurses depth-first
//! through a `callTracer` JSON trace and populates a `Vec<DetectedCall>`.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, sol};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;

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
fn execute_cross_chain_call_selector() -> [u8; 4] {
    // SolCall::SELECTOR is a const [u8; 4]
    ICrossChainManager::executeCrossChainCallCall::SELECTOR
}

/// 4-byte selector for `createCrossChainProxy(address,uint256)`.
fn create_cross_chain_proxy_selector() -> [u8; 4] {
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
struct TraceNode {
    to: Address,
    from: Address,
    input: Vec<u8>,
    value: U256,
}

/// Extract `(to, from, input_bytes, value)` from a JSON trace node.
///
/// Returns `None` if any required field is missing or unparseable.
fn parse_trace_node(node: &Value) -> Option<TraceNode> {
    let to = node
        .get("to")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Address>().ok())?;

    let from = node
        .get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Address>().ok())
        .unwrap_or(Address::ZERO);

    let input_hex = node.get("input").and_then(|v| v.as_str()).unwrap_or("0x");
    let input_clean = input_hex.strip_prefix("0x").unwrap_or(input_hex);
    let input = hex::decode(input_clean).unwrap_or_default();

    let value = node
        .get("value")
        .and_then(|v| v.as_str())
        .and_then(|s| U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok())
        .unwrap_or(U256::ZERO);

    Some(TraceNode {
        to,
        from,
        input,
        value,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
//  Helper: selector matching
// ──────────────────────────────────────────────────────────────────────────────

/// Check if `input` starts with the given 4-byte function selector.
#[inline]
fn has_selector(input: &[u8], selector: &[u8; 4]) -> bool {
    input.len() >= 4 && input[..4] == *selector
}

// ──────────────────────────────────────────────────────────────────────────────
//  Helper: child inspection
// ──────────────────────────────────────────────────────────────────────────────

/// Check if any **direct** child of `node` calls `executeCrossChainCall` on a
/// known manager address.
fn has_execute_cross_chain_call_child(node: &Value, manager_addresses: &[Address]) -> bool {
    let selector = execute_cross_chain_call_selector();
    let children = match node.get("calls").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return false,
    };

    for child in children {
        let child_to = child
            .get("to")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Address>().ok());

        let child_input_hex = child.get("input").and_then(|v| v.as_str()).unwrap_or("0x");
        let child_input_clean = child_input_hex
            .strip_prefix("0x")
            .unwrap_or(child_input_hex);
        let child_input = hex::decode(child_input_clean).unwrap_or_default();

        if let Some(to_addr) = child_to {
            if manager_addresses.contains(&to_addr) && has_selector(&child_input, &selector) {
                return true;
            }
        }
    }

    false
}

// ──────────────────────────────────────────────────────────────────────────────
//  Helper: ephemeral proxy extraction from createCrossChainProxy calls
// ──────────────────────────────────────────────────────────────────────────────

/// Try to decode a `createCrossChainProxy(address, uint256)` call's parameters
/// and extract the created proxy address from the trace node's `output` field.
///
/// The output is an ABI-encoded address (32 bytes, right-aligned at bytes 12..32).
fn try_extract_ephemeral_proxy(node: &Value, input: &[u8]) -> Option<(Address, ProxyInfo)> {
    // ABI layout: selector(4) + address(32) + uint256(32) = 68 bytes minimum
    if input.len() < 68 {
        return None;
    }

    // Decode originalAddress from bytes 4..36 (ABI-encoded address in 32-byte word)
    let original_address = Address::from_slice(&input[16..36]);

    // Decode originalRollupId from bytes 36..68 (ABI-encoded uint256)
    // Read last 8 bytes as u64 big-endian (rollup IDs fit in u64)
    let rollup_id_bytes = &input[36..68];
    let mut rollup_id: u64 = 0;
    let start = rollup_id_bytes.len().saturating_sub(8);
    for b in &rollup_id_bytes[start..] {
        rollup_id = (rollup_id << 8) | (*b as u64);
    }

    // Extract the proxy address from the output field.
    // ABI-encoded address return: 32 bytes, address at bytes 12..32.
    let output_hex = node.get("output").and_then(|v| v.as_str())?;
    let output_clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
    let output_bytes = hex::decode(output_clean).ok()?;
    if output_bytes.len() < 32 {
        return None;
    }
    let proxy_address = Address::from_slice(&output_bytes[12..32]);
    if proxy_address.is_zero() {
        return None;
    }

    tracing::debug!(
        target: "based_rollup::trace",
        %proxy_address,
        %original_address,
        rollup_id,
        "detected ephemeral proxy creation via createCrossChainProxy in trace"
    );

    Some((
        proxy_address,
        ProxyInfo {
            original_address,
            original_rollup_id: rollup_id,
        },
    ))
}

// ──────────────────────────────────────────────────────────────────────────────
//  Core: walk_trace_tree
// ──────────────────────────────────────────────────────────────────────────────

/// Walk a `callTracer` trace tree to detect all cross-chain proxy calls.
///
/// Detection is purely protocol-based:
///
/// 1. A node is a **proxy call** if it has a direct child that calls
///    `executeCrossChainCall` on a known manager address.
/// 2. **Ephemeral proxies** (created in the same tx via `createCrossChainProxy`)
///    are detected from the trace and stored in `ephemeral_proxies`.
///
/// Manager-originated calls (where `node.from` is a manager address) are
/// skipped — these are forward delivery calls (`executeOnBehalf`), not
/// outbound cross-chain triggers.
///
/// Reverted calls are **not** skipped. The trace shows what the tx *would* do;
/// entries are loaded before execution, so reverted proxy calls still need
/// detection.
///
/// # Arguments
///
/// * `node` — the current JSON trace node (root or recursive child).
/// * `manager_addresses` — addresses of known manager contracts (Rollups.sol
///   on L1, CrossChainManagerL2 on L2).
/// * `lookup` — trait object for querying persistent proxy identity on-chain.
/// * `proxy_cache` — memoizes `lookup_proxy` results across the entire trace
///   walk. `None` entries mean "we checked and it's not a proxy".
/// * `ephemeral_proxies` — proxies created within this trace via
///   `createCrossChainProxy`. Populated as the walk proceeds.
/// * `detected_calls` — accumulator for discovered cross-chain proxy calls.
pub async fn walk_trace_tree(
    node: &Value,
    manager_addresses: &[Address],
    lookup: &dyn ProxyLookup,
    proxy_cache: &mut HashMap<Address, Option<ProxyInfo>>,
    ephemeral_proxies: &mut HashMap<Address, ProxyInfo>,
    detected_calls: &mut Vec<DetectedCall>,
    unresolved_proxies: &mut HashSet<Address>,
) {
    walk_trace_tree_inner(
        node,
        manager_addresses,
        lookup,
        proxy_cache,
        ephemeral_proxies,
        detected_calls,
        unresolved_proxies,
        0,     // root node is at depth 0
        false, // root is not inside a reverted frame
    )
    .await;
}

/// Scan a `callTracer` trace tree for `createCrossChainProxy` calls and
/// extract ephemeral proxy identity from return data.
///
/// Unlike [`walk_trace_tree`], this function does NOT require a `ProxyLookup`
/// and only populates the ephemeral proxy map — no `DetectedCall` output.
///
/// Primary use case: pre-populating ephemeral proxies from earlier traces in a
/// `debug_traceCallMany` bundle. A proxy created in tx[1] must be visible when
/// walking tx[2], but `walk_trace_tree` only scans its own trace. By calling
/// this function on tx[1]'s trace first, the resulting `ephemeral_proxies` map
/// can be passed to `walk_trace_tree` for tx[2].
pub fn extract_ephemeral_proxies_from_trace(
    node: &Value,
    manager_addresses: &[Address],
    ephemeral_proxies: &mut HashMap<Address, ProxyInfo>,
) {
    // Check if this node is a createCrossChainProxy call on a manager.
    if let Some(parsed) = parse_trace_node(node) {
        let create_selector = create_cross_chain_proxy_selector();
        if manager_addresses.contains(&parsed.to) && has_selector(&parsed.input, &create_selector) {
            if let Some((proxy_addr, info)) = try_extract_ephemeral_proxy(node, &parsed.input) {
                ephemeral_proxies.insert(proxy_addr, info);
            }
        }
    }

    // Recurse into children.
    if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
        for child in calls {
            extract_ephemeral_proxies_from_trace(child, manager_addresses, ephemeral_proxies);
        }
    }
}

/// Inner recursive implementation of [`walk_trace_tree`].
///
/// Separated to allow `Box::pin` wrapping for async recursion without
/// exposing the pinning in the public API.
#[allow(clippy::too_many_arguments)]
async fn walk_trace_tree_inner(
    node: &Value,
    manager_addresses: &[Address],
    lookup: &dyn ProxyLookup,
    proxy_cache: &mut HashMap<Address, Option<ProxyInfo>>,
    ephemeral_proxies: &mut HashMap<Address, ProxyInfo>,
    detected_calls: &mut Vec<DetectedCall>,
    unresolved_proxies: &mut HashSet<Address>,
    depth: usize,
    in_reverted_frame: bool,
) {
    let parsed = match parse_trace_node(node) {
        Some(p) => p,
        None => {
            // Can't parse this node — still recurse into children in case
            // the JSON structure has intermediate wrapper nodes.
            recurse_children(
                node,
                manager_addresses,
                lookup,
                proxy_cache,
                ephemeral_proxies,
                detected_calls,
                unresolved_proxies,
                depth,
                in_reverted_frame,
            )
            .await;
            return;
        }
    };

    // ── Step 2: Skip if `from` is a manager ─────────────────────────────
    // Manager-originated calls are forward deliveries (the manager calling
    // a proxy via executeOnBehalf to deliver an incoming cross-chain call).
    // These are NOT outbound cross-chain triggers.
    // However, we still recurse into their children to find return calls
    // or nested proxy calls deeper in the tree.
    if manager_addresses.contains(&parsed.from) {
        recurse_children(
            node,
            manager_addresses,
            lookup,
            proxy_cache,
            ephemeral_proxies,
            detected_calls,
            unresolved_proxies,
            depth,
            in_reverted_frame,
        )
        .await;
        return;
    }

    // ── Step 3: Check if this node creates an ephemeral proxy ───────────
    // If `to` is a manager AND `input` starts with createCrossChainProxy
    // selector, decode the (originalAddress, rollupId) and record the
    // mapping from the returned proxy address.
    let create_selector = create_cross_chain_proxy_selector();
    if manager_addresses.contains(&parsed.to) && has_selector(&parsed.input, &create_selector) {
        if let Some((proxy_addr, info)) = try_extract_ephemeral_proxy(node, &parsed.input) {
            ephemeral_proxies.insert(proxy_addr, info);
        }
        // A createCrossChainProxy call is not itself a proxy call — recurse
        // to find subsequent calls that use the newly created proxy.
        recurse_children(
            node,
            manager_addresses,
            lookup,
            proxy_cache,
            ephemeral_proxies,
            detected_calls,
            unresolved_proxies,
            depth,
            in_reverted_frame,
        )
        .await;
        return;
    }

    // ── Step 4: Check if this node is a cross-chain proxy call ──────────
    // A node is a proxy call if any of its direct children call
    // executeCrossChainCall on a known manager.
    if has_execute_cross_chain_call_child(node, manager_addresses) {
        // This node IS a proxy. Resolve its identity.
        let info = resolve_proxy_info(parsed.to, lookup, proxy_cache, ephemeral_proxies).await;

        if let Some(proxy_info) = info {
            // Check if this node itself has an error (node-level revert).
            let node_has_error = node.get("error").and_then(|v| v.as_str()).is_some();
            tracing::info!(
                target: "based_rollup::trace",
                proxy = %parsed.to,
                destination = %proxy_info.original_address,
                rollup_id = proxy_info.original_rollup_id,
                source = %parsed.from,
                calldata_len = parsed.input.len(),
                value = %parsed.value,
                depth,
                in_reverted_frame,
                node_has_error,
                "detected cross-chain proxy call via executeCrossChainCall child"
            );

            // Capture proxy call output for post-convergence enrichment.
            // The output contains the return value from executeOnBehalf,
            // which includes scope-resolved return data for reentrant patterns.
            let proxy_output = node
                .get("output")
                .and_then(|v| v.as_str())
                .and_then(|s| hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok())
                .unwrap_or_default();

            detected_calls.push(DetectedCall {
                destination: proxy_info.original_address,
                calldata: parsed.input,
                value: parsed.value,
                source_address: parsed.from,
                trace_depth: depth,
                output: proxy_output,
                in_reverted_frame,
            });
        } else {
            // Proxy identity not found — record as unresolved so callers can
            // attempt a second-pass resolution via debug_traceCallMany with
            // authorizedProxies queries in the same bundle (seeing simulation state).
            tracing::warn!(
                target: "based_rollup::trace",
                proxy = %parsed.to,
                source = %parsed.from,
                "node has executeCrossChainCall child but proxy identity not found — marking unresolved"
            );
            unresolved_proxies.insert(parsed.to);
        }

        // ALWAYS recurse into proxy children to find reentrant cross-chain
        // calls. In deep patterns (e.g., reentrantCrossChainCalls), the
        // protocol's scope navigation (newScope → executeOnBehalf) triggers
        // additional proxy calls deeper in the trace tree. Step 2 (line 378)
        // already skips manager-originated calls, preventing false positives
        // from protocol-internal forward deliveries.
        recurse_children(
            node,
            manager_addresses,
            lookup,
            proxy_cache,
            ephemeral_proxies,
            detected_calls,
            unresolved_proxies,
            depth,
            in_reverted_frame,
        )
        .await;
        return;
    }

    // ── Step 5: Not a proxy call — recurse into all children ────────────
    recurse_children(
        node,
        manager_addresses,
        lookup,
        proxy_cache,
        ephemeral_proxies,
        detected_calls,
        unresolved_proxies,
        depth,
        in_reverted_frame,
    )
    .await;
}

// ──────────────────────────────────────────────────────────────────────────────
//  Helper: recurse into child calls
// ──────────────────────────────────────────────────────────────────────────────

/// Recurse depth-first into the `calls` array of a trace node.
/// Each child is at `depth + 1` from the current node.
#[allow(clippy::too_many_arguments)]
async fn recurse_children(
    node: &Value,
    manager_addresses: &[Address],
    lookup: &dyn ProxyLookup,
    proxy_cache: &mut HashMap<Address, Option<ProxyInfo>>,
    ephemeral_proxies: &mut HashMap<Address, ProxyInfo>,
    detected_calls: &mut Vec<DetectedCall>,
    unresolved_proxies: &mut HashSet<Address>,
    depth: usize,
    in_reverted_frame: bool,
) {
    if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
        for child in calls {
            // A child is in a reverted frame if the parent already is,
            // or if the CHILD ITSELF has an "error" field (meaning its
            // descendants will be inside a reverted context).
            let child_reverted =
                in_reverted_frame || child.get("error").and_then(|v| v.as_str()).is_some();
            Box::pin(walk_trace_tree_inner(
                child,
                manager_addresses,
                lookup,
                proxy_cache,
                ephemeral_proxies,
                detected_calls,
                unresolved_proxies,
                depth + 1,
                child_reverted,
            ))
            .await;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
//  Helper: proxy identity resolution
// ──────────────────────────────────────────────────────────────────────────────

/// Resolve a proxy's identity using (in priority order):
/// 1. The `proxy_cache` (previously resolved addresses)
/// 2. The `ephemeral_proxies` map (proxies created in the same trace)
/// 3. The on-chain `ProxyLookup` query
///
/// Results are always cached in `proxy_cache` for subsequent lookups.
async fn resolve_proxy_info(
    address: Address,
    lookup: &dyn ProxyLookup,
    proxy_cache: &mut HashMap<Address, Option<ProxyInfo>>,
    ephemeral_proxies: &HashMap<Address, ProxyInfo>,
) -> Option<ProxyInfo> {
    // 1. Check proxy_cache
    if let Some(cached) = proxy_cache.get(&address) {
        return *cached;
    }

    // 2. Check ephemeral_proxies
    if let Some(info) = ephemeral_proxies.get(&address) {
        let info = *info;
        proxy_cache.insert(address, Some(info));
        return Some(info);
    }

    // 3. Query on-chain via ProxyLookup
    let result = lookup.lookup_proxy(address).await;
    proxy_cache.insert(address, result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A mock ProxyLookup that returns a fixed set of registered proxies.
    struct MockLookup {
        proxies: HashMap<Address, ProxyInfo>,
    }

    impl MockLookup {
        fn new() -> Self {
            Self {
                proxies: HashMap::new(),
            }
        }

        fn register(&mut self, proxy_addr: Address, info: ProxyInfo) {
            self.proxies.insert(proxy_addr, info);
        }
    }

    impl ProxyLookup for MockLookup {
        fn lookup_proxy(
            &self,
            address: Address,
        ) -> Pin<Box<dyn Future<Output = Option<ProxyInfo>> + Send + '_>> {
            let result = self.proxies.get(&address).copied();
            Box::pin(async move { result })
        }
    }

    /// Helper to build hex-prefixed input from a selector and ABI-encoded params.
    fn encode_input(selector: &[u8; 4], params: &[u8]) -> String {
        format!("0x{}{}", hex::encode(selector), hex::encode(params))
    }

    /// Build a minimal trace node JSON.
    fn trace_node(to: &str, from: &str, input: &str, value: &str, calls: Vec<Value>) -> Value {
        json!({
            "to": to,
            "from": from,
            "input": input,
            "value": value,
            "calls": calls,
            "output": "0x",
            "type": "CALL"
        })
    }

    #[tokio::test]
    async fn test_selectors_are_consistent() {
        // Verify the sol!-derived selectors are the expected 4-byte values.
        let exec_sel = execute_cross_chain_call_selector();
        let create_sel = create_cross_chain_proxy_selector();
        // These should be stable keccak256 prefixes of the function signatures.
        // executeCrossChainCall(address,bytes) = keccak256("executeCrossChainCall(address,bytes)")
        let expected_exec =
            &alloy_primitives::keccak256(b"executeCrossChainCall(address,bytes)")[..4];
        assert_eq!(
            exec_sel, expected_exec,
            "executeCrossChainCall selector mismatch"
        );

        let expected_create =
            &alloy_primitives::keccak256(b"createCrossChainProxy(address,uint256)")[..4];
        assert_eq!(
            create_sel, expected_create,
            "createCrossChainProxy selector mismatch"
        );
    }

    #[tokio::test]
    async fn test_simple_proxy_call_detected() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        // Build a trace: caller -> proxy -> manager.executeCrossChainCall
        let exec_input = encode_input(
            &execute_cross_chain_call_selector(),
            &[0u8; 64], // dummy ABI params
        );
        let child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let root = trace_node(
            &format!("{proxy_addr}"),
            &format!("{caller}"),
            "0xdeadbeef",
            "0x0",
            vec![child],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(detected.len(), 1, "should detect exactly one proxy call");
        assert_eq!(detected[0].destination, original);
        assert_eq!(detected[0].source_address, caller);
        assert_eq!(detected[0].calldata, hex::decode("deadbeef").unwrap());
    }

    #[tokio::test]
    async fn test_manager_originated_call_skipped() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        // Manager calls the proxy (forward delivery) — should be skipped.
        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        // from=manager — this is a forward delivery
        let root = trace_node(
            &format!("{proxy_addr}"),
            &format!("{manager}"),
            "0xdeadbeef",
            "0x0",
            vec![child],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert!(
            detected.is_empty(),
            "manager-originated calls should be skipped"
        );
    }

    #[tokio::test]
    async fn test_ephemeral_proxy_detected() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000005555"
            .parse()
            .unwrap();
        let bridge: Address = "0x0000000000000000000000000000000000006666"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let lookup = MockLookup::new(); // no persistent proxies

        // Build createCrossChainProxy input: selector + address(32) + uint256(32)
        let mut create_params = vec![0u8; 64];
        // originalAddress at bytes 12..32 of first word
        create_params[12..32].copy_from_slice(original.as_slice());
        // originalRollupId = 1 in second word (last byte)
        create_params[63] = 1;

        let create_input = encode_input(&create_cross_chain_proxy_selector(), &create_params);

        // The output of createCrossChainProxy is an ABI-encoded address
        let mut output_bytes = vec![0u8; 32];
        output_bytes[12..32].copy_from_slice(proxy_addr.as_slice());
        let create_output = format!("0x{}", hex::encode(&output_bytes));

        // createCrossChainProxy node (bridge -> manager)
        let create_node = json!({
            "to": format!("{manager}"),
            "from": format!("{bridge}"),
            "input": create_input,
            "value": "0x0",
            "output": create_output,
            "calls": [],
            "type": "CALL"
        });

        // Proxy call node (bridge -> proxy -> manager.executeCrossChainCall)
        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let exec_child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let proxy_call_node = trace_node(
            &format!("{proxy_addr}"),
            &format!("{bridge}"),
            "0xaabbccdd",
            "0x0",
            vec![exec_child],
        );

        // Root: bridge call that first creates proxy, then calls it
        // Bridge is called by caller
        let bridge_node = trace_node(
            &format!("{bridge}"),
            &format!("{caller}"),
            "0x11223344",
            "0x0",
            vec![create_node, proxy_call_node],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &bridge_node,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(ephemeral.len(), 1, "should detect one ephemeral proxy");
        assert!(ephemeral.contains_key(&proxy_addr));
        assert_eq!(ephemeral[&proxy_addr].original_address, original);
        assert_eq!(ephemeral[&proxy_addr].original_rollup_id, 1);

        assert_eq!(
            detected.len(),
            1,
            "should detect one proxy call using ephemeral proxy"
        );
        assert_eq!(detected[0].destination, original);
        assert_eq!(detected[0].source_address, bridge);
    }

    #[tokio::test]
    async fn test_multiple_proxy_calls_in_one_tx() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_a: Address = "0x000000000000000000000000000000000000aaaa"
            .parse()
            .unwrap();
        let proxy_b: Address = "0x000000000000000000000000000000000000bbbb"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original_a: Address = "0x000000000000000000000000000000000000aa00"
            .parse()
            .unwrap();
        let original_b: Address = "0x000000000000000000000000000000000000bb00"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_a,
            ProxyInfo {
                original_address: original_a,
                original_rollup_id: 1,
            },
        );
        lookup.register(
            proxy_b,
            ProxyInfo {
                original_address: original_b,
                original_rollup_id: 2,
            },
        );

        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);

        let child_a = trace_node(
            &format!("{manager}"),
            &format!("{proxy_a}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let proxy_call_a = trace_node(
            &format!("{proxy_a}"),
            &format!("{caller}"),
            "0x11111111",
            "0x0",
            vec![child_a],
        );

        let child_b = trace_node(
            &format!("{manager}"),
            &format!("{proxy_b}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let proxy_call_b = trace_node(
            &format!("{proxy_b}"),
            &format!("{caller}"),
            "0x22222222",
            "0x0",
            vec![child_b],
        );

        // A contract that calls both proxies
        let contract: Address = "0x0000000000000000000000000000000000009999"
            .parse()
            .unwrap();
        let root = trace_node(
            &format!("{contract}"),
            &format!("{caller}"),
            "0xdeadbeef",
            "0x0",
            vec![proxy_call_a, proxy_call_b],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(detected.len(), 2, "should detect two proxy calls");
        assert_eq!(detected[0].destination, original_a);
        assert_eq!(detected[1].destination, original_b);
    }

    #[tokio::test]
    async fn test_reverted_proxy_call_still_detected() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        // Build a reverted trace node (has "error" field)
        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let child = json!({
            "to": format!("{manager}"),
            "from": format!("{proxy_addr}"),
            "input": exec_input,
            "value": "0x0",
            "calls": [],
            "output": "0x",
            "type": "CALL",
            "error": "execution reverted"
        });
        let root = json!({
            "to": format!("{proxy_addr}"),
            "from": format!("{caller}"),
            "input": "0xdeadbeef",
            "value": "0x0",
            "calls": [child],
            "output": "0x",
            "type": "CALL",
            "error": "execution reverted"
        });

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(
            detected.len(),
            1,
            "reverted proxy calls should still be detected"
        );
        assert_eq!(detected[0].destination, original);
    }

    #[tokio::test]
    async fn test_non_proxy_node_recurses() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let wrapper: Address = "0x0000000000000000000000000000000000007777"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        // Nested: caller -> wrapper -> proxy -> manager
        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let manager_child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let proxy_node = trace_node(
            &format!("{proxy_addr}"),
            &format!("{wrapper}"),
            "0xaabbccdd",
            "0x0",
            vec![manager_child],
        );
        let wrapper_node = trace_node(
            &format!("{wrapper}"),
            &format!("{caller}"),
            "0x11223344",
            "0x0",
            vec![proxy_node],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &wrapper_node,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(
            detected.len(),
            1,
            "should detect proxy call through wrapper"
        );
        assert_eq!(detected[0].destination, original);
        // source_address is whoever called the proxy — in this case the wrapper
        assert_eq!(detected[0].source_address, wrapper);
    }

    #[tokio::test]
    async fn test_proxy_cache_prevents_repeated_lookups() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        /// Counts how many times lookup_proxy is called.
        struct CountingLookup {
            proxies: HashMap<Address, ProxyInfo>,
            count: AtomicUsize,
        }

        impl ProxyLookup for CountingLookup {
            fn lookup_proxy(
                &self,
                address: Address,
            ) -> Pin<Box<dyn Future<Output = Option<ProxyInfo>> + Send + '_>> {
                self.count.fetch_add(1, Ordering::SeqCst);
                let result = self.proxies.get(&address).copied();
                Box::pin(async move { result })
            }
        }

        let mut proxies = HashMap::new();
        proxies.insert(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );
        let lookup = CountingLookup {
            proxies,
            count: AtomicUsize::new(0),
        };

        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);

        // Two separate proxy calls to the same proxy
        let child1 = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let call1 = trace_node(
            &format!("{proxy_addr}"),
            &format!("{caller}"),
            "0x11111111",
            "0x0",
            vec![child1],
        );

        let child2 = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let call2 = trace_node(
            &format!("{proxy_addr}"),
            &format!("{caller}"),
            "0x22222222",
            "0x0",
            vec![child2],
        );

        let contract: Address = "0x0000000000000000000000000000000000009999"
            .parse()
            .unwrap();
        let root = trace_node(
            &format!("{contract}"),
            &format!("{caller}"),
            "0xdeadbeef",
            "0x0",
            vec![call1, call2],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(detected.len(), 2);
        // lookup_proxy should only be called once (second call uses cache)
        assert_eq!(
            lookup.count.load(Ordering::SeqCst),
            1,
            "proxy lookup should be cached after first call"
        );
    }

    #[tokio::test]
    async fn test_has_selector() {
        let sel = [0x9a, 0xf5, 0x32, 0x59]; // just an example
        assert!(has_selector(&[0x9a, 0xf5, 0x32, 0x59, 0x00], &sel));
        assert!(has_selector(&[0x9a, 0xf5, 0x32, 0x59], &sel));
        assert!(!has_selector(&[0x9a, 0xf5, 0x32], &sel)); // too short
        assert!(!has_selector(&[0x00, 0x00, 0x00, 0x00], &sel)); // wrong
        assert!(!has_selector(&[], &sel)); // empty
    }

    #[tokio::test]
    async fn test_empty_trace_no_detection() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let lookup = MockLookup::new();

        let root = json!({
            "to": "0x0000000000000000000000000000000000009999",
            "from": "0x0000000000000000000000000000000000003333",
            "input": "0xdeadbeef",
            "value": "0x0",
            "calls": [],
            "output": "0x",
            "type": "CALL"
        });

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert!(detected.is_empty());
    }

    #[tokio::test]
    async fn test_value_propagated() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        // Send 1 ETH with the proxy call
        let root = trace_node(
            &format!("{proxy_addr}"),
            &format!("{caller}"),
            "0xdeadbeef",
            "0xde0b6b3a7640000", // 1 ETH in hex
            vec![child],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].value, U256::from(1_000_000_000_000_000_000u64));
    }
}
