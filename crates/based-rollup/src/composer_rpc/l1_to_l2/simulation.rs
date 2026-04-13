//! L2 simulation helpers for L1→L2 cross-chain call detection.
//!
//! Contains the simulation functions that run L1→L2 calls on L2 via
//! `debug_traceCallMany` bundles to capture return data and detect
//! child L2→L1 proxy calls.

use alloy_primitives::{Address, U256};
use crate::cross_chain::{RollupId, ScopePath};
use serde_json::Value;
use std::collections::HashMap;

use super::process::{
    extract_inner_destination_return_data, extract_return_data_from_trace,
    destination_call_succeeded_in_trace, walk_l2_simulation_trace,
};

/// Execute a `debug_traceCallMany` bundle on L2:
///   [0] `loadExecutionTable(entries)` — from SYSTEM_ADDRESS to CCM
///   [1] `executeIncomingCrossChainCall(...)` — from SYSTEM_ADDRESS to CCM
///
/// Returns `Some((exec_trace, success))` where `exec_trace` is the callTracer
/// output for tx[1] and `success` indicates whether the call reverted.
/// Returns `None` on RPC or parse failure.
pub(super) async fn run_l2_sim_bundle(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    sys_addr: &str,
    ccm_hex: &str,
    load_entries: &[crate::cross_chain::CrossChainExecutionEntry],
    exec_calldata: &[u8],
    value: U256,
) -> Option<(Value, bool)> {
    let load_calldata = crate::composer_rpc::entry_builder::encode_load_table(load_entries);
    let load_data = format!("0x{}", super::hex::encode(load_calldata.as_ref()));
    let exec_data = format!("0x{}", super::hex::encode(exec_calldata));
    let value_hex = format!("0x{:x}", value);

    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCallMany",
        "params": [[{
            "transactions": [
                {
                    "from": sys_addr,
                    "to": ccm_hex,
                    "data": load_data,
                    "gas": "0x1c9c380"
                },
                {
                    "from": sys_addr,
                    "to": ccm_hex,
                    "data": exec_data,
                    "value": value_hex,
                    "gas": "0x2faf080"
                }
            ]
        }], null, { "tracer": "callTracer" }],
        "id": 99961
    });

    let resp = match client.post(l2_rpc_url).json(&trace_req).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                "run_l2_sim_bundle: debug_traceCallMany request failed"
            );
            return None;
        }
    };
    let rpc_body: super::super::common::JsonRpcResponse = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                "run_l2_sim_bundle: debug_traceCallMany response parse failed"
            );
            return None;
        }
    };

    // Extract the exec trace (tx[1]).
    // result[0] = bundle traces array, result[0][1] = exec tx trace.
    // Fall back to result[0][0] if only 1 trace returned.
    let result_val = rpc_body.into_result().ok()?;
    let traces = result_val
        .get(0)
        .and_then(|b| b.as_array())?;

    let exec_trace = if traces.len() >= 2 {
        &traces[1]
    } else if !traces.is_empty() {
        tracing::warn!(
            target: "based_rollup::l1_proxy",
            trace_count = traces.len(),
            "run_l2_sim_bundle: expected 2 traces, falling back to trace[0]"
        );
        &traces[0]
    } else {
        tracing::warn!(
            target: "based_rollup::l1_proxy",
            "run_l2_sim_bundle: no traces returned"
        );
        return None;
    };

    let success = exec_trace.get("error").is_none() && exec_trace.get("revertReason").is_none();

    Some((exec_trace.clone(), success))
}

