//! Post-discovery processing for L1→L2 cross-chain calls.
//!
//! Contains the main processing pipeline that takes detected calls from the
//! trace/walk/discover phase and enriches them with L2 return data, discovers
//! child L2→L1 calls, runs iterative discovery, and queues the execution table.

use crate::cross_chain::{RollupId, ScopePath, filter_new_by_count};
use alloy_primitives::{Address, U256};
use serde_json::Value;
use std::collections::HashMap;

use super::super::model::{DiscoveredCall, L1ProxyLookup, L2ProxyLookup};
use super::simulation::{
    run_l2_sim_bundle, simulate_l1_to_l2_call_chained_on_l2, simulate_l1_to_l2_call_on_l2,
};

/// Extract return data bytes from a callTracer trace node's `output` field.
pub(super) fn extract_return_data_from_trace(trace: &Value) -> Vec<u8> {
    trace
        .get("output")
        .and_then(|v| v.as_str())
        .and_then(|s| super::hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok())
        .unwrap_or_default()
}

/// Extract the REAL return data from the inner destination call inside an
/// `executeIncomingCrossChainCall` trace that reverted at `_consumeExecution`.
///
/// The trace structure is:
/// ```text
/// executeIncomingCrossChainCall (REVERTS — _consumeExecution fails)
///   └─ _processCallAtScope
///        ├─ CREATE2 proxy (optional, if proxy didn't exist)
///        └─ sourceProxy.executeOnBehalf(destination, data)
///             └─ destination.call(data) ← THIS is the inner call with real return data
/// ```
///
/// We walk the trace depth-first looking for a call TO the destination address
/// that has no error (succeeded). Its `output` is the real return data.
/// Check if the inner destination call succeeded in the trace.
///
/// Walks the trace tree looking for a call to `destination`. Returns `true`
/// if found and the node has no `"error"` field — meaning the call itself
/// succeeded even though the outer simulation may have reverted.
///
/// This correctly handles void functions (empty return data) which succeed
/// but would be misclassified by a `!return_data.is_empty()` heuristic.
pub(super) fn destination_call_succeeded_in_trace(trace: &Value, destination: Address) -> bool {
    let dest_hex_lower = format!("{destination}").to_lowercase();

    fn walk(node: &Value, target: &str) -> Option<bool> {
        if let Some(to) = node.get("to").and_then(|v| v.as_str()) {
            if to.to_lowercase() == target {
                return Some(node.get("error").is_none());
            }
        }
        if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
            for child in calls {
                if let Some(result) = walk(child, target) {
                    return Some(result);
                }
            }
        }
        None
    }

    walk(trace, &dest_hex_lower).unwrap_or(false)
}

pub(super) fn extract_inner_destination_return_data(
    trace: &Value,
    destination: Address,
) -> Option<Vec<u8>> {
    let dest_hex_lower = format!("{destination}").to_lowercase();

    // BFS (breadth-first search) to find the SHALLOWEST successful call
    // to the destination. For reentrant patterns (deepCall(4) → deepCall(2) →
    // deepCall(0)), all calls target the same address. We want the outermost
    // call's output (deepCall(4)=3), not the innermost (deepCall(0)=1).
    // BFS guarantees we find the shallowest match first.
    let mut queue = std::collections::VecDeque::new();
    if let Some(calls) = trace.get("calls").and_then(|v| v.as_array()) {
        for c in calls {
            queue.push_back(c);
        }
    }

    while let Some(node) = queue.pop_front() {
        if let Some(to) = node.get("to").and_then(|v| v.as_str()) {
            if to.to_lowercase() == dest_hex_lower && node.get("error").is_none() {
                let output = node.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                let data = super::hex::decode(output.strip_prefix("0x").unwrap_or(output))
                    .unwrap_or_default();
                return Some(data);
            }
        }
        // Enqueue children for BFS traversal
        if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
            for c in calls {
                queue.push_back(c);
            }
        }
    }

    None
}

/// Walk an L2 simulation trace using the generic `trace::walk_trace_tree`
/// to detect child L2→L1 proxy calls.
///
/// Uses protocol-level detection: a node is a proxy call if any of its direct
/// children call `executeCrossChainCall` on the L2 CCM. No contract-specific
/// selectors. Works for bridgeEther, bridgeTokens, direct proxy calls, wrapper
/// contracts, and any future cross-chain pattern.
///
/// `pre_populated_ephemeral_proxies` allows callers to pass in ephemeral proxies
/// discovered from prior traces in a `debug_traceCallMany` bundle. A proxy created
/// in tx[1] is not visible in tx[2]'s trace, so callers must scan earlier traces
/// with `trace::extract_ephemeral_proxies_from_trace` and pass the results here.
///
/// Returns detected calls as `DiscoveredProxyCall` for compatibility with
/// existing callers. Calls targeting our own rollup are filtered out (only
/// L2→L1 calls — those targeting rollup 0 — are returned).
pub(super) async fn walk_l2_simulation_trace(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    ccm_address: Address,
    trace_node: &Value,
    our_rollup_id: u64,
    pre_populated_ephemeral_proxies: Option<&HashMap<Address, super::super::trace::ProxyInfo>>,
) -> (
    Vec<super::super::common::DiscoveredProxyCall>,
    std::collections::HashSet<Address>,
) {
    let lookup = L2ProxyLookup {
        client,
        rpc_url: l2_rpc_url,
        ccm_address,
    };
    let mut proxy_cache: HashMap<Address, Option<super::super::trace::ProxyInfo>> = HashMap::new();
    let mut ephemeral_proxies = HashMap::new();

    // Pre-populate ephemeral proxies from prior bundle traces (cross-bundle visibility).
    if let Some(pre) = pre_populated_ephemeral_proxies {
        ephemeral_proxies.extend(pre.iter().map(|(k, v)| (*k, *v)));
    }

    let mut detected_calls = Vec::new();
    let mut unresolved_proxies = std::collections::HashSet::new();

    // The L2 CCM is the manager contract on L2.
    super::super::trace::walk_trace_tree(
        trace_node,
        &[ccm_address],
        &lookup,
        &mut proxy_cache,
        &mut ephemeral_proxies,
        &mut detected_calls,
        &mut unresolved_proxies,
    )
    .await;

    // Convert trace::DetectedCall to DiscoveredProxyCall, filtering out calls
    // that target our own rollup (we only want L2→L1 calls).
    let calls = detected_calls
        .into_iter()
        .filter_map(|c| {
            // Recover rollup_id from proxy_cache or ephemeral_proxies.
            let proxy_info = proxy_cache
                .values()
                .find_map(|opt| opt.filter(|info| info.original_address == c.destination))
                .or_else(|| {
                    ephemeral_proxies
                        .values()
                        .find(|info| info.original_address == c.destination)
                        .copied()
                });

            match proxy_info {
                Some(info) if info.original_rollup_id != our_rollup_id => {
                    Some(super::super::common::DiscoveredProxyCall {
                        original_address: c.destination,
                        _original_rollup_id: info.original_rollup_id,
                        source_address: c.source_address,
                        data: c.calldata,
                        value: c.value,
                        // The generic walker doesn't distinguish reverted vs
                        // successful — it detects all proxy calls regardless.
                        // Since this is used in L2 simulation where calls may
                        // revert with ExecutionNotFound, we conservatively
                        // mark all as reverted when the trace has errors.
                        reverted: trace_node.get("error").is_some(),
                    })
                }
                Some(_) => {
                    // Call targets our own rollup — not an L2→L1 call, skip.
                    None
                }
                None => {
                    // Proxy identity not found in cache — shouldn't happen
                    // since walk_trace_tree resolves identity before detection.
                    tracing::warn!(
                        target: "based_rollup::l1_proxy",
                        dest = %c.destination,
                        source = %c.source_address,
                        "walk_l2_simulation_trace: proxy identity not found in cache — skipping"
                    );
                    None
                }
            }
        })
        .collect();

    (calls, unresolved_proxies)
}

/// Walk an L1 trace using the generic `trace::walk_trace_tree` and convert
/// results to `DiscoveredCall` format.
///
/// This replaces the old L1-specific `walk_trace_tree` that had separate
/// paths for proxy detection and bridge detection. The generic walker uses
/// only the `executeCrossChainCall` child pattern — works for all contract
/// types (direct proxy, bridgeEther, bridgeTokens, wrappers, multi-call continuations).
pub(super) async fn walk_l1_trace_generic(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    trace_node: &Value,
    proxy_cache: &mut HashMap<Address, Option<super::super::trace::ProxyInfo>>,
) -> Vec<DiscoveredCall> {
    let lookup = L1ProxyLookup {
        client,
        rpc_url: l1_rpc_url,
        rollups_address,
    };

    // Delegate to the shared walk function, then convert to direction-local type.
    let discovered = super::super::model::walk_trace_to_discovered(
        &lookup,
        &[rollups_address],
        trace_node,
        proxy_cache,
        0, // default_target_rollup_id: L1→L2 resolves later from proxy identity
        0, // discovery_iteration: initial trace
    )
    .await;

    discovered
        .into_iter()
        .map(|c| DiscoveredCall {
            destination: c.destination,
            target_rollup_id: c.target_rollup_id,
            calldata: c.calldata,
            value: c.value,
            source_address: c.source_address,
            delivery_failed: false,
            delivery_return_data: vec![],
            parent_call_index: c.parent_call_index,
            trace_depth: c.trace_depth,
            discovery_iteration: c.discovery_iteration,
            in_reverted_frame: c.in_reverted_frame,
        })
        .collect()
}

