//! L1 delivery simulation for L2→L1 cross-chain calls.
//!
//! Contains the core `simulate_l1_delivery` function (single-call),
//! `simulate_l1_combined_delivery` (multi-call), and their helper functions.
//! Shared between `l2_to_l1.rs` and `direction.rs`
//! (L2ToL1::enrich_calls_before_retrace).

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use serde_json::Value;

use super::common::{get_l1_block_context, get_rollup_state_root, get_verification_key};
use super::model::{DiscoveredCall, L1ProxyLookup, ReturnEdge};
use crate::cross_chain::{self, ScopePath, filter_new_by_count};

/// Maximum number of iterative discovery rounds in `simulate_l1_delivery`.
pub(crate) const MAX_SIMULATION_ITERATIONS: usize = 10;

/// Simulate L1 delivery of an L2→L1 cross-chain call via `debug_traceCallMany`.
///
/// Builds a `[postBatch(entries), executeL2TX(rlpTx)]` bundle, signs the ECDSA
/// proof, and traces the execution on L1. Iteratively discovers L1→L2 return
/// calls (continuations) by rebuilding entries incorporating the return calls
/// and re-simulating until convergence or `MAX_SIMULATION_ITERATIONS` is reached.
///
/// Returns `(return_data, failed, detected_return_calls)`.
/// Returns `None` if the simulation cannot be performed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn simulate_l1_delivery(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<&str>,
    rollup_id: u64,
    _trigger_user: Address,
    destination: Address,
    data: &[u8],
    value: U256,
    rlp_encoded_tx: &[u8],
    root_scope: &[U256],
    known_delivery_return_data: &[u8],
    known_delivery_failed: bool,
) -> Option<(Vec<u8>, bool, Vec<ReturnEdge>)> {
    // First check if destination has code on L1.
    // If it's an EOA, return data is empty and we skip simulation.
    let code_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getCode",
        "params": [format!("{destination}"), "latest"],
        "id": 1
    });
    let code_resp = client.post(l1_rpc_url).json(&code_req).send().await.ok()?;
    let code_body: super::common::JsonRpcResponse = code_resp.json().await.ok()?;
    let code_hex = code_body.result_str()?;
    if code_hex == "0x" || code_hex == "0x0" {
        // EOA target — no return data
        return Some((vec![], false, vec![]));
    }

    // Contract target — full traceCallMany simulation.
    // Build preliminary L1 deferred entries, sign postBatch proof, simulate
    // [postBatch, executeL2TX] on L1, and extract delivery return data.
    tracing::info!(
        target: "based_rollup::proxy",
        %destination,
        data_len = data.len(),
        "L1 target has code — running full traceCallMany simulation"
    );

    // Parse builder private key — required for signing postBatch proof.
    let key_hex = builder_private_key?;
    let key_clean = key_hex.strip_prefix("0x").unwrap_or(key_hex);
    let builder_key = match key_clean.parse::<alloy_signer_local::PrivateKeySigner>() {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::proxy",
                %e,
                "failed to parse builder private key for L1 simulation"
            );
            return Some((vec![], false, vec![]));
        }
    };

    // When delivery failure is already known (from iterative L2 discovery's direct L1
    // simulation), skip the full postBatch+executeL2TX bundle simulation entirely.
    // The bundle simulation produces wrong return data for failed deliveries because
    // placeholder state deltas cause _findAndApplyExecution to miss entries (stateRoot
    // != currentState), making executeL2TX revert with ExecutionNotFound.
    // The direct L1 simulation (debug_traceCallMany to destination) already captured
    // the correct revert data — use it as-is.
    if known_delivery_failed && !known_delivery_return_data.is_empty() {
        tracing::info!(
            target: "based_rollup::proxy",
            %destination,
            return_data_len = known_delivery_return_data.len(),
            return_data_hex = %format!("0x{}", hex::encode(&known_delivery_return_data[..known_delivery_return_data.len().min(40)])),
            "simulate_l1_delivery: skipping bundle simulation — using known delivery failure data"
        );
        return Some((known_delivery_return_data.to_vec(), true, vec![]));
    }

    // Iterative discovery loop: simulate, extract return calls, rebuild entries, repeat.
    // Seed with known delivery data from the L2 iterative discovery's own L1 simulation.
    // This gives correct RESULT hashes on iteration 1, avoiding the convergence cycle
    // (empty → real → confirm) for calls where delivery_return_data is already known.
    let mut all_return_calls: Vec<ReturnEdge> = Vec::new();
    let mut final_return_data: Vec<u8> = known_delivery_return_data.to_vec();
    let mut prev_return_data: Vec<u8> = known_delivery_return_data.to_vec();
    let mut final_delivery_failed = known_delivery_failed;

    tracing::info!(
        target: "based_rollup::proxy",
        %destination,
        known_delivery_return_data_len = known_delivery_return_data.len(),
        known_delivery_return_data_hex = %format!("0x{}", hex::encode(&known_delivery_return_data[..known_delivery_return_data.len().min(40)])),
        known_delivery_failed,
        "simulate_l1_delivery: starting with known delivery data"
    );

    for iteration in 1..=MAX_SIMULATION_ITERATIONS {
        tracing::info!(
            target: "based_rollup::proxy",
            iteration,
            known_return_calls = all_return_calls.len(),
            "L1 delivery simulation iteration"
        );

        // Build L1 deferred entries. On first iteration, use simple L2TX+RESULT entries.
        // On subsequent iterations, include continuation entries for discovered return calls.
        let entries = if all_return_calls.is_empty() {
            // Simple case: just the original L2→L1 call
            let call_entries = super::entry_builder::build_l2_to_l1_entries(
                destination,
                data.to_vec(),
                value,
                _trigger_user,
                rollup_id,
                rlp_encoded_tx.to_vec(), // RLP-encoded L2 tx for L2TX trigger
                final_return_data.clone(), // use known data (from iterative discovery or previous iteration)
                final_delivery_failed,     // use known failed flag
                root_scope.to_vec(),       // l1_delivery_scope from trace depth
                cross_chain::TxOutcome::Success, // tx_reverts (simulation path, not real queueing)
            );
            call_entries.l1_deferred_entries
        } else {
            // Continuation case: use the SAME table builder functions as the real
            // batch (build_l2_to_l1_continuation_entries) to ensure identical entry
            // ordering. Manual construction produced [L2TX, scope_RESULT, child...]
            // while the real batch produces [trigger, child..., scope_RESULT], causing
            // swap-and-pop to consume entries in wrong order during simulation.
            let root_call = crate::table_builder::L2DetectedCall {
                destination,
                data: data.to_vec(),
                value,
                source_address: _trigger_user,
                delivery_return_data: final_return_data.clone(),
                delivery_failed: final_delivery_failed,
                scope: ScopePath::from_parts(root_scope.to_vec()),
                in_reverted_frame: false,
            };

            let return_calls_for_builder: Vec<crate::table_builder::L2ReturnCall> =
                all_return_calls
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

            let analyzed = super::entry_builder::analyze_l2_to_l1_continuations(
                &[root_call],
                &return_calls_for_builder,
                rollup_id,
            );
            let continuation = crate::table_builder::build_l2_to_l1_continuation_entries(
                &analyzed,
                cross_chain::RollupId::new(alloy_primitives::U256::from(rollup_id)),
                rlp_encoded_tx,
                cross_chain::TxOutcome::Success, // tx_reverts
            );

            tracing::info!(
                target: "based_rollup::proxy",
                l1_entry_count = continuation.l1_entries.len(),
                return_call_count = all_return_calls.len(),
                "built simulation entries via table builder (same path as real batch)"
            );

            continuation.l1_entries
        };

        // Clear placeholder state deltas for simulation-only entries.
        // Entries are built with currentState=0x0 / newState=0x0 (placeholders
        // for the driver to fill with real intermediate roots). In the simulation,
        // Rollups.sol._findAndApplyExecution checks delta.currentState against the
        // on-chain root — 0x0 never matches. Clearing deltas makes the match
        // unconditional (empty deltas → no state root check).
        // Same fix as L1→L2 path (commit 6885cf0).
        let mut entries = entries;
        for e in &mut entries {
            e.state_deltas.clear();
        }

        if entries.is_empty() {
            tracing::warn!(
                target: "based_rollup::proxy",
                "entry building produced no L1 deferred entries"
            );
            return Some((final_return_data, final_delivery_failed, all_return_calls));
        }

        // Get L1 block context for proof signing.
        let (block_number, block_hash, _parent_hash) =
            match super::common::get_l1_block_context(client, l1_rpc_url).await {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        %e,
                        "failed to get L1 block context for simulation"
                    );
                    return Some((final_return_data, final_delivery_failed, all_return_calls));
                }
            };

        let trace_block_number = block_number + 1;
        let trace_parent_hash = block_hash;
        // For traceCallMany simulation, we control the block timestamp via blockOverride.
        // Use current time — the override ensures consistency between signed proof and simulation.
        // Fallback to 0 on the vanishingly rare SystemTime-before-epoch case.
        let trace_block_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Get verification key from Rollups contract.
        let vk = match super::common::get_verification_key(
            client,
            l1_rpc_url,
            rollups_address,
            rollup_id,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "failed to get verification key for simulation"
                );
                return Some((final_return_data, final_delivery_failed, all_return_calls));
            }
        };

        // Sign ECDSA proof for postBatch.
        let call_data_bytes = alloy_primitives::Bytes::new();
        let entry_hashes = cross_chain::compute_entry_hashes(&entries, vk);
        let public_inputs_hash = cross_chain::compute_public_inputs_hash(
            &entry_hashes,
            &call_data_bytes,
            trace_parent_hash,
            trace_block_timestamp,
        );

        use alloy_signer::SignerSync;
        let sig = match builder_key.sign_hash_sync(&public_inputs_hash) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "failed to sign proof for L1 simulation"
                );
                return Some((final_return_data, final_delivery_failed, all_return_calls));
            }
        };
        let sig_bytes = sig.as_bytes();
        let mut proof_bytes = sig_bytes.to_vec();
        if proof_bytes.len() == 65 && proof_bytes[64] < 27 {
            proof_bytes[64] += 27;
        }
        let proof = alloy_primitives::Bytes::from(proof_bytes);

        // Encode postBatch calldata.
        let post_batch_calldata =
            cross_chain::encode_post_batch_calldata(&entries, call_data_bytes, proof);

        // Encode executeL2TX calldata using typed ABI encoding (NEVER hardcode selectors).
        let execute_l2tx_calldata = cross_chain::IRollups::executeL2TXCall {
            rollupId: alloy_primitives::U256::from(rollup_id),
            rlpEncodedTx: rlp_encoded_tx.to_vec().into(),
        }
        .abi_encode();

        // Build traceCallMany request: [postBatch, executeL2TX] in a single bundle.
        let builder_addr_hex = format!("{builder_address}");
        let rollups_hex = format!("{rollups_address}");
        let post_batch_data = format!("0x{}", hex::encode(post_batch_calldata.as_ref()));
        let execute_l2tx_data = format!("0x{}", hex::encode(&execute_l2tx_calldata));
        let next_block = format!("{:#x}", trace_block_number);

        let trace_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCallMany",
            "params": [
                [
                    {
                        "transactions": [
                            {
                                "from": builder_addr_hex,
                                "to": rollups_hex,
                                "data": post_batch_data,
                                "gas": "0x1c9c380"
                            },
                            {
                                "from": builder_addr_hex,
                                "to": rollups_hex,
                                "data": execute_l2tx_data,
                                "gas": "0xc35000"
                            }
                        ],
                        "blockOverride": {
                            "number": next_block,
                            "time": format!("{:#x}", trace_block_timestamp)
                        }
                    }
                ],
                null,
                { "tracer": "callTracer" }
            ],
            "id": 4
        });

        let rpc_resp: super::common::JsonRpcResponse =
            match client.post(l1_rpc_url).json(&trace_req).send().await {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            target: "based_rollup::proxy",
                            %e,
                            "traceCallMany response parse failed"
                        );
                        return Some((final_return_data, final_delivery_failed, all_return_calls));
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        %e,
                        "traceCallMany request failed"
                    );
                    return Some((final_return_data, final_delivery_failed, all_return_calls));
                }
            };

        // Extract traces from result — now 2 traces: [postBatch, executeL2TX].
        let result_val = match rpc_resp.into_result() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "traceCallMany returned error"
                );
                return Some((final_return_data, final_delivery_failed, all_return_calls));
            }
        };
        let bundle_traces = match result_val.get(0).and_then(|b| b.as_array()) {
            Some(arr) if arr.len() >= 2 => arr,
            _ => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    "traceCallMany returned unexpected structure (expected 2 traces)"
                );
                return Some((final_return_data, final_delivery_failed, all_return_calls));
            }
        };

        // Check postBatch result (tx0). Log if it reverted but DO NOT bail out.
        // The trigger trace (tx1) still contains inner calls even when postBatch
        // reverts (e.g., proof signature mismatch). These inner calls are essential
        // for depth-N return call detection — the standalone fallback that was here
        // previously called destination.call() directly (without entries/scope
        // navigation), which broke all depth-2+ patterns by missing return calls.
        let tx0_trace = &bundle_traces[0];
        if tx0_trace.get("error").is_some() || tx0_trace.get("revertReason").is_some() {
            let error_msg = tx0_trace
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            tracing::warn!(
                target: "based_rollup::proxy",
                error = error_msg,
                iteration,
                "postBatch reverted in L1 delivery simulation — \
                 continuing with trigger trace for return call detection"
            );
            // Fall through to trigger trace extraction below.
            // The trigger tx (tx1) also reverts, but callTracer still captures
            // all inner calls (proxy calls, executeCrossChainCall, etc.) which
            // are needed for depth-N return call discovery.
        }

        // Extract delivery output from executeL2TX trace (tx1).
        let trigger_trace = &bundle_traces[1];
        let (return_data, _delivery_failed) =
            extract_delivery_output_from_trigger_trace(trigger_trace, destination);

        // Trigger simulation is unreliable for L2→L1 calls: entries have
        // placeholder state deltas and the ECDSA proof may not match real L1
        // state. When delivery failure was already detected by the caller
        // (known_delivery_failed=true via iterative L2 discovery), preserve it.
        // Otherwise assume delivery succeeds — §4f + rewind handles real
        // failures on L1.
        if !known_delivery_failed {
            final_delivery_failed = false;
        }

        // Extract L1→L2 return calls from the trigger trace BEFORE deciding
        // the return data fallback — we need to know whether this is depth-1
        // (no return calls) or depth-2+ (has return calls) to choose the
        // correct strategy.
        let new_return_calls = extract_l1_to_l2_return_calls(
            client,
            l1_rpc_url,
            rollups_address,
            trigger_trace,
            rollup_id,
            root_scope,
        )
        .await;

        // Filter out already-known return calls using count-based comparison.
        // Supports legitimate duplicate return calls with identical
        // (destination, data, value, source_address) tuples. The CALL action hash
        // includes value and sourceAddress, so two calls with different ETH values
        // are distinct even if destination and data match.
        let truly_new = filter_new_by_count(new_return_calls, &all_return_calls, |a, b| {
            a.destination == b.destination
                && a.data == b.data
                && a.value == b.value
                && a.source_address == b.source_address
        });

        // Delivery return data from the trigger trace.
        // Per §C.6, the L2TX terminal RESULT is always void — we don't need
        // delivery_return_data for the terminal. But we still capture it for
        // the action_hash of the scope resolution entry (which matches what
        // _processCallAtScope produces on L1).
        // The trigger trace may be reverted (placeholder entries), so
        // find_delivery_call extracts data from inner calls even in reverted frames.
        final_return_data = return_data.clone();

        // Check convergence: no new return calls AND return data stabilized.
        // The return data may change across iterations: iteration N captures
        // revert data (delivery fails without entries for return calls),
        // iteration N+1 captures success data (entries now present). We need
        // one more iteration after return data changes so the entries are
        // rebuilt with the correct RESULT hash (#238).
        let return_data_changed = final_return_data != prev_return_data;
        if truly_new.is_empty() && !return_data_changed {
            tracing::info!(
                target: "based_rollup::proxy",
                iteration,
                total_return_calls = all_return_calls.len(),
                return_data_len = final_return_data.len(),
                delivery_hex = %format!("0x{}", hex::encode(&final_return_data)),
                "L1 delivery simulation converged — no new return calls, return data stable"
            );
            // Log each return call's return_data for hash comparison
            for (ri, rc) in all_return_calls.iter().enumerate() {
                tracing::info!(
                    target: "based_rollup::proxy",
                    ri,
                    dest = %rc.destination,
                    return_data_hex = %format!("0x{}", hex::encode(&rc.return_data)),
                    return_data_len = rc.return_data.len(),
                    delivery_failed = rc.delivery_failed,
                    "return call return_data at convergence"
                );
            }
            break;
        }
        if truly_new.is_empty() && return_data_changed {
            tracing::info!(
                target: "based_rollup::proxy",
                iteration,
                old_len = prev_return_data.len(),
                new_len = final_return_data.len(),
                "L1 delivery simulation: no new return calls but return data changed — re-iterating for correct RESULT hash"
            );
            prev_return_data = final_return_data.clone();
            continue;
        }

        tracing::info!(
            target: "based_rollup::proxy",
            iteration,
            new_return_calls = truly_new.len(),
            "discovered new L1->L2 return calls in delivery trace — re-simulating"
        );

        all_return_calls.extend(truly_new);

        // Enrich return calls with L2 return data via debug_traceCallMany.
        // This data is used by the next iteration's simulation bundle so the
        // inner RESULT entries carry real data instead of void (#246).
        super::l2_to_l1::enrich_return_calls_via_l2_trace(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            &mut all_return_calls,
            rollup_id,
        )
        .await;

        prev_return_data = final_return_data.clone();
    }

    if !all_return_calls.is_empty() {
        tracing::info!(
            target: "based_rollup::proxy",
            count = all_return_calls.len(),
            return_data_len = final_return_data.len(),
            delivery_failed = final_delivery_failed,
            "L1 delivery simulation complete (multi-call continuation pattern detected)"
        );
    } else {
        tracing::info!(
            target: "based_rollup::proxy",
            %destination,
            return_data_len = final_return_data.len(),
            delivery_failed = final_delivery_failed,
            "L1 delivery simulation complete"
        );
    }

    Some((final_return_data, final_delivery_failed, all_return_calls))
}

