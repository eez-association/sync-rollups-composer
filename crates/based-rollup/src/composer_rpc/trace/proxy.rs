//! Proxy identity resolution and ephemeral proxy extraction.
//!
//! Resolves cross-chain proxy addresses to their `(originalAddress, originalRollupId)`
//! identity using cached lookups, ephemeral proxy maps (from `createCrossChainProxy`
//! in the same trace), and on-chain `ProxyLookup` queries.

use alloy_primitives::Address;
use serde_json::Value;
use std::collections::HashMap;

use super::types::{
    CallTraceNode, ProxyInfo, ProxyLookup, create_cross_chain_proxy_selector, has_selector,
    trace_node_from_typed,
};

// ──────────────────────────────────────────────────────────────────────────────
//  Ephemeral proxy extraction from createCrossChainProxy calls
// ──────────────────────────────────────────────────────────────────────────────

/// Try to decode a `createCrossChainProxy(address, uint256)` call's parameters
/// and extract the created proxy address from the trace node's `output` field.
///
/// The output is an ABI-encoded address (32 bytes, right-aligned at bytes 12..32).
pub(crate) fn try_extract_ephemeral_proxy(
    node: &CallTraceNode,
    input: &[u8],
) -> Option<(Address, ProxyInfo)> {
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
    let output_bytes = node.output_bytes();
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

/// Scan a `callTracer` trace tree for `createCrossChainProxy` calls and
/// extract ephemeral proxy identity from return data.
///
/// Unlike [`super::walker::walk_trace_tree`], this function does NOT require a
/// `ProxyLookup` and only populates the ephemeral proxy map — no `DetectedCall`
/// output.
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
    // Try to deserialize as CallTraceNode for typed access.
    if let Some(typed) = CallTraceNode::try_parse(node) {
        extract_ephemeral_proxies_typed(&typed, manager_addresses, ephemeral_proxies);
    }
}

/// Internal typed implementation of ephemeral proxy extraction.
fn extract_ephemeral_proxies_typed(
    node: &CallTraceNode,
    manager_addresses: &[Address],
    ephemeral_proxies: &mut HashMap<Address, ProxyInfo>,
) {
    if let Some(parsed) = trace_node_from_typed(node) {
        let create_selector = create_cross_chain_proxy_selector();
        if manager_addresses.contains(&parsed.to) && has_selector(&parsed.input, &create_selector) {
            if let Some((proxy_addr, info)) = try_extract_ephemeral_proxy(node, &parsed.input) {
                ephemeral_proxies.insert(proxy_addr, info);
            }
        }
    }

    // Recurse into children.
    for child in node.children() {
        extract_ephemeral_proxies_typed(child, manager_addresses, ephemeral_proxies);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
//  Proxy identity resolution
// ──────────────────────────────────────────────────────────────────────────────

/// Resolve a proxy's identity using (in priority order):
/// 1. The `proxy_cache` (previously resolved addresses)
/// 2. The `ephemeral_proxies` map (proxies created in the same trace)
/// 3. The on-chain `ProxyLookup` query
///
/// Results are always cached in `proxy_cache` for subsequent lookups.
pub(crate) async fn resolve_proxy_info(
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