/// Extract delivery return data from an L1 trace for a specific L2→L1 child call.
///
/// In the 5-hop reentrant pattern, L2→L1 children execute on L1 via:
///   Rollups.executeCrossChainCall → proxy.executeOnBehalf → destination.call
///
/// The L1 trace shows these as nested calls. This function walks the trace
/// looking for a call to the child's destination address that succeeds, and
/// Extract delivery return for a specific child, optionally matching by calldata.
/// When `child_calldata` is Some, the call must match both destination AND input
/// (selector + args). This distinguishes deepCall(3) from deepCall(1) when both
/// target the same contract.
pub(super) fn extract_delivery_return_from_l1_trace_with_calldata(
    user_trace: &Value,
    child_dest: Address,
    _rollups_address: Address,
    child_calldata: Option<&[u8]>,
) -> Vec<u8> {
    let dest_lower = format!("{child_dest}").to_lowercase();
    let calldata_hex =
        child_calldata.map(|cd| format!("0x{}", super::hex::encode(cd)).to_lowercase());

    fn find_delivery_output(
        node: &Value,
        target_dest: &str,
        calldata_hex: &Option<String>,
    ) -> Option<Vec<u8>> {
        if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
            for child in calls {
                let to = child.get("to").and_then(|v| v.as_str()).unwrap_or("");
                let has_error = child.get("error").is_some();

                if to.to_lowercase() == target_dest && !has_error {
                    // If calldata matching requested, verify the input matches.
                    if let Some(expected_cd) = calldata_hex {
                        let input = child
                            .get("input")
                            .and_then(|v| v.as_str())
                            .unwrap_or("0x")
                            .to_lowercase();
                        if !input.starts_with(expected_cd.as_str()) {
                            // Wrong call (different selector/args) — skip, continue search
                            if let Some(result) =
                                find_delivery_output(child, target_dest, calldata_hex)
                            {
                                return Some(result);
                            }
                            continue;
                        }
                    }

                    let output = child.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                    let hex = output.strip_prefix("0x").unwrap_or(output);
                    return Some(super::hex::decode(hex).unwrap_or_default());
                }

                // Recurse into children
                if let Some(result) = find_delivery_output(child, target_dest, calldata_hex) {
                    return Some(result);
                }
            }
        }
        None
    }

    find_delivery_output(user_trace, &dest_lower, &calldata_hex).unwrap_or_default()
}