/// Compute the CrossChainProxy address on L1 for a given trigger user.
///
/// Calls `computeCrossChainProxyAddress(originalAddress, originalRollupId)`
/// on the Rollups contract.
pub(crate) async fn compute_proxy_address_on_l1(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    trigger_user: Address,
    rollup_id: u64,
) -> eyre::Result<Address> {
    // Encode computeCrossChainProxyAddress(address, uint256)
    let compute_data = cross_chain::IRollups::computeCrossChainProxyAddressCall {
        originalAddress: trigger_user,
        originalRollupId: U256::from(rollup_id),
    }
    .abi_encode();
    let compute_hex = format!("0x{}", hex::encode(&compute_data));

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": format!("{rollups_address}"), "data": compute_hex}, "latest"],
        "id": 99994
    });

    let rpc_resp: super::common::JsonRpcResponse = client
        .post(l1_rpc_url)
        .json(&req)
        .send()
        .await?
        .json()
        .await?;

    let result_val = rpc_resp
        .into_result()
        .map_err(|e| eyre::eyre!("computeCrossChainProxyAddress failed: {e}"))?;

    let result_hex = result_val
        .as_str()
        .ok_or_else(|| eyre::eyre!("no result from computeCrossChainProxyAddress"))?;

    let hex_clean = result_hex.strip_prefix("0x").unwrap_or(result_hex);
    if hex_clean.len() < 64 {
        return Err(eyre::eyre!("proxy address return too short"));
    }
    let addr_bytes = hex::decode(&hex_clean[..64])
        .map_err(|_| eyre::eyre!("invalid hex in proxy address return"))?;
    if addr_bytes.len() < 32 {
        return Err(eyre::eyre!("proxy address bytes too short"));
    }
    Ok(Address::from_slice(&addr_bytes[12..32]))
}

