//! L2 trace enrichment and simulation helpers for L2→L1 cross-chain calls.
//!
//! Contains functions that enrich return calls via L2 `debug_traceCallMany`
//! simulation, look up proxy addresses, and simulate return call delivery
//! to detect deeper cross-chain calls.

use alloy_primitives::{Address, U256};
use serde_json::Value;

use crate::cross_chain::RollupId;
use super::super::model::{DiscoveredCall, L2ProxyLookup, ReturnEdge};

/// A cross-chain proxy call discovered by walking an L2 callTracer trace tree.
///
/// Simulate return calls on L2 via `debug_traceCallMany` to capture their actual
/// return data. For each return call with empty `return_data`:
/// 1. Compute L2 proxy address via `computeCrossChainProxyAddress`
/// 2. Run `debug_traceCallMany` on L2 with `[direct_call]`
/// 3. Extract return data from trace output
/// 4. Store in `rc.return_data` / `rc.delivery_failed`
///
/// This enrichment ensures that the next iteration of `simulate_l1_delivery` builds
/// inner RESULT entries with real data (instead of `data: vec![]`), so the L1
/// delivery function (e.g., Logger) receives the correct inner return value (#246).
pub(crate) async fn enrich_return_calls_via_l2_trace(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    return_calls: &mut [ReturnEdge],
    rollup_id: u64,
) {
    // Shared proxy cache across all return calls in this enrichment pass.
    let mut proxy_cache: std::collections::HashMap<Address, Option<super::super::trace::ProxyInfo>> =
        std::collections::HashMap::new();

    // --- Phase 1: Collect indices that need enrichment and resolve proxy addresses ---
    let needs_enrichment: Vec<usize> = if return_calls.len() >= 2 {
        // Chained enrichment: include ALL calls so state carries correctly.
        // Transactional: original data backed up inside try_chained_l2_enrichment.
        (0..return_calls.len()).collect()
    } else {
        // Single call: only enrich if empty
        (0..return_calls.len())
            .filter(|&i| {
                return_calls[i].return_data.is_empty() && !return_calls[i].delivery_failed
            })
            .collect()
    };

    if needs_enrichment.is_empty() {
        return;
    }

    // Resolve proxy addresses for all calls that need enrichment. Cache by source
    // address so duplicate sources (same L1 contract) share a single RPC call.
    let mut proxy_addr_cache: std::collections::HashMap<Address, Option<String>> =
        std::collections::HashMap::new();
    for &idx in &needs_enrichment {
        let source = return_calls[idx].source_address;
        if proxy_addr_cache.contains_key(&source) {
            continue;
        }
        let proxy_from =
            lookup_l2_proxy_address(client, l2_rpc_url, cross_chain_manager_address, source).await;
        proxy_addr_cache.insert(source, proxy_from);
    }

    // --- Phase 2: If 2+ calls need enrichment, try chained simulation ---
    if needs_enrichment.len() >= 2 {
        let chained_ok = try_chained_l2_enrichment(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            rollup_id,
            return_calls,
            &needs_enrichment,
            &proxy_addr_cache,
        )
        .await;
        if chained_ok {
            return;
        }
        // Chained simulation failed or a call reverted — fall through to per-call.
        tracing::info!(
            target: "based_rollup::composer_rpc",
            count = needs_enrichment.len(),
            "chained L2 enrichment failed or had reverts — falling back to per-call simulation"
        );
    }

    // --- Phase 3: Per-call enrichment (0-1 calls, or fallback from chained) ---
    for &i in &needs_enrichment {
        // Skip if already enriched (e.g. by a partial chained success — not currently
        // possible since we only return true on full success, but defensive).
        if !return_calls[i].return_data.is_empty() || return_calls[i].delivery_failed {
            continue;
        }

        let dest = return_calls[i].destination;
        let source = return_calls[i].source_address;
        let data = return_calls[i].data.clone();
        let value = return_calls[i].value;

        let proxy_from = proxy_addr_cache.get(&source).cloned().flatten();

        // Build debug_traceCallMany request on L2.
        // For a leaf return call (e.g., Counter.increment()), a direct call suffices.
        // The trace includes the full call tree including output.
        let source_hex = format!("{source}");
        let from_addr = if let Some(ref proxy) = proxy_from {
            proxy.as_str()
        } else {
            // Proxy address lookup failed — the L2 proxy for this L1 source may not
            // exist yet, or the CCM query failed. Fall back to source_address directly,
            // but warn loudly: msg.sender will be the raw L1 address instead of the
            // L2 proxy, so msg.sender-sensitive contracts will see the wrong caller.
            // Return data may be incorrect for contracts that gate logic on msg.sender.
            tracing::warn!(
                target: "based_rollup::composer_rpc",
                dest = %dest,
                source = %source,
                "proxy address lookup failed for L2 return call enrichment — \
                 falling back to source_address as msg.sender. Return data may \
                 be incorrect for msg.sender-sensitive contracts (#254 item 10c)"
            );
            source_hex.as_str()
        };
        let trace_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCallMany",
            "params": [[{
                "transactions": [{
                    "from": from_addr,
                    "to": format!("{dest}"),
                    "data": format!("0x{}", hex::encode(&data)),
                    "value": format!("0x{:x}", value),
                    "gas": "0x2faf080"
                }]
            }], null, { "tracer": "callTracer" }],
            "id": 99956
        });

        match client.post(l2_rpc_url).json(&trace_req).send().await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(body) => {
                    // Extract output from trace: result[0][0].output
                    if let Some(trace) = body
                        .get("result")
                        .and_then(|r| r.get(0))
                        .and_then(|b| b.as_array())
                        .and_then(|arr| arr.first())
                    {
                        // Check for revert.
                        let trace_error = trace.get("error").and_then(|v| v.as_str());
                        if trace_error.is_some() {
                            // Walk the trace tree using the generic walk_trace_tree
                            // path with ephemeral proxy support. Detects all cross-chain
                            // proxy calls in the trace regardless of revert reason.
                            let discovered = walk_l2_trace_for_discovered_proxy_calls(
                                client,
                                l2_rpc_url,
                                cross_chain_manager_address,
                                trace,
                                rollup_id,
                                &mut proxy_cache,
                            )
                            .await;

                            // Filter to reverted proxy calls only.
                            let reverted_proxies: Vec<_> =
                                discovered.into_iter().filter(|d| d.reverted).collect();

                            if !reverted_proxies.is_empty() {
                                tracing::info!(
                                    target: "based_rollup::composer_rpc",
                                    outer_dest = %dest,
                                    source = %source,
                                    reverted_proxy_count = reverted_proxies.len(),
                                    "L2 return call trace reverted — found {} proxy call(s) \
                                     via authorizedProxies, retrying with loadExecutionTable",
                                    reverted_proxies.len()
                                );

                                // Build combined placeholder L2 entries for ALL reverted
                                // proxy calls so the retry trace has all entries loaded.
                                let mut all_placeholder_entries = Vec::new();
                                for rp in &reverted_proxies {
                                    let placeholder =
                                        crate::composer_rpc::entry_builder::build_l2_to_l1_entries(
                                            rp.original_address,
                                            rp.data.clone(),
                                            rp.value,
                                            rp.source_address,
                                            rollup_id,
                                            vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
                                            vec![],     // delivery_return_data placeholder
                                            false,      // delivery_failed placeholder
                                            vec![],     // l1_delivery_scope placeholder
                                            crate::cross_chain::TxOutcome::Success,      // tx_reverts
                                        );
                                    all_placeholder_entries.extend(placeholder.l2_table_entries);
                                }

                                if !all_placeholder_entries.is_empty() {
                                    // Query SYSTEM_ADDRESS from the CCM.
                                    let system_addr = {
                                        // SYSTEM_ADDRESS() — typed ABI encoding via sol! macro — NEVER hardcode selectors.
                                        let sys_calldata =
                                            super::super::common::encode_system_address_calldata();
                                        let sys_req = serde_json::json!({
                                            "jsonrpc": "2.0",
                                            "method": "eth_call",
                                            "params": [{
                                                "to": format!("{cross_chain_manager_address}"),
                                                "data": sys_calldata
                                            }, "latest"],
                                            "id": 99957
                                        });
                                        if let Ok(r) =
                                            client.post(l2_rpc_url).json(&sys_req).send().await
                                        {
                                            if let Ok(b) = r.json::<Value>().await {
                                                b.get("result").and_then(|v| v.as_str()).and_then(
                                                    |s| {
                                                        let clean =
                                                            s.strip_prefix("0x").unwrap_or(s);
                                                        if clean.len() >= 64 {
                                                            Some(format!("0x{}", &clean[24..64]))
                                                        } else {
                                                            None
                                                        }
                                                    },
                                                )
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    };

                                    if let Some(sys_addr) = system_addr {
                                        let load_calldata = crate::composer_rpc::entry_builder::encode_load_table(
                                            &all_placeholder_entries,
                                        );
                                        let load_data =
                                            format!("0x{}", hex::encode(load_calldata.as_ref()));
                                        let ccm_hex = format!("{cross_chain_manager_address}");

                                        let bundle_trace_req = serde_json::json!({
                                            "jsonrpc": "2.0",
                                            "method": "debug_traceCallMany",
                                            "params": [
                                                [{
                                                    "transactions": [
                                                        {
                                                            "from": sys_addr,
                                                            "to": ccm_hex,
                                                            "data": load_data,
                                                            "gas": "0x1c9c380"
                                                        },
                                                        {
                                                            "from": from_addr,
                                                            "to": format!("{dest}"),
                                                            "data": format!("0x{}", hex::encode(&data)),
                                                            "value": format!("0x{:x}", value),
                                                            "gas": "0x2faf080"
                                                        }
                                                    ]
                                                }],
                                                null,
                                                { "tracer": "callTracer" }
                                            ],
                                            "id": 99958
                                        });

                                        match client
                                            .post(l2_rpc_url)
                                            .json(&bundle_trace_req)
                                            .send()
                                            .await
                                        {
                                            Ok(r2) => {
                                                if let Ok(b2) = r2.json::<Value>().await {
                                                    // Extract tx1 (index 1) trace — the return call
                                                    // execution with entries loaded.
                                                    if let Some(traces) = b2
                                                        .get("result")
                                                        .and_then(|r| r.get(0))
                                                        .and_then(|b| b.as_array())
                                                    {
                                                        if traces.len() >= 2 {
                                                            let t2 = &traces[1];
                                                            let t2_error = t2
                                                                .get("error")
                                                                .and_then(|v| v.as_str());
                                                            if t2_error.is_some() {
                                                                tracing::info!(
                                                                    target: "based_rollup::composer_rpc",
                                                                    dest = %dest,
                                                                    error = ?t2_error,
                                                                    "L2 return call still reverts after \
                                                                     loadExecutionTable — marking as failed"
                                                                );
                                                                return_calls[i]
                                                                    .delivery_failed = true;
                                                            } else if let Some(out_hex) = t2
                                                                .get("output")
                                                                .and_then(|v| v.as_str())
                                                            {
                                                                let clean = out_hex
                                                                    .strip_prefix("0x")
                                                                    .unwrap_or(out_hex);
                                                                if let Ok(bytes) =
                                                                    hex::decode(clean)
                                                                {
                                                                    if !bytes.is_empty() {
                                                                        tracing::info!(
                                                                            target: "based_rollup::composer_rpc",
                                                                            dest = %dest,
                                                                            source = %source,
                                                                            return_data_len = bytes.len(),
                                                                            "enriched non-leaf return call with L2 \
                                                                             return data via loadExecutionTable + \
                                                                             authorizedProxies detection"
                                                                        );
                                                                        return_calls[i]
                                                                            .return_data = bytes;
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    target: "based_rollup::composer_rpc",
                                                    %e,
                                                    dest = %dest,
                                                    "failed to send loadExecutionTable fallback \
                                                     trace for return call enrichment"
                                                );
                                            }
                                        }
                                    } else {
                                        tracing::warn!(
                                            target: "based_rollup::composer_rpc",
                                            dest = %dest,
                                            "could not query SYSTEM_ADDRESS from CCM — \
                                             cannot retry with loadExecutionTable"
                                        );
                                        return_calls[i].delivery_failed = true;
                                    }
                                } else {
                                    // No placeholder entries could be built — mark as failed.
                                    return_calls[i].delivery_failed = true;
                                }
                                continue;
                            }

                            // No proxy calls found in trace — revert is not proxy-related.
                            tracing::info!(
                                target: "based_rollup::composer_rpc",
                                dest = %dest,
                                source = %source,
                                error = ?trace_error,
                                "L2 return call trace reverted (no proxy calls found) — marking as failed"
                            );
                            return_calls[i].delivery_failed = true;
                            continue;
                        }

                        // Extract output hex.
                        if let Some(output_hex) = trace.get("output").and_then(|v| v.as_str()) {
                            let clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
                            if let Ok(bytes) = hex::decode(clean) {
                                if !bytes.is_empty() {
                                    tracing::info!(
                                        target: "based_rollup::composer_rpc",
                                        dest = %dest,
                                        source = %source,
                                        return_data_len = bytes.len(),
                                        "enriched return call with L2 return data via debug_traceCallMany (#246)"
                                    );
                                    return_calls[i].return_data = bytes;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "based_rollup::composer_rpc",
                        %e,
                        dest = %dest,
                        "failed to parse L2 trace response for return call enrichment (#246)"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::composer_rpc",
                    %e,
                    dest = %dest,
                    "failed to send L2 trace request for return call enrichment (#246)"
                );
            }
        }
    }
}

/// Look up the L2 proxy address for an L1 source address via
/// `computeCrossChainProxyAddress(originalAddress, originalRollupId=0)`.
/// Returns `Some("0x...")` hex string on success, `None` on failure.
/// Uses typed ABI encoding — NEVER hardcode selectors.
async fn lookup_l2_proxy_address(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    source: Address,
) -> Option<String> {
    use alloy_sol_types::SolCall;
    let compute_data = crate::cross_chain::IRollups::computeCrossChainProxyAddressCall {
        originalAddress: source,
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
        "id": 99955
    });
    let resp = client.post(l2_rpc_url).json(&req).send().await.ok()?;
    let body: Value = resp.json().await.ok()?;
    let s = body.get("result")?.as_str()?;
    let clean = s.strip_prefix("0x").unwrap_or(s);
    if clean.len() >= 64 {
        Some(format!("0x{}", &clean[24..64]))
    } else {
        None
    }
}

/// Attempt chained L2 enrichment: build a SINGLE `debug_traceCallMany` bundle
/// containing ALL return calls that need enrichment, so state from earlier calls
/// accumulates into later calls (e.g., Counter.increment() x2 sees counter=0
/// then counter=1, instead of both seeing counter=0).
///
/// **Transactional**: backs up existing `return_data` and `delivery_failed`
/// for all calls before attempting enrichment. On partial failure, reverted calls
/// get their backup restored.
///
/// **Phase 1** (simple chained trace): bundles all calls in one `debug_traceCallMany`.
/// If all succeed, keeps new data and returns `true`.
///
/// **Phase 2** (loadExecutionTable retry): if Phase 1 has any reverted calls, uses
/// `walk_l2_trace_for_discovered_proxy_calls` to discover inner proxy calls, builds
/// placeholder entries, prepends `loadExecutionTable` to the chained bundle, and
/// re-simulates. Calls that still revert get their backup restored.
///
/// Returns `true` if all calls were successfully enriched. Returns `false` if
/// the bundled trace failed at the transport level or Phase 2 could not recover
/// all calls — the caller should fall back to per-call simulation.
async fn try_chained_l2_enrichment(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    rollup_id: u64,
    return_calls: &mut [ReturnEdge],
    needs_enrichment: &[usize],
    proxy_addr_cache: &std::collections::HashMap<Address, Option<String>>,
) -> bool {
    // --- Backup existing data (transactional safety) ---
    let backups: Vec<(Vec<u8>, bool)> = needs_enrichment
        .iter()
        .map(|&idx| {
            (
                return_calls[idx].return_data.clone(),
                return_calls[idx].delivery_failed,
            )
        })
        .collect();

    // Build the transaction array for the bundle — one tx per return call,
    // all in a single bundle so state accumulates across transactions.
    let mut from_addrs: Vec<String> = Vec::with_capacity(needs_enrichment.len());

    for &idx in needs_enrichment {
        let rc = &return_calls[idx];
        let source = rc.source_address;

        let proxy_from = proxy_addr_cache.get(&source).and_then(|v| v.as_ref());
        let from_str = if let Some(proxy) = proxy_from {
            proxy.clone()
        } else {
            format!("{source}")
        };
        from_addrs.push(from_str);
    }

    let transactions: Vec<Value> = needs_enrichment
        .iter()
        .enumerate()
        .map(|(pos, &idx)| {
            let rc = &return_calls[idx];
            serde_json::json!({
                "from": from_addrs[pos],
                "to": format!("{}", rc.destination),
                "data": format!("0x{}", hex::encode(&rc.data)),
                "value": format!("0x{:x}", rc.value),
                "gas": "0x2faf080"
            })
        })
        .collect();

    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCallMany",
        "params": [[{
            "transactions": transactions
        }], null, { "tracer": "callTracer" }],
        "id": 99960
    });

    tracing::info!(
        target: "based_rollup::composer_rpc",
        tx_count = transactions.len(),
        "attempting chained L2 enrichment via single debug_traceCallMany bundle"
    );

    // Send the bundled trace request.
    let resp = match client.post(l2_rpc_url).json(&trace_req).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::composer_rpc",
                %e,
                "chained L2 enrichment: transport error sending bundled trace"
            );
            return false;
        }
    };

    let body: Value = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::composer_rpc",
                %e,
                "chained L2 enrichment: failed to parse bundled trace response"
            );
            return false;
        }
    };

    // Expected structure: result[0] = array of N trace objects (one per tx in the bundle).
    let traces = match body
        .get("result")
        .and_then(|r| r.get(0))
        .and_then(|b| b.as_array())
    {
        Some(arr) => arr,
        None => {
            tracing::warn!(
                target: "based_rollup::composer_rpc",
                "chained L2 enrichment: unexpected response shape — result[0] is not an array"
            );
            return false;
        }
    };

    if traces.len() != needs_enrichment.len() {
        tracing::warn!(
            target: "based_rollup::composer_rpc",
            expected = needs_enrichment.len(),
            got = traces.len(),
            "chained L2 enrichment: trace count mismatch"
        );
        return false;
    }

    // --- Phase 1: Check for reverts and extract output ---
    let mut reverted_positions: Vec<usize> = Vec::new();
    for (pos, trace) in traces.iter().enumerate() {
        if trace.get("error").and_then(|v| v.as_str()).is_some() {
            reverted_positions.push(pos);
        }
    }

    if reverted_positions.is_empty() {
        // All calls succeeded — extract output from each trace and write back.
        for (pos, trace) in traces.iter().enumerate() {
            let idx = needs_enrichment[pos];
            if let Some(output_hex) = trace.get("output").and_then(|v| v.as_str()) {
                let clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
                if let Ok(bytes) = hex::decode(clean) {
                    if !bytes.is_empty() {
                        tracing::info!(
                            target: "based_rollup::composer_rpc",
                            dest = %return_calls[idx].destination,
                            source = %return_calls[idx].source_address,
                            return_data_len = bytes.len(),
                            pos,
                            "enriched return call via chained L2 simulation (state accumulated)"
                        );
                        return_calls[idx].return_data = bytes;
                    }
                }
            }
        }
        return true;
    }

    // --- Phase 2: Some calls reverted — try loadExecutionTable retry ---
    tracing::info!(
        target: "based_rollup::composer_rpc",
        reverted_count = reverted_positions.len(),
        total = needs_enrichment.len(),
        "chained L2 enrichment Phase 1: {} call(s) reverted — attempting Phase 2 with loadExecutionTable",
        reverted_positions.len()
    );

    // For each reverted call, walk its trace to find inner proxy calls that need
    // loadExecutionTable entries.
    let mut proxy_cache: std::collections::HashMap<Address, Option<super::super::trace::ProxyInfo>> =
        std::collections::HashMap::new();
    let mut all_placeholder_entries = Vec::new();

    for &pos in &reverted_positions {
        let trace = &traces[pos];
        let discovered = walk_l2_trace_for_discovered_proxy_calls(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            trace,
            rollup_id,
            &mut proxy_cache,
        )
        .await;

        let reverted_proxies: Vec<_> = discovered.into_iter().filter(|d| d.reverted).collect();

        for rp in &reverted_proxies {
            let placeholder = crate::composer_rpc::entry_builder::build_l2_to_l1_entries(
                rp.original_address,
                rp.data.clone(),
                rp.value,
                rp.source_address,
                rollup_id,
                vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
                vec![],     // delivery_return_data placeholder
                false,      // delivery_failed placeholder
                vec![],     // l1_delivery_scope placeholder
                crate::cross_chain::TxOutcome::Success,      // tx_reverts
            );
            all_placeholder_entries.extend(placeholder.l2_table_entries);
        }
    }

    if all_placeholder_entries.is_empty() {
        // No proxy calls found in any reverted trace — restore backups for
        // reverted calls and report failure so per-call fallback handles them.
        for &pos in &reverted_positions {
            let idx = needs_enrichment[pos];
            return_calls[idx].return_data = backups[pos].0.clone();
            return_calls[idx].delivery_failed = backups[pos].1;
        }
        tracing::info!(
            target: "based_rollup::composer_rpc",
            "chained L2 enrichment Phase 2: no proxy calls found in reverted traces — falling back"
        );
        return false;
    }

    // Query SYSTEM_ADDRESS from the CCM.
    // Uses typed ABI encoding via sol! macro — NEVER hardcode selectors.
    let system_addr = {
        let sys_calldata = super::super::common::encode_system_address_calldata();
        let sys_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{
                "to": format!("{cross_chain_manager_address}"),
                "data": sys_calldata
            }, "latest"],
            "id": 99961
        });
        if let Ok(r) = client.post(l2_rpc_url).json(&sys_req).send().await {
            if let Ok(b) = r.json::<Value>().await {
                b.get("result").and_then(|v| v.as_str()).and_then(|s| {
                    let clean = s.strip_prefix("0x").unwrap_or(s);
                    if clean.len() >= 64 {
                        Some(format!("0x{}", &clean[24..64]))
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        } else {
            None
        }
    };

    let sys_addr = match system_addr {
        Some(addr) => addr,
        None => {
            tracing::warn!(
                target: "based_rollup::composer_rpc",
                "chained L2 enrichment Phase 2: could not query SYSTEM_ADDRESS from CCM — falling back"
            );
            // Restore backups for reverted calls.
            for &pos in &reverted_positions {
                let idx = needs_enrichment[pos];
                return_calls[idx].return_data = backups[pos].0.clone();
                return_calls[idx].delivery_failed = backups[pos].1;
            }
            return false;
        }
    };

    // Build Phase 2 bundle: [loadExecutionTable_tx, call_0, call_1, ..., call_N-1].
    let load_calldata =
        crate::composer_rpc::entry_builder::encode_load_table(&all_placeholder_entries);
    let load_data = format!("0x{}", hex::encode(load_calldata.as_ref()));
    let ccm_hex = format!("{cross_chain_manager_address}");

    let mut phase2_transactions = Vec::with_capacity(1 + needs_enrichment.len());
    // Index 0: loadExecutionTable transaction.
    phase2_transactions.push(serde_json::json!({
        "from": sys_addr,
        "to": ccm_hex,
        "data": load_data,
        "gas": "0x1c9c380"
    }));
    // Indices 1..N: same call transactions as Phase 1.
    phase2_transactions.extend(transactions.iter().cloned());

    let phase2_trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCallMany",
        "params": [[{
            "transactions": phase2_transactions
        }], null, { "tracer": "callTracer" }],
        "id": 99962
    });

    tracing::info!(
        target: "based_rollup::composer_rpc",
        tx_count = phase2_transactions.len(),
        placeholder_entries = all_placeholder_entries.len(),
        "chained L2 enrichment Phase 2: retrying with loadExecutionTable + all calls"
    );

    let resp2 = match client.post(l2_rpc_url).json(&phase2_trace_req).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::composer_rpc",
                %e,
                "chained L2 enrichment Phase 2: transport error"
            );
            // Restore backups for reverted calls.
            for &pos in &reverted_positions {
                let idx = needs_enrichment[pos];
                return_calls[idx].return_data = backups[pos].0.clone();
                return_calls[idx].delivery_failed = backups[pos].1;
            }
            return false;
        }
    };

    let body2: Value = match resp2.json().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::composer_rpc",
                %e,
                "chained L2 enrichment Phase 2: failed to parse response"
            );
            for &pos in &reverted_positions {
                let idx = needs_enrichment[pos];
                return_calls[idx].return_data = backups[pos].0.clone();
                return_calls[idx].delivery_failed = backups[pos].1;
            }
            return false;
        }
    };

    // Expected: result[0] = array of 1+N traces (index 0 = loadExecutionTable, 1..N = calls).
    let traces2 = match body2
        .get("result")
        .and_then(|r| r.get(0))
        .and_then(|b| b.as_array())
    {
        Some(arr) if arr.len() == 1 + needs_enrichment.len() => arr,
        _ => {
            tracing::warn!(
                target: "based_rollup::composer_rpc",
                expected = 1 + needs_enrichment.len(),
                "chained L2 enrichment Phase 2: unexpected trace count"
            );
            for &pos in &reverted_positions {
                let idx = needs_enrichment[pos];
                return_calls[idx].return_data = backups[pos].0.clone();
                return_calls[idx].delivery_failed = backups[pos].1;
            }
            return false;
        }
    };

    // Parse traces at indices 1..N (skip index 0 which is loadExecutionTable).
    let mut all_enriched = true;
    for (pos, &idx) in needs_enrichment.iter().enumerate() {
        let trace = &traces2[1 + pos]; // +1 to skip loadExecutionTable trace
        let trace_error = trace.get("error").and_then(|v| v.as_str());

        if trace_error.is_some() {
            // This call still reverts even with loadExecutionTable — restore backup.
            tracing::info!(
                target: "based_rollup::composer_rpc",
                dest = %return_calls[idx].destination,
                pos,
                error = ?trace_error,
                "chained L2 enrichment Phase 2: call {} still reverts — restoring backup",
                pos
            );
            return_calls[idx].return_data = backups[pos].0.clone();
            return_calls[idx].delivery_failed = backups[pos].1;
            all_enriched = false;
        } else if let Some(output_hex) = trace.get("output").and_then(|v| v.as_str()) {
            let clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
            if let Ok(bytes) = hex::decode(clean) {
                if !bytes.is_empty() {
                    tracing::info!(
                        target: "based_rollup::composer_rpc",
                        dest = %return_calls[idx].destination,
                        source = %return_calls[idx].source_address,
                        return_data_len = bytes.len(),
                        pos,
                        "enriched return call via chained L2 Phase 2 (loadExecutionTable + state accumulated)"
                    );
                    return_calls[idx].return_data = bytes;
                }
            }
        }
    }

    all_enriched
}

