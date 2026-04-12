//! Post-discovery processing for L2→L1 cross-chain calls.
//!
//! Contains the main routing logic that decides how detected calls are queued
//! (multi-call continuation, independent duplicate, single-call with depth, or
//! simple single-call), plus the RPC queuing functions that communicate with the
//! driver.

use alloy_primitives::{Address, U256};
use serde_json::Value;

use crate::cross_chain::{filter_new_by_count, ScopePath};
use super::enrichment;
use super::super::delivery::{simulate_chained_delivery_l2_to_l1, simulate_l1_combined_delivery};
use super::super::model::{DiscoveredCall, ReturnEdge};

// Re-export for backward compatibility with internal callers.
use super::super::delivery::simulate_l1_delivery;

/// Post-discovery processing: compute tx outcome, verify return calls via
/// retrace, then route through the appropriate queuing path (partial revert,
/// duplicate, multi-call, single-call with depth, or simple single-call).
///
/// Separated from `trace_and_detect_l2_internal_calls` for readability.
/// The logic and behavior are identical — this is a purely mechanical extraction.
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_l2_to_l1_calls(
    client: &reqwest::Client,
    upstream_url: &str,
    raw_tx_hex: &str,
    l1_rpc_url: &str,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<&str>,
    rollup_id: u64,
    cross_chain_manager_address: Address,
    detected_calls: &mut Vec<DiscoveredCall>,
    early_return_calls: &[ReturnEdge],
    tx_bytes: &[u8],
    sender: Address,
    to_addr: Address,
    value: U256,
    input: &[u8],
    top_level_error: bool,
    last_user_trace_had_error: bool,
) -> bool {
    // Detect persistent revert: if the L2 tx STILL reverts after loading all entries,
    // the revert is from business logic (not missing entries). The L1 entries need
    // REVERT/REVERT_CONTINUE to undo cross-chain effects (§D.12 atomicity).
    let tx_outcome = crate::cross_chain::TxOutcome::from_bool(if top_level_error {
        last_user_trace_had_error
    } else {
        false
    });

    if tx_outcome.is_revert() {
        tracing::info!(
            target: "based_rollup::proxy",
            detected_calls = detected_calls.len(),
            "L2 tx reverts after cross-chain calls — will build REVERT entries for L1 atomicity"
        );
        for (i, call) in detected_calls.iter().enumerate() {
            tracing::info!(
                target: "based_rollup::proxy",
                idx = i,
                destination = %call.destination,
                source = %call.source_address,
                delivery_return_data_len = call.delivery_return_data.len(),
                delivery_return_data_hex = %format!("0x{}", hex::encode(&call.delivery_return_data)),
                delivery_failed = call.delivery_failed,
                trace_depth = call.trace_depth,
                "tx_reverts: detected call delivery details"
            );
        }
    }

    // Post-discovery verification: if early return calls were found, retrace the user
    // tx on L2 with continuation entries (scope navigation) to discover calls that are
    // only reachable through return call delivery. This handles future patterns where
    // a contract's behavior depends on side effects of the return call (e.g., token
    // transfers that gate subsequent cross-chain calls).
    //
    // This is ADDITIVE: the iterative loop already discovered all calls reachable
    // without scope navigation. This step can only ADD new calls, never remove them.
    if !early_return_calls.is_empty() && !detected_calls.is_empty() {
        // Convert our types to table_builder types for continuation entry building.
        let tb_l2_calls: Vec<crate::table_builder::L2DetectedCall> = detected_calls
            .iter()
            .map(|c| crate::table_builder::L2DetectedCall {
                destination: c.destination,
                data: c.calldata.clone(),
                value: c.value,
                source_address: c.source_address,
                delivery_return_data: c.delivery_return_data.clone(),
                delivery_failed: c.delivery_failed,
                scope: ScopePath::from_parts(vec![U256::ZERO; c.trace_depth.max(1)]),
                in_reverted_frame: c.in_reverted_frame,
            })
            .collect();
        let tb_return_calls: Vec<crate::table_builder::L2ReturnCall> = early_return_calls
            .iter()
            .map(|rc| crate::table_builder::L2ReturnCall {
                destination: rc.destination,
                data: rc.data.clone(),
                value: rc.value,
                source_address: rc.source_address,
                parent_call_index: rc.parent_call_index,
                l2_return_data: rc.return_data.clone(),
                l2_delivery_failed: rc.delivery_failed,
                scope: rc.scope.clone(),
            })
            .collect();

        let analyzed = crate::table_builder::analyze_l2_to_l1_continuation_calls(
            &tb_l2_calls,
            &tb_return_calls,
            rollup_id,
        );

        if !analyzed.is_empty() {
            let continuation = crate::table_builder::build_l2_to_l1_continuation_entries(
                &analyzed,
                crate::cross_chain::RollupId::new(U256::from(rollup_id)),
                tx_bytes,
                tx_outcome,
            );

            if !continuation.l2_entries.is_empty() {
                let load_calldata = crate::cross_chain::encode_load_execution_table_calldata(
                    &continuation.l2_entries,
                );
                let load_data = format!("0x{}", hex::encode(load_calldata.as_ref()));
                let ccm_hex = format!("{cross_chain_manager_address}");
                let builder_hex = format!("{builder_address}");

                let retrace_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "debug_traceCallMany",
                    "params": [
                        [{
                            "transactions": [
                                {
                                    "from": builder_hex,
                                    "to": ccm_hex,
                                    "data": load_data,
                                    "gas": "0x1c9c380"
                                },
                                {
                                    "from": format!("{sender}"),
                                    "to": format!("{to_addr}"),
                                    "data": format!("0x{}", hex::encode(input)),
                                    "value": format!("0x{:x}", value),
                                    "gas": "0x2faf080"
                                }
                            ]
                        }],
                        null,
                        { "tracer": "callTracer" }
                    ],
                    "id": 99990
                });

                tracing::info!(
                    target: "based_rollup::proxy",
                    l2_entry_count = continuation.l2_entries.len(),
                    "post-discovery retrace: retracing user tx with continuation entries"
                );

                let mut proxy_cache =
                    std::collections::HashMap::<Address, Option<super::super::trace::ProxyInfo>>::new();
                if let Ok(resp) = client.post(upstream_url).json(&retrace_req).send().await {
                    if let Ok(body) = resp.json::<Value>().await {
                        if let Some(traces) = body
                            .get("result")
                            .and_then(|r| r.get(0))
                            .and_then(|b| b.as_array())
                        {
                            if traces.len() >= 2 {
                                let new_detected = enrichment::walk_l2_trace_generic(
                                    client,
                                    upstream_url,
                                    cross_chain_manager_address,
                                    &traces[1],
                                    &mut proxy_cache,
                                )
                                .await;

                                let truly_new =
                                    filter_new_by_count(new_detected, detected_calls, |a, b| {
                                        a.destination == b.destination
                                            && a.calldata == b.calldata
                                            && a.value == b.value
                                            && a.source_address == b.source_address
                                    });

                                if !truly_new.is_empty() {
                                    tracing::info!(
                                        target: "based_rollup::proxy",
                                        new_count = truly_new.len(),
                                        "post-discovery retrace found NEW L2→L1 calls \
                                         (only reachable via scope navigation)"
                                    );
                                    detected_calls.extend(truly_new);
                                } else {
                                    tracing::info!(
                                        target: "based_rollup::proxy",
                                        "post-discovery retrace: no new calls (existing discovery sufficient)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Route through the appropriate queuing path based on call count.
    //
    // Partial revert pattern: when some calls have in_reverted_frame=true and others
    // don't, ALL calls must go through the multi-call path (buildL2ToL1ExecutionTable)
    // to generate a single L2TX trigger with REVERT/REVERT_CONTINUE entries. This takes
    // priority over the duplicate-call independent routing.
    let has_partial_revert = detected_calls.iter().any(|c| c.in_reverted_frame)
        && detected_calls.iter().any(|c| !c.in_reverted_frame);

    if has_partial_revert && detected_calls.len() >= 2 {
        tracing::info!(
            target: "based_rollup::proxy",
            reverted = detected_calls.iter().filter(|c| c.in_reverted_frame).count(),
            non_reverted = detected_calls.iter().filter(|c| !c.in_reverted_frame).count(),
            "partial revert pattern: routing ALL calls through multi-call path for REVERT/REVERT_CONTINUE"
        );
        return queue_l2_to_l1_multi_call_entries(
            client,
            upstream_url,
            raw_tx_hex,
            detected_calls,
            early_return_calls,
            rollup_id,
            tx_outcome,
        )
        .await
        .is_some();
    }

    // Check for duplicate calls (same action identity). Identical calls must
    // route independently — the continuation path produces chained RESULT→CALL
    // entries with return-data-dependent hashes that break for identical calls
    // because return data is state-dependent (#256).
    let has_duplicates = crate::cross_chain::has_duplicate_calls(
        &detected_calls
            .iter()
            .map(|c| {
                (
                    c.destination,
                    c.calldata.as_slice(),
                    c.value,
                    c.source_address,
                )
            })
            .collect::<Vec<_>>(),
    );

    if detected_calls.len() >= 2 && has_duplicates {
        // Check if any duplicate call has return calls (nested L2→L1→L2 pattern).
        // If so, route via buildL2ToL1ExecutionTable (supports return calls)
        // instead of initiateL2CrossChainCall (simple only).
        let detected_return_calls: Vec<ReturnEdge> = {
            // Run simulate_l1_delivery for ONE call to check for return calls.
            // If it finds return calls, ALL duplicate calls need the continuation path.
            let first = &detected_calls[0];
            let root_scope: Vec<U256> = if first.trace_depth <= 1 {
                vec![]
            } else {
                vec![U256::ZERO; first.trace_depth]
            };
            if let Some((_ret, _failed, returns)) = simulate_l1_delivery(
                client,
                l1_rpc_url,
                upstream_url,
                cross_chain_manager_address,
                rollups_address,
                builder_address,
                builder_private_key,
                rollup_id,
                first.source_address,
                first.destination,
                &first.calldata,
                first.value,
                tx_bytes,
                &root_scope,
                &first.delivery_return_data,
                first.delivery_failed,
            )
            .await
            {
                returns
            } else {
                vec![]
            }
        };

        if !detected_return_calls.is_empty() {
            // Duplicate NESTED calls: each gets return calls with REAL chained data.
            // The return calls target the same L1 contract (e.g., Counter) but each
            // invocation returns different data (state-dependent: 1, 2, 3...).
            // Simulate ALL return calls in a single chained debug_traceCallMany bundle
            // so each sees cumulative state from prior calls.
            let first = &detected_calls[0];
            let matching_count = detected_calls
                .iter()
                .filter(|c| c.destination == first.destination && c.calldata == first.calldata)
                .count();

            // Simulate return calls CHAINED on the DESTINATION chain (L2 for L2→L1→L2).
            // Return calls go back to L2 via executeIncomingCrossChainCall. The actual
            // destination is the L2 contract (e.g., CounterL2). We simulate calling
            // that contract directly on L2 to get sequential return data.
            //
            // For Counter.increment() called twice: returns [1, 2] (state-dependent).
            let rc_template = &detected_return_calls[0];
            let mut rc_txs: Vec<Value> = Vec::new();
            for _ in 0..matching_count {
                rc_txs.push(serde_json::json!({
                    "from": format!("{}", rc_template.source_address),
                    "to": format!("{}", rc_template.destination),
                    "data": format!("0x{}", hex::encode(&rc_template.data)),
                    "value": format!("0x{:x}", rc_template.value),
                    "gas": "0x2faf080"
                }));
            }

            // Simulate chained on L2 (upstream_url = L2 RPC)
            let sim_req = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "debug_traceCallMany",
                "params": [
                    [{ "transactions": rc_txs }],
                    null,
                    { "tracer": "callTracer" }
                ],
                "id": 99976
            });

            let mut chained_rc_data: Vec<(Vec<u8>, bool)> = Vec::new();
            if let Ok(resp) = client.post(upstream_url).json(&sim_req).send().await {
                if let Ok(body) = resp.json::<Value>().await {
                    if let Some(traces) = body
                        .get("result")
                        .and_then(|r| r.get(0))
                        .and_then(|b| b.as_array())
                    {
                        for trace in traces {
                            let has_error = trace.get("error").is_some();
                            let output =
                                trace.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                            let hex = output.strip_prefix("0x").unwrap_or(output);
                            let bytes = hex::decode(hex).unwrap_or_default();
                            chained_rc_data.push((bytes, has_error));
                        }
                    }
                }
            }

            tracing::info!(
                target: "based_rollup::composer_rpc::l2_to_l1",
                count = detected_calls.len(),
                matching_nested = matching_count,
                return_calls_simulated = chained_rc_data.len(),
                "duplicate NESTED calls — chained L1 return call simulation"
            );
            for (idx, (data, failed)) in chained_rc_data.iter().enumerate() {
                tracing::info!(
                    target: "based_rollup::composer_rpc::l2_to_l1",
                    idx,
                    return_data_len = data.len(),
                    return_data_hex = %format!("0x{}", hex::encode(&data[..data.len().min(32)])),
                    delivery_failed = failed,
                    "chained return call result"
                );
            }

            // Build return calls with REAL sequential data
            let mut all_return_calls: Vec<ReturnEdge> = Vec::new();
            let mut rc_data_idx = 0;
            for (i, call) in detected_calls.iter().enumerate() {
                let is_same_pattern =
                    call.destination == first.destination && call.calldata == first.calldata;
                if is_same_pattern {
                    for rc in &detected_return_calls {
                        let mut rc_clone = rc.clone();
                        rc_clone.parent_call_index = crate::cross_chain::ParentLink::Child(crate::cross_chain::AbsoluteCallIndex::new(i));
                        // Use REAL chained data instead of cloned data
                        if rc_data_idx < chained_rc_data.len() {
                            let (ref data, failed) = chained_rc_data[rc_data_idx];
                            rc_clone.return_data = data.clone();
                            rc_clone.delivery_failed = failed;
                        }
                        all_return_calls.push(rc_clone);
                    }
                    rc_data_idx += 1;
                }
            }

            return queue_l2_to_l1_multi_call_entries(
                client,
                upstream_url,
                raw_tx_hex,
                detected_calls,
                &all_return_calls,
                rollup_id,
                tx_outcome,
            )
            .await
            .is_some();
        } else {
            tracing::info!(
                target: "based_rollup::composer_rpc::l2_to_l1",
                count = detected_calls.len(),
                "duplicate simple calls detected — routing independently with chained simulation"
            );
            return queue_independent_calls_l2_to_l1(
                client,
                l1_rpc_url,
                upstream_url,
                raw_tx_hex,
                detected_calls,
                rollups_address,
                rollup_id,
                tx_outcome,
            )
            .await;
        }
    }

    if detected_calls.len() >= 2 {
        // Multi-call L2→L1: recursive ping-pong discovery loop.
        //
        // Phase A: Simulate L2→L1 calls on L1 (combined delivery) to discover
        //          L1→L2 return calls.
        // Phase B: Simulate those return calls on L2 to discover further L2→L1 calls.
        // Repeat until no new calls are found or MAX_RECURSIVE_DEPTH is reached.
        //
        // Depth-1 behavior (existing multi-call continuations) is preserved: the loop runs one
        // Phase A iteration, Phase B returns empty (stub), and the loop exits.
        const MAX_RECURSIVE_DEPTH: usize = 5;

        let mut all_detected_l2_calls = detected_calls.clone();
        // Seed with return calls already discovered during L2 iterative discovery.
        // This avoids redundant rediscovery in Phase A's first iteration.
        let mut all_return_calls: Vec<ReturnEdge> = early_return_calls.to_vec();
        if !all_return_calls.is_empty() {
            tracing::info!(
                target: "based_rollup::proxy",
                count = all_return_calls.len(),
                "Phase A/B seeded with early return calls from L2 iterative discovery"
            );
        }
        let mut current_l2_calls = detected_calls.clone();

        for depth in 0..MAX_RECURSIVE_DEPTH {
            // Phase A: Simulate current L2→L1 calls on L1.
            let call_refs: Vec<&DiscoveredCall> = current_l2_calls.iter().collect();
            let sim_results = simulate_l1_combined_delivery(
                client,
                l1_rpc_url,
                upstream_url,
                cross_chain_manager_address,
                rollups_address,
                builder_address,
                builder_private_key,
                rollup_id,
                &call_refs,
                tx_bytes,
            )
            .await;

            let sim_results_vec = sim_results.unwrap_or_default();

            // Update delivery_return_data and delivery_failed on the current L2 calls from simulation results.
            // sim_results_vec[i] corresponds to current_l2_calls[i].
            for (i, (data, failed, _rcs)) in sim_results_vec.iter().enumerate() {
                // Find the corresponding call in all_detected_l2_calls.
                // The current_l2_calls were appended at the end of all_detected_l2_calls
                // (or ARE all_detected_l2_calls for depth==0).
                let global_idx = all_detected_l2_calls.len() - current_l2_calls.len() + i;
                if global_idx < all_detected_l2_calls.len() {
                    if !data.is_empty() {
                        all_detected_l2_calls[global_idx].delivery_return_data = data.clone();
                    }
                    all_detected_l2_calls[global_idx].delivery_failed = *failed;
                }
            }

            // Remap parent_call_index from local (relative to current_l2_calls
            // slice) to global (relative to all_detected_l2_calls). At depth 0
            // the offset is 0 so this is a no-op; at depth > 0 it shifts indices
            // to point at the correct entry in the global array.
            let global_offset = all_detected_l2_calls.len() - current_l2_calls.len();
            let new_return_calls: Vec<ReturnEdge> = sim_results_vec
                .into_iter()
                .flat_map(|(_data, _failed, rcs)| rcs)
                .map(|mut rc| {
                    // Rebase: if the return call is currently linked to a child
                    // index `i` in the bundle slice, it must be rebased to
                    // `i + global_offset` in the absolute `all_detected_l2_calls`
                    // slice (closes invariant #7 partially — see ParentLink docs).
                    if let Some(idx) = rc.parent_call_index.child_index_mut() {
                        *idx = crate::cross_chain::AbsoluteCallIndex::from_usize_at_boundary(
                            idx.as_usize() + global_offset,
                        );
                    }
                    rc
                })
                .collect();

            if new_return_calls.is_empty() {
                if depth == 0 {
                    tracing::info!(
                        target: "based_rollup::proxy",
                        "combined L1 simulation returned no return calls — \
                         analytical fallback in table_builder will handle continuation construction"
                    );
                } else {
                    tracing::info!(
                        target: "based_rollup::proxy",
                        depth,
                        "recursive discovery: no return calls at depth {depth}, stopping"
                    );
                }
                break;
            }

            tracing::info!(
                target: "based_rollup::proxy",
                depth,
                count = new_return_calls.len(),
                "recursive discovery phase A: L1 simulation discovered {} return calls at depth {}",
                new_return_calls.len(), depth
            );

            all_return_calls.extend(new_return_calls.clone());

            // Phase B: Simulate return calls on L2 to find deeper L2→L1 calls.
            let new_l2_calls = enrichment::simulate_l2_return_call_delivery(
                client,
                upstream_url,
                cross_chain_manager_address,
                &new_return_calls,
                rollup_id,
            )
            .await;

            if new_l2_calls.is_empty() {
                tracing::info!(
                    target: "based_rollup::proxy",
                    depth,
                    "recursive discovery phase B: no new L2 calls at depth {depth}, converged"
                );
                break;
            }

            tracing::info!(
                target: "based_rollup::proxy",
                depth,
                new_l2_calls = new_l2_calls.len(),
                "recursive discovery phase B: found {} new L2 calls at depth {}",
                new_l2_calls.len(), depth
            );

            all_detected_l2_calls.extend(new_l2_calls.clone());
            current_l2_calls = new_l2_calls; // next iteration simulates these
        }

        return queue_l2_to_l1_multi_call_entries(
            client,
            upstream_url,
            raw_tx_hex,
            &all_detected_l2_calls,
            &all_return_calls,
            rollup_id,
            tx_outcome,
        )
        .await
        .is_some();
    }

    // Single call path: simulate on L1 to get delivery data + return calls.
    let call = &detected_calls[0];
    let root_scope: Vec<U256> = if call.trace_depth <= 1 {
        vec![]
    } else {
        vec![U256::ZERO; call.trace_depth]
    };
    let (delivery_return_data, delivery_failed, return_calls) = simulate_l1_delivery(
        client,
        l1_rpc_url,
        upstream_url,
        cross_chain_manager_address,
        rollups_address,
        builder_address,
        builder_private_key,
        rollup_id,
        call.source_address,
        call.destination,
        &call.calldata,
        call.value,
        tx_bytes,
        &root_scope,
        &call.delivery_return_data,
        call.delivery_failed,
    )
    .await
    .unwrap_or((vec![], false, vec![]));

    // Depth > 1 recursive discovery: if the L1 simulation discovered return calls,
    // run Phase A/B loop to discover arbitrarily deep L2→L1 ↔ L1→L2 chains.
    //
    // NOTE: The phase labels here appear reversed relative to docs/DERIVATION.md §14f, which
    // defines Phase A = "simulate L2-to-L1 calls on L1" and Phase B = "simulate return
    // calls on L2". That ordering reflects the multi-call path, which starts from scratch.
    // Here, the single-call path already ran the initial L1 simulation (Phase A in spec
    // terms) via simulate_l1_delivery above. The loop therefore begins with what the spec
    // calls Phase B (simulate return calls on L2), and uses the spec's Phase A as the
    // inner step. The phase labels in the log messages below match that mid-cycle entry
    // point and should NOT be renamed to match the spec's multi-call ordering.
    //
    // Uses the same MAX_RECURSIVE_DEPTH=5 as the multi-call path.
    if !return_calls.is_empty() {
        const MAX_RECURSIVE_DEPTH: usize = 5;

        let mut all_l2_calls = detected_calls.clone(); // starts with [original_call]
        // Propagate delivery_return_data and delivery_failed from simulate_l1_delivery to the first call.
        if !all_l2_calls.is_empty() {
            all_l2_calls[0].delivery_return_data = delivery_return_data.clone();
            all_l2_calls[0].delivery_failed = delivery_failed;
        }
        let mut all_return_calls = return_calls.clone();
        let mut current_return_calls = return_calls.clone();

        for depth in 0..MAX_RECURSIVE_DEPTH {
            tracing::info!(
                target: "based_rollup::proxy",
                depth,
                return_calls_count = current_return_calls.len(),
                "single-call depth > 1: Phase A — simulating {} return calls on L2",
                current_return_calls.len()
            );

            // Phase A: simulate return calls on L2 to find nested L2→L1 calls.
            let nested_l2_calls = enrichment::simulate_l2_return_call_delivery(
                client,
                upstream_url,
                cross_chain_manager_address,
                &current_return_calls,
                rollup_id,
            )
            .await;

            if nested_l2_calls.is_empty() {
                tracing::info!(
                    target: "based_rollup::proxy",
                    depth,
                    "single-call depth > 1: Phase A found no nested calls, converged"
                );
                break;
            }

            tracing::info!(
                target: "based_rollup::proxy",
                depth,
                nested_calls = nested_l2_calls.len(),
                total_l2_calls = all_l2_calls.len() + nested_l2_calls.len(),
                "single-call depth > 1: Phase A found {} nested L2→L1 calls",
                nested_l2_calls.len()
            );

            all_l2_calls.extend(nested_l2_calls.clone());

            // Phase B: simulate nested L2→L1 calls on L1 to find more return calls.
            let new_return_calls = {
                let call_refs: Vec<&DiscoveredCall> = nested_l2_calls.iter().collect();
                let sim_results = simulate_l1_combined_delivery(
                    client,
                    l1_rpc_url,
                    upstream_url,
                    cross_chain_manager_address,
                    rollups_address,
                    builder_address,
                    builder_private_key,
                    rollup_id,
                    &call_refs,
                    tx_bytes,
                )
                .await;

                let sim_results_vec = sim_results.unwrap_or_default();

                // Update delivery_return_data and delivery_failed on nested calls from simulation results.
                for (i, (data, failed, _rcs)) in sim_results_vec.iter().enumerate() {
                    // nested_l2_calls were just appended to all_l2_calls above.
                    let global_idx = all_l2_calls.len() - nested_l2_calls.len() + i;
                    if global_idx < all_l2_calls.len() {
                        if !data.is_empty() {
                            all_l2_calls[global_idx].delivery_return_data = data.clone();
                        }
                        all_l2_calls[global_idx].delivery_failed = *failed;
                    }
                }

                // Remap parent_call_index from local (relative to nested_l2_calls
                // slice passed to simulate_l1_combined_delivery) to global (relative
                // to all_l2_calls). nested_l2_calls were just appended at the end of
                // all_l2_calls, so offset = all_l2_calls.len() - nested_l2_calls.len().
                let global_offset = all_l2_calls.len() - nested_l2_calls.len();
                let rcs: Vec<ReturnEdge> = sim_results_vec
                    .into_iter()
                    .flat_map(|(_data, _failed, rcs)| rcs)
                    .map(|mut rc| {
                        // Rebase from `nested_l2_calls`-local index to absolute
                        // `all_l2_calls` index (closes invariant #7 partially).
                        if let Some(idx) = rc.parent_call_index.child_index_mut() {
                            *idx = crate::cross_chain::AbsoluteCallIndex::from_usize_at_boundary(
                                idx.as_usize() + global_offset,
                            );
                        }
                        rc
                    })
                    .collect();
                rcs
            };

            if new_return_calls.is_empty() {
                tracing::info!(
                    target: "based_rollup::proxy",
                    depth,
                    "single-call depth > 1: Phase B found no return calls, converged"
                );
                break;
            }

            tracing::info!(
                target: "based_rollup::proxy",
                depth,
                new_return_calls = new_return_calls.len(),
                "single-call depth > 1: Phase B found {} return calls",
                new_return_calls.len()
            );

            all_return_calls.extend(new_return_calls.clone());
            current_return_calls = new_return_calls;
        }

        // If we found any nested L2→L1 calls OR return calls, promote to multi-call path.
        // The return call check is critical: a depth-2 pattern with 1 L2→L1 call + 1
        // terminal L1→L2 return call (e.g., Logger→Logger→Counter) has all_l2_calls=1
        // but needs continuation entries for the inner return call (issue #245).
        if all_l2_calls.len() > 1 || !all_return_calls.is_empty() {
            tracing::info!(
                target: "based_rollup::proxy",
                total_l2_calls = all_l2_calls.len(),
                total_return_calls = all_return_calls.len(),
                "depth > 1 detected — promoting to multi-call path"
            );

            // Assign parent_call_index: each return call links to the L2→L1 call
            // whose L1 execution produced it. This creates NESTED scopes on L2
            // (each executeCrossChainCall gets its own scope chain with one child).
            //
            // With nested scopes, all RESULT{L2,void} scope-exit entries are
            // identical → consumption order doesn't matter for swap-and-pop.
            // Sequential scopes (all children on call[0]) would mix callReturn
            // and terminal RESULT under the same hash, which breaks under
            // cross-hash swap-and-pop disruption.
            // The initial return calls (from simulate_l1_delivery) have
            // parent_call_index=None. Assign them to call[0] — they were
            // discovered by simulating the first L2→L1 call's delivery.
            // Phase B return calls already have correct parent from the fix above.
            let mut return_calls_with_parent = Vec::new();
            for rc in &all_return_calls {
                let mut rc_clone = rc.clone();
                if rc_clone.parent_call_index.is_root() {
                    rc_clone.parent_call_index = crate::cross_chain::ParentLink::Child(crate::cross_chain::AbsoluteCallIndex::new(0));
                }

                // Simulate the return call's execution on L2 via debug_traceCallMany
                // to capture its return data. Using trace instead of eth_call provides
                // full call tree visibility for ExecutionNotFound detection and error
                // differentiation (consistent with enrich_return_calls_via_l2_trace).
                //
                // On L2, _processCallAtScope calls proxy.executeOnBehalf(destination, data)
                // which does destination.call(data). executeOnBehalf uses assembly return,
                // so the return data is the raw bytes from destination.call(data).
                //
                // We use from=proxy so msg.sender matches the real context. If proxy
                // address lookup fails, fall back to source_address directly.
                if rc_clone.return_data.is_empty() && !rc_clone.delivery_failed {
                    // Compute L2 proxy address for rc.source_address (L1 contract)
                    let proxy_from = {
                        use alloy_sol_types::SolCall;
                        let compute_data =
                            crate::cross_chain::IRollups::computeCrossChainProxyAddressCall {
                                originalAddress: rc.source_address,
                                originalRollupId: alloy_primitives::U256::ZERO,
                            }
                            .abi_encode();
                        let compute_hex = format!("0x{}", hex::encode(&compute_data));
                        let req = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "eth_call",
                            "params": [{
                                "to": format!("{cross_chain_manager_address}"),
                                "data": compute_hex
                            }, "latest"],
                            "id": 99961
                        });
                        async {
                            let resp = client.post(upstream_url).json(&req).send().await.ok()?;
                            let body: Value = resp.json().await.ok()?;
                            let s = body.get("result")?.as_str()?;
                            let clean = s.strip_prefix("0x").unwrap_or(s);
                            if clean.len() >= 64 {
                                Some(format!("0x{}", &clean[24..64]))
                            } else {
                                None
                            }
                        }
                        .await
                    };

                    let source_hex = format!("{}", rc.source_address);
                    let from_addr = if let Some(ref proxy) = proxy_from {
                        proxy.as_str()
                    } else {
                        source_hex.as_str()
                    };

                    let trace_req = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "debug_traceCallMany",
                        "params": [[{
                            "transactions": [{
                                "from": from_addr,
                                "to": format!("{}", rc.destination),
                                "data": format!("0x{}", hex::encode(&rc.data)),
                                "value": format!("0x{:x}", rc.value),
                                "gas": "0x2faf080"
                            }]
                        }], null, { "tracer": "callTracer" }],
                        "id": 99960
                    });

                    match client.post(upstream_url).json(&trace_req).send().await {
                        Ok(resp) => {
                            if let Ok(body) = resp.json::<Value>().await {
                                if let Some(trace) = body
                                    .get("result")
                                    .and_then(|r| r.get(0))
                                    .and_then(|b| b.as_array())
                                    .and_then(|arr| arr.first())
                                {
                                    let trace_error = trace.get("error").and_then(|v| v.as_str());
                                    if trace_error.is_some() {
                                        // Use the generic walk_trace_tree path to check if the
                                        // revert involves a cross-chain proxy call. Supports
                                        // both persistent and ephemeral proxy detection.
                                        let mut fpc_cache: std::collections::HashMap<
                                            Address,
                                            Option<super::super::trace::ProxyInfo>,
                                        > = std::collections::HashMap::new();
                                        let discovered_in_phase_b =
                                            enrichment::walk_l2_trace_for_discovered_proxy_calls(
                                                client,
                                                upstream_url,
                                                cross_chain_manager_address,
                                                trace,
                                                rollup_id,
                                                &mut fpc_cache,
                                            )
                                            .await;

                                        let has_reverted_proxies =
                                            discovered_in_phase_b.iter().any(|d| d.reverted);

                                        if has_reverted_proxies {
                                            // Reverted proxy calls found — a wrapper contract
                                            // needs table entries loaded. Leave return_data
                                            // empty and delivery_failed false so
                                            // enrich_return_calls_via_l2_trace (called after
                                            // this loop) retries with loadExecutionTable.
                                            tracing::info!(
                                                target: "based_rollup::proxy",
                                                dest = %rc.destination,
                                                error = ?trace_error,
                                                proxy_count = discovered_in_phase_b.len(),
                                                "L2 return call trace reverted with proxy calls \
                                                 in promotion path — deferring to \
                                                 enrich_return_calls_via_l2_trace"
                                            );
                                        } else {
                                            tracing::info!(
                                                target: "based_rollup::proxy",
                                                dest = %rc.destination,
                                                error = ?trace_error,
                                                "L2 return call trace reverted in promotion \
                                                 path (no proxy calls found) — marking as failed"
                                            );
                                            rc_clone.delivery_failed = true;
                                        }
                                    } else if let Some(output_hex) =
                                        trace.get("output").and_then(|v| v.as_str())
                                    {
                                        let clean =
                                            output_hex.strip_prefix("0x").unwrap_or(output_hex);
                                        if let Ok(bytes) = hex::decode(clean) {
                                            if !bytes.is_empty() {
                                                tracing::info!(
                                                    target: "based_rollup::proxy",
                                                    dest = %rc.destination,
                                                    return_data_len = bytes.len(),
                                                    "captured L2 return data for return \
                                                     call via debug_traceCallMany (issue #245)"
                                                );
                                                rc_clone.return_data = bytes;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "based_rollup::proxy",
                                %e,
                                dest = %rc.destination,
                                "failed to trace L2 return call in promotion path"
                            );
                        }
                    }
                }

                return_calls_with_parent.push(rc_clone);
            }

            // Enrich any return calls that still need L2 return data. This
            // handles the non-leaf retry case: if the Phase-B trace above
            // reverted with ExecutionNotFound (a wrapper contract needs table
            // entries), enrich_return_calls_via_l2_trace will retry with
            // loadExecutionTable + the actual call in a bundled trace.
            enrichment::enrich_return_calls_via_l2_trace(
                client,
                upstream_url,
                cross_chain_manager_address,
                &mut return_calls_with_parent,
                rollup_id,
            )
            .await;

            return queue_l2_to_l1_multi_call_entries(
                client,
                upstream_url,
                raw_tx_hex,
                &all_l2_calls,
                &return_calls_with_parent,
                rollup_id,
                tx_outcome,
            )
            .await
            .is_some();
        }
    }

    // Simple single-call path (no depth > 1): use initiateL2CrossChainCall
    // Scope = vec![0; trace_depth] — determines newScope nesting in executeL2TX on L1.
    let l1_scope: Vec<String> = (0..if call.trace_depth <= 1 {
        0
    } else {
        call.trace_depth
    })
        .map(|_| "0x0".to_string())
        .collect();
    let initiate_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "syncrollups_initiateL2CrossChainCall",
        "params": [{
            "destination": format!("{}", call.destination),
            "data": format!("0x{}", hex::encode(&call.calldata)),
            "value": format!("{}", call.value),
            "sourceAddress": format!("{}", call.source_address),
            "deliveryReturnData": format!("0x{}", hex::encode(&delivery_return_data)),
            "deliveryFailed": delivery_failed,
            "rawL2Tx": raw_tx_hex,
            "l1DeliveryScope": l1_scope,
            "txReverts": tx_outcome
        }],
        "id": 99988
    });

    if let Ok(resp) = client.post(upstream_url).json(&initiate_req).send().await {
        if let Ok(body) = resp.json::<Value>().await {
            if body.get("error").is_none() {
                return true;
            }
        }
    }

    false
}

/// Queue entries for an L2→L1 multi-call continuation pattern using `buildL2ToL1ExecutionTable`.
///
/// When `simulate_l1_delivery` discovers L1→L2 return calls, the original L2→L1 calls
/// plus the return calls form a continuation chain. This function packages them as
/// `BuildL2ToL1ExecutionTableParams` and queues them atomically via the RPC.
///
/// The RPC builds L2 table entries (CALL+RESULT pairs for each L2→L1 call) and
/// 3 L1 deferred entries (continuation structure), queued as a single `QueuedL2ToL1Call`.
#[allow(clippy::too_many_arguments)]
async fn queue_l2_to_l1_multi_call_entries(
    client: &reqwest::Client,
    upstream_url: &str,
    raw_tx_hex: &str,
    detected_l2_calls: &[DiscoveredCall],
    return_calls: &[ReturnEdge],
    _rollup_id: u64,
    tx_outcome: crate::cross_chain::TxOutcome,
) -> Option<()> {
    if detected_l2_calls.is_empty() {
        return None;
    }

    for (i, call) in detected_l2_calls.iter().enumerate() {
        tracing::info!(
            target: "based_rollup::proxy",
            idx = i,
            destination = %call.destination,
            source_address = %call.source_address,
            calldata_len = call.calldata.len(),
            calldata_hex = %format!("0x{}", hex::encode(&call.calldata)),
            value = %call.value,
            "L2->L1 multi-call: sending call to buildL2ToL1ExecutionTable"
        );
    }
    for (i, rc) in return_calls.iter().enumerate() {
        tracing::info!(
            target: "based_rollup::proxy",
            idx = i,
            destination = %rc.destination,
            source_address = %rc.source_address,
            data_len = rc.data.len(),
            data_hex = %format!("0x{}", hex::encode(&rc.data)),
            value = %rc.value,
            "L2->L1 multi-call: sending return call to buildL2ToL1ExecutionTable"
        );
    }

    // Build l2Calls from ALL detected L2→L1 calls.
    let l2_calls: Vec<serde_json::Value> = detected_l2_calls
        .iter()
        .map(|call| {
            let scope_vals: Vec<String> = (0..call.trace_depth.max(1))
                .map(|_| "0x0".to_string())
                .collect();
            serde_json::json!({
                "destination": format!("{}", call.destination),
                "data": format!("0x{}", hex::encode(&call.calldata)),
                "value": format!("{}", call.value),
                "sourceAddress": format!("{}", call.source_address),
                "deliveryReturnData": format!("0x{}", hex::encode(&call.delivery_return_data)),
                "deliveryFailed": call.delivery_failed,
                "inRevertedFrame": call.in_reverted_frame,
                "scope": scope_vals
            })
        })
        .collect();

    // Build returnCalls from the detected L1→L2 return calls.
    let rpc_return_calls: Vec<serde_json::Value> = return_calls
        .iter()
        .map(|rc| {
            let mut obj = serde_json::json!({
                "destination": format!("{}", rc.destination),
                "data": format!("0x{}", hex::encode(&rc.data)),
                "value": format!("{}", rc.value),
                "sourceAddress": format!("{}", rc.source_address)
            });
            if let Some(idx) = rc.parent_call_index.child_index() {
                obj.as_object_mut()
                    .expect("serde_json::json!({ ... }) always yields an Object").insert(
                    "parentCallIndex".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(idx.as_usize())),
                );
            }
            if !rc.return_data.is_empty() {
                obj.as_object_mut()
                    .expect("serde_json::json!({ ... }) always yields an Object").insert(
                    "l2ReturnData".to_string(),
                    serde_json::Value::String(format!("0x{}", hex::encode(&rc.return_data))),
                );
            }
            if rc.delivery_failed {
                obj.as_object_mut()
                    .expect("serde_json::json!({ ... }) always yields an Object").insert(
                    "l2DeliveryFailed".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            obj
        })
        .collect();

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "syncrollups_buildL2ToL1ExecutionTable",
        "params": [{
            "l2Calls": l2_calls,
            "returnCalls": rpc_return_calls,
            "gasPrice": 0,
            "rawL2Tx": raw_tx_hex,
            "txReverts": tx_outcome
        }],
        "id": 99992
    });

    let first = &detected_l2_calls[0];
    match client.post(upstream_url).json(&req).send().await {
        Ok(resp) => {
            if let Ok(body) = resp.json::<Value>().await {
                if let Some(error) = body.get("error") {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        %error,
                        "buildL2ToL1ExecutionTable failed for multi-call continuation"
                    );
                    // Fall back to simple initiateL2CrossChainCall for the first call.
                    return queue_l2_to_l1_fallback(
                        client,
                        upstream_url,
                        raw_tx_hex,
                        first.destination,
                        &first.calldata,
                        first.value,
                        first.source_address,
                        first.trace_depth,
                        tx_outcome,
                    )
                    .await;
                }

                let l2_count = body
                    .get("result")
                    .and_then(|v| v.get("l2EntryCount"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let l1_count = body
                    .get("result")
                    .and_then(|v| v.get("l1EntryCount"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                tracing::info!(
                    target: "based_rollup::proxy",
                    l2_entries = l2_count,
                    l1_entries = l1_count,
                    l2_call_count = detected_l2_calls.len(),
                    return_call_count = return_calls.len(),
                    "L2→L1 multi-call continuation entries queued via buildL2ToL1ExecutionTable"
                );

                return Some(());
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::proxy",
                %e,
                "buildL2ToL1ExecutionTable request failed"
            );
        }
    }

    // Fall back to simple L2→L1 call if the new RPC failed.
    queue_l2_to_l1_fallback(
        client,
        upstream_url,
        raw_tx_hex,
        first.destination,
        &first.calldata,
        first.value,
        first.source_address,
        first.trace_depth,
        tx_outcome,
    )
    .await
}

/// Fallback: queue a simple L2→L1 call entry when the continuation RPC fails.
#[allow(clippy::too_many_arguments)]
async fn queue_l2_to_l1_fallback(
    client: &reqwest::Client,
    upstream_url: &str,
    raw_tx_hex: &str,
    destination: Address,
    data: &[u8],
    value: U256,
    sender: Address,
    trace_depth: usize,
    tx_outcome: crate::cross_chain::TxOutcome,
) -> Option<()> {
    tracing::info!(
        target: "based_rollup::proxy",
        "falling back to initiateL2CrossChainCall for L2→L1 multi-call continuation"
    );
    let l1_scope: Vec<String> = (0..if trace_depth <= 1 { 0 } else { trace_depth })
        .map(|_| "0x0".to_string())
        .collect();
    let initiate_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "syncrollups_initiateL2CrossChainCall",
        "params": [{
            "destination": format!("{destination}"),
            "data": format!("0x{}", hex::encode(data)),
            "value": format!("{value}"),
            "sourceAddress": format!("{sender}"),
            "deliveryReturnData": "0x",
            "deliveryFailed": false,
            "rawL2Tx": raw_tx_hex,
            "l1DeliveryScope": l1_scope,
            "txReverts": tx_outcome
        }],
        "id": 99991
    });

    tracing::info!(
        target: "based_rollup::proxy",
        %destination,
        %sender,
        tx_reverts = tx_outcome.is_revert(),
        trace_depth,
        request_json = %serde_json::to_string(&initiate_req).unwrap_or_default(),
        "queue_l2_to_l1_fallback: sending initiateL2CrossChainCall"
    );

    if let Ok(resp) = client.post(upstream_url).json(&initiate_req).send().await {
        if let Ok(body) = resp.json::<Value>().await {
            if body.get("error").is_none() {
                return Some(());
            }
        }
    }
    None
}

/// Queue N identical L2→L1 cross-chain calls independently, each with its own
/// CALL+RESULT pair. Uses chained simulation so each call's delivery return data
/// reflects state changes from previous calls on L1.
/// Direction: L2→L1 (composer RPC for withdrawals / L2→L1 proxy calls).
///
/// For duplicate calls (e.g., a contract calling the same L1 target twice),
/// the continuation path produces chained RESULT→CALL entries with hashes that
/// depend on delivery return data. Since delivery return data is state-dependent,
/// each call must be routed independently with its own pre-computed delivery data.
#[allow(clippy::too_many_arguments)]
async fn queue_independent_calls_l2_to_l1(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    upstream_url: &str,
    raw_tx_hex: &str,
    detected_calls: &[DiscoveredCall],
    rollups_address: Address,
    rollup_id: u64,
    tx_outcome: crate::cross_chain::TxOutcome,
) -> bool {
    // 1. Run chained simulation to get correct per-call delivery return data.
    let chained_results = simulate_chained_delivery_l2_to_l1(
        client,
        l1_rpc_url,
        upstream_url,
        rollups_address,
        rollup_id,
        Address::ZERO, // builder_address not needed for delivery simulation
        "",            // builder_private_key not needed
        detected_calls,
        0, // trace_block_number — computed internally
        0, // trace_block_timestamp — computed internally
    )
    .await;

    tracing::info!(
        target: "based_rollup::composer_rpc::l2_to_l1",
        call_count = detected_calls.len(),
        chained_results_count = chained_results.len(),
        "routing {} identical calls independently with chained simulation",
        detected_calls.len(),
    );

    // 2. Queue each call via initiateL2CrossChainCall with pre-computed delivery data.
    let mut any_success = false;
    for (i, call) in detected_calls.iter().enumerate() {
        let (delivery_return_data, delivery_failed) = if i < chained_results.len() {
            let (data, success) = &chained_results[i];
            (data.clone(), !success) // delivery_failed = !success
        } else {
            (vec![], false) // fallback: empty return data, not failed
        };

        // First call carries the raw L2 tx; subsequent calls use empty string
        // so the driver forwards the raw tx exactly once.
        let raw_l2_tx = if i == 0 { raw_tx_hex } else { "" };

        // Build the RPC request WITH pre-computed delivery return data.
        let l1_scope: Vec<String> = (0..if call.trace_depth <= 1 {
            0
        } else {
            call.trace_depth
        })
            .map(|_| "0x0".to_string())
            .collect();
        let initiate_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "syncrollups_initiateL2CrossChainCall",
            "params": [{
                "destination": format!("{}", call.destination),
                "data": format!("0x{}", hex::encode(&call.calldata)),
                "value": format!("{}", call.value),
                "sourceAddress": format!("{}", call.source_address),
                "deliveryReturnData": format!("0x{}", hex::encode(&delivery_return_data)),
                "deliveryFailed": delivery_failed,
                "rawL2Tx": raw_l2_tx,
                "l1DeliveryScope": l1_scope,
                "txReverts": tx_outcome
            }],
            "id": 99970 + i as u64
        });

        let resp = match client.post(upstream_url).json(&initiate_req).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::composer_rpc::l2_to_l1",
                    call_idx = i,
                    %e,
                    "initiateL2CrossChainCall request failed for independent call"
                );
                continue;
            }
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::composer_rpc::l2_to_l1",
                    call_idx = i,
                    %e,
                    "initiateL2CrossChainCall response parse failed for independent call"
                );
                continue;
            }
        };

        if let Some(error) = body.get("error") {
            tracing::warn!(
                target: "based_rollup::composer_rpc::l2_to_l1",
                call_idx = i,
                ?error,
                "initiateL2CrossChainCall failed for independent call"
            );
            continue;
        }

        tracing::info!(
            target: "based_rollup::composer_rpc::l2_to_l1",
            call_idx = i,
            delivery_return_data_len = delivery_return_data.len(),
            delivery_failed,
            "independent call queued successfully"
        );

        any_success = true;
    }

    any_success
}