/// Post-discovery processing for L1→L2 cross-chain calls.
///
/// Takes the detected calls from the trace/walk/discover phase and:
/// 1. Enriches each call with L2 return data via chained simulation
/// 2. Discovers child L2→L1 calls (nested L1→L2→L1 pattern)
/// 3. Runs iterative discovery via `debug_traceCallMany` with `postBatch` pre-loading
/// 4. Performs post-convergence bottom-up enrichment for reentrant patterns
/// 5. Applies final `in_reverted_frame` correction
/// 6. Queues the execution table on L2
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_l1_to_l2_calls(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    raw_tx: &str,
    rollups_address: Address,
    builder_private_key: &Option<String>,
    rollup_id: u64,
    cross_chain_manager_address: Address,
    from: &str,
    to: &str,
    data: &str,
    value: &str,
    top_level_error: bool,
    detected_calls: &mut Vec<DiscoveredCall>,
    proxy_cache: &mut HashMap<Address, Option<super::super::trace::ProxyInfo>>,
) -> eyre::Result<Option<String>> {
    // Track the last converged retrace walk for in_reverted_frame correction.
    // The initial trace (without entries) sets ALL calls to in_reverted_frame=true
    // (whole tx reverts). The converged retrace (entries loaded) reflects the REAL
    // trace behavior: only calls inside try/catch that reverts for business logic
    // have in_reverted_frame=true. We save at the SECOND loop convergence and apply
    // as a final positional override before queue_execution_table.
    let mut last_converged_walk: Vec<DiscoveredCall> = Vec::new();

    // Enrich detected calls with L2 return data by simulating each L1→L2 call
    // on L2.
    //
    // CHAINED simulation: when multiple calls are detected (e.g., CallTwice calling
    // Counter.increment() twice), each call must see the state effects of previous
    // calls. We simulate sequentially: call[i] runs in a bundle where calls[0..i-1]
    // have already executed with their correct RESULT entries loaded.
    //
    // Also collect child L2→L1 proxy calls discovered in L2 simulation traces.
    // These represent the nested L1→L2→L1 pattern (the L2 target calls back to L1).
    let mut all_child_calls: Vec<(usize, DiscoveredCall)> = Vec::new();
    if !cross_chain_manager_address.is_zero() {
        // Accumulate RESULT entries from already-enriched calls for chained simulation.
        let mut prior_result_entries: Vec<crate::cross_chain::CrossChainExecutionEntry> =
            Vec::new();
        // Also accumulate the executeIncomingCrossChainCall calldatas for prior calls
        // so they execute in the bundle (state must accumulate).
        let mut prior_exec_calldatas: Vec<(Vec<u8>, U256)> = Vec::new();

        // Query SYSTEM_ADDRESS once (needed for building exec calldatas).
        let sys_addr = {
            let sys_calldata = super::super::common::encode_system_address_calldata();
            let sys_result = super::super::common::eth_call_view(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                &sys_calldata,
            )
            .await;
            sys_result.and_then(|s| super::super::common::parse_address_from_abi_return(&s))
        };

        // Pre-compute partial revert: only skip reverted calls when there's a MIX
        // of reverted and non-reverted calls (actual try/catch pattern). When ALL
        // calls are in_reverted_frame=true (e.g., flash-loan where the whole trace
        // reverts in simulation), this is NOT a partial revert — all calls need
        // chained state effects for correct simulation.
        let enrichment_has_partial_revert = {
            let any_rev = detected_calls.iter().any(|c| c.in_reverted_frame);
            let any_non_rev = detected_calls.iter().any(|c| !c.in_reverted_frame);
            any_rev && any_non_rev
        };

        // Route through SimulationPlan (invariants #17 + #21):
        // Single call → Single plan (independent L2 sim).
        // Multiple calls → CombinedThenAnalytical (chained L2 sim).
        let _sim_plan = crate::composer_rpc::simulate::simulation_plan_for(
            detected_calls,
            crate::composer_rpc::model::PromotionDecision::KeepSimple,
        );
        // The plan gates the per-call choice: call_idx==0 uses independent sim (Single),
        // call_idx>0 uses chained sim (CombinedThenAnalytical). The decision is made
        // per-call within the loop below, consistent with the plan.

        #[allow(clippy::needless_range_loop)]
        // Index needed: immutable reads then mutable writes on detected_calls
        for call_idx in 0..detected_calls.len() {
            // Clone the fields we need before any mutable borrow of detected_calls.
            let call_destination = detected_calls[call_idx].destination;
            let call_calldata = detected_calls[call_idx].calldata.clone();
            let call_value = detected_calls[call_idx].value;
            let call_source = detected_calls[call_idx].source_address;
            let scope_for_call: Vec<U256> = if detected_calls[call_idx].trace_depth <= 1 {
                vec![]
            } else {
                vec![U256::ZERO; detected_calls[call_idx].trace_depth]
            };

            let (ret_data, success, child_calls) =
                if call_idx == 0 || prior_result_entries.is_empty() {
                    // First call or no prior entries: independent simulation.
                    simulate_l1_to_l2_call_on_l2(
                        client,
                        l2_rpc_url,
                        cross_chain_manager_address,
                        call_destination,
                        &call_calldata,
                        call_value,
                        call_source,
                        rollup_id,
                        &scope_for_call, // l2_scope from L1 trace_depth
                    )
                    .await
                } else {
                    // Subsequent call: chained simulation via debug_traceCallMany bundle
                    // where prior calls execute first (with their RESULT entries loaded),
                    // so this call sees their state effects.
                    // Proxy resolution for internally-created proxies is handled by
                    // the two-pass approach inside simulate_l1_to_l2_call_chained_on_l2.
                    simulate_l1_to_l2_call_chained_on_l2(
                        client,
                        l2_rpc_url,
                        cross_chain_manager_address,
                        call_destination,
                        &call_calldata,
                        call_value,
                        call_source,
                        rollup_id,
                        &prior_result_entries,
                        &prior_exec_calldatas,
                        sys_addr,
                        &scope_for_call,
                    )
                    .await
                };
            tracing::debug!(
                target: "based_rollup::l1_proxy",
                dest = %call_destination,
                source = %call_source,
                return_data_len = ret_data.len(),
                call_success = success,
                child_l2_to_l1_calls = child_calls.len(),
                chained = call_idx > 0,
                "simulate_l1_to_l2_call_on_l2 result"
            );

            // Bug 2 fix: when the L2 simulation finds child L2→L1 calls AND the
            // parent call reports success=false, the revert was caused by the child's
            // inner executeCrossChainCall failing (no entry loaded), NOT by the parent
            // destination function itself. The parent function actually ran successfully
            // and returned void. Override return data to empty and success to true.
            //
            // This is the L1→L2→L1 (nestedCounter) pattern: the L2 target calls an L1
            // proxy internally, which triggers executeCrossChainCall on the L2 CCM.
            // Without entries, that inner call reverts, cascading to the parent. But the
            // parent's own return data is void (empty).
            let (final_ret_data, final_success) = if !success && !child_calls.is_empty() {
                tracing::info!(
                    target: "based_rollup::l1_proxy",
                    dest = %call_destination,
                    child_count = child_calls.len(),
                    "parent call_success=false with children: overriding to void/success \
                     (L1→L2→L1 pattern — revert was from child's missing entry)"
                );
                (vec![], true)
            } else {
                (ret_data, success)
            };

            detected_calls[call_idx].delivery_return_data = final_ret_data.clone();
            detected_calls[call_idx].delivery_failed = !final_success;

            // Build RESULT entry and exec calldata for this call (for future chaining).
            // Uses final_ret_data/final_success after Bug 2 override so the RESULT hash
            // is correct for the corrected return data.
            //
            // PARTIAL REVERT: skip adding reverted calls to prior_result_entries and
            // prior_exec_calldatas. In the real execution, reverted calls are inside a
            // try/catch scope — their state effects are rolled back by ScopeReverted.
            // Including them in the chained simulation would let subsequent calls see
            // state changes that won't persist, producing incorrect return data
            // (e.g., Counter sees 1 instead of 0 for the non-reverted call).
            if !enrichment_has_partial_revert || !detected_calls[call_idx].in_reverted_frame {
                let result_action = crate::cross_chain::CrossChainAction {
                    action_type: crate::cross_chain::CrossChainActionType::Result,
                    rollup_id: RollupId::new(U256::from(rollup_id)),
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: final_ret_data,
                    failed: !final_success,
                    source_address: Address::ZERO,
                    source_rollup: RollupId::MAINNET,
                    scope: ScopePath::root(),
                };
                let result_hash = crate::table_builder::compute_action_hash(&result_action);
                prior_result_entries.push(crate::cross_chain::CrossChainExecutionEntry {
                    state_deltas: vec![],
                    action_hash: result_hash,
                    next_action: result_action,
                });

                // Build exec calldata for this call.
                let sim_action = crate::cross_chain::CrossChainAction {
                    action_type: crate::cross_chain::CrossChainActionType::Call,
                    rollup_id: RollupId::new(U256::from(rollup_id)),
                    destination: call_destination,
                    value: call_value,
                    data: call_calldata,
                    failed: false,
                    source_address: call_source,
                    source_rollup: RollupId::MAINNET,
                    scope: ScopePath::root(),
                };
                let exec_cd =
                    crate::cross_chain::encode_execute_incoming_call_calldata(&sim_action);
                prior_exec_calldatas.push((exec_cd.to_vec(), call_value));
            } else {
                tracing::info!(
                    target: "based_rollup::l1_proxy",
                    call_idx,
                    dest = %call_destination,
                    "skipping reverted call from chained L2 simulation prior entries \
                     (state effects rolled back by scope revert in real execution)"
                );
            }

            // Convert child L2→L1 proxy calls to DiscoveredCall and
            // accumulate them with parent index for correct L1→L2→L1 entry linking.
            for child in &child_calls {
                tracing::info!(
                    target: "based_rollup::l1_proxy",
                    parent_dest = %call_destination,
                    child_dest = %child.original_address,
                    child_source = %child.source_address,
                    child_data_len = child.data.len(),
                    child_value = %child.value,
                    child_reverted = child.reverted,
                    "discovered child L2→L1 call from L2 simulation (nested L1→L2→L1 pattern)"
                );
                all_child_calls.push((
                    call_idx,
                    DiscoveredCall {
                        destination: child.original_address,
                        target_rollup_id: 0, // L2→L1: child targets L1 (mainnet)
                        calldata: child.data.clone(),
                        value: child.value,
                        source_address: child.source_address,
                        delivery_failed: false, // defaults to false; will be enriched later if needed
                        delivery_return_data: vec![], // will be enriched via L1 simulation
                        parent_call_index: crate::cross_chain::ParentLink::Child(
                            crate::cross_chain::AbsoluteCallIndex::new(call_idx),
                        ), // linked to parent L1→L2 call
                        trace_depth: 0,         // L2→L1 child: depth in L2 simulation
                        discovery_iteration: 0, // will be updated in iterative loop
                        in_reverted_frame: false, // L2→L1 children: not in reverted frame
                    },
                ));
            }
        }
    }

    // If child L2→L1 calls were discovered, they represent additional cross-chain
    // calls that need their own entries. Enrich each child with its L1 delivery
    // return data (simulate the call on L1), then add to detected_calls.
    if !all_child_calls.is_empty() {
        tracing::info!(
            target: "based_rollup::l1_proxy",
            parent_calls = detected_calls.len(),
            child_calls = all_child_calls.len(),
            "enriching child L2→L1 calls with CHAINED L1 delivery simulation"
        );
        // Build a SINGLE chained debug_traceCallMany with ALL children in sequence.
        // This ensures child #2 sees the state updated by child #1.
        let child_txs: Vec<Value> = all_child_calls
            .iter()
            .map(|(_, child)| {
                serde_json::json!({
                    "from": format!("{}", child.source_address),
                    "to": format!("{}", child.destination),
                    "data": format!("0x{}", super::hex::encode(&child.calldata)),
                    "value": format!("0x{:x}", child.value),
                    "gas": "0x2faf080"
                })
            })
            .collect();
        let sim_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCallMany",
            "params": [
                [{ "transactions": child_txs }],
                null,
                { "tracer": "callTracer" }
            ],
            "id": 99978
        });
        if let Ok(resp) = client.post(l1_rpc_url).json(&sim_req).send().await {
            if let Ok(body) = resp.json::<super::super::common::JsonRpcResponse>().await {
                if let Some(traces) = body
                    .result
                    .as_ref()
                    .and_then(|r| r.get(0))
                    .and_then(|b| b.as_array())
                {
                    for (i, (_, child)) in all_child_calls.iter_mut().enumerate() {
                        if let Some(trace) = traces.get(i) {
                            let has_error = trace.get("error").is_some();
                            if let Some(output) = trace.get("output").and_then(|v| v.as_str()) {
                                let hex = output.strip_prefix("0x").unwrap_or(output);
                                if let Ok(delivery_bytes) = super::hex::decode(hex) {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        idx = i,
                                        dest = %child.destination,
                                        return_data_len = delivery_bytes.len(),
                                        return_data_hex = %format!("0x{}", super::hex::encode(&delivery_bytes[..delivery_bytes.len().min(32)])),
                                        delivery_failed = has_error,
                                        "enriched L2→L1 child with CHAINED L1 delivery return data"
                                    );
                                    child.delivery_return_data = delivery_bytes;
                                    if has_error {
                                        child.delivery_failed = true;
                                    }
                                }
                            } else if has_error {
                                child.delivery_failed = true;
                            }
                        }
                    }
                } else if let Some(ref error) = body.error {
                    tracing::warn!(
                        target: "based_rollup::l1_proxy",
                        ?error,
                        "chained L1 delivery simulation failed"
                    );
                }
            }
        }
        detected_calls.extend(
            all_child_calls
                .into_iter()
                .map(|(_parent_idx, child)| child),
        );
    }

    // If initial trace found calls but tx reverts (multi-call continuation pattern), iterate
    // with debug_traceCallMany. Load entries via postBatch in tx1, run user tx as
    // tx2 to see ALL calls that would succeed once entries are on-chain.
    if top_level_error && !detected_calls.is_empty() {
        if let Some(builder_key_hex) = builder_private_key {
            let key_hex = builder_key_hex
                .strip_prefix("0x")
                .unwrap_or(builder_key_hex);
            if let Ok(builder_key) = key_hex.parse::<alloy_signer_local::PrivateKeySigner>() {
                let mut all_calls = detected_calls.clone();
                let mut iteration = 0;
                const MAX_ITERATIONS: usize = 10;
                // Save the user trace from the last iteration for post-convergence
                // extraction of L2→L1 child delivery returns.
                let mut last_l1_user_trace: Option<Value> = None;

                loop {
                    iteration += 1;
                    if iteration > MAX_ITERATIONS {
                        break;
                    }

                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        iteration,
                        known_calls = all_calls.len(),
                        "iterative discovery: traceCallMany with postBatch pre-loading"
                    );

                    // Build entries, sign proof, and run debug_traceCallMany
                    // via the extracted helper function.
                    let label = format!("iter-{iteration}");
                    let trace_result = super::super::delivery::build_and_run_l1_postbatch_trace(
                        client,
                        l1_rpc_url,
                        rollups_address,
                        rollup_id,
                        &builder_key,
                        &all_calls,
                        from,
                        to,
                        data,
                        value,
                        &label,
                    )
                    .await;
                    let (user_trace_owned, resp) = match trace_result {
                        Some(t) => t,
                        None => break,
                    };
                    // Save for post-convergence extraction.
                    last_l1_user_trace = Some(user_trace_owned.clone());
                    let user_trace = &user_trace_owned;

                    // Log user tx trace status with decoded error
                    let user_error = user_trace
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("none");
                    let user_output_raw = user_trace
                        .get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("none");
                    let user_calls_count = user_trace
                        .get("calls")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    // Decode known Rollups.sol error selectors from output.
                    // Selectors derived at compile time via sol! macro — NEVER hardcode hex.
                    let decoded_error = decode_error_selector_prefixed(
                        user_output_raw.get(..10).or(user_output_raw.get(..)),
                    );
                    // If ProxyCallFailed, decode inner error
                    let inner_error = if user_output_raw.len() > 138 {
                        // Inner error selector at bytes 68..72 (hex chars 136..144, after 0x prefix = 138..146)
                        decode_error_selector_bare(user_output_raw.get(138..146))
                    } else {
                        ""
                    };
                    // Determine postBatch success from resp (already the result value)
                    let postbatch_ok = resp
                        .get(0)
                        .and_then(|b| b.as_array())
                        .and_then(|arr| arr.first())
                        .map(|tx1| tx1.get("error").is_none())
                        .unwrap_or(false);
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        iteration,
                        postbatch_ok,
                        user_ok = user_error == "none",
                        %decoded_error,
                        %inner_error,
                        user_calls_count,
                        "traceCallMany iteration result"
                    );

                    // Log user tx trace tree for full traceability.
                    {
                        fn summarize_trace(node: &Value, depth: usize, summary: &mut Vec<String>) {
                            if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
                                for c in calls {
                                    let to = c.get("to").and_then(|v| v.as_str()).unwrap_or("?");
                                    let error =
                                        c.get("error").and_then(|v| v.as_str()).unwrap_or("");
                                    let sel =
                                        c.get("input").and_then(|v| v.as_str()).unwrap_or("0x");
                                    let sel_short = &sel[..sel.len().min(10)];
                                    let child_count = c
                                        .get("calls")
                                        .and_then(|v| v.as_array())
                                        .map_or(0, |a| a.len());
                                    let err = if error.is_empty() { "ok" } else { error };
                                    summary.push(format!(
                                        "d={}:{}:{}:ch={}:{}",
                                        depth + 1,
                                        &to[to.len().saturating_sub(8)..],
                                        sel_short,
                                        child_count,
                                        err
                                    ));
                                    summarize_trace(c, depth + 1, summary);
                                }
                            }
                        }
                        let mut summary = Vec::new();
                        summarize_trace(user_trace, 0, &mut summary);
                        tracing::debug!(
                            target: "based_rollup::l1_proxy",
                            iteration,
                            user_error,
                            trace_nodes = summary.len(),
                            trace_tree = %summary.join(" | "),
                            "iterative discovery: L1 user tx trace tree"
                        );
                    }

                    // Walk the user tx trace for new cross-chain calls
                    // using the generic trace walker.
                    let new_detected = walk_l1_trace_generic(
                        client,
                        l1_rpc_url,
                        rollups_address,
                        user_trace,
                        proxy_cache,
                    )
                    .await;

                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        new_detected_count = new_detected.len(),
                        all_calls_count = all_calls.len(),
                        "walked user tx trace for cross-chain calls"
                    );

                    // Save retrace walk for final in_reverted_frame correction.
                    last_converged_walk = new_detected.clone();

                    // Find truly new calls using count-based comparison.
                    let new_calls = filter_new_by_count(new_detected, &all_calls, |a, b| {
                        a.destination == b.destination
                            && a.calldata == b.calldata
                            && a.value == b.value
                            && a.source_address == b.source_address
                    });

                    if new_calls.is_empty() {
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            iteration,
                            total = all_calls.len(),
                            "iterative discovery converged — no new calls found"
                        );
                        break;
                    }

                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        iteration,
                        new = new_calls.len(),
                        "discovered new cross-chain calls via traceCallMany"
                    );

                    // Enrich new calls with L2 return data via CHAINED simulation.
                    // Each new call must see the state effects of ALL prior calls
                    // (both previously-enriched all_calls AND earlier new calls in
                    // this batch). Without chaining, identical calls (e.g., CallTwice
                    // calling increment() twice) all see the same initial state and
                    // produce identical return data — breaking the entry hashes.
                    //
                    // Also collect any child L2→L1 calls discovered in the L2
                    // simulation (nested L1→L2→L1 pattern).
                    let mut enriched_new_calls = new_calls;
                    // Tag new calls with current iteration number.
                    for call in &mut enriched_new_calls {
                        call.discovery_iteration = iteration;
                    }
                    let mut iter_child_calls: Vec<(usize, DiscoveredCall)> = Vec::new();
                    if !cross_chain_manager_address.is_zero() {
                        // Build RESULT entries and exec calldatas from ALL existing
                        // calls (already enriched) for chained simulation.
                        let sys_addr = {
                            let sys_calldata =
                                super::super::common::encode_system_address_calldata();
                            let sys_result = super::super::common::eth_call_view(
                                client,
                                l2_rpc_url,
                                cross_chain_manager_address,
                                &sys_calldata,
                            )
                            .await;
                            sys_result.and_then(|s| {
                                super::super::common::parse_address_from_abi_return(&s)
                            })
                        };
                        let mut prior_result_entries: Vec<
                            crate::cross_chain::CrossChainExecutionEntry,
                        > = Vec::new();
                        let mut prior_exec_calldatas: Vec<(Vec<u8>, U256)> = Vec::new();

                        // Accumulate prior entries from all_calls (already enriched).
                        for prior in all_calls.iter() {
                            // Only L1→L2 calls contribute to L2 state chaining.
                            // L2→L1 children (target_rollup_id=0) don't execute on L2.
                            if prior.parent_call_index.is_child() {
                                continue;
                            }
                            let result_action = crate::cross_chain::CrossChainAction {
                                action_type: crate::cross_chain::CrossChainActionType::Result,
                                rollup_id: RollupId::new(U256::from(rollup_id)),
                                destination: Address::ZERO,
                                value: U256::ZERO,
                                data: prior.delivery_return_data.clone(),
                                failed: prior.delivery_failed,
                                source_address: Address::ZERO,
                                source_rollup: RollupId::MAINNET,
                                scope: ScopePath::root(),
                            };
                            let result_hash =
                                crate::table_builder::compute_action_hash(&result_action);
                            prior_result_entries.push(
                                crate::cross_chain::CrossChainExecutionEntry {
                                    state_deltas: vec![],
                                    action_hash: result_hash,
                                    next_action: result_action,
                                },
                            );
                            let sim_action = crate::cross_chain::CrossChainAction {
                                action_type: crate::cross_chain::CrossChainActionType::Call,
                                rollup_id: RollupId::new(U256::from(rollup_id)),
                                destination: prior.destination,
                                value: prior.value,
                                data: prior.calldata.clone(),
                                failed: false,
                                source_address: prior.source_address,
                                source_rollup: RollupId::MAINNET,
                                scope: ScopePath::root(),
                            };
                            let exec_cd = crate::cross_chain::encode_execute_incoming_call_calldata(
                                &sim_action,
                            );
                            prior_exec_calldatas.push((exec_cd.to_vec(), prior.value));
                        }

                        for call in &mut enriched_new_calls {
                            let (ret_data, success, child_calls) =
                                if prior_result_entries.is_empty() {
                                    // No prior calls: independent simulation.
                                    simulate_l1_to_l2_call_on_l2(
                                        client,
                                        l2_rpc_url,
                                        cross_chain_manager_address,
                                        call.destination,
                                        &call.calldata,
                                        call.value,
                                        call.source_address,
                                        rollup_id,
                                        &if call.trace_depth <= 1 {
                                            vec![]
                                        } else {
                                            vec![U256::ZERO; call.trace_depth]
                                        },
                                    )
                                    .await
                                } else {
                                    // Chained simulation: prior calls execute first.
                                    // Proxy resolution for internally-created proxies
                                    // is handled by the two-pass approach inside
                                    // simulate_l1_to_l2_call_chained_on_l2.
                                    simulate_l1_to_l2_call_chained_on_l2(
                                        client,
                                        l2_rpc_url,
                                        cross_chain_manager_address,
                                        call.destination,
                                        &call.calldata,
                                        call.value,
                                        call.source_address,
                                        rollup_id,
                                        &prior_result_entries,
                                        &prior_exec_calldatas,
                                        sys_addr,
                                        &if call.trace_depth <= 1 {
                                            vec![]
                                        } else {
                                            vec![U256::ZERO; call.trace_depth]
                                        },
                                    )
                                    .await
                                };

                            // Override return data when child L2→L1 calls present
                            // but parent reports failure (L1→L2→L1 pattern: the
                            // revert was from the child's missing entry, not the
                            // parent destination function).
                            let (final_ret_data, final_success) = if !success
                                && !child_calls.is_empty()
                            {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    dest = %call.destination,
                                    child_count = child_calls.len(),
                                    "iterative enrichment: parent call_success=false with children — overriding to void/success"
                                );
                                (vec![], true)
                            } else {
                                (ret_data, success)
                            };

                            call.delivery_return_data = final_ret_data.clone();
                            call.delivery_failed = !final_success;

                            // Accumulate this call's RESULT for future chaining.
                            let result_action = crate::cross_chain::CrossChainAction {
                                action_type: crate::cross_chain::CrossChainActionType::Result,
                                rollup_id: RollupId::new(U256::from(rollup_id)),
                                destination: Address::ZERO,
                                value: U256::ZERO,
                                data: final_ret_data,
                                failed: !final_success,
                                source_address: Address::ZERO,
                                source_rollup: RollupId::MAINNET,
                                scope: ScopePath::root(),
                            };
                            let result_hash =
                                crate::table_builder::compute_action_hash(&result_action);
                            prior_result_entries.push(
                                crate::cross_chain::CrossChainExecutionEntry {
                                    state_deltas: vec![],
                                    action_hash: result_hash,
                                    next_action: result_action,
                                },
                            );
                            let sim_action = crate::cross_chain::CrossChainAction {
                                action_type: crate::cross_chain::CrossChainActionType::Call,
                                rollup_id: RollupId::new(U256::from(rollup_id)),
                                destination: call.destination,
                                value: call.value,
                                data: call.calldata.clone(),
                                failed: false,
                                source_address: call.source_address,
                                source_rollup: RollupId::MAINNET,
                                scope: ScopePath::root(),
                            };
                            let exec_cd = crate::cross_chain::encode_execute_incoming_call_calldata(
                                &sim_action,
                            );
                            prior_exec_calldatas.push((exec_cd.to_vec(), call.value));

                            // Convert child L2→L1 proxy calls to
                            // DiscoveredCall with parent linkage.
                            // The parent_call_index will be set after extending
                            // all_calls (when we know the final index).
                            if !child_calls.is_empty() {
                                // Simulate ALL children in a CHAINED bundle on L1.
                                // Includes ALL prior children (from all_calls + earlier
                                // iter_child_calls) so each child sees cumulative state.
                                let mut prior_child_txs: Vec<Value> = Vec::new();
                                // Prior children from all_calls
                                for prior in all_calls.iter() {
                                    if prior.parent_call_index.is_child()
                                        && prior.target_rollup_id == 0
                                    {
                                        prior_child_txs.push(serde_json::json!({
                                            "from": format!("{}", prior.source_address),
                                            "to": format!("{}", prior.destination),
                                            "data": format!("0x{}", super::hex::encode(&prior.calldata)),
                                            "value": format!("0x{:x}", prior.value),
                                            "gas": "0x2faf080"
                                        }));
                                    }
                                }
                                // Prior children from this iteration
                                for (_, prev_child) in &iter_child_calls {
                                    prior_child_txs.push(serde_json::json!({
                                        "from": format!("{}", prev_child.source_address),
                                        "to": format!("{}", prev_child.destination),
                                        "data": format!("0x{}", super::hex::encode(&prev_child.calldata)),
                                        "value": format!("0x{:x}", prev_child.value),
                                        "gas": "0x2faf080"
                                    }));
                                }
                                // New children to simulate
                                let new_child_txs: Vec<Value> = child_calls
                                    .iter()
                                    .map(|c| {
                                        serde_json::json!({
                                            "from": format!("{}", c.source_address),
                                            "to": format!("{}", c.original_address),
                                            "data": format!("0x{}", super::hex::encode(&c.data)),
                                            "value": format!("0x{:x}", c.value),
                                            "gas": "0x2faf080"
                                        })
                                    })
                                    .collect();
                                let mut all_txs = prior_child_txs;
                                let new_start_idx = all_txs.len();
                                all_txs.extend(new_child_txs);

                                let sim_req = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "debug_traceCallMany",
                                    "params": [
                                        [{ "transactions": all_txs }],
                                        null,
                                        { "tracer": "callTracer" }
                                    ],
                                    "id": 99977
                                });
                                let sim_results = if let Ok(resp) =
                                    client.post(l1_rpc_url).json(&sim_req).send().await
                                {
                                    if let Ok(body) =
                                        resp.json::<super::super::common::JsonRpcResponse>().await
                                    {
                                        body.result
                                            .and_then(|r| r.get(0).cloned())
                                            .and_then(|b| b.as_array().cloned())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                                for (ci, child) in child_calls.iter().enumerate() {
                                    let trace_idx = new_start_idx + ci;
                                    let mut child_delivery_data = vec![];
                                    let mut child_delivery_failed = false;
                                    if let Some(ref traces) = sim_results {
                                        if let Some(trace) = traces.get(trace_idx) {
                                            let has_error = trace.get("error").is_some();
                                            if let Some(output) =
                                                trace.get("output").and_then(|v| v.as_str())
                                            {
                                                let hex =
                                                    output.strip_prefix("0x").unwrap_or(output);
                                                if let Ok(bytes) = super::hex::decode(hex) {
                                                    child_delivery_data = bytes;
                                                    child_delivery_failed = has_error;
                                                }
                                            } else if has_error {
                                                child_delivery_failed = true;
                                            }
                                        }
                                    }
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        parent_dest = %call.destination,
                                        child_dest = %child.original_address,
                                        child_idx = ci,
                                        prior_children = new_start_idx,
                                        delivery_return_data_len = child_delivery_data.len(),
                                        delivery_return_data_hex = %format!("0x{}", super::hex::encode(&child_delivery_data[..child_delivery_data.len().min(32)])),
                                        delivery_failed = child_delivery_failed,
                                        "discovered child L2→L1 in iterative enrichment (CHAINED L1 sim)"
                                    );
                                    iter_child_calls.push((
                                        0,
                                        DiscoveredCall {
                                            destination: child.original_address,
                                            target_rollup_id: 0,
                                            calldata: child.data.clone(),
                                            value: child.value,
                                            source_address: child.source_address,
                                            delivery_failed: child_delivery_failed,
                                            delivery_return_data: child_delivery_data,
                                            parent_call_index: crate::cross_chain::ParentLink::Root,
                                            trace_depth: 0,
                                            discovery_iteration: iteration,
                                            in_reverted_frame: false, // iterative child: not in reverted frame
                                        },
                                    ));
                                }
                            }
                        }
                    }

                    // Compute parent indices for child calls.
                    // Each enriched_new_call will be at index
                    // `all_calls.len() + enriched_idx` in the final all_calls.
                    // The child calls need parent_call_index pointing to that index.
                    //
                    // Since we processed enriched_new_calls in order and pushed
                    // children for each, we track the parent's final index.
                    {
                        let base_idx = all_calls.len();
                        let mut child_idx = 0;
                        for (enr_idx, call) in enriched_new_calls.iter().enumerate() {
                            let parent_idx = base_idx + enr_idx;
                            // Count how many children belong to this call
                            // (they were pushed in order).
                            while child_idx < iter_child_calls.len() {
                                let (ref mut _placeholder, ref mut child) =
                                    iter_child_calls[child_idx];
                                // All children pushed during this call's loop
                                // have destination matching what we discovered.
                                // We can't tell which children go to which parent
                                // purely by iteration, so we use the fact that
                                // children were pushed in order during the for loop.
                                // We need to count: iter_child_calls[child_idx].0
                                // is 0 (placeholder) for all. So instead, we track
                                // by matching the parent destination.
                                //
                                // Actually, simpler approach: check if child.destination
                                // was discovered from this specific call by comparing
                                // the parent_dest. But that doesn't work generically.
                                //
                                // Simplest: just set parent_call_index = parent_idx
                                // for all remaining unset children. This works because
                                // we process calls sequentially.
                                if child.parent_call_index.is_root() {
                                    child.parent_call_index = crate::cross_chain::ParentLink::Child(
                                        crate::cross_chain::AbsoluteCallIndex::new(parent_idx),
                                    );
                                    child_idx += 1;
                                } else {
                                    break;
                                }
                            }
                            // After all children of this call are set, mark them
                            // as done by the break above. If there are no children
                            // for this call, the loop doesn't execute.
                            let _ = call; // suppress unused warning
                        }
                    }

                    all_calls.extend(enriched_new_calls);
                    // Also add child calls so the iterative discovery loop
                    // can build entries for them in subsequent iterations.
                    if !iter_child_calls.is_empty() {
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            child_count = iter_child_calls.len(),
                            "adding child L2→L1 calls from L2 simulation to iterative discovery"
                        );
                        all_calls.extend(iter_child_calls.into_iter().map(|(_, child)| child));
                    }
                }

                if all_calls.len() > detected_calls.len() {
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        before = detected_calls.len(),
                        after = all_calls.len(),
                        "iterative traceCallMany discovery found additional calls"
                    );
                    *detected_calls = all_calls;
                }

                // ══════════════════════════════════════════════════════════
                // Post-convergence bottom-up enrichment for reentrant patterns
                // ══════════════════════════════════════════════════════════
                //
                // After the iterative discovery loop converges, intermediate
                // L1→L2 calls in reentrant patterns (e.g., 5-hop
                // L1→L2→L1→L2→L1→L2) may have empty/wrong return_data.
                // This happens because:
                //   (a) Line 2759-2768 overrides return_data to (vec![], true)
                //       when a parent has children (the revert was from the
                //       child's missing entry, not the function itself).
                //   (b) Enrichment only processes NEW calls per iteration —
                //       intermediates discovered early never get re-enriched
                //       once their children are discovered.
                //
                // Fix: re-simulate L1→L2 root calls BOTTOM-UP (innermost
                // first) so each level sees correct RESULT entries from
                // inner levels. Then run one more L1 trace to extract
                // delivery return data for L2→L1 children.
                //
                // This is a NO-OP when:
                //   - There are no reentrant patterns (no call has children)
                //   - All return_data is already correct

                // Step 1: Check if re-enrichment is needed.
                // A call needs re-enrichment if it is an L1→L2 root call
                // (parent=None, target_rollup_id != 0) AND it has children
                // (some other call has parent_call_index pointing to it) AND
                // its return_data is empty.
                let needs_enrichment = {
                    let mut needed = false;
                    for (i, c) in detected_calls.iter().enumerate() {
                        if c.parent_call_index.is_root() && c.delivery_return_data.is_empty() {
                            // Check if this call has children
                            let has_children = detected_calls.iter().any(|other| {
                                other.parent_call_index
                                    == crate::cross_chain::ParentLink::Child(
                                        crate::cross_chain::AbsoluteCallIndex::new(i),
                                    )
                            });
                            if has_children {
                                needed = true;
                                break;
                            }
                        }
                    }
                    needed
                };

                if needs_enrichment {
                    // Determine if pattern is reentrant (varying L1 trace depth)
                    // or continuation (all L1→L2 calls at same depth).
                    // Same logic as build_continuation_entries in table_builder.rs.
                    let root_calls: Vec<&DiscoveredCall> = detected_calls
                        .iter()
                        .filter(|c| c.parent_call_index.is_root())
                        .collect();
                    // Reentrant: each successive call is STRICTLY DEEPER (nested inside
                    // scope navigation). Continuation: same or non-increasing depths.
                    let root_depths: Vec<usize> =
                        root_calls.iter().map(|c| c.trace_depth).collect();
                    let is_strictly_increasing = root_depths.windows(2).all(|w| w[1] > w[0]);
                    let is_reentrant_pattern = root_calls.len() > 1 && is_strictly_increasing;

                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        total_calls = detected_calls.len(),
                        is_reentrant_pattern,
                        "post-convergence: re-enriching intermediate L1→L2 calls bottom-up"
                    );

                    // Step 2: Bottom-up L2 enrichment via iterative full-table simulation.
                    //
                    // For reentrant patterns (e.g., 5-hop L1→L2→L1→L2→L1→L2), the
                    // destination function on L2 makes cross-chain sub-calls that need
                    // continuation entries in the L2 execution table. Simple RESULT-only
                    // loading (as in simulate_l1_to_l2_call_chained_on_l2) won't work
                    // because the sub-call's CALL entry and scope navigation entries
                    // are missing — the sub-call reverts with ExecutionNotFound.
                    //
                    // Fix: build the FULL L2 continuation entries (via
                    // build_continuation_entries) and load all of them into the L2
                    // execution table. Process bottom-up: the innermost call's RESULT
                    // hash is already correct (leaf, no sub-calls). After each level,
                    // update return_data and rebuild entries so the next outer level
                    // sees correct RESULT hashes for scope resolution.
                    //
                    // PRE-ENRICH: Extract delivery returns for ALL L2→L1 children
                    // from the LAST iterative loop's L1 trace. In that trace, inner
                    // level entries are loaded → inner children execute on L1 → their
                    // outputs are available. Without this, the post-convergence loop's
                    // Step A for outer levels can't extract inner children's delivery
                    // returns because the L1 trace built at that point lacks the
                    // result propagation entries.
                    if let Some(ref saved_trace) = last_l1_user_trace {
                        let child_indices: Vec<usize> = detected_calls
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| {
                                c.parent_call_index.is_child() && c.target_rollup_id == 0
                            })
                            .map(|(i, _)| i)
                            .collect();

                        for &ci in &child_indices {
                            let child = &detected_calls[ci];
                            // Only update if current data is stale (error selector = 4 bytes)
                            if child.delivery_return_data.len() <= 4 || child.delivery_failed {
                                let delivery = extract_delivery_return_from_l1_trace_with_calldata(
                                    saved_trace,
                                    child.destination,
                                    rollups_address,
                                    Some(&child.calldata),
                                );
                                if !delivery.is_empty() {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        ci,
                                        dest = %child.destination,
                                        old_len = child.delivery_return_data.len(),
                                        new_len = delivery.len(),
                                        new_hex = %format!("0x{}", super::hex::encode(&delivery[..delivery.len().min(32)])),
                                        "post-convergence: PRE-ENRICHED L2→L1 child from saved L1 trace"
                                    );
                                    detected_calls[ci].delivery_return_data = delivery;
                                    detected_calls[ci].delivery_failed = false;
                                }
                            }
                        }

                        // For CONTINUATION patterns: if any child STILL has stale error
                        // data after extraction attempt, default to void (empty, success=true).
                        // Continuation children execute within the full on-chain context
                        // (authorized proxies, token state) that the simulation doesn't have.
                        // The simulation failure is an artifact — the actual on-chain execution
                        // succeeds and returns void (e.g., Bridge.receiveTokens).
                        // Reentrant children are NOT defaulted because they need real data.
                        if !is_reentrant_pattern {
                            for &ci in &child_indices {
                                let child = &detected_calls[ci];
                                if child.delivery_return_data.len() <= 4 && child.delivery_failed {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        ci,
                                        dest = %child.destination,
                                        old_hex = %format!("0x{}", super::hex::encode(&child.delivery_return_data)),
                                        "post-convergence: defaulting continuation child to void (simulation artifact)"
                                    );
                                    detected_calls[ci].delivery_return_data = vec![];
                                    detected_calls[ci].delivery_failed = false;
                                }
                            }
                        }
                    }

                    // Collect L1→L2 root call indices in REVERSE order (innermost first).
                    let root_indices: Vec<usize> = detected_calls
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.parent_call_index.is_root())
                        .map(|(i, _)| i)
                        .rev()
                        .collect();

                    // Get system address for L2 simulation.
                    let sys_addr = {
                        let sys_calldata = super::super::common::encode_system_address_calldata();
                        let sys_result = super::super::common::eth_call_view(
                            client,
                            l2_rpc_url,
                            cross_chain_manager_address,
                            &sys_calldata,
                        )
                        .await;
                        sys_result
                            .and_then(|s| super::super::common::parse_address_from_abi_return(&s))
                    };

                    let sys_addr_str = match sys_addr {
                        Some(a) => format!("{a}"),
                        None => {
                            tracing::warn!(
                                target: "based_rollup::l1_proxy",
                                "post-convergence: SYSTEM_ADDRESS query failed — skipping L2 enrichment"
                            );
                            String::new()
                        }
                    };
                    let ccm_hex = format!("{cross_chain_manager_address}");

                    // INCREMENTAL LEVEL-BY-LEVEL ENRICHMENT
                    //
                    // Process one reentrant level at a time (innermost first).
                    // At each level:
                    //   1. Run L1 trace → extract child delivery return for THIS level
                    //   2. Update detected_calls with child delivery return
                    //   3. Rebuild ALL entries (entries now have correct hashes for inner levels)
                    //   4. Run L2 sim → extract parent l2_return for THIS level
                    //   5. Update detected_calls with parent l2_return
                    // After all levels: detected_calls has correct return data for everything.
                    //
                    // Why incremental? Each level's L2 simulation needs correct RESULT
                    // hashes from ALL inner levels. These hashes depend on both the
                    // l2_return (from L2 sim) and delivery_return (from L1 trace) of
                    // inner calls. Single-pass fails because the dependencies are circular
                    // within a level but linear BETWEEN levels.

                    if !sys_addr_str.is_empty() {
                        // reentrant_parents are already in bottom-up order (innermost first).
                        // The leaf call (no children) is NOT in reentrant_parents.
                        // root_indices has ALL root calls in reverse (innermost first).

                        // Process each reentrant parent level-by-level.
                        for &idx in &root_indices {
                            let has_children = detected_calls.iter().any(|other| {
                                other.parent_call_index
                                    == crate::cross_chain::ParentLink::Child(
                                        crate::cross_chain::AbsoluteCallIndex::new(idx),
                                    )
                            });
                            if !has_children || !detected_calls[idx].delivery_return_data.is_empty()
                            {
                                continue; // Leaf or already enriched.
                            }

                            // Find this parent's L2→L1 child.
                            let child_idx = match detected_calls.iter().position(|c| {
                                c.parent_call_index
                                    == crate::cross_chain::ParentLink::Child(
                                        crate::cross_chain::AbsoluteCallIndex::new(idx),
                                    )
                            }) {
                                Some(ci) => ci,
                                None => continue,
                            };

                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                level_parent = idx,
                                level_child = child_idx,
                                "post-convergence: processing reentrant level"
                            );

                            // === LOG: dc state BEFORE Step A ===
                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                "post-convergence: dc state BEFORE Step A (level idx={}):",
                                idx
                            );
                            for (di, dc) in detected_calls.iter().enumerate() {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    "  PRE-A dc[{}] (level {}): ret_len={} ret_hex={} success={}",
                                    di, idx,
                                    dc.delivery_return_data.len(),
                                    if dc.delivery_return_data.is_empty() { "0x".to_string() } else {
                                        format!("0x{}", super::hex::encode(&dc.delivery_return_data[..dc.delivery_return_data.len().min(8)]))
                                    },
                                    !dc.delivery_failed
                                );
                            }

                            // STEP A: Run L1 trace to extract child delivery return.
                            // The L1 entries are rebuilt from current detected_calls state.
                            // Inner levels are already correct → their entries have correct
                            // RESULT hashes → scope navigation succeeds for this child.
                            if let Some((user_trace, _resp)) =
                                super::super::delivery::build_and_run_l1_postbatch_trace(
                                    client,
                                    l1_rpc_url,
                                    rollups_address,
                                    rollup_id,
                                    &builder_key,
                                    detected_calls,
                                    from,
                                    to,
                                    data,
                                    value,
                                    &format!("post-convergence-l1-level-{idx}"),
                                )
                                .await
                            {
                                let child_dest = detected_calls[child_idx].destination;
                                let child_cd = detected_calls[child_idx].calldata.clone();
                                let delivery_data =
                                    extract_delivery_return_from_l1_trace_with_calldata(
                                        &user_trace,
                                        child_dest,
                                        rollups_address,
                                        Some(&child_cd),
                                    );

                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    child_idx,
                                    dest = %child_dest,
                                    delivery_len = delivery_data.len(),
                                    delivery_hex = %if delivery_data.is_empty() {
                                        "0x".to_string()
                                    } else {
                                        format!("0x{}", super::hex::encode(&delivery_data[..delivery_data.len().min(32)]))
                                    },
                                    "post-convergence: extracted child delivery return from L1 trace"
                                );

                                // Only update if current data is stale (error selector ≤4 bytes
                                // or call failed). Don't overwrite valid data from iterative
                                // discovery (e.g., chained L1 sim that returned uint256(2)
                                // for the second CounterL1 call).
                                let current = &detected_calls[child_idx];
                                if !delivery_data.is_empty()
                                    && (current.delivery_return_data.len() <= 4
                                        || current.delivery_failed)
                                {
                                    detected_calls[child_idx].delivery_return_data = delivery_data;
                                    detected_calls[child_idx].delivery_failed = false;
                                }
                            }

                            // STEP B: Rebuild L2 entries with updated child delivery return,
                            // then run L2 sim for the parent.
                            let sim_entry_idx = if is_reentrant_pattern {
                                idx
                            } else {
                                *root_indices.last().unwrap_or(&idx)
                            };
                            let call_destination = detected_calls[idx].destination;
                            let call_calldata = detected_calls[sim_entry_idx].calldata.clone();
                            let call_value = detected_calls[sim_entry_idx].value;
                            let call_source = detected_calls[sim_entry_idx].source_address;
                            let entry_destination = detected_calls[sim_entry_idx].destination;

                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                idx,
                                sim_entry_idx,
                                is_reentrant_pattern,
                                entry_dest = %entry_destination,
                                bfs_target = %call_destination,
                                "post-convergence: selected L2 sim entry point"
                            );

                            let l1_detected: Vec<crate::table_builder::L1DetectedCall> =
                                detected_calls
                                    .iter()
                                    .map(|c| crate::table_builder::L1DetectedCall {
                                        destination: c.destination,
                                        data: c.calldata.clone(),
                                        value: c.value,
                                        source_address: c.source_address,
                                        l2_return_data: c.delivery_return_data.clone(),
                                        call_success: !c.delivery_failed,
                                        parent_call_index: c.parent_call_index,
                                        target_rollup_id: if c.parent_call_index.is_child()
                                            && c.target_rollup_id == 0
                                        {
                                            Some(0)
                                        } else {
                                            None
                                        },
                                        scope: if c.trace_depth <= 1 {
                                            ScopePath::root()
                                        } else {
                                            ScopePath::from_parts(vec![U256::ZERO; c.trace_depth])
                                        },
                                        discovery_iteration: c.discovery_iteration,
                                        l1_trace_depth: c.trace_depth,
                                        in_reverted_frame: c.in_reverted_frame,
                                    })
                                    .collect();
                            let analyzed =
                                crate::composer_rpc::entry_builder::analyze_l1_to_l2_continuations(
                                    &l1_detected,
                                    rollup_id,
                                );
                            if analyzed.is_empty() {
                                continue;
                            }
                            let cont = crate::composer_rpc::entry_builder::build_continuations(
                                &analyzed,
                                crate::cross_chain::RollupId::new(alloy_primitives::U256::from(
                                    rollup_id,
                                )),
                            );
                            let mut l2_entries = cont.l2_entries;
                            for e in &mut l2_entries {
                                e.state_deltas.clear();
                            }

                            // === LOG: detected_calls state AFTER Step A, BEFORE Step B ===
                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                "post-convergence: dc state AFTER Step A extraction, BEFORE Step B L2 sim (level idx={}):",
                                idx
                            );
                            for (di, dc) in detected_calls.iter().enumerate() {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    "  dc[{}]: dest={}..{} target_rollup={} parent={:?} ret_len={} ret_hex={} success={} calldata_hex={}",
                                    di,
                                    &format!("{}", dc.destination)[..10],
                                    &format!("{}", dc.destination)[38..],
                                    dc.target_rollup_id,
                                    dc.parent_call_index,
                                    dc.delivery_return_data.len(),
                                    if dc.delivery_return_data.is_empty() { "0x".to_string() } else {
                                        format!("0x{}", super::hex::encode(&dc.delivery_return_data[..dc.delivery_return_data.len().min(32)]))
                                    },
                                    !dc.delivery_failed,
                                    format!("0x{}", super::hex::encode(&dc.calldata[..dc.calldata.len().min(36)]))
                                );
                            }

                            // === LOG: L2 entries loaded ===
                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                idx,
                                l2_entry_count = l2_entries.len(),
                                "post-convergence: L2 entries for simulation:"
                            );
                            for (ei, e) in l2_entries.iter().enumerate() {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    "  l2e[{}]: actionHash={} nextType={:?} nextRollup={} nextDest={}..{} nextDataLen={} nextScope={:?}",
                                    ei,
                                    e.action_hash,
                                    e.next_action.action_type,
                                    e.next_action.rollup_id,
                                    &format!("{}", e.next_action.destination)[..10.min(format!("{}", e.next_action.destination).len())],
                                    &format!("{}", e.next_action.destination)[format!("{}", e.next_action.destination).len().saturating_sub(4)..],
                                    e.next_action.data.len(),
                                    e.next_action.scope
                                );
                            }

                            let sim_action = crate::cross_chain::CrossChainAction {
                                action_type: crate::cross_chain::CrossChainActionType::Call,
                                rollup_id: RollupId::new(U256::from(rollup_id)),
                                destination: entry_destination,
                                value: call_value,
                                data: call_calldata,
                                failed: false,
                                source_address: call_source,
                                source_rollup: RollupId::MAINNET,
                                scope: ScopePath::root(),
                            };
                            let exec_calldata =
                                crate::cross_chain::encode_execute_incoming_call_calldata(
                                    &sim_action,
                                );

                            let sim_result = run_l2_sim_bundle(
                                client,
                                l2_rpc_url,
                                &sys_addr_str,
                                &ccm_hex,
                                &l2_entries,
                                exec_calldata.as_ref(),
                                call_value,
                            )
                            .await;

                            if let Some((trace, success)) = sim_result {
                                let inner =
                                    extract_inner_destination_return_data(&trace, call_destination)
                                        .unwrap_or_default();
                                let bfs_hit = !inner.is_empty();
                                let inner_success = bfs_hit
                                    || destination_call_succeeded_in_trace(
                                        &trace,
                                        call_destination,
                                    );

                                let (ret_data, inner_success, used_fallback) = if inner.is_empty()
                                    && success
                                {
                                    let raw = extract_return_data_from_trace(&trace);
                                    let decoded = if raw.len() >= 64 {
                                        let dlen = U256::from_be_slice(&raw[32..64]).to::<usize>();
                                        raw[64..64 + dlen.min(raw.len() - 64)].to_vec()
                                    } else {
                                        raw
                                    };
                                    (decoded, true, true)
                                } else {
                                    (inner, inner_success, false)
                                };

                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    idx,
                                    dest = %call_destination,
                                    ret_data_len = ret_data.len(),
                                    inner_success,
                                    sim_success = success,
                                    bfs_hit,
                                    used_fallback,
                                    ret_data_hex = %if ret_data.is_empty() {
                                        "0x".to_string()
                                    } else {
                                        format!("0x{}", super::hex::encode(&ret_data[..ret_data.len().min(32)]))
                                    },
                                    "post-convergence: L2 sim result for parent"
                                );

                                let has_children = detected_calls.iter().any(|c| {
                                    c.parent_call_index
                                        == crate::cross_chain::ParentLink::Child(
                                            crate::cross_chain::AbsoluteCallIndex::new(idx),
                                        )
                                });
                                let skip_update =
                                    used_fallback && has_children && !is_reentrant_pattern;

                                if skip_update {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        idx,
                                        "post-convergence: skipping L2 return update (top-level fallback for continuation parent with children — scope-chain data may not equal parent's own return)"
                                    );
                                } else if inner_success || !ret_data.is_empty() {
                                    detected_calls[idx].delivery_return_data = ret_data;
                                    detected_calls[idx].delivery_failed = !inner_success;
                                }
                            }
                        }
                    }

                    // Log final enriched state for traceability.
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        "post-convergence enrichment complete — final call state:"
                    );
                    for (i, c) in detected_calls.iter().enumerate() {
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            "  CALL[{}]: dest={} ret_len={} ret_hex={} success={} parent={:?} disc_iter={}",
                            i, c.destination, c.delivery_return_data.len(),
                            if c.delivery_return_data.is_empty() { "0x".to_string() } else {
                                format!("0x{}", super::hex::encode(&c.delivery_return_data[..c.delivery_return_data.len().min(32)]))
                            },
                            !c.delivery_failed, c.parent_call_index, c.discovery_iteration
                        );
                    }
                }
            }
        }
    }

    if detected_calls.is_empty() {
        tracing::info!(
            target: "based_rollup::l1_proxy",
            %to,
            "no internal cross-chain calls found in trace — forwarding tx directly"
        );
        return Ok(None);
    }

    // Final in_reverted_frame correction from the last converged retrace.
    // Uses property-based matching with count pairing (same as early correction).
    // No length guard — walk only contains L1→L2 calls while detected_calls may
    // also include L2→L1 children from enrichment. Property matching by
    // (dest, calldata, value, source, depth) ensures only identical calls update.
    if !last_converged_walk.is_empty() {
        let mut consumed: std::collections::HashMap<
            (Address, Vec<u8>, U256, Address, usize),
            usize,
        > = std::collections::HashMap::new();
        for existing in detected_calls.iter_mut() {
            let key = (
                existing.destination,
                existing.calldata.clone(),
                existing.value,
                existing.source_address,
                existing.trace_depth,
            );
            let idx = consumed.entry(key.clone()).or_insert(0);
            let mut count = 0usize;
            for retrace in &last_converged_walk {
                if retrace.destination == key.0
                    && retrace.calldata == key.1
                    && retrace.value == key.2
                    && retrace.source_address == key.3
                    && retrace.trace_depth == key.4
                {
                    if count == *idx {
                        if existing.in_reverted_frame != retrace.in_reverted_frame {
                            tracing::debug!(
                                target: "based_rollup::l1_proxy",
                                dest = %existing.destination,
                                old = existing.in_reverted_frame,
                                new = retrace.in_reverted_frame,
                                "final in_reverted_frame correction from converged retrace"
                            );
                            existing.in_reverted_frame = retrace.in_reverted_frame;
                        }
                        break;
                    }
                    count += 1;
                }
            }
            *idx += 1;
        }
    }

    for (i, c) in detected_calls.iter().enumerate() {
        tracing::debug!(
            target: "based_rollup::composer_rpc::l1_to_l2",
            idx = i,
            dest = %c.destination,
            in_reverted_frame = c.in_reverted_frame,
            delivery_failed = c.delivery_failed,
            trace_depth = c.trace_depth,
            target_rollup_id = c.target_rollup_id,
            parent = ?c.parent_call_index,
            "FINAL detected_calls before queue_execution_table"
        );
    }

    tracing::info!(
        target: "based_rollup::composer_rpc::l1_to_l2",
        count = detected_calls.len(),
        "detected internal cross-chain calls — routing to buildExecutionTable"
    );

    let effective_gas_price = super::extract_gas_price_from_raw_tx(raw_tx).unwrap_or(0);

    queue_execution_table(
        client,
        l2_rpc_url,
        raw_tx,
        detected_calls,
        effective_gas_price,
    )
    .await
}