/// Walk the trigger trace to extract the delivery call's return data and success status.
///
/// The trigger trace call tree is:
/// ```text
/// proxy fallback → Rollups.executeCrossChainCall → _processCallAtScope → CALL(destination, data, value)
/// ```
/// We find the deepest CALL to `destination` and extract its output.
pub(crate) fn extract_delivery_output_from_trigger_trace(
    trigger_trace: &Value,
    destination: Address,
) -> (Vec<u8>, bool) {
    let top_level_reverted =
        trigger_trace.get("error").is_some() || trigger_trace.get("revertReason").is_some();

    if top_level_reverted {
        let error = trigger_trace
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        tracing::warn!(
            target: "based_rollup::proxy",
            error,
            "trigger tx reverted in L1 simulation"
        );
    }

    // Walk the trace tree looking for the delivery call to destination.
    // Even when the top-level trigger reverts (common for L2→L1 simulations
    // with placeholder entries/proofs), the INNER delivery call may have
    // succeeded and its output is visible in the callTracer subcalls.
    // The output is critical: it becomes the RESULT entry's data field,
    // and a mismatch (void vs actual return data) causes ExecutionNotFound.
    if let Some((output, failed)) = find_delivery_call(trigger_trace, destination) {
        if failed {
            // The delivery call itself reverted. This happens when the destination
            // function triggers further cross-chain calls (non-leaf delivery) and
            // the entries for those calls aren't loaded yet — the inner CCM
            // `executeCrossChainCall` reverts with ExecutionNotFound, propagating
            // up through the proxy to the delivery call.
            //
            // The `output` here is the error selector (e.g., 0xed6bc750 =
            // ExecutionNotFound), NOT the function's actual return data. Using it
            // as RESULT entry data produces a wrong hash, forcing 2 extra
            // convergence iterations (error→void→confirm).
            //
            // Return void instead: all non-leaf deliveries in practice return void
            // (incrementProxy, deepCall, callBoth, etc.). If a future contract has
            // a non-leaf delivery returning non-void, the RESULT hash mismatch
            // triggers ExecutionNotFound on L1 → rewind (existing safety net).
            //
            // Evidence: 16/16 non-leaf delivery successes across 14 E2E tests
            // return void (0 bytes). 0/16 return non-void.
            return (vec![], false);
        }
        return (output, failed);
    }

    // Only return revert if we truly couldn't find any delivery output.
    if top_level_reverted {
        return (vec![], true);
    }

    // If we couldn't find the delivery call in the trace, return empty.
    tracing::info!(
        target: "based_rollup::proxy",
        %destination,
        "could not find delivery call to destination in trigger trace — returning empty"
    );
    (vec![], false)
}

/// Recursively search the call trace for a CALL to the given destination address.
/// Returns the deepest match's (output_bytes, failed).
fn find_delivery_call(trace: &Value, destination: Address) -> Option<(Vec<u8>, bool)> {
    let dest_lower = format!("{destination}").to_lowercase();

    // Check subcalls first (depth-first) to find the deepest match.
    if let Some(calls) = trace.get("calls").and_then(|v| v.as_array()) {
        for subcall in calls {
            if let Some(result) = find_delivery_call(subcall, destination) {
                return Some(result);
            }
        }
    }

    // Check this node.
    let to = trace.get("to").and_then(|v| v.as_str()).unwrap_or("");
    if to.to_lowercase() == dest_lower {
        let failed = trace.get("error").is_some();
        let output_hex = trace.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
        let hex_clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
        let output_bytes = hex::decode(hex_clean).unwrap_or_default();
        return Some((output_bytes, failed));
    }

    None
}