/// Walk an L2 trace using the generic `trace::walk_trace_tree` and convert results
/// to `DiscoveredCall` format used by the rest of this module.
pub(super) async fn walk_l2_trace_generic(
    client: &reqwest::Client,
    upstream_url: &str,
    ccm_address: Address,
    trace_node: &Value,
    proxy_cache: &mut std::collections::HashMap<Address, Option<super::super::trace::ProxyInfo>>,
) -> Vec<DiscoveredCall> {
    let lookup = L2ProxyLookup {
        client,
        rpc_url: upstream_url,
        ccm_address,
    };

    // Delegate to the shared walk function. walk_trace_to_discovered already returns
    // DiscoveredCall with the correct defaults (delivery_return_data=[], delivery_failed=false,
    // parent_call_index=Root, discovery_iteration=0, target_rollup_id=0).
    super::super::model::walk_trace_to_discovered(
        &lookup,
        &[ccm_address],
        trace_node,
        proxy_cache,
        0, // default_target_rollup_id: L2→L1 targets L1 (rollup 0)
        0, // discovery_iteration: initial trace
    )
    .await
}

/// Walk an L2 trace using the generic `trace::walk_trace_tree` and return results
/// as `DiscoveredProxyCall` (compatible with callers that need `reverted` flag).
///
/// Replaces the legacy `find_failed_proxy_calls_in_l2_trace` which used a separate
/// `authorizedProxies`-only detection path without ephemeral proxy support.
///
/// The `reverted` flag is inferred from whether the trace node has an `error` field.
/// Since `walk_trace_tree` detects calls regardless of revert status, all detected
/// calls in a reverted trace are marked as reverted.
pub(super) async fn walk_l2_trace_for_discovered_proxy_calls(
    client: &reqwest::Client,
    upstream_url: &str,
    ccm_address: Address,
    trace_node: &Value,
    our_rollup_id: u64,
    proxy_cache: &mut std::collections::HashMap<Address, Option<super::super::trace::ProxyInfo>>,
) -> Vec<super::super::common::DiscoveredProxyCall> {
    let lookup = L2ProxyLookup {
        client,
        rpc_url: upstream_url,
        ccm_address,
    };
    let mut ephemeral_proxies = std::collections::HashMap::new();
    let mut detected_calls = Vec::new();

    super::super::trace::walk_trace_tree(
        trace_node,
        &[ccm_address],
        &lookup,
        proxy_cache,
        &mut ephemeral_proxies,
        &mut detected_calls,
        &mut std::collections::HashSet::new(),
    )
    .await;

    // Convert trace::DetectedCall to DiscoveredProxyCall, filtering out calls
    // targeting our own rollup (only include L2→L1 or L2→otherRollup calls).
    // The `reverted` flag reflects the trace node's error status.
    let trace_has_error = trace_node.get("error").is_some();

    detected_calls
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
                        reverted: trace_has_error,
                    })
                }
                _ => None, // Targets our rollup or identity not found — skip
            }
        })
        .collect()
}