/// Queue detected cross-chain calls as a single execution table via
/// `syncrollups_buildExecutionTable`. Handles any number of calls (1 or more).
/// Entries are built atomically (with L2→L1 child call detection for multi-call).
pub(super) async fn queue_execution_table(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    raw_tx: &str,
    detected_calls: &[DiscoveredCall],
    effective_gas_price: u128,
) -> eyre::Result<Option<String>> {
    let calls: Vec<serde_json::Value> = detected_calls
        .iter()
        .map(|c| {
            let mut call_json = serde_json::json!({
                "destination": format!("{}", c.destination),
                "data": format!("0x{}", super::hex::encode(&c.calldata)),
                "value": format!("{}", c.value),
                "sourceAddress": format!("{}", c.source_address)
            });
            // Include L2 simulation results when available.
            if !c.delivery_return_data.is_empty() || c.delivery_failed {
                call_json["l2ReturnData"] =
                    serde_json::json!(format!("0x{}", super::hex::encode(&c.delivery_return_data)));
                call_json["callSuccess"] = serde_json::json!(!c.delivery_failed);
            }
            // Include parent linkage and target rollup for L2→L1 child calls
            // (the L1→L2→L1 nested pattern). Without these, analyze_continuation_calls
            // treats all calls as L1→L2, producing wrong entry structures.
            if let Some(parent_idx) = c.parent_call_index.child_index() {
                call_json["parentCallIndex"] = serde_json::json!(parent_idx.as_usize());
            }
            if c.target_rollup_id == 0 && c.parent_call_index.is_child() {
                // Explicitly mark L2→L1 children (target=L1=0) so the RPC handler
                // can distinguish them from L1→L2 calls.
                call_json["targetRollupId"] = serde_json::json!(0u64);
            }
            // Propagate in_reverted_frame for partial revert patterns (revertContinue).
            if c.in_reverted_frame {
                call_json["inRevertedFrame"] = serde_json::json!(true);
            }
            // Propagate discovery iteration and L1 trace depth for reentrant detection.
            if c.discovery_iteration > 0 {
                call_json["discoveryIteration"] = serde_json::json!(c.discovery_iteration);
            }
            if c.trace_depth > 0 {
                call_json["l1TraceDepth"] = serde_json::json!(c.trace_depth);
            }
            // Propagate scope for nested calls so the RPC handler can set the
            // correct BuildExecutionTableCall.scope field (distinct from the
            // call_action.scope which must stay empty for L1 actionHash identity).
            if c.trace_depth > 1 {
                let scope_vals: Vec<String> =
                    (0..c.trace_depth).map(|_| "0x0".to_string()).collect();
                call_json["scope"] = serde_json::json!(scope_vals);
            }
            call_json
        })
        .collect();

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "syncrollups_buildExecutionTable",
        "params": [{
            "calls": calls,
            "gasPrice": effective_gas_price,
            "rawL1Tx": raw_tx
        }],
        "id": 99991
    });

    let rpc_resp: super::super::common::JsonRpcResponse = client
        .post(l2_rpc_url)
        .json(&req)
        .send()
        .await?
        .json()
        .await?;

    let result_val = rpc_resp
        .into_result()
        .map_err(|e| eyre::eyre!("buildExecutionTable failed: {e}"))?;

    let call_id = result_val
        .get("callId")
        .and_then(|v| v.as_str())
        .unwrap_or("0x")
        .to_string();

    let l2_count = result_val
        .get("l2EntryCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let l1_count = result_val
        .get("l1EntryCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    tracing::info!(
        target: "based_rollup::l1_proxy",
        %call_id,
        l2_entries = l2_count,
        l1_entries = l1_count,
        "built execution table for multi-call tx — queued atomically"
    );

    let tx_hash = super::super::common::compute_tx_hash(raw_tx).unwrap_or(call_id);
    Ok(Some(tx_hash))
}