/// Extract L1→L2 return calls from the trigger trace using the generic
/// `trace::walk_trace_tree` with ephemeral proxy support.
///
/// Walks the trigger trace depth-first via the protocol-level detection
/// (`executeCrossChainCall` child pattern on Rollups.sol). Filters results
/// to only include return calls targeting our rollup. Manager-originated
/// calls (forward delivery by Rollups) are automatically skipped by the
/// generic walker.
///
/// Uses `authorizedProxies` for persistent proxies AND scans the trace for
/// `createCrossChainProxy` to detect ephemeral proxies (created during the
/// trigger execution).
pub(crate) async fn extract_l1_to_l2_return_calls(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    trigger_trace: &Value,
    our_rollup_id: u64,
    parent_scope: &[U256],
) -> Vec<ReturnEdge> {
    let lookup = L1ProxyLookup {
        client,
        rpc_url: l1_rpc_url,
        rollups_address,
    };
    let mut proxy_cache: std::collections::HashMap<Address, Option<super::trace::ProxyInfo>> =
        std::collections::HashMap::new();
    let mut ephemeral_proxies = std::collections::HashMap::new();
    let mut detected_calls = Vec::new();

    // Rollups.sol is the manager contract on L1.
    super::trace::walk_trace_tree(
        trigger_trace,
        &[rollups_address],
        &lookup,
        &mut proxy_cache,
        &mut ephemeral_proxies,
        &mut detected_calls,
        &mut std::collections::HashSet::new(),
    )
    .await;

    // Convert trace::DetectedCall to ReturnEdge, filtering to only
    // include return calls targeting our rollup.
    //
    // walk_trace_tree returns ALL proxy calls (both the original L2→L1 trigger
    // and any L1→L2 return calls). We filter by rollup_id to keep only calls
    // that target our rollup (return calls) and exclude calls targeting L1 or
    // other rollups (forward calls).
    //
    // The walker already skips manager-originated calls (from=Rollups), so
    // forward delivery calls (Rollups calling proxy.executeOnBehalf) are not
    // in the results.
    detected_calls
        .into_iter()
        .filter_map(|c| {
            // Recover rollup_id from the proxy cache or ephemeral proxies.
            // The walker resolved proxy identity for detection — look it up
            // by matching original_address == c.destination.
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
                Some(info) if info.original_rollup_id == our_rollup_id => {
                    tracing::info!(
                        target: "based_rollup::proxy",
                        dest = %c.destination,
                        source = %c.source_address,
                        data_len = c.calldata.len(),
                        value = %c.value,
                        rollup_id = info.original_rollup_id,
                        "detected L1->L2 return call in delivery trace via walk_trace_tree"
                    );
                    // Accumulated scope: parent's scope ++ [0].
                    // Return calls are always first-children of their triggering scope
                    // (per reentrantCrossChainCalls E2E spec). The trace_depth from the
                    // L1 trigger trace is NOT used because it includes protocol-internal
                    // frames (Rollups.newScope, executeOnBehalf) that inflate the depth.
                    let mut accumulated = parent_scope.to_vec();
                    accumulated.push(U256::ZERO);
                    Some(ReturnEdge {
                        destination: c.destination,
                        data: c.calldata,
                        value: c.value,
                        source_address: c.source_address,
                        parent_call_index: crate::cross_chain::ParentLink::Root,
                        return_data: vec![],
                        delivery_failed: false,
                        scope: ScopePath::from_parts(accumulated),
                    })
                }
                _ => None, // Not targeting our rollup — skip (forward call or other rollup)
            }
        })
        .collect()
}