/// Simulate L1→L2 return calls on L2 to detect further L2→L1 calls (depth > 1).
///
/// Uses `debug_traceCallMany` with `[loadExecutionTable, returnCallExecution]` so that
/// the return call's execution has placeholder entries loaded. Without pre-loading,
/// `callTracer` doesn't show subcalls when the top-level reverts (the L2 proxy call
/// to CCM.executeCrossChainCall fails without entries → entire trace collapses).
///
/// For each return call, we:
/// 1. Build a placeholder L2→L1 entry for the return call itself (so CCM has *something*
///    to consume — the exact entry won't match but the call still executes inside the
///    proxy's scope navigation before hitting executeCrossChainCall for the nested call)
/// 2. Actually: the return call executes via `_processCallAtScope` on L2, which calls
///    `proxy.executeOnBehalf(destination, data)`. The destination contract then calls
///    another proxy → CCM.executeCrossChainCall for the NESTED call. That inner call
///    is the one that reverts (no entries for it). But we need to SEE the proxy call.
/// 3. Solution: trace `destination.data()` directly with loadExecutionTable pre-loading
///    a dummy entry so the call doesn't revert at the top level.
///
/// Simpler approach that works: trace the return call as if the CCM is calling
/// `destination.data()` directly. Build a dummy L2→L1 entry for any call the destination
/// makes to a proxy (the nested L2→L1 call). Pre-load it via loadExecutionTable so the
/// nested call doesn't revert. Then walk the trace.
pub(super) async fn simulate_l2_return_call_delivery(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    ccm_address: Address,
    return_calls: &[ReturnEdge],
    rollup_id: u64,
) -> Vec<DiscoveredCall> {
    if return_calls.is_empty() {
        return Vec::new();
    }

    tracing::info!(
        target: "based_rollup::proxy",
        return_call_count = return_calls.len(),
        "simulating L1→L2 return calls on L2 to detect depth > 1 L2→L1 calls"
    );

    let mut all_detected: Vec<DiscoveredCall> = Vec::new();
    let mut proxy_cache: std::collections::HashMap<Address, Option<super::super::trace::ProxyInfo>> =
        std::collections::HashMap::new();

    // loadExecutionTable requires the system address (onlySystemAddress modifier).
    // On our L2 chain, CCM.SYSTEM_ADDRESS is the builder address (set in constructor).
    // We need the builder address to call loadExecutionTable in the simulation.
    // The caller (trace_and_detect_l2_internal_calls) has builder_address, but this
    // function doesn't. Use the existing iterative discovery pattern: query SYSTEM_ADDRESS
    // from the CCM, or use a known builder address.
    // For now, query it via eth_call.
    // SYSTEM_ADDRESS() — typed ABI encoding via sol! macro — NEVER hardcode selectors.
    let system_addr = {
        let sys_calldata = super::super::common::encode_system_address_calldata();
        let sys_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": format!("{ccm_address}"), "data": sys_calldata}, "latest"],
            "id": 99969
        });
        let sys_addr = if let Ok(resp) = client.post(l2_rpc_url).json(&sys_req).send().await {
            if let Ok(body) = resp.json::<Value>().await {
                body.get("result").and_then(|v| v.as_str()).and_then(|s| {
                    let clean = s.strip_prefix("0x").unwrap_or(s);
                    if clean.len() >= 64 {
                        Some(format!("0x{}", &clean[24..64]))
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        } else {
            None
        };
        sys_addr.unwrap_or_else(|| format!("{ccm_address}"))
    };
    let ccm_hex = format!("{ccm_address}");

    for (i, rc) in return_calls.iter().enumerate() {
        let data_prefix = if rc.data.len() >= 4 {
            format!("0x{}", hex::encode(&rc.data[..4]))
        } else {
            format!("0x{}", hex::encode(&rc.data))
        };

        tracing::info!(
            target: "based_rollup::proxy",
            idx = i,
            destination = %rc.destination,
            source_address = %rc.source_address,
            data_len = rc.data.len(),
            selector = %data_prefix,
            value = %rc.value,
            scope_len = rc.scope.len(),
            "simulating return call on L2 via executeIncomingCrossChainCall (protocol path)"
        );

        // Simulate the return call using the real protocol path:
        // tx[0]: loadExecutionTable(placeholder entries) — makes scope navigation work
        // tx[1]: executeIncomingCrossChainCall(dest, value, data, source, rollupId, scope)
        //
        // Previous approach called debug_traceCall(from=CCM, to=dest) directly, which:
        // - Used wrong msg.sender (CCM instead of proxy)
        // - Skipped scope navigation entirely
        // - Didn't match real execution context
        //
        // The protocol path goes through CCM → proxy → destination, matching
        // real L2 block execution where the driver calls executeIncomingCrossChainCall.

        // Build the executeIncomingCrossChainCall calldata using the return call's info.
        let incoming_action = crate::cross_chain::CrossChainAction {
            action_type: crate::cross_chain::CrossChainActionType::Call,
            rollup_id: RollupId::new(U256::from(rollup_id)),
            destination: rc.destination,
            value: rc.value,
            data: rc.data.clone(),
            failed: false,
            source_address: rc.source_address,
            source_rollup: RollupId::MAINNET, // L1 (MAINNET_ROLLUP_ID)
            scope: rc.scope.clone(),
        };
        let exec_calldata =
            crate::cross_chain::encode_execute_incoming_call_calldata(&incoming_action);
        let exec_data = format!("0x{}", hex::encode(exec_calldata.as_ref()));

        // Build placeholder L2 entries for loadExecutionTable so scope navigation
        // can find entries if the destination triggers further cross-chain calls.
        let placeholder_entries = crate::composer_rpc::entry_builder::build_l2_to_l1_entries(
            rc.destination,
            rc.data.clone(),
            rc.value,
            rc.source_address,
            rollup_id,
            vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
            vec![],     // delivery_return_data placeholder
            false,      // delivery_failed placeholder
            vec![],     // l1_delivery_scope placeholder
            crate::cross_chain::TxOutcome::Success,      // tx_reverts
        );

        let mut detected_for_call: Vec<DiscoveredCall> = Vec::new();

        // Build traceCallMany: [loadExecutionTable, executeIncomingCrossChainCall]
        let load_calldata = crate::composer_rpc::entry_builder::encode_load_table(
            &placeholder_entries.l2_table_entries,
        );
        let load_data = format!("0x{}", hex::encode(load_calldata.as_ref()));

        let trace_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCallMany",
            "params": [
                [
                    {
                        "transactions": [
                            {
                                "from": system_addr,
                                "to": ccm_hex,
                                "data": load_data,
                                "gas": "0x1c9c380"
                            },
                            {
                                "from": system_addr,
                                "to": ccm_hex,
                                "data": exec_data,
                                "value": format!("0x{:x}", rc.value),
                                "gas": "0x2faf080"
                            }
                        ]
                    }
                ],
                null,
                { "tracer": "callTracer" }
            ],
            "id": 99970
        });

        if let Ok(resp) = client.post(l2_rpc_url).json(&trace_req).send().await {
            if let Ok(body) = resp.json::<Value>().await {
                // Extract tx[1] trace (executeIncomingCrossChainCall with entries loaded)
                if let Some(traces) = body
                    .get("result")
                    .and_then(|r| r.get(0))
                    .and_then(|b| b.as_array())
                {
                    if traces.len() >= 2 {
                        detected_for_call = walk_l2_trace_generic(
                            client,
                            l2_rpc_url,
                            ccm_address,
                            &traces[1],
                            &mut proxy_cache,
                        )
                        .await;
                    }
                }
            }
        }

        if !detected_for_call.is_empty() {
            tracing::info!(
                target: "based_rollup::proxy",
                idx = i,
                destination = %rc.destination,
                count = detected_for_call.len(),
                "return call simulation detected {} new L2→L1 calls",
                detected_for_call.len()
            );

            all_detected.extend(detected_for_call);
        }
    }

    if !all_detected.is_empty() {
        tracing::info!(
            target: "based_rollup::proxy",
            total = all_detected.len(),
            "L2 return call simulation found {} total new L2→L1 calls across {} return calls",
            all_detected.len(), return_calls.len()
        );
    }

    all_detected
}