/// Simulate an L1->L2 call on L2 to capture the actual return data.
///
/// Uses a single simulation path: `SYSTEM_ADDRESS -> CCM.executeIncomingCrossChainCall(...)`.
/// This mirrors the real L2 execution path where the CCM has pre-minted ETH balance,
/// so ETH deposits work correctly (unlike the old proxy-based simulation that failed
/// because the proxy had no balance).
///
/// The simulation runs as a `debug_traceCallMany` bundle:
///   [0] `loadExecutionTable(entries)` — empty on initial sim, populated on retry
///   [1] `executeIncomingCrossChainCall(dest, value, data, source, sourceRollup=0, scope=[])`
///
/// Returns `(return_data, call_success, child_l2_to_l1_calls)`. On simulation
/// failure, returns `(vec![], false, vec![])` as a safe fallback.
///
/// The third element contains child L2→L1 proxy calls discovered in the L2
/// simulation trace. These are L2→L1 calls made by the destination contract
/// during L2 execution (the nested L1→L2→L1 pattern). Callers must propagate
/// these children to build proper continuation entries with scope navigation.
#[allow(clippy::too_many_arguments)]
pub(super) async fn simulate_l1_to_l2_call_on_l2(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    destination: Address,
    data: &[u8],
    value: U256,
    source_address: Address,
    rollup_id: u64,
    l2_scope: &[U256],
) -> (Vec<u8>, bool, Vec<super::super::common::DiscoveredProxyCall>) {
    // Step 1: Query SYSTEM_ADDRESS from the CCM.
    // Uses typed ABI encoding via sol! macro — NEVER hardcode selectors.
    let sys_calldata = super::super::common::encode_system_address_calldata();
    let sys_result = super::super::common::eth_call_view(
        client,
        l2_rpc_url,
        cross_chain_manager_address,
        &sys_calldata,
    )
    .await;

    let sys_addr = match sys_result.and_then(|s| super::super::common::parse_address_from_abi_return(&s)) {
        Some(addr) => addr,
        None => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                dest = %destination,
                "SYSTEM_ADDRESS query failed on L2 CCM — cannot simulate"
            );
            return (vec![], false, vec![]);
        }
    };

    let sys_addr_hex = format!("{sys_addr}");
    let ccm_hex = format!("{cross_chain_manager_address}");

    tracing::info!(
        target: "based_rollup::l1_proxy",
        dest = %destination,
        source = %source_address,
        %sys_addr,
        "simulating L1→L2 call via executeIncomingCrossChainCall"
    );

    // Step 2: Build executeIncomingCrossChainCall calldata.
    // Scope reflects the nesting depth on L1 (symmetric with L2→L1 rule).
    let sim_action = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Call,
        rollup_id: RollupId::new(U256::from(rollup_id)),
        destination,
        value,
        data: data.to_vec(),
        failed: false,
        source_address,
        source_rollup: RollupId::MAINNET, // L1 = rollup 0
        scope: ScopePath::from_parts(l2_scope.to_vec()),
    };
    let exec_calldata = crate::cross_chain::encode_execute_incoming_call_calldata(&sim_action);

    // Step 3: Initial simulation with empty entries.
    let (trace, success) = match run_l2_sim_bundle(
        client,
        l2_rpc_url,
        &sys_addr_hex,
        &ccm_hex,
        &[], // empty entries for initial sim
        exec_calldata.as_ref(),
        value,
    )
    .await
    {
        Some(result) => result,
        None => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                dest = %destination,
                "L2 call simulation (initial) returned no trace"
            );
            return (vec![], false, vec![]);
        }
    };

    // Step 4: Walk trace for child L2→L1 proxy calls.
    let children = if !cross_chain_manager_address.is_zero() {
        let (calls, _unresolved) = walk_l2_simulation_trace(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            &trace,
            rollup_id,
            None, // no prior bundle traces for independent simulation
        )
        .await;
        calls
    } else {
        Vec::new()
    };

    // Step 5: Extract return data.
    let return_data = extract_return_data_from_trace(&trace);

    tracing::debug!(
        target: "based_rollup::l1_proxy",
        dest = %destination,
        source = %source_address,
        return_data_len = return_data.len(),
        call_success = success,
        child_count = children.len(),
        "initial L2 simulation result"
    );

    // Step 6: If children found, retry with placeholder entries so the
    // L2 target contract can complete its full execution path.
    if !children.is_empty() {
        tracing::info!(
            target: "based_rollup::l1_proxy",
            dest = %destination,
            source = %source_address,
            child_count = children.len(),
            "L2 simulation found {} child L2→L1 call(s) — retrying with loadExecutionTable",
            children.len()
        );

        let mut placeholders = Vec::new();
        for child in &children {
            let placeholder = crate::composer_rpc::entry_builder::build_l2_to_l1_entries(
                child.original_address,
                child.data.clone(),
                child.value,
                child.source_address,
                rollup_id,
                vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
                vec![],     // delivery_return_data placeholder
                false,      // delivery_failed placeholder
                vec![],     // l1_delivery_scope placeholder
                crate::cross_chain::TxOutcome::Success,      // tx_reverts
            );
            placeholders.extend(placeholder.l2_table_entries);
        }

        if let Some((retry_trace, retry_success)) = run_l2_sim_bundle(
            client,
            l2_rpc_url,
            &sys_addr_hex,
            &ccm_hex,
            &placeholders,
            exec_calldata.as_ref(),
            value,
        )
        .await
        {
            let (retry_children, _unresolved) = walk_l2_simulation_trace(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                &retry_trace,
                rollup_id,
                None, // no prior bundle traces for retry
            )
            .await;
            let retry_data = extract_return_data_from_trace(&retry_trace);

            if retry_success {
                tracing::info!(
                    target: "based_rollup::l1_proxy",
                    dest = %destination,
                    source = %source_address,
                    return_data_len = retry_data.len(),
                    child_calls = retry_children.len(),
                    "non-leaf L2 call succeeded after loadExecutionTable retry"
                );
                // Use inner destination return data (raw), not top-level ABI-wrapped output
                let retry_inner = extract_inner_destination_return_data(&retry_trace, destination)
                    .unwrap_or(retry_data);
                return (retry_inner, true, retry_children);
            }

            tracing::info!(
                target: "based_rollup::l1_proxy",
                dest = %destination,
                child_count = children.len(),
                "L2 call still reverts after loadExecutionTable — \
                 marking as failed but propagating {} child L2→L1 calls",
                children.len()
            );
        }

        // Retry failed or was not attempted — propagate children from initial trace.
        return (return_data, false, children);
    }

    // The initial simulation reverts because _consumeExecution(RESULT hash) fails
    // (no entry loaded). BUT the destination call DID execute inside the trace:
    //   executeIncomingCrossChainCall → _processCallAtScope → proxy.executeOnBehalf(dest, data)
    //   → destination.call(data) → SUCCEEDS with returnData
    //   → _consumeExecution(hash(RESULT{data=returnData})) → REVERTS (no entry)
    //
    // Extract the REAL return data from the inner destination call in the trace,
    // then do a second simulation with the correct RESULT entry loaded.
    let (inner_return_data, inner_success) = if !success {
        // The outer simulation reverted (expected: no RESULT entry in Run 1).
        // Extract the REAL return data from the inner destination call.
        // Also check the trace node for an "error" field to determine success.
        let extracted = extract_inner_destination_return_data(&trace, destination);
        let inner_data = extracted.unwrap_or_default();
        // Check if the destination call itself succeeded (no "error" in its trace node).
        // A void function returns empty data but succeeds — the old heuristic
        // `!inner_data.is_empty()` misclassified void functions as failed.
        let inner_ok = destination_call_succeeded_in_trace(&trace, destination);
        (inner_data, inner_ok)
    } else {
        (return_data.clone(), success)
    };

    tracing::info!(
        target: "based_rollup::l1_proxy",
        dest = %destination,
        source = %source_address,
        sim_reverted = !success,
        inner_return_data_len = inner_return_data.len(),
        inner_success,
        "extracted return data from inner destination call"
    );

    // Build RESULT entry with the real return data from Run 1
    let result_action = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Result,
        rollup_id: RollupId::new(U256::from(rollup_id)),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: inner_return_data.clone(),
        failed: !inner_success,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
        scope: ScopePath::root(),
    };
    let result_hash = crate::table_builder::compute_action_hash(&result_action);
    let result_entry = crate::cross_chain::CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: result_hash,
        next_action: result_action.clone(),
    };

    // Run 2: retry with the RESULT entry loaded — should NOT revert
    if let Some((retry_trace, retry_success)) = run_l2_sim_bundle(
        client,
        l2_rpc_url,
        &sys_addr_hex,
        &ccm_hex,
        &[result_entry],
        exec_calldata.as_ref(),
        value,
    )
    .await
    {
        let retry_data = extract_return_data_from_trace(&retry_trace);
        let (retry_children, _unresolved) = walk_l2_simulation_trace(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            &retry_trace,
            rollup_id,
            None, // no prior bundle traces for retry
        )
        .await;

        tracing::info!(
            target: "based_rollup::l1_proxy",
            dest = %destination,
            retry_success,
            return_data_len = retry_data.len(),
            child_count = retry_children.len(),
            "L2 simulation Run 2 (with RESULT entry) complete"
        );

        if retry_success {
            // Use the INNER destination return data from the retry trace,
            // not the top-level output (which is ABI-wrapped by executeIncomingCrossChainCall).
            // The RESULT entry hash uses the raw destination return data.
            let retry_inner = extract_inner_destination_return_data(&retry_trace, destination)
                .unwrap_or(retry_data);
            return (retry_inner, true, retry_children);
        } else {
            // Run 2 also failed — extract revert data from the retry trace.
            // When the destination contract always reverts (e.g., RevertCounter),
            // Run 2 (with RESULT entry loaded) actually reaches the destination,
            // so the retry trace contains the real revert data. Run 1 may not
            // reach the destination (ExecutionNotFound reverts first).
            tracing::info!(
                target: "based_rollup::l1_proxy",
                dest = %destination,
                retry_trace_has_calls = retry_trace.get("calls").and_then(|v| v.as_array()).map_or(0, |a| a.len()),
                retry_trace_error = retry_trace.get("error").and_then(|v| v.as_str()).unwrap_or("none"),
                "Run 2 failed — attempting to extract revert data from retry trace"
            );
            let retry_inner = extract_inner_destination_return_data(&retry_trace, destination);
            if let Some(ref data) = retry_inner {
                if !data.is_empty() {
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        dest = %destination,
                        retry_data_len = data.len(),
                        "Run 2 failed but captured revert data from trace"
                    );
                    return (data.clone(), false, retry_children);
                }
            }
            // Trace extraction didn't find the destination call in the CCM trace
            // (nested self-calls may not expose children). Use debug_traceCallMany
            // with a direct call to the destination to capture revert data.
            let direct_req = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "debug_traceCallMany",
                "params": [
                    [{ "transactions": [{
                        "from": format!("{source_address}"),
                        "to": format!("{destination}"),
                        "data": format!("0x{}", super::hex::encode(data)),
                        "gas": "0x2faf080"
                    }] }],
                    null,
                    { "tracer": "callTracer" }
                ],
                "id": 99959
            });
            if let Ok(resp) = client.post(l2_rpc_url).json(&direct_req).send().await {
                if let Ok(body) = resp.json::<super::super::common::JsonRpcResponse>().await {
                    if let Some(trace) = body.result
                        .as_ref()
                        .and_then(|r| r.get(0))
                        .and_then(|b| b.as_array())
                        .and_then(|a| a.first())
                    {
                        let has_error = trace.get("error").is_some();
                        let output = trace.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                        let clean = output.strip_prefix("0x").unwrap_or(output);
                        if let Ok(revert_data) = super::hex::decode(clean) {
                            if has_error && !revert_data.is_empty() {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    dest = %destination,
                                    data_len = revert_data.len(),
                                    data_hex = %format!("0x{}", super::hex::encode(&revert_data[..revert_data.len().min(20)])),
                                    "captured revert data from direct debug_traceCallMany"
                                );
                                return (revert_data, false, retry_children);
                            }
                        }
                    }
                }
            }
        }
    }

    // Both runs failed without capturing destination data
    (inner_return_data, inner_success, children)
}