/// Build L1 execution entries from detected calls and run a `debug_traceCallMany`
/// bundle on L1: `[postBatch(entries), userTx]`. Returns the user tx trace (bundle[0][1])
/// and the full JSON-RPC response, or `None` on failure.
///
/// This encapsulates the entry-building, proof-signing, and traceCallMany execution
/// that is shared between the iterative discovery loop and the post-convergence
/// enrichment pass. Callers provide `label` for log attribution.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_and_run_l1_postbatch_trace(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    rollup_id: u64,
    builder_key: &alloy_signer_local::PrivateKeySigner,
    detected_calls: &[DiscoveredCall],
    user_from: &str,
    user_to: &str,
    user_data: &str,
    user_value: &str,
    label: &str,
) -> Option<(Value, Value)> {
    // Build L1DetectedCall entries from known calls
    let l1_detected: Vec<crate::table_builder::L1DetectedCall> = detected_calls
        .iter()
        .map(|c| crate::table_builder::L1DetectedCall {
            destination: c.destination,
            data: c.calldata.clone(),
            value: c.value,
            source_address: c.source_address,
            l2_return_data: c.delivery_return_data.clone(),
            call_success: !c.delivery_failed,
            parent_call_index: c.parent_call_index,
            target_rollup_id: if c.parent_call_index.is_child() && c.target_rollup_id == 0 {
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

    let analyzed = super::entry_builder::analyze_l1_to_l2_continuations(&l1_detected, rollup_id);

    // Log the call tree
    tracing::info!(
        target: "based_rollup::l1_proxy",
        "({label}) call tree: {} calls, {} analyzed", detected_calls.len(), analyzed.len()
    );
    for (i, c) in detected_calls.iter().enumerate() {
        let sel = if c.calldata.len() >= 4 {
            format!("0x{}", hex::encode(&c.calldata[..4]))
        } else {
            "0x".to_string()
        };
        tracing::info!(
            target: "based_rollup::l1_proxy",
            "  ({label}) CALL[{}]: dest={} src={} sel={} ret_len={} parent={:?}",
            i, c.destination, c.source_address, sel, c.delivery_return_data.len(), c.parent_call_index
        );
    }

    for (i, a) in analyzed.iter().enumerate() {
        tracing::info!(
            target: "based_rollup::l1_proxy",
            label, i,
            direction = ?a.direction,
            is_continuation = a.is_continuation,
            parent = ?a.parent_call_index,
            depth = a.depth,
            dest = %a.call_action.destination,
            value = %a.call_action.value,
            scope_len = a.scope.len(),
            delivery_data_len = a.delivery_return_data.len(),
            l2_return_data_len = a.l2_return_data.len(),
            "analyzed call"
        );
    }

    let entries = if analyzed.is_empty() {
        // Fallback: build simple CALL+RESULT pairs, then convert to L1 format.
        let l2_pairs: Vec<_> = l1_detected
            .iter()
            .flat_map(|c| {
                let (call_entry, result_entry) = super::entry_builder::build_simple_pair(
                    cross_chain::RollupId::new(alloy_primitives::U256::from(rollup_id)),
                    c.destination,
                    c.data.clone(),
                    c.value,
                    c.source_address,
                    cross_chain::RollupId::MAINNET,
                    c.call_success,
                    c.l2_return_data.clone(),
                );
                vec![call_entry, result_entry]
            })
            .collect();
        super::entry_builder::pairs_to_l1_format(&l2_pairs)
    } else {
        let cont = super::entry_builder::build_continuations(
            &analyzed,
            cross_chain::RollupId::new(alloy_primitives::U256::from(rollup_id)),
        );
        tracing::info!(
            target: "based_rollup::l1_proxy",
            l2_entries = cont.l2_entries.len(),
            l1_entries = cont.l1_entries.len(),
            "({label}) built continuation entries"
        );
        cont.l1_entries
    };

    // Log entries with state delta details
    for (idx, e) in entries.iter().enumerate() {
        let delta_info = if let Some(d) = e.state_deltas.first() {
            format!(
                "rollup={} current={} new={} ether_delta={}",
                d.rollup_id, d.current_state, d.new_state, d.ether_delta
            )
        } else {
            "no deltas".to_string()
        };
        tracing::info!(
            target: "based_rollup::l1_proxy",
            label, idx,
            action_hash = %e.action_hash,
            next_type = ?e.next_action.action_type,
            next_dest = %e.next_action.destination,
            next_scope_len = e.next_action.scope.len(),
            next_data_len = e.next_action.data.len(),
            next_value = %e.next_action.value,
            delta = %delta_info,
            "L1 entry for postBatch simulation"
        );
    }

    // Fix placeholder state deltas for simulation entries.
    // build_continuation_entries produces entries with placeholder
    // currentState=0x0 / newState=0x0. _findAndApplyExecution checks
    // rollups[rollupId].stateRoot == delta.currentState — placeholders
    // won't match. Query the real on-chain stateRoot and set identity
    // deltas (current=real, new=real) while preserving ether_delta.
    let mut entries = entries;
    let on_chain_state_root = get_rollup_state_root(client, l1_rpc_url, rollups_address, rollup_id)
        .await
        .unwrap_or(alloy_primitives::B256::ZERO);
    for e in &mut entries {
        for d in &mut e.state_deltas {
            d.current_state = on_chain_state_root;
            d.new_state = on_chain_state_root;
            // ether_delta is preserved from build_continuation_entries
        }
    }

    if entries.is_empty() {
        return None;
    }

    // Get L1 block context for proof signing.
    let block_ctx = get_l1_block_context(client, l1_rpc_url).await;
    let (block_number, block_hash, _parent_hash) = match block_ctx {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::warn!(target: "based_rollup::l1_proxy", %e, "({label}) failed to get L1 block context");
            return None;
        }
    };

    // Get verification key from Rollups contract
    let vk = match get_verification_key(client, l1_rpc_url, rollups_address, rollup_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "based_rollup::l1_proxy", %e, "({label}) failed to get verification key");
            return None;
        }
    };

    // Sign ECDSA proof for postBatch in traceCallMany context.
    let trace_parent_hash = block_hash;
    // `UNIX_EPOCH` is by definition <= now on any system with a sane clock; we
    // still handle the rare SystemTime-before-epoch case defensively so there
    // is no `.unwrap()` in production code.
    let trace_block_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let call_data_bytes = alloy_primitives::Bytes::new();
    let entry_hashes = cross_chain::compute_entry_hashes(&entries, vk);
    let public_inputs_hash = cross_chain::compute_public_inputs_hash(
        &entry_hashes,
        &call_data_bytes,
        trace_parent_hash,
        trace_block_timestamp,
    );

    use alloy_signer::SignerSync;
    let sig = match builder_key.sign_hash_sync(&public_inputs_hash) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "based_rollup::l1_proxy", %e, "({label}) failed to sign proof");
            return None;
        }
    };
    let sig_bytes = sig.as_bytes();
    let mut proof_bytes = sig_bytes.to_vec();
    if proof_bytes.len() == 65 && proof_bytes[64] < 27 {
        proof_bytes[64] += 27;
    }
    let proof = alloy_primitives::Bytes::from(proof_bytes);

    // Encode postBatch calldata
    let post_batch_calldata =
        cross_chain::encode_post_batch_calldata(&entries, call_data_bytes, proof);

    // Build traceCallMany request: [postBatch, userTx] in a single bundle
    let builder_addr = format!("{}", builder_key.address());
    let rollups_hex = format!("{rollups_address}");
    let post_batch_data = format!("0x{}", hex::encode(post_batch_calldata.as_ref()));

    let next_block = format!("{:#x}", block_number + 1);
    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCallMany",
        "params": [
            [
                {
                    "transactions": [
                        {
                            "from": builder_addr,
                            "to": rollups_hex,
                            "data": post_batch_data,
                            "gas": "0x1c9c380"
                        },
                        {
                            "from": user_from,
                            "to": user_to,
                            "data": user_data,
                            "value": user_value,
                            "gas": "0x2faf080"
                        }
                    ],
                    "blockOverride": {
                        "number": next_block,
                        "time": format!("{:#x}", trace_block_timestamp)
                    }
                }
            ],
            null,
            { "tracer": "callTracer" }
        ],
        "id": 3
    });

    let rpc_resp: super::common::JsonRpcResponse = match client
        .post(l1_rpc_url)
        .json(&trace_req)
        .send()
        .await
    {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "based_rollup::l1_proxy", %e, "({label}) traceCallMany response parse failed");
                return None;
            }
        },
        Err(e) => {
            tracing::warn!(target: "based_rollup::l1_proxy", %e, "({label}) traceCallMany request failed");
            return None;
        }
    };

    // Extract traces from result.
    let result_val = match rpc_resp.into_result() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                "({label}) traceCallMany returned error"
            );
            return None;
        }
    };
    let bundle_traces = match result_val.get(0).and_then(|b| b.as_array()) {
        Some(arr) if arr.len() >= 2 => arr,
        _ => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                "({label}) traceCallMany returned unexpected structure"
            );
            return None;
        }
    };

    // Check if postBatch succeeded
    let tx1_trace = &bundle_traces[0];
    if tx1_trace.get("error").is_some() || tx1_trace.get("revertReason").is_some() {
        let error_msg = tx1_trace
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let revert_reason = tx1_trace
            .get("revertReason")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        let output = tx1_trace
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        tracing::warn!(
            target: "based_rollup::l1_proxy",
            error = error_msg,
            revert_reason,
            output,
            "({label}) postBatch reverted in traceCallMany — entries may be invalid"
        );
        // Still return the user tx trace — caller may need partial results
    }

    let user_trace = bundle_traces[1].clone();
    Some((user_trace, result_val))
}