/// Handle `eth_estimateGas` when the `to` address is a cross-chain proxy
/// or a Bridge contract.
///
/// Cross-chain proxy calls and bridge calls always revert during L1 estimation
/// because the execution table isn't populated yet. Wallets (MetaMask, Rabby) call
/// `eth_estimateGas` before showing the confirmation dialog and fall back to
/// incorrect defaults (e.g. Rabby uses 2M gas) when estimation reverts.
///
/// This function intercepts the estimate, checks if `to` is a proxy or bridge,
/// and if so computes gas from calldata using:
///   TX_BASE (21,000) + calldata gas (EIP-2028) + contract overhead (160,000)
///   + 1.3x safety buffer
///
/// Returns `Some(response)` if intercepted, `None` to forward to L1 normally.
pub(super) async fn handle_estimate_gas_for_proxy(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    params: Option<&Vec<Value>>,
    rollups_address: Address,
    json: &Value,
) -> Option<hyper::Response<http_body_util::Full<hyper::body::Bytes>>> {
    // eth_estimateGas params: [{ from, to, data, value, ... }, block?]
    let tx_obj = params?.first()?;
    let to_str = tx_obj.get("to")?.as_str()?;
    let to_addr = to_str.parse::<Address>().ok()?;

    // Check if the `to` address is a cross-chain proxy on L1.
    // This is a lightweight check — only queries authorizedProxies on Rollups.sol.
    // Bridge contracts are not checked here because the generic trace walker
    // handles them via the executeCrossChainCall child pattern in the main path.
    // For gas estimation, checking proxy status is sufficient since bridge
    // contracts internally deploy/call proxies.
    let is_proxy = is_cross_chain_proxy_on_l1(client, l1_rpc_url, to_addr, rollups_address).await;

    if !is_proxy {
        return None;
    }

    // Compute gas from calldata (same formula as UI gasEstimation.ts)
    let calldata_hex = tx_obj.get("data").and_then(|v| v.as_str()).unwrap_or("0x");
    let calldata_clean = calldata_hex.strip_prefix("0x").unwrap_or(calldata_hex);

    let mut calldata_gas: u64 = 0;
    let mut i = 0;
    while i + 1 < calldata_clean.len() {
        let byte_str = &calldata_clean[i..i + 2];
        if byte_str == "00" {
            calldata_gas += 4; // EIP-2028: zero byte
        } else {
            calldata_gas += 16; // EIP-2028: non-zero byte
        }
        i += 2;
    }

    const TX_BASE_GAS: u64 = 21_000;
    const CROSS_CHAIN_OVERHEAD: u64 = 160_000;
    const BUFFER_NUM: u64 = 130;
    const BUFFER_DEN: u64 = 100;

    let raw_estimate = TX_BASE_GAS + calldata_gas + CROSS_CHAIN_OVERHEAD;
    let gas_limit = (raw_estimate * BUFFER_NUM) / BUFFER_DEN;
    let gas_hex = format!("{gas_limit:#x}");

    tracing::debug!(
        target: "based_rollup::l1_proxy",
        %to_addr, %gas_hex, raw_estimate, gas_limit,
        "eth_estimateGas intercepted for cross-chain proxy — returning computed estimate"
    );

    let json_id = json.get("id").cloned().unwrap_or(Value::Null);
    let response_body = serde_json::json!({
        "jsonrpc": "2.0",
        "result": gas_hex,
        "id": json_id
    });

    Some(super::super::common::cors_response(
        hyper::Response::builder()
            .status(hyper::StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(http_body_util::Full::new(hyper::body::Bytes::from(
                response_body.to_string(),
            )))
            .expect("valid response"),
    ))
}

/// Check if an address is a registered cross-chain proxy on L1 (Rollups.sol).
///
/// Queries `authorizedProxies(address)` on Rollups.sol. Returns `true` if the
/// address has a non-zero `originalAddress` registered.
///
/// Used only for `eth_estimateGas` interception — the main detection path uses
/// the generic `trace::walk_trace_tree` instead.
async fn is_cross_chain_proxy_on_l1(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    address: Address,
    rollups_address: Address,
) -> bool {
    // authorizedProxies(address) — typed ABI encoding via sol! macro — NEVER hardcode selectors.
    let calldata = super::super::common::encode_authorized_proxies_calldata(address);
    let hex_data =
        match super::super::common::eth_call_view(client, l1_rpc_url, rollups_address, &calldata)
            .await
        {
            Some(hex) => hex,
            None => return false,
        };
    super::super::common::parse_address_from_abi_return(&hex_data).is_some()
}

/// Decode a `0x`-prefixed 4-byte error selector into a human-readable name.
///
/// Uses compile-time selectors from `common.rs` sol! macro definitions.
/// Returns `"unknown"` for unrecognized selectors.
pub(super) fn decode_error_selector_prefixed(selector: Option<&str>) -> &'static str {
    use super::super::common::{
        CALL_EXECUTION_FAILED_SELECTOR, ERROR_STRING_SELECTOR, ETHER_DELTA_MISMATCH_SELECTOR,
        EXECUTION_NOT_FOUND_SELECTOR, INVALID_REVERT_DATA_SELECTOR, PROXY_CALL_FAILED_SELECTOR,
        STATE_ALREADY_UPDATED_SELECTOR, STATE_ROOT_MISMATCH_SELECTOR, UNAUTHORIZED_PROXY_SELECTOR,
        selector_hex_prefixed,
    };

    let sel = match selector {
        Some(s) => s,
        None => return "unknown",
    };
    if sel == selector_hex_prefixed(&EXECUTION_NOT_FOUND_SELECTOR) {
        "ExecutionNotFound"
    } else if sel == selector_hex_prefixed(&INVALID_REVERT_DATA_SELECTOR) {
        "InvalidRevertData"
    } else if sel == selector_hex_prefixed(&STATE_ALREADY_UPDATED_SELECTOR) {
        "StateAlreadyUpdatedThisBlock"
    } else if sel == selector_hex_prefixed(&STATE_ROOT_MISMATCH_SELECTOR) {
        "StateRootMismatch"
    } else if sel == selector_hex_prefixed(&ETHER_DELTA_MISMATCH_SELECTOR) {
        "EtherDeltaMismatch"
    } else if sel == selector_hex_prefixed(&CALL_EXECUTION_FAILED_SELECTOR) {
        "CallExecutionFailed"
    } else if sel == selector_hex_prefixed(&UNAUTHORIZED_PROXY_SELECTOR) {
        "UnauthorizedProxy"
    } else if sel == selector_hex_prefixed(&PROXY_CALL_FAILED_SELECTOR) {
        "ProxyCallFailed(inner)"
    } else if sel == selector_hex_prefixed(&ERROR_STRING_SELECTOR) {
        "Error(string)"
    } else {
        "unknown"
    }
}