/// Chained L2 simulation: simulate an L1→L2 call on L2 where prior calls have
/// already executed, so this call sees their accumulated state effects.
///
/// Builds a single `debug_traceCallMany` bundle:
///   tx[0]: `loadExecutionTable(prior_result_entries ++ void_result_for_this_call)`
///   tx[1..N]: `executeIncomingCrossChainCall(prior_call_0)` ... for each prior call
///   tx[N+1]: `executeIncomingCrossChainCall(this_call)`
///
/// Prior calls run with their correct RESULT entries loaded, so they succeed and
/// mutate L2 state. This call may revert (its RESULT entry is a void placeholder),
/// but the inner destination call still executes — we extract its return data from
/// the trace.
///
/// Used for multi-call patterns like CallTwice where identical calls need to see
/// each other's state effects (e.g., Counter.increment() twice → returns 1, then 2).
#[allow(clippy::too_many_arguments)]
pub(super) async fn simulate_l1_to_l2_call_chained_on_l2(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    destination: Address,
    data: &[u8],
    value: U256,
    source_address: Address,
    rollup_id: u64,
    prior_result_entries: &[crate::cross_chain::CrossChainExecutionEntry],
    prior_exec_calldatas: &[(Vec<u8>, U256)],
    sys_addr: Option<Address>,
    l2_scope: &[U256],
) -> (Vec<u8>, bool, Vec<super::super::common::DiscoveredProxyCall>) {
    let sys_addr = match sys_addr {
        Some(a) => a,
        None => {
            // Fall back to independent simulation if SYSTEM_ADDRESS unavailable.
            return simulate_l1_to_l2_call_on_l2(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                destination,
                data,
                value,
                source_address,
                rollup_id,
                l2_scope, // l2_scope from L1 trace_depth
            )
            .await;
        }
    };

    let sys_addr_hex = format!("{sys_addr}");
    let ccm_hex = format!("{cross_chain_manager_address}");

    // Build the current call's exec calldata.
    let sim_action = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Call,
        rollup_id: RollupId::new(U256::from(rollup_id)),
        destination,
        value,
        data: data.to_vec(),
        failed: false,
        source_address,
        source_rollup: RollupId::MAINNET,
        scope: ScopePath::from_parts(l2_scope.to_vec()),
    };
    let exec_calldata = crate::cross_chain::encode_execute_incoming_call_calldata(&sim_action);

    // Build a void RESULT entry for this call (placeholder — will cause _consumeExecution
    // to fail, but the inner destination call still executes).
    let void_result = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Result,
        rollup_id: RollupId::new(U256::from(rollup_id)),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
        scope: ScopePath::root(),
    };
    let void_hash = crate::table_builder::compute_action_hash(&void_result);
    let void_entry = crate::cross_chain::CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: void_hash,
        next_action: void_result,
    };

    // Combine all entries: prior RESULT entries + void placeholder for current call.
    let mut all_entries: Vec<crate::cross_chain::CrossChainExecutionEntry> =
        prior_result_entries.to_vec();
    all_entries.push(void_entry);

    // Build the loadExecutionTable calldata.
    let load_calldata = crate::composer_rpc::entry_builder::encode_load_table(&all_entries);
    let load_data = format!("0x{}", super::hex::encode(load_calldata.as_ref()));

    // Build the transaction array: loadTable + prior execs + current exec.
    let mut transactions = Vec::new();

    // tx[0]: loadExecutionTable
    transactions.push(serde_json::json!({
        "from": sys_addr_hex,
        "to": ccm_hex,
        "data": load_data,
        "gas": "0x1c9c380"
    }));

    // tx[1..N]: prior executeIncomingCrossChainCall calls
    for (cd, val) in prior_exec_calldatas {
        transactions.push(serde_json::json!({
            "from": sys_addr_hex,
            "to": ccm_hex,
            "data": format!("0x{}", super::hex::encode(cd)),
            "value": format!("0x{:x}", val),
            "gas": "0x2faf080"
        }));
    }

    // tx[N+1]: current executeIncomingCrossChainCall
    transactions.push(serde_json::json!({
        "from": sys_addr_hex,
        "to": ccm_hex,
        "data": format!("0x{}", super::hex::encode(exec_calldata.as_ref())),
        "value": format!("0x{:x}", value),
        "gas": "0x2faf080"
    }));

    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCallMany",
        "params": [[{
            "transactions": transactions
        }], null, { "tracer": "callTracer" }],
        "id": 99962
    });

    let resp = match client.post(l2_rpc_url).json(&trace_req).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                "chained L2 simulation: transport error"
            );
            // Fall back to independent simulation.
            return simulate_l1_to_l2_call_on_l2(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                destination,
                data,
                value,
                source_address,
                rollup_id,
                l2_scope, // l2_scope from L1 trace_depth
            )
            .await;
        }
    };

    let rpc_body: super::super::common::JsonRpcResponse = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                "chained L2 simulation: response parse error"
            );
            return simulate_l1_to_l2_call_on_l2(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                destination,
                data,
                value,
                source_address,
                rollup_id,
                l2_scope, // l2_scope from L1 trace_depth
            )
            .await;
        }
    };

    // Extract the last trace (current call's trace).
    // result[0] = bundle traces array, result[0][last] = current call trace.
    let result_val = match rpc_body.into_result() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                "chained L2 simulation: RPC error"
            );
            return simulate_l1_to_l2_call_on_l2(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                destination,
                data,
                value,
                source_address,
                rollup_id,
                l2_scope,
            )
            .await;
        }
    };
    let traces = match result_val
        .get(0)
        .and_then(|b| b.as_array())
    {
        Some(arr) => {
            // Log each tx's success/failure for generic debugging of any chained simulation.
            for (ti, t) in arr.iter().enumerate() {
                let err = t.get("error").and_then(|v| v.as_str()).unwrap_or("none");
                tracing::debug!(
                    target: "based_rollup::l1_proxy",
                    ti,
                    total = arr.len(),
                    error = err,
                    "chained L2 sim: bundle tx result"
                );
            }
            arr
        }
        None => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                "chained L2 simulation: no traces in response"
            );
            return simulate_l1_to_l2_call_on_l2(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                destination,
                data,
                value,
                source_address,
                rollup_id,
                l2_scope, // l2_scope from L1 trace_depth
            )
            .await;
        }
    };

    // The last trace is the current call.
    let expected_count = 1 + prior_exec_calldatas.len() + 1; // loadTable + priors + current
    if traces.len() < expected_count {
        tracing::warn!(
            target: "based_rollup::l1_proxy",
            expected = expected_count,
            got = traces.len(),
            "chained L2 simulation: unexpected trace count"
        );
        return simulate_l1_to_l2_call_on_l2(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            destination,
            data,
            value,
            source_address,
            rollup_id,
            l2_scope, // l2_scope from L1 trace_depth
        )
        .await;
    }

    let current_trace = &traces[traces.len() - 1];

    // Scan prior traces for external createCrossChainProxy calls
    // (e.g., if user code explicitly creates proxies during delivery).
    let mut bundle_ephemeral_proxies: HashMap<Address, super::super::trace::ProxyInfo> = HashMap::new();
    for prior_trace in &traces[..traces.len() - 1] {
        super::super::trace::extract_ephemeral_proxies_from_trace(
            prior_trace,
            &[cross_chain_manager_address],
            &mut bundle_ephemeral_proxies,
        );
    }

    // Walk trace for child L2→L1 proxy calls, passing ephemeral proxies
    // from prior bundle traces for cross-bundle visibility.
    //
    // Two-pass proxy resolution:
    // Pass 1: Walk the trace. Proxies created by CCM's internal
    //   _createCrossChainProxyInternal during executeIncomingCrossChainCall are
    //   NOT visible via callTracer (internal calls). The ProxyLookup queries
    //   authorizedProxies at "latest" (real L2 state), which doesn't see
    //   simulation-only state. Unresolved proxies are collected.
    // Pass 2: If unresolved proxies exist, re-run the SAME bundle via
    //   debug_traceCallMany with extra authorizedProxies(addr) query txs
    //   appended. These queries execute within the simulation state (after
    //   loadTable + prior execs + current exec), so they see proxies created
    //   during delivery. Parse the query results and re-walk the trace with
    //   the resolved identities pre-populated.

    let children = if !cross_chain_manager_address.is_zero() {
        let pre = if bundle_ephemeral_proxies.is_empty() {
            None
        } else {
            Some(&bundle_ephemeral_proxies)
        };

        // Pass 1: initial walk.
        let (pass1_children, unresolved) = walk_l2_simulation_trace(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            current_trace,
            rollup_id,
            pre,
        )
        .await;

        tracing::debug!(
            target: "based_rollup::l1_proxy",
            pass1_children = pass1_children.len(),
            unresolved = unresolved.len(),
            "chained L2 sim: walk_l2_simulation_trace result"
        );

        if unresolved.is_empty() {
            // All proxies resolved on first pass — no second RPC call needed.
            pass1_children
        } else {
            // Pass 2: re-run bundle with authorizedProxies queries appended.
            tracing::info!(
                target: "based_rollup::l1_proxy",
                count = unresolved.len(),
                addrs = ?unresolved,
                "chained L2 sim: pass 2 — resolving unresolved proxies via debug_traceCallMany"
            );

            // Build the same bundle as pass 1, plus one authorizedProxies(addr)
            // query tx per unresolved proxy address.
            let unresolved_addrs: Vec<Address> = unresolved.into_iter().collect();
            let mut resolution_txs = transactions.clone();
            for addr in &unresolved_addrs {
                let calldata = super::super::common::encode_authorized_proxies_calldata(*addr);
                resolution_txs.push(serde_json::json!({
                    "from": sys_addr_hex,
                    "to": ccm_hex,
                    "data": calldata,
                    "gas": "0x100000"
                }));
            }

            let trace_req2 = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "debug_traceCallMany",
                "params": [[{
                    "transactions": resolution_txs
                }], null, { "tracer": "callTracer" }],
                "id": 99963
            });

            let mut resolved_proxies = bundle_ephemeral_proxies.clone();
            if let Ok(resp2) = client.post(l2_rpc_url).json(&trace_req2).send().await {
                if let Ok(body2) = resp2.json::<super::super::common::JsonRpcResponse>().await {
                    if let Some(traces2) = body2.result
                        .as_ref()
                        .and_then(|r| r.get(0))
                        .and_then(|b| b.as_array())
                    {
                        // The resolution query traces start after the original bundle txs.
                        let resolution_start = transactions.len();
                        for (i, addr) in unresolved_addrs.iter().enumerate() {
                            let trace_idx = resolution_start + i;
                            if trace_idx >= traces2.len() {
                                tracing::warn!(
                                    target: "based_rollup::l1_proxy",
                                    %addr,
                                    trace_idx,
                                    total_traces = traces2.len(),
                                    "pass 2: resolution trace missing for address"
                                );
                                continue;
                            }
                            // The authorizedProxies view call returns data in the
                            // trace's `output` field: ABI-encoded (address, uint256).
                            let resolution_trace = &traces2[trace_idx];
                            let output_hex = resolution_trace
                                .get("output")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0x");
                            let output_clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
                            if output_clean.len() < 128 {
                                tracing::debug!(
                                    target: "based_rollup::l1_proxy",
                                    %addr,
                                    output_len = output_clean.len(),
                                    "pass 2: authorizedProxies output too short — proxy not found"
                                );
                                continue;
                            }
                            // First 32 bytes: originalAddress (address in last 20 bytes)
                            if let Ok(addr_bytes) = super::hex::decode(&output_clean[..64]) {
                                if addr_bytes.len() >= 32 {
                                    let original_address = Address::from_slice(&addr_bytes[12..32]);
                                    if original_address.is_zero() {
                                        continue;
                                    }
                                    // Second 32 bytes: originalRollupId (uint256, last 8 bytes as u64)
                                    if let Ok(rid_bytes) = super::hex::decode(&output_clean[64..128]) {
                                        if rid_bytes.len() >= 32 {
                                            let start = rid_bytes.len().saturating_sub(8);
                                            let mut rid: u64 = 0;
                                            for b in &rid_bytes[start..] {
                                                rid = (rid << 8) | (*b as u64);
                                            }
                                            tracing::info!(
                                                target: "based_rollup::l1_proxy",
                                                proxy = %addr,
                                                %original_address,
                                                rid,
                                                "pass 2: resolved proxy identity from simulation state"
                                            );
                                            resolved_proxies.insert(
                                                *addr,
                                                super::super::trace::ProxyInfo {
                                                    original_address,
                                                    original_rollup_id: rid,
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        // Re-walk the current trace (from the pass 2 bundle, same index)
                        // with resolved proxies pre-populated.
                        if !resolved_proxies.is_empty() {
                            let pass2_current_trace =
                                &traces2[traces2.len() - 1 - unresolved_addrs.len()];
                            let (pass2_children, pass2_unresolved) = walk_l2_simulation_trace(
                                client,
                                l2_rpc_url,
                                cross_chain_manager_address,
                                pass2_current_trace,
                                rollup_id,
                                Some(&resolved_proxies),
                            )
                            .await;
                            if !pass2_unresolved.is_empty() {
                                tracing::warn!(
                                    target: "based_rollup::l1_proxy",
                                    still_unresolved = pass2_unresolved.len(),
                                    "pass 2: some proxies still unresolved after resolution attempt"
                                );
                            }
                            pass2_children
                        } else {
                            pass1_children
                        }
                    } else {
                        tracing::warn!(
                            target: "based_rollup::l1_proxy",
                            "pass 2: no traces in debug_traceCallMany response"
                        );
                        pass1_children
                    }
                } else {
                    tracing::warn!(
                        target: "based_rollup::l1_proxy",
                        "pass 2: failed to parse debug_traceCallMany response"
                    );
                    pass1_children
                }
            } else {
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    "pass 2: debug_traceCallMany request failed"
                );
                pass1_children
            }
        }
    } else {
        Vec::new()
    };

    // Extract return data from the current call's trace.
    // The executeIncomingCrossChainCall may revert (void RESULT placeholder doesn't
    // match the actual return data), so extract the INNER destination return data.
    let has_error =
        current_trace.get("error").is_some() || current_trace.get("revertReason").is_some();

    let return_data = if has_error {
        extract_inner_destination_return_data(current_trace, destination).unwrap_or_default()
    } else {
        extract_inner_destination_return_data(current_trace, destination)
            .unwrap_or_else(|| extract_return_data_from_trace(current_trace))
    };
    let success = if has_error {
        // If inner return data found, the destination call succeeded even though
        // the top-level executeIncomingCrossChainCall reverted.
        !return_data.is_empty() || data.is_empty()
    } else {
        true
    };

    tracing::info!(
        target: "based_rollup::l1_proxy",
        %destination,
        %source_address,
        return_data_len = return_data.len(),
        success,
        child_count = children.len(),
        prior_calls = prior_exec_calldatas.len(),
        "chained L2 simulation complete"
    );

    (return_data, success, children)
}