/// Simulate ALL L2→L1 calls in one `debug_traceCallMany` bundle so later calls can see
/// state effects from earlier ones (e.g., tokens minted by call_0 available to call_1).
///
/// This is the multi-call counterpart of `simulate_l1_delivery`. Instead of simulating
/// each call independently, it builds a combined bundle:
///   `[postBatch(combined_entries), createProxy(user0), trigger0, createProxy(user1), trigger1, ...]`
///
/// Uses the same iterative discovery loop: if trigger traces reveal new return calls,
/// entries are rebuilt with continuation structure and re-simulated until convergence
/// or `MAX_SIMULATION_ITERATIONS`.
///
/// Returns `None` if the simulation cannot be performed. Otherwise returns a vec of
/// per-call results: `(delivery_return_data, delivery_failed, detected_return_calls)`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn simulate_l1_combined_delivery(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<&str>,
    rollup_id: u64,
    calls: &[&DiscoveredCall],
    rlp_encoded_tx: &[u8],
) -> Option<Vec<(Vec<u8>, bool, Vec<ReturnEdge>)>> {
    if calls.is_empty() {
        return Some(vec![]);
    }

    // Check all destinations have code on L1.
    for call in calls {
        let code_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getCode",
            "params": [format!("{}", call.destination), "latest"],
            "id": 1
        });
        let code_resp = client.post(l1_rpc_url).json(&code_req).send().await.ok()?;
        let code_body: super::common::JsonRpcResponse = code_resp.json().await.ok()?;
        let code_hex = code_body.result_str()?;
        if code_hex == "0x" || code_hex == "0x0" {
            tracing::info!(
                target: "based_rollup::proxy",
                destination = %call.destination,
                "L1 target is EOA — skipping combined simulation"
            );
            return None;
        }
    }

    tracing::info!(
        target: "based_rollup::proxy",
        num_calls = calls.len(),
        "running combined L1 delivery simulation for multi-call L2→L1"
    );

    // Parse builder private key — required for signing postBatch proof.
    let key_hex = builder_private_key?;
    let key_clean = key_hex.strip_prefix("0x").unwrap_or(key_hex);
    let builder_key = match key_clean.parse::<alloy_signer_local::PrivateKeySigner>() {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::proxy",
                %e,
                "failed to parse builder private key for combined L1 simulation"
            );
            return None;
        }
    };

    // Iterative discovery loop.
    let mut all_return_calls: Vec<ReturnEdge> = Vec::new();
    // Per-call results: seed from known delivery data instead of empty placeholders.
    // The L2 iterative discovery already captured real delivery return data via its
    // own L1 simulation. Using it here gives correct RESULT hashes on iteration 1,
    // eliminating the placeholder→real→confirm convergence cycle for leaf deliveries.
    let mut per_call_return_data: Vec<Vec<u8>> = calls
        .iter()
        .map(|c| c.delivery_return_data.clone())
        .collect();
    let mut per_call_delivery_failed: Vec<bool> = calls.iter().map(|c| c.delivery_failed).collect();
    // Track previous return data for convergence (#254 item 7).
    let mut prev_per_call_return_data: Vec<Vec<u8>> = per_call_return_data.clone();

    for iteration in 1..=MAX_SIMULATION_ITERATIONS {
        tracing::info!(
            target: "based_rollup::proxy",
            iteration,
            known_return_calls = all_return_calls.len(),
            "combined L1 delivery simulation iteration"
        );

        // Build combined L1 deferred entries for all calls.
        // For multi-call patterns (siblings), each call needs scope=[..., sibling_index]
        // for executeL2TX to route between them via scope navigation.
        let is_multi_call = calls.len() > 1;
        let mut combined_entries: Vec<cross_chain::CrossChainExecutionEntry> = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            // Collect return calls belonging to this trigger call.
            let my_return_calls: Vec<&ReturnEdge> = all_return_calls
                .iter()
                .filter(|rc| {
                    rc.parent_call_index
                        == cross_chain::ParentLink::Child(cross_chain::AbsoluteCallIndex::new(i))
                })
                .collect();

            // Scope: for single calls, use trace_depth-1 (direct caller excluded).
            // For multi-call (siblings), append sibling_index for routing.
            let call_scope = if is_multi_call {
                let mut s = if call.trace_depth <= 1 {
                    vec![]
                } else {
                    vec![U256::ZERO; call.trace_depth]
                };
                s.push(U256::from(i));
                s
            } else if call.trace_depth <= 1 {
                vec![]
            } else {
                vec![U256::ZERO; call.trace_depth]
            };

            let entries = if my_return_calls.is_empty() {
                // Simple case: just this L2→L1 call.
                let call_entries = super::entry_builder::build_l2_to_l1_entries(
                    call.destination,
                    call.calldata.to_vec(),
                    call.value,
                    call.source_address,
                    rollup_id,
                    rlp_encoded_tx.to_vec(),
                    per_call_return_data[i].clone(),
                    per_call_delivery_failed[i],
                    call_scope.clone(),
                    cross_chain::TxOutcome::Success, // tx_reverts
                );
                call_entries.l1_deferred_entries
            } else {
                // Continuation case: use the SAME table builder functions as the
                // real batch to ensure identical entry ordering (same fix as
                // simulate_l1_delivery).
                let root_call = crate::table_builder::L2DetectedCall {
                    destination: call.destination,
                    data: call.calldata.to_vec(),
                    value: call.value,
                    source_address: call.source_address,
                    delivery_return_data: per_call_return_data[i].clone(),
                    delivery_failed: per_call_delivery_failed[i],
                    scope: ScopePath::from_parts(call_scope.clone()),
                    in_reverted_frame: false,
                };

                let return_calls_for_builder: Vec<crate::table_builder::L2ReturnCall> =
                    my_return_calls
                        .iter()
                        .map(|rc| crate::table_builder::L2ReturnCall {
                            destination: rc.destination,
                            data: rc.data.clone(),
                            value: rc.value,
                            source_address: rc.source_address,
                            // parent_call_index in ReturnEdge refers to the
                            // combined simulation's call index. For the table builder,
                            // we're building entries for a single root call, so set to
                            // None (defaults to last L2→L1 call, which is the only one).
                            parent_call_index: cross_chain::ParentLink::Root,
                            l2_return_data: rc.return_data.clone(),
                            l2_delivery_failed: rc.delivery_failed,
                            scope: rc.scope.clone(),
                        })
                        .collect();

                let analyzed = super::entry_builder::analyze_l2_to_l1_continuations(
                    &[root_call],
                    &return_calls_for_builder,
                    rollup_id,
                );
                let continuation = crate::table_builder::build_l2_to_l1_continuation_entries(
                    &analyzed,
                    cross_chain::RollupId::new(alloy_primitives::U256::from(rollup_id)),
                    rlp_encoded_tx,
                    cross_chain::TxOutcome::Success, // tx_reverts
                );

                tracing::info!(
                    target: "based_rollup::proxy",
                    call_idx = i,
                    l1_entry_count = continuation.l1_entries.len(),
                    return_call_count = my_return_calls.len(),
                    "built combined simulation entries via table builder"
                );

                continuation.l1_entries
            };

            combined_entries.extend(entries);
        }

        if combined_entries.is_empty() {
            tracing::warn!(
                target: "based_rollup::proxy",
                "combined entry building produced no L1 deferred entries"
            );
            break;
        }

        tracing::info!(
            target: "based_rollup::proxy",
            iteration,
            total_entries = combined_entries.len(),
            "built combined L1 deferred entries for all calls"
        );

        // Get L1 block context for proof signing.
        let (block_number, block_hash, _parent_hash) =
            match get_l1_block_context(client, l1_rpc_url).await {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        %e,
                        "failed to get L1 block context for combined simulation"
                    );
                    break;
                }
            };

        let trace_block_number = block_number + 1;
        let trace_parent_hash = block_hash;
        let trace_block_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Get verification key from Rollups contract.
        let vk = match get_verification_key(client, l1_rpc_url, rollups_address, rollup_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "failed to get verification key for combined simulation"
                );
                break;
            }
        };

        // Sign ECDSA proof for combined postBatch.
        let call_data_bytes = alloy_primitives::Bytes::new();
        let entry_hashes = cross_chain::compute_entry_hashes(&combined_entries, vk);
        let public_inputs_hash = cross_chain::compute_public_inputs_hash(
            &entry_hashes,
            &call_data_bytes,
            trace_parent_hash,
            trace_block_timestamp,
        );

        use alloy_signer::SignerSync;
        let sig = match builder_key.sign_hash_sync(&public_inputs_hash) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "failed to sign proof for combined L1 simulation"
                );
                break;
            }
        };
        let sig_bytes = sig.as_bytes();
        let mut proof_bytes = sig_bytes.to_vec();
        if proof_bytes.len() == 65 && proof_bytes[64] < 27 {
            proof_bytes[64] += 27;
        }
        let proof = alloy_primitives::Bytes::from(proof_bytes);

        // Encode postBatch calldata.
        let post_batch_calldata =
            cross_chain::encode_post_batch_calldata(&combined_entries, call_data_bytes, proof);

        // Build the traceCallMany bundle:
        //   tx0: postBatch(combined_entries)
        //   tx1: executeL2TX(rollupId, rlpTx)
        // One executeL2TX handles all entries via scope resolution.
        let builder_addr_hex = format!("{builder_address}");
        let rollups_hex = format!("{rollups_address}");
        let post_batch_data = format!("0x{}", hex::encode(post_batch_calldata.as_ref()));
        let next_block = format!("{:#x}", trace_block_number);

        // Encode executeL2TX calldata using typed ABI encoding (NEVER hardcode selectors).
        let execute_l2tx_calldata = cross_chain::IRollups::executeL2TXCall {
            rollupId: alloy_primitives::U256::from(rollup_id),
            rlpEncodedTx: rlp_encoded_tx.to_vec().into(),
        }
        .abi_encode();
        let execute_l2tx_data = format!("0x{}", hex::encode(&execute_l2tx_calldata));

        let transactions = vec![
            serde_json::json!({
                "from": builder_addr_hex,
                "to": rollups_hex,
                "data": post_batch_data,
                "gas": "0x1c9c380"
            }),
            serde_json::json!({
                "from": builder_addr_hex,
                "to": rollups_hex,
                "data": execute_l2tx_data,
                "gas": "0xc35000"
            }),
        ];

        // All calls share a single executeL2TX trigger at index 1.
        let call_trigger_tx_indices: Vec<usize> = (0..calls.len()).map(|_| 1).collect();

        let expected_trace_count = transactions.len();

        let trace_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCallMany",
            "params": [
                [
                    {
                        "transactions": transactions,
                        "blockOverride": {
                            "number": next_block,
                            "time": format!("{:#x}", trace_block_timestamp)
                        }
                    }
                ],
                null,
                { "tracer": "callTracer" }
            ],
            "id": 5
        });

        let rpc_resp: super::common::JsonRpcResponse =
            match client.post(l1_rpc_url).json(&trace_req).send().await {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            target: "based_rollup::proxy",
                            %e,
                            "combined traceCallMany response parse failed"
                        );
                        break;
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        %e,
                        "combined traceCallMany request failed"
                    );
                    break;
                }
            };

        // Extract traces from result.
        let result_val = match rpc_resp.into_result() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "combined traceCallMany returned error"
                );
                break;
            }
        };
        let bundle_traces = match result_val.get(0).and_then(|b| b.as_array()) {
            Some(arr) if arr.len() >= expected_trace_count => arr,
            _ => {
                let actual = result_val
                    .get(0)
                    .and_then(|b| b.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                tracing::warn!(
                    target: "based_rollup::proxy",
                    expected = expected_trace_count,
                    actual,
                    "combined traceCallMany returned unexpected trace count"
                );
                break;
            }
        };

        // Check postBatch result (tx0).
        let tx0_trace = &bundle_traces[0];
        if tx0_trace.get("error").is_some() || tx0_trace.get("revertReason").is_some() {
            let error_msg = tx0_trace
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            tracing::warn!(
                target: "based_rollup::proxy",
                error = error_msg,
                iteration,
                "postBatch reverted in combined L1 delivery simulation"
            );
            if iteration == 1 {
                return None;
            }
            break;
        }

        // Parse each trigger trace to extract delivery output and return calls.
        let mut new_return_calls_this_iteration: Vec<ReturnEdge> = Vec::new();

        for (call_idx, &trigger_tx_idx) in call_trigger_tx_indices.iter().enumerate() {
            let trigger_trace = &bundle_traces[trigger_tx_idx];

            let (return_data, _delivery_failed) = extract_delivery_output_from_trigger_trace(
                trigger_trace,
                calls[call_idx].destination,
            );

            per_call_return_data[call_idx] = return_data.clone();
            // Trigger simulation is unreliable for L2→L1 calls with placeholder
            // state deltas. Always assume delivery succeeds — §4f + rewind
            // handles real failures.
            per_call_delivery_failed[call_idx] = false;

            // Extract L1→L2 return calls from this trigger trace.
            // Parent scope = this call's scope from L2 trace depth.
            let call_scope: Vec<U256> = if calls.len() > 1 {
                let mut s = if calls[call_idx].trace_depth <= 1 {
                    vec![]
                } else {
                    vec![U256::ZERO; calls[call_idx].trace_depth]
                };
                s.push(U256::from(call_idx));
                s
            } else if calls[call_idx].trace_depth <= 1 {
                vec![]
            } else {
                vec![U256::ZERO; calls[call_idx].trace_depth]
            };
            let new_returns = extract_l1_to_l2_return_calls(
                client,
                l1_rpc_url,
                rollups_address,
                trigger_trace,
                rollup_id,
                &call_scope,
            )
            .await;

            // Tag return calls with parent_call_index so we know which trigger produced them.
            for mut rc in new_returns {
                rc.parent_call_index =
                    cross_chain::ParentLink::Child(cross_chain::AbsoluteCallIndex::new(call_idx));
                new_return_calls_this_iteration.push(rc);
            }
        }

        // Filter out already-known return calls using count-based comparison.
        // Supports legitimate duplicate return calls with identical
        // (destination, data, value, source_address, parent_call_index) tuples.
        // The CALL action hash includes value and sourceAddress, so two calls with
        // different ETH values are distinct even if destination and data match.
        let truly_new = filter_new_by_count(
            new_return_calls_this_iteration,
            &all_return_calls,
            |a, b| {
                a.destination == b.destination
                    && a.data == b.data
                    && a.value == b.value
                    && a.source_address == b.source_address
                    && a.parent_call_index == b.parent_call_index
            },
        );

        // Check convergence: no new return calls AND return data stabilized (#254 item 7).
        let return_data_changed = per_call_return_data != prev_per_call_return_data;
        if truly_new.is_empty() && !return_data_changed {
            tracing::info!(
                target: "based_rollup::proxy",
                iteration,
                total_return_calls = all_return_calls.len(),
                "combined L1 delivery simulation converged — no new return calls, data stable"
            );
            break;
        }
        if truly_new.is_empty() && return_data_changed {
            tracing::info!(
                target: "based_rollup::proxy",
                iteration,
                "combined L1 delivery: no new return calls but return data changed — re-iterating"
            );
            prev_per_call_return_data = per_call_return_data.clone();
            continue;
        }

        tracing::info!(
            target: "based_rollup::proxy",
            iteration,
            new_return_calls = truly_new.len(),
            "discovered new return calls in combined delivery trace — re-simulating"
        );

        all_return_calls.extend(truly_new);

        // Enrich return calls with L2 return data (#254 item 6).
        super::l2_to_l1::enrich_return_calls_via_l2_trace(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            &mut all_return_calls,
            rollup_id,
        )
        .await;

        prev_per_call_return_data = per_call_return_data.clone();
    }

    // Build per-call results.
    let mut results: Vec<(Vec<u8>, bool, Vec<ReturnEdge>)> = Vec::new();
    for (i, _call) in calls.iter().enumerate() {
        let my_return_calls: Vec<ReturnEdge> = all_return_calls
            .iter()
            .filter(|rc| {
                rc.parent_call_index
                    == cross_chain::ParentLink::Child(cross_chain::AbsoluteCallIndex::new(i))
            })
            .cloned()
            .collect();

        results.push((
            per_call_return_data[i].clone(),
            per_call_delivery_failed[i],
            my_return_calls,
        ));
    }

    if !all_return_calls.is_empty() {
        tracing::info!(
            target: "based_rollup::proxy",
            total_return_calls = all_return_calls.len(),
            num_calls = calls.len(),
            "combined L1 delivery simulation complete (return calls discovered)"
        );
    } else {
        tracing::info!(
            target: "based_rollup::proxy",
            num_calls = calls.len(),
            "combined L1 delivery simulation complete (no return calls)"
        );
    }

    Some(results)
}