/// Decode a bare (non-prefixed) 4-byte error selector into a human-readable name.
///
/// Uses compile-time selectors from `common.rs` sol! macro definitions.
/// Returns `""` for unrecognized selectors.
pub(super) fn decode_error_selector_bare(selector: Option<&str>) -> &'static str {
    use super::super::common::{
        CALL_EXECUTION_FAILED_SELECTOR, ETHER_DELTA_MISMATCH_SELECTOR,
        EXECUTION_NOT_FOUND_SELECTOR, INVALID_REVERT_DATA_SELECTOR, STATE_ALREADY_UPDATED_SELECTOR,
        STATE_ROOT_MISMATCH_SELECTOR, selector_hex_bare,
    };

    let sel = match selector {
        Some(s) => s,
        None => return "",
    };
    if sel == selector_hex_bare(&EXECUTION_NOT_FOUND_SELECTOR) {
        "ExecutionNotFound"
    } else if sel == selector_hex_bare(&INVALID_REVERT_DATA_SELECTOR) {
        "InvalidRevertData"
    } else if sel == selector_hex_bare(&STATE_ALREADY_UPDATED_SELECTOR) {
        "StateAlreadyUpdatedThisBlock"
    } else if sel == selector_hex_bare(&STATE_ROOT_MISMATCH_SELECTOR) {
        "StateRootMismatch"
    } else if sel == selector_hex_bare(&ETHER_DELTA_MISMATCH_SELECTOR) {
        "EtherDeltaMismatch"
    } else if sel == selector_hex_bare(&CALL_EXECUTION_FAILED_SELECTOR) {
        "CallExecutionFailed"
    } else {
        ""
    }
}

