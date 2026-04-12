//! Trace tree walker — depth-first traversal of `callTracer` output.
//!
//! The single entry point [`walk_trace_tree`] recurses through a `callTracer`
//! JSON trace and populates a `Vec<DetectedCall>` with all cross-chain proxy
//! calls detected via protocol-level mechanisms.

use alloy_primitives::Address;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

use super::proxy::{resolve_proxy_info, try_extract_ephemeral_proxy};
use super::types::{
    DetectedCall, ProxyInfo, ProxyLookup, create_cross_chain_proxy_selector,
    execute_cross_chain_call_selector, has_selector, parse_trace_node,
};

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
            let child_self_error = child.get("error").and_then(|v| v.as_str());
            let child_reverted = in_reverted_frame || child_self_error.is_some();
            // Diagnostic log: when a child becomes reverted because of its OWN
            // error (not just inherited), log the to/from/error so we can tell
            // which call is poisoning the descendants.
            if !in_reverted_frame && child_self_error.is_some() {
                let child_to = child.get("to").and_then(|v| v.as_str()).unwrap_or("?");
                let child_from = child.get("from").and_then(|v| v.as_str()).unwrap_or("?");
                let child_input = child
                    .get("input")
                    .and_then(|v| v.as_str())
                    .map(|s| s.chars().take(10).collect::<String>())
                    .unwrap_or_default();
                tracing::debug!(
                    target: "based_rollup::trace",
                    parent_depth = depth,
                    child_to,
                    child_from,
                    child_sel = %child_input,
                    child_error = %child_self_error.unwrap_or("?"),
                    "child node has error — descendants will be in_reverted_frame"
                );
            }
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