/// Simulate L2→L1 calls chained in a single `debug_traceCallMany` bundle.
///
/// Unlike `simulate_l1_delivery` which builds full [postBatch, createProxy, trigger]
/// bundles, this function simulates DIRECT calls from the L1 proxy to the destination
/// — lighter weight but sufficient for capturing per-call return data with state
/// accumulation. Does NOT discover return calls (use `simulate_l1_combined_delivery`
/// for that).
///
/// Returns `Vec<(return_data, call_success)>` with one entry per call.
/// On any transport/parse failure, falls back to per-call independent simulation.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn simulate_chained_delivery_l2_to_l1(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    _l2_rpc_url: &str,
    rollups_address: Address,
    rollup_id: u64,
    _builder_address: Address,
    _builder_private_key: &str,
    calls: &[DiscoveredCall],
    _trace_block_number: u64,
    _trace_block_timestamp: u64,
) -> Vec<(Vec<u8>, bool)> {
    if calls.is_empty() {
        return vec![];
    }

    // Step 1: Compute the L1 proxy address for the source.
    // All identical calls share the same source_address, so compute once.
    let source_address = calls[0].source_address;
    let proxy_from = match compute_proxy_address_on_l1(
        client,
        l1_rpc_url,
        rollups_address,
        source_address,
        rollup_id,
    )
    .await
    {
        Ok(addr) => addr,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::proxy",
                %e,
                source = %source_address,
                "chained L1 proxy address lookup failed — falling back to per-call simulation"
            );
            return fallback_per_call_l2_to_l1_simulation(client, l1_rpc_url, calls).await;
        }
    };

    // Step 2: Check that all destinations have code on L1 (not EOAs).
    for call in calls {
        let code_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getCode",
            "params": [format!("{}", call.destination), "latest"],
            "id": 1
        });
        let code_resp = match client.post(l1_rpc_url).json(&code_req).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "chained L2→L1: eth_getCode failed — falling back"
                );
                return fallback_per_call_l2_to_l1_simulation(client, l1_rpc_url, calls).await;
            }
        };
        let code_body: super::common::JsonRpcResponse = match code_resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "chained L2→L1: eth_getCode parse failed — falling back"
                );
                return fallback_per_call_l2_to_l1_simulation(client, l1_rpc_url, calls).await;
            }
        };
        let code_hex = code_body.result_str().unwrap_or("0x");
        if code_hex == "0x" || code_hex == "0x0" {
            tracing::info!(
                target: "based_rollup::proxy",
                destination = %call.destination,
                "chained L2→L1: destination is EOA — returning empty for all calls"
            );
            // If any destination is an EOA, return empty for ALL calls
            // (all identical calls share the same destination).
            return calls.iter().map(|_| (vec![], true)).collect();
        }
    }

    // Step 3: Get L1 block context for blockOverride.
    let (block_number, _block_hash, _parent_hash) =
        match get_l1_block_context(client, l1_rpc_url).await {
            Ok(ctx) => ctx,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "chained L2→L1: failed to get L1 block context — falling back"
                );
                return fallback_per_call_l2_to_l1_simulation(client, l1_rpc_url, calls).await;
            }
        };

    let trace_block_number_val = block_number + 1;
    let trace_block_timestamp_val = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Step 4: Build debug_traceCallMany request with ONE bundle containing N delivery
    // calls. Each tx executes from the L1 proxy to the destination, seeing state
    // effects from previous calls in the bundle.
    let proxy_from_hex = format!("{proxy_from}");
    let next_block = format!("{:#x}", trace_block_number_val);

    let transactions: Vec<Value> = calls
        .iter()
        .map(|call| {
            serde_json::json!({
                "from": &proxy_from_hex,
                "to": format!("{}", call.destination),
                "data": format!("0x{}", hex::encode(&call.calldata)),
                "value": format!("0x{:x}", call.value),
                "gas": "0x2faf080"
            })
        })
        .collect();

    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCallMany",
        "params": [
            [
                {
                    "transactions": transactions,
                    "blockOverride": {
                        "number": next_block,
                        "time": format!("{:#x}", trace_block_timestamp_val)
                    }
                }
            ],
            null,
            { "tracer": "callTracer" }
        ],
        "id": 99940
    });

    tracing::info!(
        target: "based_rollup::proxy",
        num_calls = calls.len(),
        proxy = %proxy_from,
        "chained L2→L1 delivery simulation: debug_traceCallMany with {} txs in one bundle",
        calls.len()
    );

    // Step 5: Execute the simulation.
    let resp = match client.post(l1_rpc_url).json(&trace_req).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::proxy",
                %e,
                "chained L2→L1 simulation (debug_traceCallMany) request failed — falling back"
            );
            return fallback_per_call_l2_to_l1_simulation(client, l1_rpc_url, calls).await;
        }
    };
    let rpc_body: super::common::JsonRpcResponse = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::proxy",
                %e,
                "chained L2→L1 simulation response parse failed — falling back"
            );
            return fallback_per_call_l2_to_l1_simulation(client, l1_rpc_url, calls).await;
        }
    };

    // Step 6: Parse the response.
    // Structure: result[0] is an array of N trace objects (one per tx in the bundle).
    let result_val = match rpc_body.into_result() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::proxy",
                %e,
                "chained L2→L1 simulation returned RPC error — falling back"
            );
            return fallback_per_call_l2_to_l1_simulation(client, l1_rpc_url, calls).await;
        }
    };
    let traces = match result_val.get(0).and_then(|b| b.as_array()) {
        Some(arr) if arr.len() == calls.len() => arr,
        _ => {
            let actual_len = result_val
                .get(0)
                .and_then(|b| b.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            tracing::warn!(
                target: "based_rollup::proxy",
                expected = calls.len(),
                actual = actual_len,
                "chained L2→L1 simulation returned unexpected trace count — falling back"
            );
            return fallback_per_call_l2_to_l1_simulation(client, l1_rpc_url, calls).await;
        }
    };

    // Step 7: Extract per-call results from each trace.
    let mut results = Vec::with_capacity(calls.len());
    for (i, trace) in traces.iter().enumerate() {
        let has_error = trace.get("error").is_some() || trace.get("revertReason").is_some();
        let output_hex = trace.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
        let hex_clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
        let output_bytes = hex::decode(hex_clean).unwrap_or_default();

        let success = !has_error;

        tracing::info!(
            target: "based_rollup::proxy",
            idx = i,
            success,
            return_data_len = output_bytes.len(),
            "chained L2→L1 simulation: call {} result",
            i
        );

        results.push((output_bytes, success));
    }

    results
}