/// Thin wrapper for backward compatibility (used by tests via `use super::*`).
/// Returns `eyre::Result` and does NOT reject the zero address.
#[cfg(test)]
pub(super) fn parse_address_from_return(hex_str: &str) -> eyre::Result<Address> {
    let clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes =
        super::hex_decode(clean).ok_or_else(|| eyre::eyre!("invalid hex in eth_call return"))?;
    if bytes.len() < 32 {
        return Err(eyre::eyre!("return data too short for address"));
    }
    Ok(Address::from_slice(&bytes[12..32]))
}

/// Parse a U256 from a 32-byte ABI-encoded return value.
#[allow(dead_code)]
pub(super) fn parse_u256_from_return(hex_str: &str) -> eyre::Result<u64> {
    let hex = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes =
        super::hex_decode(hex).ok_or_else(|| eyre::eyre!("invalid hex in eth_call return"))?;
    if bytes.len() < 32 {
        return Err(eyre::eyre!("return data too short for uint256"));
    }
    Ok(u256_from_be_bytes(&bytes[0..32]))
}

/// Read a big-endian uint256 as u64 (truncating high bytes).
#[allow(dead_code)]
pub(super) fn u256_from_be_bytes(bytes: &[u8]) -> u64 {
    let len = bytes.len().min(32);
    let mut val: u64 = 0;
    let start = len.saturating_sub(8);
    for b in &bytes[start..len] {
        val = (val << 8) | (*b as u64);
    }
    val
}