/// Fallback: simulate each L2->L1 call independently via direct eth_call.
///
/// Used when the chained simulation fails. Runs each call as an independent
/// `debug_traceCallMany` with a single tx — no state accumulation between calls.
pub(crate) async fn fallback_per_call_l2_to_l1_simulation(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    calls: &[DiscoveredCall],
) -> Vec<(Vec<u8>, bool)> {
    tracing::info!(
        target: "based_rollup::proxy",
        num_calls = calls.len(),
        "falling back to per-call L2→L1 simulation (no state accumulation)"
    );
    let mut results = Vec::with_capacity(calls.len());
    for call in calls {
        // Simple standalone trace for each call — from zero address to destination.
        // This won't accumulate state but gives a safe fallback.
        let trace_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCallMany",
            "params": [[{
                "transactions": [{
                    "from": format!("{}", Address::ZERO),
                    "to": format!("{}", call.destination),
                    "data": format!("0x{}", hex::encode(&call.calldata)),
                    "value": format!("0x{:x}", call.value),
                    "gas": "0x2faf080"
                }]
            }], null, { "tracer": "callTracer" }],
            "id": 99941
        });

        let rpc_resp: super::common::JsonRpcResponse =
            match client.post(l1_rpc_url).json(&trace_req).send().await {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(_) => {
                        results.push((vec![], true));
                        continue;
                    }
                },
                Err(_) => {
                    results.push((vec![], true));
                    continue;
                }
            };

        let result_val = match rpc_resp.into_result() {
            Ok(v) => v,
            Err(_) => {
                results.push((vec![], true));
                continue;
            }
        };
        let trace = result_val
            .get(0)
            .and_then(|b| b.as_array())
            .and_then(|arr| arr.first());

        match trace {
            Some(t) => {
                let has_error = t.get("error").is_some() || t.get("revertReason").is_some();
                let output_hex = t.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                let hex_clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
                let output_bytes = hex::decode(hex_clean).unwrap_or_default();
                results.push((output_bytes, !has_error));
            }
            None => {
                results.push((vec![], true));
            }
        }
    }
    results
}
