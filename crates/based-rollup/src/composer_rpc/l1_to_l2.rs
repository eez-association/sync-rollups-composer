//! L1 RPC proxy for transparent cross-chain call detection.
//!
//! Sits in front of the L1 RPC and transparently forwards all requests.
//! Intercepts `eth_sendRawTransaction` to detect cross-chain calls:
//!
//! 1. **Detect**: Check if tx targets a CrossChainProxy (via `authorizedProxies`
//!    mapping on Rollups.sol — returns `ProxyInfo(originalAddress, originalRollupId)`)
//! 2. **Queue**: Call `syncrollups_initiateCrossChainCall` on the builder's L2 RPC
//!    with the gas price and raw L1 tx bundled atomically. The driver sorts entries
//!    by gas price descending (matching L1 miner ordering) before computing chained
//!    state deltas, then forwards the L1 txs after `postBatch`.
//!
//! The driver batches all entries into a single `postBatch`, then forwards queued
//! L1 txs — no nonce contention with the proposer's `submitBatch`.
//!
//! Users point MetaMask at this proxy for transparent synchronous composability.

use crate::cross_chain::filter_new_by_count;
use alloy_primitives::{Address, U256};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes as HyperBytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::net::TcpListener;

// Shared helpers from the common module.
use super::common::{
    cors_response, detect_cross_chain_proxy_on_l2, error_response, eth_call_view, extract_methods,
    get_l1_block_context, get_verification_key, parse_address_from_abi_return,
};

/// Decode a `0x`-prefixed 4-byte error selector into a human-readable name.
///
/// Uses compile-time selectors from `common.rs` sol! macro definitions.
/// Returns `"unknown"` for unrecognized selectors.
fn decode_error_selector_prefixed(selector: Option<&str>) -> &'static str {
    use super::common::{
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
fn decode_error_selector_bare(selector: Option<&str>) -> &'static str {
    use super::common::{
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

/// Run the L1 RPC proxy server.
#[allow(clippy::too_many_arguments)]
pub async fn run_l1_rpc_proxy(
    l1_proxy_port: u16,
    l1_rpc_url: String,
    l2_rpc_url: String,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<String>,
    rollup_id: u64,
    cross_chain_manager_address: Address,
) -> eyre::Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], l1_proxy_port));
    let listener = TcpListener::bind(addr).await?;

    tracing::info!(
        target: "based_rollup::l1_proxy",
        %l1_proxy_port,
        %l1_rpc_url,
        %l2_rpc_url,
        %rollups_address,
        %builder_address,
        "L1 RPC proxy listening"
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build reqwest client");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "based_rollup::l1_proxy", %e, "accept failed");
                // Brief backoff to prevent CPU-saturating spin on persistent errors
                // (e.g., file descriptor exhaustion).
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };

        let client = client.clone();
        let l1_rpc_url = l1_rpc_url.clone();
        let l2_rpc_url = l2_rpc_url.clone();
        let builder_private_key = builder_private_key.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let client = client.clone();
                let l1_rpc_url = l1_rpc_url.clone();
                let l2_rpc_url = l2_rpc_url.clone();
                let builder_private_key = builder_private_key.clone();
                handle_request(
                    req,
                    client,
                    l1_rpc_url,
                    l2_rpc_url,
                    rollups_address,
                    builder_address,
                    builder_private_key,
                    rollup_id,
                    cross_chain_manager_address,
                    peer,
                )
            });

            let io = TokioIo::new(stream);
            if let Err(e) = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, service)
                .await
            {
                if !e.is_incomplete_message() {
                    tracing::debug!(
                        target: "based_rollup::l1_proxy",
                        %e, %peer,
                        "connection error"
                    );
                }
            }
        });
    }
}

/// Handle a single JSON-RPC request.
#[allow(clippy::too_many_arguments)]
async fn handle_request(
    req: Request<hyper::body::Incoming>,
    client: reqwest::Client,
    l1_rpc_url: String,
    l2_rpc_url: String,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<String>,
    rollup_id: u64,
    cross_chain_manager_address: Address,
    _peer: SocketAddr,
) -> Result<Response<Full<HyperBytes>>, hyper::Error> {
    // Handle CORS preflight
    if req.method() == hyper::Method::OPTIONS {
        return Ok(cors_response(
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Full::new(HyperBytes::new()))
                .expect("valid response"),
        ));
    }

    // Only handle POST (JSON-RPC)
    if req.method() != hyper::Method::POST {
        return Ok(cors_response(
            Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(Full::new(HyperBytes::from("Method Not Allowed")))
                .expect("valid response"),
        ));
    }

    // Read request body (cap at 10 MB to prevent memory exhaustion)
    const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;
    let body_bytes = match req.collect().await {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            if bytes.len() > MAX_BODY_SIZE {
                return Ok(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body too large",
                ));
            }
            bytes
        }
        Err(e) => {
            tracing::debug!(target: "based_rollup::l1_proxy", %e, "failed to read request body");
            return Ok(error_response(StatusCode::BAD_REQUEST, "bad request body"));
        }
    };

    // Try to parse as JSON-RPC
    let maybe_json: Option<Value> = serde_json::from_slice(&body_bytes).ok();

    // Intercept specific JSON-RPC methods for cross-chain handling
    if let Some(ref json) = maybe_json {
        let methods = extract_methods(json);
        for (method, params) in &methods {
            if method == "eth_sendRawTransaction" {
                if let Some(raw_tx) = params.and_then(|p| p.first()).and_then(|v| v.as_str()) {
                    match handle_cross_chain_tx(
                        &client,
                        &l1_rpc_url,
                        &l2_rpc_url,
                        raw_tx,
                        rollups_address,
                        builder_address,
                        builder_private_key.clone(),
                        rollup_id,
                        cross_chain_manager_address,
                    )
                    .await
                    {
                        Ok(Some(tx_hash)) => {
                            // Cross-chain tx queued — entries + user tx sent to builder.
                            // Return the tx hash directly WITHOUT forwarding to L1.
                            // The driver will submit postBatch then forward the raw tx.
                            let json_id = json.get("id").cloned().unwrap_or(Value::Null);
                            let response_body = serde_json::json!({
                                "jsonrpc": "2.0",
                                "result": tx_hash,
                                "id": json_id
                            });
                            return Ok(cors_response(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("Content-Type", "application/json")
                                    .body(Full::new(HyperBytes::from(response_body.to_string())))
                                    .expect("valid response"),
                            ));
                        }
                        Ok(None) => {
                            // Not a cross-chain tx, forward normally (fall through)
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "based_rollup::l1_proxy",
                                %e,
                                "cross-chain handling failed, forwarding tx anyway"
                            );
                        }
                    }
                }
            }

            // Intercept eth_estimateGas for cross-chain proxy addresses.
            // Wallets (MetaMask, Rabby) call this before showing the confirmation
            // dialog. For cross-chain proxy calls, L1 estimation always reverts
            // because the execution table isn't populated yet, causing wallets to
            // fall back to incorrect defaults (e.g. Rabby uses 2M gas).
            // We compute gas from calldata instead.
            if method == "eth_estimateGas" {
                if let Some(result) = handle_estimate_gas_for_proxy(
                    &client,
                    &l1_rpc_url,
                    *params,
                    rollups_address,
                    json,
                )
                .await
                {
                    return Ok(result);
                }
            }
        }
    }

    // Forward the original request to L1 as-is
    let resp = match client
        .post(&l1_rpc_url)
        .header("Content-Type", "application/json")
        .body(body_bytes.to_vec())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "based_rollup::l1_proxy", %e, "L1 request failed");
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("L1 upstream error: {e}"),
            ));
        }
    };

    let status = resp.status();
    let resp_bytes = resp.bytes().await.unwrap_or_default();

    Ok(cors_response(
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Full::new(HyperBytes::from(resp_bytes.to_vec())))
            .expect("valid response"),
    ))
}

/// Handle a potential cross-chain transaction.
///
/// Returns `Ok(Some(tx_hash))` if a cross-chain call was detected and both
/// the execution entries and the user's raw L1 tx were queued for atomic
/// submission by the driver. The caller should return `tx_hash` to the user
/// and NOT forward the tx to L1.
///
/// Returns `Ok(None)` if this is not a cross-chain tx (just forward normally).
/// Returns `Err` if detection/queuing failed.
///
/// Uses a single code path: trace the tx with `debug_traceCall` and walk the
/// call tree with the generic `trace::walk_trace_tree`. No special-case
/// detection for direct proxy calls or bridge contracts — the generic walker
/// detects all patterns (direct proxy, bridgeEther, bridgeTokens, wrapper
/// contracts, multi-call continuations) via the `executeCrossChainCall` child pattern.
#[allow(clippy::too_many_arguments)]
async fn handle_cross_chain_tx(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    raw_tx: &str,
    rollups_address: Address,
    _builder_address: Address,
    builder_private_key: Option<String>,
    rollup_id: u64,
    cross_chain_manager_address: Address,
) -> eyre::Result<Option<String>> {
    // Decode the raw transaction to extract fields needed by the trace path.
    let tx_obj = decode_raw_tx_for_trace(raw_tx)?;

    // Contract creation cannot contain cross-chain calls.
    if tx_obj.get("to").and_then(|v| v.as_str()).is_none() {
        return Ok(None);
    }

    // Single code path: trace the tx and detect all cross-chain calls
    // via the generic walk_trace_tree (executeCrossChainCall child pattern).
    trace_and_detect_internal_calls(
        client,
        l1_rpc_url,
        l2_rpc_url,
        raw_tx,
        &tx_obj,
        rollups_address,
        builder_private_key,
        rollup_id,
        cross_chain_manager_address,
    )
    .await
}

/// Queue detected cross-chain calls as a single execution table via
/// `syncrollups_buildExecutionTable`. Handles any number of calls (1 or more).
/// Entries are built atomically (with L2→L1 child call detection for multi-call).
async fn queue_execution_table(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    raw_tx: &str,
    detected_calls: &[DetectedInternalCall],
    effective_gas_price: u128,
) -> eyre::Result<Option<String>> {
    let calls: Vec<serde_json::Value> = detected_calls
        .iter()
        .map(|c| {
            let mut call_json = serde_json::json!({
                "destination": format!("{}", c.destination),
                "data": format!("0x{}", hex::encode(&c.calldata)),
                "value": format!("{}", c.value),
                "sourceAddress": format!("{}", c.source_address)
            });
            // Include L2 simulation results when available.
            if !c.return_data.is_empty() || !c.call_success {
                call_json["l2ReturnData"] =
                    serde_json::json!(format!("0x{}", hex::encode(&c.return_data)));
                call_json["callSuccess"] = serde_json::json!(c.call_success);
            }
            // Include parent linkage and target rollup for L2→L1 child calls
            // (the L1→L2→L1 nested pattern). Without these, analyze_continuation_calls
            // treats all calls as L1→L2, producing wrong entry structures.
            if let Some(parent_idx) = c.parent_call_index {
                call_json["parentCallIndex"] = serde_json::json!(parent_idx);
            }
            if c.target_rollup_id == 0 && c.parent_call_index.is_some() {
                // Explicitly mark L2→L1 children (target=L1=0) so the RPC handler
                // can distinguish them from L1→L2 calls.
                call_json["targetRollupId"] = serde_json::json!(0u64);
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

    let resp = client
        .post(l2_rpc_url)
        .json(&req)
        .send()
        .await?
        .json::<Value>()
        .await?;

    if let Some(error) = resp.get("error") {
        return Err(eyre::eyre!("buildExecutionTable failed: {error}"));
    }

    let call_id = resp
        .get("result")
        .and_then(|v| v.get("callId"))
        .and_then(|v| v.as_str())
        .unwrap_or("0x")
        .to_string();

    let l2_count = resp
        .get("result")
        .and_then(|v| v.get("l2EntryCount"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let l1_count = resp
        .get("result")
        .and_then(|v| v.get("l1EntryCount"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    tracing::info!(
        target: "based_rollup::l1_proxy",
        %call_id,
        l2_entries = l2_count,
        l1_entries = l1_count,
        "built execution table for multi-call tx — queued atomically"
    );

    let tx_hash = compute_tx_hash_from_raw(raw_tx).unwrap_or(call_id);
    Ok(Some(tx_hash))
}

/// A detected internal cross-chain call from a trace walk.
#[derive(Clone)]
struct DetectedInternalCall {
    /// Destination address on the target rollup.
    destination: Address,
    /// Rollup ID of the target rollup (0 = L1/mainnet, 1+ = L2 rollups).
    /// Used to distinguish L1→L2 calls (target_rollup_id > 0) from
    /// L2→L1 children (target_rollup_id = 0) discovered in L2 simulation.
    target_rollup_id: u64,
    /// Calldata to execute on the destination (inner calldata for proxy, parsed for bridge).
    calldata: Vec<u8>,
    /// ETH value sent with the call.
    value: U256,
    /// The address that called the proxy/bridge (msg.sender in that frame).
    source_address: Address,
    /// Whether the L2 call succeeded (from L2 simulation).
    /// Defaults to `true` when L2 simulation is not performed.
    call_success: bool,
    /// Return data from simulating this call on L2 (via `debug_traceCallMany`).
    /// Used for the RESULT action hash — if the L2 target returns non-void data,
    /// the RESULT hash must include it (contracts/sync-rollups-protocol/docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md §C.2).
    /// Empty when the call returns void or when simulation was not performed.
    return_data: Vec<u8>,
    /// Index of the parent L1→L2 call whose L2 execution triggers this child.
    /// `None` for root-level L1→L2 calls; `Some(i)` for L2→L1 child calls
    /// discovered inside call[i]'s L2 simulation (the L1→L2→L1 pattern).
    parent_call_index: Option<usize>,
}

/// Execute a `debug_traceCallMany` bundle on L2:
///   [0] `loadExecutionTable(entries)` — from SYSTEM_ADDRESS to CCM
///   [1] `executeIncomingCrossChainCall(...)` — from SYSTEM_ADDRESS to CCM
///
/// Returns `Some((exec_trace, success))` where `exec_trace` is the callTracer
/// output for tx[1] and `success` indicates whether the call reverted.
/// Returns `None` on RPC or parse failure.
async fn run_l2_sim_bundle(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    sys_addr: &str,
    ccm_hex: &str,
    load_entries: &[crate::cross_chain::CrossChainExecutionEntry],
    exec_calldata: &[u8],
    value: U256,
) -> Option<(Value, bool)> {
    let load_calldata = crate::cross_chain::encode_load_execution_table_calldata(load_entries);
    let load_data = format!("0x{}", hex::encode(load_calldata.as_ref()));
    let exec_data = format!("0x{}", hex::encode(exec_calldata));
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
    let body: Value = match resp.json().await {
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
    let traces = body
        .get("result")
        .and_then(|r| r.get(0))
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

/// Extract return data bytes from a callTracer trace node's `output` field.
fn extract_return_data_from_trace(trace: &Value) -> Vec<u8> {
    trace
        .get("output")
        .and_then(|v| v.as_str())
        .and_then(|s| hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok())
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
fn destination_call_succeeded_in_trace(trace: &Value, destination: Address) -> bool {
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

fn extract_inner_destination_return_data(trace: &Value, destination: Address) -> Option<Vec<u8>> {
    extract_inner_destination_output(trace, destination, false)
}

/// Like extract_inner_destination_return_data but also extracts output from reverted calls.
/// Used for terminal revert patterns where the revert reason bytes are needed for
/// RESULT(failed, data=revertData) entries.
fn extract_inner_destination_revert_data(trace: &Value, destination: Address) -> Option<Vec<u8>> {
    extract_inner_destination_output(trace, destination, true)
}

fn extract_inner_destination_output(
    trace: &Value,
    destination: Address,
    include_reverted: bool,
) -> Option<Vec<u8>> {
    let dest_hex_lower = format!("{destination}").to_lowercase();

    fn walk(node: &Value, target: &str, include_reverted: bool) -> Option<Vec<u8>> {
        if let Some(to) = node.get("to").and_then(|v| v.as_str()) {
            if to.to_lowercase() == target {
                if !include_reverted && node.get("error").is_some() {
                    return None;
                }
                let output = node.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                let data =
                    hex::decode(output.strip_prefix("0x").unwrap_or(output)).unwrap_or_default();
                return Some(data);
            }
        }
        // Recurse into children
        if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
            for child in calls {
                if let Some(result) = walk(child, target, include_reverted) {
                    return Some(result);
                }
            }
        }
        None
    }

    walk(trace, &dest_hex_lower, include_reverted)
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
async fn simulate_l1_to_l2_call_on_l2(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    destination: Address,
    data: &[u8],
    value: U256,
    source_address: Address,
    rollup_id: u64,
) -> (Vec<u8>, bool, Vec<super::common::DiscoveredProxyCall>) {
    // Step 1: Query SYSTEM_ADDRESS from the CCM.
    // Uses typed ABI encoding via sol! macro — NEVER hardcode selectors.
    let sys_calldata = super::common::encode_system_address_calldata();
    let sys_result = super::common::eth_call_view(
        client,
        l2_rpc_url,
        cross_chain_manager_address,
        &sys_calldata,
    )
    .await;

    let sys_addr = match sys_result.and_then(|s| super::common::parse_address_from_abi_return(&s)) {
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
    let sim_action = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Call,
        rollup_id: U256::from(rollup_id),
        destination,
        value,
        data: data.to_vec(),
        failed: false,
        source_address,
        source_rollup: U256::ZERO, // L1 = rollup 0
        scope: vec![],
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
            // Use 32-byte zero placeholder as delivery return data (not empty).
            // Empty data causes ABI decode failure when the caller expects a
            // return value (e.g., deepCall returns uint256 — Solidity's decoder
            // reverts on 0 bytes). 32 bytes of zeros = abi.encode(uint256(0)).
            let placeholder_return = vec![0u8; 32];
            let placeholder = crate::cross_chain::build_l2_to_l1_call_entries(
                child.original_address,
                child.data.clone(),
                child.value,
                child.source_address,
                rollup_id,
                vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
                placeholder_return, // 32-byte placeholder return data
                false,              // delivery_failed placeholder
            );
            placeholders.extend(placeholder.l2_table_entries);
        }

        // Retry: load child entries, then call the target DIRECTLY (not via
        // executeIncomingCrossChainCall) to capture return data. Using the CCM
        // path requires a RESULT entry whose hash depends on the unknown return
        // data — a circular dependency. Direct call bypasses the CCM's
        // _consumeExecution for the RESULT, while still using child entries
        // for the inner executeCrossChainCall calls.
        {
            let load_cd = crate::cross_chain::encode_load_execution_table_calldata(&placeholders);
            let load_data = format!("0x{}", hex::encode(load_cd.as_ref()));
            let dest_hex = format!("{destination}");
            let call_data = format!("0x{}", hex::encode(data));
            // Use SYSTEM_ADDRESS as caller (has funds on L2)
            let proxy_hex = sys_addr_hex.clone();

            let trace_req = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "debug_traceCallMany",
                "params": [[{
                    "transactions": [
                        { "from": &sys_addr_hex, "to": &ccm_hex, "data": load_data, "gas": "0x1c9c380" },
                        { "from": proxy_hex, "to": dest_hex, "data": call_data, "value": format!("0x{:x}", value), "gas": "0x2faf080" }
                    ]
                }], null, { "tracer": "callTracer" }],
                "id": 99962
            });

            if let Ok(resp) = client.post(l2_rpc_url).json(&trace_req).send().await {
                if let Ok(body) = resp.json::<Value>().await {
                    let traces = body.get("result").and_then(|r| r.get(0)).and_then(|b| b.as_array());
                    if let Some(arr) = traces {
                        if arr.len() >= 2 {
                            let call_trace = &arr[1];
                            let call_ok = call_trace.get("error").is_none();
                            let call_output = call_trace.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                            let output_bytes = hex::decode(call_output.strip_prefix("0x").unwrap_or(call_output)).unwrap_or_default();
                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                dest = %destination,
                                call_ok,
                                output_len = output_bytes.len(),
                                "direct L2 call retry with child entries loaded"
                            );
                            if call_ok && !output_bytes.is_empty() {
                                let (retry_children, _) = walk_l2_simulation_trace(
                                    client, l2_rpc_url, cross_chain_manager_address,
                                    call_trace, rollup_id, None,
                                ).await;
                                return (output_bytes, true, retry_children);
                            }
                        }
                    }
                }
            }
        }

        // Fallback: still try the CCM path for compatibility
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

            // Log the revert reason from the retry trace
            let retry_error = retry_trace.get("error").and_then(|v| v.as_str()).unwrap_or("none");
            let retry_output = retry_trace.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
            let error_name = decode_error_selector_prefixed(retry_output.get(..10));
            tracing::info!(
                target: "based_rollup::l1_proxy",
                dest = %destination,
                child_count = children.len(),
                retry_error,
                error_name,
                retry_output_len = retry_output.len(),
                placeholder_count = placeholders.len(),
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
        // Extract return data from the inner destination call.
        // Use _revert_data variant to also capture revert reason bytes when the
        // call failed (needed for RESULT(failed, data=revertData) terminal entries).
        let extracted = extract_inner_destination_revert_data(&trace, destination);
        let inner_data = extracted.unwrap_or_default();
        // Check if the destination call itself succeeded (no "error" in its trace node).
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

    // Build RESULT entry for Run 2.
    // When inner_success=true: use inner_return_data (real return data).
    // When inner_success=false: use EMPTY data for the Run 2 RESULT entry.
    //   The inner_return_data has revert bytes from Run 1, but Run 2 may succeed
    //   (e.g., flash-loan: child entries get loaded, call succeeds). Using revert
    //   bytes would produce a wrong hash. If Run 2 also fails (terminal revert),
    //   the final return path uses inner_return_data for the detected call.
    let run2_result_data = if inner_success {
        inner_return_data.clone()
    } else {
        vec![]
    };
    let result_action = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Result,
        rollup_id: U256::from(rollup_id),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: run2_result_data,
        failed: !inner_success,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
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
        }
    }

    // Run 2 failed — use the inner return data from Run 1
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
async fn simulate_l1_to_l2_call_chained_on_l2(
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
) -> (Vec<u8>, bool, Vec<super::common::DiscoveredProxyCall>) {
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
            )
            .await;
        }
    };

    let sys_addr_hex = format!("{sys_addr}");
    let ccm_hex = format!("{cross_chain_manager_address}");

    // Build the current call's exec calldata.
    let sim_action = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Call,
        rollup_id: U256::from(rollup_id),
        destination,
        value,
        data: data.to_vec(),
        failed: false,
        source_address,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let exec_calldata = crate::cross_chain::encode_execute_incoming_call_calldata(&sim_action);

    // Build a void RESULT entry for this call (placeholder — will cause _consumeExecution
    // to fail, but the inner destination call still executes).
    let void_result = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Result,
        rollup_id: U256::from(rollup_id),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
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
    let load_calldata = crate::cross_chain::encode_load_execution_table_calldata(&all_entries);
    let load_data = format!("0x{}", hex::encode(load_calldata.as_ref()));

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
            "data": format!("0x{}", hex::encode(cd)),
            "value": format!("0x{:x}", val),
            "gas": "0x2faf080"
        }));
    }

    // tx[N+1]: current executeIncomingCrossChainCall
    transactions.push(serde_json::json!({
        "from": sys_addr_hex,
        "to": ccm_hex,
        "data": format!("0x{}", hex::encode(exec_calldata.as_ref())),
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
            )
            .await;
        }
    };

    let body: serde_json::Value = match resp.json().await {
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
            )
            .await;
        }
    };

    // Extract the last trace (current call's trace).
    // result[0] = bundle traces array, result[0][last] = current call trace.
    let traces = match body
        .get("result")
        .and_then(|r| r.get(0))
        .and_then(|b| b.as_array())
    {
        Some(arr) => arr,
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
        )
        .await;
    }

    let current_trace = &traces[traces.len() - 1];

    // Scan prior traces for external createCrossChainProxy calls
    // (e.g., if user code explicitly creates proxies during delivery).
    let mut bundle_ephemeral_proxies: HashMap<Address, super::trace::ProxyInfo> = HashMap::new();
    for prior_trace in &traces[..traces.len() - 1] {
        super::trace::extract_ephemeral_proxies_from_trace(
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
                let calldata = super::common::encode_authorized_proxies_calldata(*addr);
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
                if let Ok(body2) = resp2.json::<serde_json::Value>().await {
                    if let Some(traces2) = body2
                        .get("result")
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
                            if let Ok(addr_bytes) = hex::decode(&output_clean[..64]) {
                                if addr_bytes.len() >= 32 {
                                    let original_address = Address::from_slice(&addr_bytes[12..32]);
                                    if original_address.is_zero() {
                                        continue;
                                    }
                                    // Second 32 bytes: originalRollupId (uint256, last 8 bytes as u64)
                                    if let Ok(rid_bytes) = hex::decode(&output_clean[64..128]) {
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
                                                super::trace::ProxyInfo {
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

// ──────────────────────────────────────────────────────────────────────────────
//  L1 ProxyLookup implementation for generic trace::walk_trace_tree
// ──────────────────────────────────────────────────────────────────────────────

/// Queries `authorizedProxies(address)` on Rollups.sol (L1) to resolve proxy
/// identity for the generic trace walker.
struct L1ProxyLookup<'a> {
    client: &'a reqwest::Client,
    l1_rpc_url: &'a str,
    rollups_address: Address,
}

impl super::trace::ProxyLookup for L1ProxyLookup<'_> {
    fn lookup_proxy(
        &self,
        address: Address,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<super::trace::ProxyInfo>> + Send + '_>,
    > {
        Box::pin(async move {
            // authorizedProxies(address) — query Rollups.sol for proxy identity.
            // Uses typed ABI encoding via sol! macro — NEVER hardcode selectors.
            // Uses eth_call_view (read-only view call, not tracing — appropriate per spec).
            let calldata = super::common::encode_authorized_proxies_calldata(address);

            let hex_data = eth_call_view(
                self.client,
                self.l1_rpc_url,
                self.rollups_address,
                &calldata,
            )
            .await?;

            // First 32 bytes = originalAddress
            let addr = parse_address_from_abi_return(&hex_data)?;

            // Second 32 bytes = originalRollupId (uint256, read last 8 bytes as u64)
            let hex_clean = hex_data.strip_prefix("0x").unwrap_or(&hex_data);
            if hex_clean.len() < 128 {
                return None;
            }
            let rid_bytes = hex::decode(&hex_clean[64..128]).ok()?;
            if rid_bytes.len() < 32 {
                return None;
            }
            let mut rid: u64 = 0;
            let start = rid_bytes.len().saturating_sub(8);
            for b in &rid_bytes[start..] {
                rid = (rid << 8) | (*b as u64);
            }

            Some(super::trace::ProxyInfo {
                original_address: addr,
                original_rollup_id: rid,
            })
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────
//  L2 ProxyLookup implementation for generic trace::walk_trace_tree
// ──────────────────────────────────────────────────────────────────────────────

/// Queries `authorizedProxies(address)` on the L2 CCM to resolve proxy
/// identity for the generic trace walker. Used when walking L2 simulation
/// traces to detect child L2→L1 proxy calls (nested L1→L2→L1 pattern).
struct L2ProxyLookup<'a> {
    client: &'a reqwest::Client,
    l2_rpc_url: &'a str,
    ccm_address: Address,
}

impl super::trace::ProxyLookup for L2ProxyLookup<'_> {
    fn lookup_proxy(
        &self,
        address: Address,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<super::trace::ProxyInfo>> + Send + '_>,
    > {
        Box::pin(async move {
            let result = detect_cross_chain_proxy_on_l2(
                self.client,
                self.l2_rpc_url,
                address,
                self.ccm_address,
            )
            .await;
            result.map(|(addr, rid)| super::trace::ProxyInfo {
                original_address: addr,
                original_rollup_id: rid,
            })
        })
    }
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
async fn walk_l2_simulation_trace(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    ccm_address: Address,
    trace_node: &Value,
    our_rollup_id: u64,
    pre_populated_ephemeral_proxies: Option<&HashMap<Address, super::trace::ProxyInfo>>,
) -> (
    Vec<super::common::DiscoveredProxyCall>,
    std::collections::HashSet<Address>,
) {
    let lookup = L2ProxyLookup {
        client,
        l2_rpc_url,
        ccm_address,
    };
    let mut proxy_cache: HashMap<Address, Option<super::trace::ProxyInfo>> = HashMap::new();
    let mut ephemeral_proxies = HashMap::new();

    // Pre-populate ephemeral proxies from prior bundle traces (cross-bundle visibility).
    if let Some(pre) = pre_populated_ephemeral_proxies {
        ephemeral_proxies.extend(pre.iter().map(|(k, v)| (*k, *v)));
    }

    let mut detected_calls = Vec::new();
    let mut unresolved_proxies = std::collections::HashSet::new();

    // The L2 CCM is the manager contract on L2.
    super::trace::walk_trace_tree(
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
                    Some(super::common::DiscoveredProxyCall {
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
/// results to `DetectedInternalCall` format.
///
/// This replaces the old L1-specific `walk_trace_tree` that had separate
/// paths for proxy detection and bridge detection. The generic walker uses
/// only the `executeCrossChainCall` child pattern — works for all contract
/// types (direct proxy, bridgeEther, bridgeTokens, wrappers, multi-call continuations).
async fn walk_l1_trace_generic(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    trace_node: &Value,
    proxy_cache: &mut HashMap<Address, Option<super::trace::ProxyInfo>>,
) -> Vec<DetectedInternalCall> {
    let lookup = L1ProxyLookup {
        client,
        l1_rpc_url,
        rollups_address,
    };
    let mut ephemeral_proxies = HashMap::new();
    let mut detected_calls = Vec::new();

    // Rollups.sol is the manager contract on L1.
    super::trace::walk_trace_tree(
        trace_node,
        &[rollups_address],
        &lookup,
        proxy_cache,
        &mut ephemeral_proxies,
        &mut detected_calls,
        &mut std::collections::HashSet::new(),
    )
    .await;

    // Convert trace::DetectedCall to DetectedInternalCall.
    detected_calls
        .into_iter()
        .map(|c| DetectedInternalCall {
            destination: c.destination,
            target_rollup_id: 0, // L1→L2: target resolved later from proxy identity
            calldata: c.calldata,
            value: c.value,
            source_address: c.source_address,
            call_success: true,
            return_data: vec![],
            parent_call_index: None, // root-level L1→L2 call
        })
        .collect()
}

/// Trace a transaction using `debug_traceCall` with `callTracer` and detect
/// all cross-chain proxy calls via the generic `trace::walk_trace_tree`.
///
/// Uses protocol-level detection only: a node is a proxy call if any of its
/// direct children call `executeCrossChainCall` on Rollups.sol. No contract-
/// specific selectors (bridgeEther, bridgeTokens, etc.) — works for any
/// contract that uses CrossChainProxy.
///
/// Returns `Ok(Some(tx_hash))` if cross-chain calls were found and queued.
/// Returns `Ok(None)` if no cross-chain calls were detected.
#[allow(clippy::too_many_arguments)]
async fn trace_and_detect_internal_calls(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    raw_tx: &str,
    tx_obj: &Value,
    rollups_address: Address,
    builder_private_key: Option<String>,
    rollup_id: u64,
    cross_chain_manager_address: Address,
) -> eyre::Result<Option<String>> {
    // Build the debug_traceCall request from decoded tx fields
    let from = tx_obj
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0000000000000000000000000000000000000000");
    let to = match tx_obj.get("to").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return Ok(None), // Contract creation — cannot contain cross-chain calls
    };
    let data = tx_obj.get("data").and_then(|v| v.as_str()).unwrap_or("0x");
    let value = tx_obj
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0");

    tracing::info!(
        target: "based_rollup::l1_proxy",
        %to, %from,
        "slow path: tracing tx with debug_traceCall to detect internal cross-chain calls"
    );

    // First trace: normal (no state overrides).
    // If this finds only 1 cross-chain call but the tx reverts internally,
    // we retry with state overrides (mock Rollups) to discover hidden calls
    // that would execute after entries are posted (multi-call continuation pattern).
    let trace_result = {
        let trace_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCall",
            "params": [
                {
                    "from": from,
                    "to": to,
                    "data": data,
                    "value": value,
                    "gas": "0x2faf080"
                },
                "latest",
                { "tracer": "callTracer" }
            ],
            "id": 1
        });

        let resp = client
            .post(l1_rpc_url)
            .json(&trace_req)
            .send()
            .await?
            .json::<Value>()
            .await?;

        if let Some(error) = resp.get("error") {
            tracing::debug!(
                target: "based_rollup::l1_proxy",
                ?error,
                "debug_traceCall failed — forwarding tx without cross-chain detection"
            );
            return Ok(None);
        }

        match resp.get("result").cloned() {
            Some(r) => r,
            None => return Ok(None),
        }
    };

    // Check if the top-level call reverted — indicates the tx needs entries posted first.
    let top_level_error = trace_result.get("error").is_some()
        || trace_result.get("revertReason").is_some()
        || trace_result
            .get("output")
            .and_then(|v| v.as_str())
            .map(|s| {
                s.starts_with(&super::common::selector_hex_prefixed(
                    &super::common::ERROR_STRING_SELECTOR,
                ))
            }) // Error(string) selector
            .unwrap_or(false);

    // Walk the trace tree using the generic trace::walk_trace_tree.
    // This detects ALL cross-chain proxy calls via the executeCrossChainCall
    // child pattern — no contract-specific selectors needed.
    let mut proxy_cache: HashMap<Address, Option<super::trace::ProxyInfo>> = HashMap::new();
    let mut detected_calls = walk_l1_trace_generic(
        client,
        l1_rpc_url,
        rollups_address,
        &trace_result,
        &mut proxy_cache,
    )
    .await;

    // Enrich detected calls with L2 return data by simulating each L1→L2 call
    // on L2. The RESULT action hash includes the exact return bytes from the
    // target contract (contracts/sync-rollups-protocol/docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md §C.2).
    //
    // CHAINED simulation: when multiple calls are detected (e.g., CallTwice calling
    // Counter.increment() twice), each call must see the state effects of previous
    // calls. We simulate sequentially: call[i] runs in a bundle where calls[0..i-1]
    // have already executed with their correct RESULT entries loaded.
    //
    // Also collect child L2→L1 proxy calls discovered in L2 simulation traces.
    // These represent the nested L1→L2→L1 pattern (the L2 target calls back to L1).
    let mut all_child_calls: Vec<(usize, DetectedInternalCall)> = Vec::new();
    if !cross_chain_manager_address.is_zero() {
        // Accumulate RESULT entries from already-enriched calls for chained simulation.
        let mut prior_result_entries: Vec<crate::cross_chain::CrossChainExecutionEntry> =
            Vec::new();
        // Also accumulate the executeIncomingCrossChainCall calldatas for prior calls
        // so they execute in the bundle (state must accumulate).
        let mut prior_exec_calldatas: Vec<(Vec<u8>, U256)> = Vec::new();

        // Query SYSTEM_ADDRESS once (needed for building exec calldatas).
        let sys_addr = {
            let sys_calldata = super::common::encode_system_address_calldata();
            let sys_result = super::common::eth_call_view(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                &sys_calldata,
            )
            .await;
            sys_result.and_then(|s| super::common::parse_address_from_abi_return(&s))
        };

        #[allow(clippy::needless_range_loop)]
        // Index needed: immutable reads then mutable writes on detected_calls
        for call_idx in 0..detected_calls.len() {
            // Clone the fields we need before any mutable borrow of detected_calls.
            let call_destination = detected_calls[call_idx].destination;
            let call_calldata = detected_calls[call_idx].calldata.clone();
            let call_value = detected_calls[call_idx].value;
            let call_source = detected_calls[call_idx].source_address;

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

            detected_calls[call_idx].return_data = final_ret_data.clone();
            detected_calls[call_idx].call_success = final_success;

            // Build RESULT entry and exec calldata for this call (for future chaining).
            // Uses final_ret_data/final_success after Bug 2 override so the RESULT hash
            // is correct for the corrected return data.
            let result_action = crate::cross_chain::CrossChainAction {
                action_type: crate::cross_chain::CrossChainActionType::Result,
                rollup_id: U256::from(rollup_id),
                destination: Address::ZERO,
                value: U256::ZERO,
                data: final_ret_data,
                failed: !final_success,
                source_address: Address::ZERO,
                source_rollup: U256::ZERO,
                scope: vec![],
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
                rollup_id: U256::from(rollup_id),
                destination: call_destination,
                value: call_value,
                data: call_calldata,
                failed: false,
                source_address: call_source,
                source_rollup: U256::ZERO,
                scope: vec![],
            };
            let exec_cd = crate::cross_chain::encode_execute_incoming_call_calldata(&sim_action);
            prior_exec_calldatas.push((exec_cd.to_vec(), call_value));

            // Convert child L2→L1 proxy calls to DetectedInternalCall and
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
                    DetectedInternalCall {
                        destination: child.original_address,
                        target_rollup_id: 0, // L2→L1: child targets L1 (mainnet)
                        calldata: child.data.clone(),
                        value: child.value,
                        source_address: child.source_address,
                        call_success: true, // defaults to true; will be enriched later if needed
                        return_data: vec![], // will be enriched via L1 simulation
                        parent_call_index: Some(call_idx), // linked to parent L1→L2 call
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
            "enriching child L2→L1 calls with L1 delivery simulation"
        );
        for (_parent_idx, child) in &mut all_child_calls {
            // Simulate the child's L1 delivery via debug_traceCallMany.
            // Matches real execution: _processCallAtScope computes the L1 proxy
            // for (sourceAddress, sourceRollup), then proxy.executeOnBehalf calls
            // destination.call{value}(data) — so msg.sender = proxy address.

            // Step 1: Compute L1 proxy address for this child's source.
            let proxy_from = super::l2_to_l1::compute_proxy_address_on_l1(
                client,
                l1_rpc_url,
                rollups_address,
                child.source_address,
                rollup_id,
            )
            .await
            .unwrap_or(Address::ZERO);

            // Step 2: Simulate delivery via debug_traceCallMany (from=proxy, to=destination).
            let trace_req = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "debug_traceCallMany",
                "params": [[{
                    "transactions": [{
                        "from": format!("{proxy_from}"),
                        "to": format!("{}", child.destination),
                        "data": format!("0x{}", hex::encode(&child.calldata)),
                        "value": format!("0x{:x}", child.value),
                        "gas": "0x2faf080"
                    }]
                }], null, { "tracer": "callTracer" }],
                "id": 99987
            });

            if let Ok(resp) = client.post(l1_rpc_url).json(&trace_req).send().await {
                if let Ok(body) = resp.json::<Value>().await {
                    // Extract trace: result[bundle_idx=0][tx_idx=0]
                    let trace = body
                        .get("result")
                        .and_then(|r| r.get(0))
                        .and_then(|b| b.as_array())
                        .and_then(|arr| arr.first());

                    if let Some(t) = trace {
                        let has_error =
                            t.get("error").is_some() || t.get("revertReason").is_some();
                        let output_hex =
                            t.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                        let output_bytes = hex::decode(
                            output_hex.strip_prefix("0x").unwrap_or(output_hex),
                        )
                        .unwrap_or_default();

                        if has_error {
                            // Check if the revert is ExecutionNotFound — this means
                            // the child's delivery calls deeper into the chain (reentrant
                            // pattern) and the deeper entries aren't loaded in the simulation.
                            // In real execution, all entries are loaded and the delivery succeeds.
                            // Treat ExecutionNotFound as void/success; propagate real errors.
                            let is_exec_not_found = output_bytes.len() >= 4
                                && output_bytes[..4]
                                    == super::common::EXECUTION_NOT_FOUND_SELECTOR;
                            if is_exec_not_found {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    dest = %child.destination,
                                    proxy = %proxy_from,
                                    "L1 delivery reverted with ExecutionNotFound — \
                                     treating as void/success (nested reentrant pattern)"
                                );
                                // Keep defaults: return_data=[], call_success=true
                            } else {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    dest = %child.destination,
                                    proxy = %proxy_from,
                                    revert_data_len = output_bytes.len(),
                                    "L1 delivery simulation reverted for child call"
                                );
                                child.return_data = output_bytes;
                                child.call_success = false;
                            }
                        } else if !output_bytes.is_empty() {
                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                dest = %child.destination,
                                proxy = %proxy_from,
                                return_data_len = output_bytes.len(),
                                "enriched L2→L1 child with L1 delivery return data"
                            );
                            child.return_data = output_bytes;
                        }
                    }
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
        if let Some(ref builder_key_hex) = builder_private_key {
            let key_hex = builder_key_hex
                .strip_prefix("0x")
                .unwrap_or(builder_key_hex);
            if let Ok(builder_key) = key_hex.parse::<alloy_signer_local::PrivateKeySigner>() {
                let mut all_calls = detected_calls.clone();
                let mut iteration = 0;
                const MAX_ITERATIONS: usize = 10;

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

                    // Build L1DetectedCall entries from known calls
                    let l1_detected: Vec<crate::table_builder::L1DetectedCall> = all_calls
                        .iter()
                        .map(|c| crate::table_builder::L1DetectedCall {
                            destination: c.destination,
                            data: c.calldata.clone(),
                            value: c.value,
                            source_address: c.source_address,
                            l2_return_data: c.return_data.clone(),
                            call_success: c.call_success,
                            parent_call_index: c.parent_call_index,
                            target_rollup_id: if c.parent_call_index.is_some()
                                && c.target_rollup_id == 0
                            {
                                Some(0)
                            } else {
                                None
                            },
                        })
                        .collect();

                    let analyzed =
                        crate::table_builder::analyze_continuation_calls(&l1_detected, rollup_id);

                    // Log the call tree for debugging
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        "╔══ Iterative Discovery — Call Tree (iteration {}) ══",
                        iteration
                    );
                    for (i, c) in all_calls.iter().enumerate() {
                        let sel = if c.calldata.len() >= 4 {
                            format!("0x{}", hex::encode(&c.calldata[..4]))
                        } else {
                            "0x".to_string()
                        };
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            "║ CALL[{}]: dest={} src={} sel={} data_len={} value={}",
                            i, c.destination, c.source_address, sel, c.calldata.len(), c.value
                        );
                    }
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        "╚══════════════════════════════════════════════════"
                    );

                    // Build DISCOVERY entries for traceCallMany. These entries
                    // are posted via postBatch so Rollups.sol can consume them,
                    // enabling the user tx to execute deeper and reveal new calls.
                    //
                    // For L1→L2 calls WITH children (nested pattern):
                    //   hash(CALL(L2)) → CALL(L1, child, scope=[0])
                    //   hash(RESULT(L1, void)) → RESULT(L2, void)
                    // For L1→L2 calls WITHOUT children (terminal):
                    //   hash(CALL(L2)) → RESULT(L2, void)
                    //
                    // These are simpler than final entries — no return data,
                    // no ether deltas. Just enough for scope navigation to work.
                    let our_rollup = alloy_primitives::U256::from(rollup_id);
                    let mainnet = alloy_primitives::U256::ZERO;
                    let result_l2_void = crate::cross_chain::CrossChainAction {
                        action_type: crate::cross_chain::CrossChainActionType::Result,
                        rollup_id: our_rollup,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: vec![],
                        failed: false,
                        source_address: Address::ZERO,
                        source_rollup: U256::ZERO,
                        scope: vec![],
                    };
                    let result_l1_void = crate::cross_chain::CrossChainAction {
                        action_type: crate::cross_chain::CrossChainActionType::Result,
                        rollup_id: mainnet,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: vec![],
                        failed: false,
                        source_address: Address::ZERO,
                        source_rollup: U256::ZERO,
                        scope: vec![],
                    };
                    let mut entries = Vec::new();
                    for (i, call) in all_calls.iter().enumerate() {
                        if call.parent_call_index.is_some() {
                            continue; // L2→L1 children don't need their own L1 entries
                        }
                        // Build CALL action for this L1→L2 call
                        let call_action = crate::cross_chain::CrossChainAction {
                            action_type: crate::cross_chain::CrossChainActionType::Call,
                            rollup_id: our_rollup,
                            destination: call.destination,
                            value: call.value,
                            data: call.calldata.clone(),
                            failed: false,
                            source_address: call.source_address,
                            source_rollup: mainnet,
                            scope: vec![],
                        };
                        let call_hash = crate::table_builder::compute_action_hash(&call_action);

                        // Find child L2→L1 call for this L1→L2 call
                        let child = all_calls.iter().find(|c| c.parent_call_index == Some(i));

                        if let Some(child_call) = child {
                            // Has child: CALL → CALL(child, scope=[0])
                            let child_action = crate::cross_chain::CrossChainAction {
                                action_type: crate::cross_chain::CrossChainActionType::Call,
                                rollup_id: mainnet,
                                destination: child_call.destination,
                                value: child_call.value,
                                data: child_call.calldata.clone(),
                                failed: false,
                                source_address: child_call.source_address,
                                source_rollup: our_rollup,
                                scope: vec![U256::ZERO],
                            };
                            entries.push(crate::cross_chain::CrossChainExecutionEntry {
                                state_deltas: vec![],
                                action_hash: call_hash,
                                next_action: child_action,
                            });
                            // No resolution entry: the RESULT hash depends on the delivery
                            // return data which isn't known yet. The scope navigation will
                            // execute the child and then fail at _consumeExecution(RESULT)
                            // with ExecutionNotFound — but by then the trace contains the
                            // delivery return value which we can extract later.
                        } else {
                            // No child: CALL → RESULT(L2, void)
                            entries.push(crate::cross_chain::CrossChainExecutionEntry {
                                state_deltas: vec![],
                                action_hash: call_hash,
                                next_action: result_l2_void.clone(),
                            });
                        }
                    }
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        entry_count = entries.len(),
                        l1_to_l2_calls = all_calls.iter().filter(|c| c.parent_call_index.is_none()).count(),
                        "built discovery entries for traceCallMany"
                    );

                    // Clear state deltas for simulation-only entries.
                    // build_continuation_entries produces entries with placeholder
                    // currentState=0x0 / newState=0x0 (to be filled by the driver
                    // with real intermediate roots before the actual postBatch).
                    // In the traceCallMany discovery loop, these entries are used
                    // directly — Rollups.sol._findAndApplyExecution checks
                    // `rollups[delta.rollupId].stateRoot != delta.currentState`
                    // and rejects entries whose currentState doesn't match the
                    // on-chain state. With placeholder 0x0, the check ALWAYS fails
                    // and ExecutionNotFound is returned, preventing the user tx
                    // from succeeding and hiding subsequent cross-chain calls.
                    //
                    // Fix: clear state_deltas so _findAndApplyExecution's
                    // allMatch stays true (no deltas to check → unconditional match).
                    // This is safe because the simulation only cares about
                    // entry consumption (actionHash matching), not state transitions.
                    let mut entries = entries;
                    for e in &mut entries {
                        e.state_deltas.clear();
                    }

                    // Log entry details
                    for (i, e) in entries.iter().enumerate() {
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            "  entry[{}]: action_hash={} next_action_type={:?} deltas={}",
                            i, e.action_hash, e.next_action.action_type, e.state_deltas.len()
                        );
                    }

                    if entries.is_empty() {
                        break;
                    }

                    // Get L1 block context for proof signing.
                    // traceCallMany runs at "latest" block context, so inside the EVM:
                    //   block.number = latest_number
                    //   blockhash(block.number - 1) = parent_hash of latest
                    let block_ctx = get_l1_block_context(client, l1_rpc_url).await;
                    let (block_number, _block_hash, _parent_hash) = match block_ctx {
                        Ok(ctx) => ctx,
                        Err(e) => {
                            tracing::warn!(target: "based_rollup::l1_proxy", %e, "failed to get L1 block context");
                            break;
                        }
                    };

                    // Get verification key from Rollups contract
                    let vk = match get_verification_key(
                        client,
                        l1_rpc_url,
                        rollups_address,
                        rollup_id,
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(target: "based_rollup::l1_proxy", %e, "failed to get verification key");
                            break;
                        }
                    };

                    // Sign ECDSA proof for postBatch in traceCallMany context.
                    // We use blockOverride { number: block_number + 1 } to avoid
                    // StateAlreadyUpdatedThisBlock. Inside the EVM:
                    //   block.number = block_number + 1
                    //   blockhash(block.number - 1) = hash of latest block = _block_hash
                    //   block.timestamp = predicted current time
                    let trace_block_number = block_number + 1;
                    let trace_parent_hash = _block_hash; // hash of latest = parent of simulated block
                    // For traceCallMany simulation, we control the block timestamp via blockOverride.
                    // Use current time — the override ensures consistency between signed proof and simulation.
                    let trace_block_timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        trace_block_number,
                        %trace_parent_hash,
                        entry_count = entries.len(),
                        "signing proof for traceCallMany postBatch"
                    );

                    let call_data_bytes = alloy_primitives::Bytes::new();
                    let entry_hashes = crate::cross_chain::compute_entry_hashes(&entries, vk);
                    let public_inputs_hash = crate::cross_chain::compute_public_inputs_hash(
                        &entry_hashes,
                        &call_data_bytes,
                        trace_parent_hash,
                        trace_block_timestamp,
                    );

                    use alloy_signer::SignerSync;
                    let sig = match builder_key.sign_hash_sync(&public_inputs_hash) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(target: "based_rollup::l1_proxy", %e, "failed to sign proof");
                            break;
                        }
                    };
                    let sig_bytes = sig.as_bytes();
                    let mut proof_bytes = sig_bytes.to_vec();
                    if proof_bytes.len() == 65 && proof_bytes[64] < 27 {
                        proof_bytes[64] += 27;
                    }
                    let proof = alloy_primitives::Bytes::from(proof_bytes);

                    // Encode postBatch calldata
                    let post_batch_calldata = crate::cross_chain::encode_post_batch_calldata(
                        &entries,
                        call_data_bytes,
                        proof,
                    );

                    // Build traceCallMany request: [postBatch, userTx] in a single bundle
                    let builder_addr = format!("{}", builder_key.address());
                    let rollups_hex = format!("{rollups_address}");
                    let post_batch_data =
                        format!("0x{}", hex::encode(post_batch_calldata.as_ref()));

                    // reth's debug_traceCallMany format:
                    // params: [bundles, stateContext?, tracingOptions?]
                    // bundle: { transactions: [tx1, tx2, ...], blockOverride?: {...} }
                    // Both txs in ONE bundle so state from tx1 (postBatch) is visible to tx2.
                    //
                    // blockOverride with number = block_number + 1 is critical:
                    // Rollups.postBatch reverts with StateAlreadyUpdatedThisBlock if
                    // lastStateUpdateBlock == block.number. Since the builder may have
                    // already submitted in the current block, we simulate at block+1.
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
                                            "from": from,
                                            "to": to,
                                            "data": data,
                                            "value": value,
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

                    let resp = match client.post(l1_rpc_url).json(&trace_req).send().await {
                        Ok(r) => match r.json::<Value>().await {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(target: "based_rollup::l1_proxy", %e, "traceCallMany response parse failed");
                                break;
                            }
                        },
                        Err(e) => {
                            tracing::warn!(target: "based_rollup::l1_proxy", %e, "traceCallMany request failed");
                            break;
                        }
                    };

                    // Extract traces from result.
                    // debug_traceCallMany returns Vec<Vec<GethTrace>>:
                    //   result[bundle_idx][tx_idx]
                    // We have 1 bundle with 2 txs: result[0][0]=postBatch, result[0][1]=userTx
                    let bundle_traces = match resp
                        .get("result")
                        .and_then(|r| r.get(0))
                        .and_then(|b| b.as_array())
                    {
                        Some(arr) if arr.len() >= 2 => arr,
                        _ => {
                            if let Some(error) = resp.get("error") {
                                tracing::warn!(
                                    target: "based_rollup::l1_proxy",
                                    ?error,
                                    "traceCallMany returned error"
                                );
                            } else {
                                tracing::warn!(
                                    target: "based_rollup::l1_proxy",
                                    "traceCallMany returned unexpected structure"
                                );
                            }
                            break;
                        }
                    };

                    // Check if postBatch succeeded (tx1 trace)
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
                            "postBatch reverted in traceCallMany — entries may be invalid"
                        );
                        // Still try to walk the user tx trace
                    }

                    let user_trace = &bundle_traces[1];

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
                    tracing::debug!(
                        target: "based_rollup::l1_proxy",
                        iteration,
                        postbatch_ok = tx1_trace.get("error").is_none(),
                        user_ok = user_error == "none",
                        %decoded_error,
                        %inner_error,
                        user_calls_count,
                        "traceCallMany iteration result"
                    );

                    // Walk the user tx trace for new cross-chain calls
                    // using the generic trace walker.
                    let new_detected = walk_l1_trace_generic(
                        client,
                        l1_rpc_url,
                        rollups_address,
                        user_trace,
                        &mut proxy_cache,
                    )
                    .await;

                    tracing::debug!(
                        target: "based_rollup::l1_proxy",
                        new_detected_count = new_detected.len(),
                        all_calls_count = all_calls.len(),
                        "walked user tx trace for cross-chain calls"
                    );

                    // Find truly new calls using count-based comparison.
                    // A call is "new" only if new_detected has MORE of that
                    // (dest, calldata, value, source_address) tuple than all_calls —
                    // supports legitimate duplicate calls (e.g., CallTwice calling
                    // increment() twice). The CALL action hash includes value and
                    // sourceAddress, so two calls to the same proxy with different
                    // ETH values or from different sources are distinct.
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

                        // Post-convergence enrichment: re-simulate to capture return data.
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            total = all_calls.len(),
                            children = all_calls.iter().filter(|c| c.parent_call_index.is_some()).count(),
                            parents_void = all_calls.iter().filter(|c| c.parent_call_index.is_none() && c.return_data.is_empty()).count(),
                            "post-convergence enrichment starting"
                        );
                        for call in &mut all_calls {
                            if call.parent_call_index.is_none() {
                                continue; // Only enrich L2→L1 children
                            }
                            if !call.return_data.is_empty() && call.call_success {
                                continue; // Already has valid return data
                            }
                            // Find the delivery output from the REVERTED scope navigation
                            // in the converged L1 trace. The scope executes the delivery
                            // (deepCall runs on L1) but then fails at _consumeExecution
                            // for the RESULT. The delivery output is still in the trace
                            // even though the scope reverted. Use _revert_data variant
                            // which includes output from reverted calls.
                            let delivery_data =
                                extract_inner_destination_revert_data(user_trace, call.destination);
                            if let Some(data) = delivery_data {
                                if !data.is_empty() {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        dest = %call.destination,
                                        data_len = data.len(),
                                        "post-convergence: extracted delivery return data from converged trace"
                                    );
                                    call.return_data = data;
                                    call.call_success = true;
                                }
                            }
                        }

                        // Re-enrich L1→L2 parent calls that have void l2_return_data
                        // (because their initial L2 sim failed due to missing child entries).
                        // Build L2 entries including CHILD entries so the sim can execute
                        // through scope navigation (executeCrossChainCall on L2 needs the
                        // child CALL→RESULT entry loaded).
                        if !cross_chain_manager_address.is_zero() {
                            let sys_cd = super::common::encode_system_address_calldata();
                            let sys_res = super::common::eth_call_view(
                                client, l2_rpc_url, cross_chain_manager_address, &sys_cd,
                            ).await;
                            let sys = sys_res.and_then(|s| super::common::parse_address_from_abi_return(&s));
                            let sys_hex = sys.map(|a| format!("{a}")).unwrap_or_default();
                            let ccm_hex = format!("{cross_chain_manager_address}");
                            let our_rollup = U256::from(rollup_id);

                            // Build entry set: RESULT entries for L1→L2 calls + CHILD entries
                            let mut sim_entries = Vec::new();
                            let mut exec_calldatas: Vec<(Vec<u8>, U256)> = Vec::new();
                            for (ci, c) in all_calls.iter().enumerate() {
                                if c.parent_call_index.is_some() { continue; }
                                // RESULT entry for this call
                                let ra = crate::cross_chain::CrossChainAction {
                                    action_type: crate::cross_chain::CrossChainActionType::Result,
                                    rollup_id: our_rollup,
                                    destination: Address::ZERO, value: U256::ZERO,
                                    data: c.return_data.clone(),
                                    failed: !c.call_success,
                                    source_address: Address::ZERO, source_rollup: U256::ZERO,
                                    scope: vec![],
                                };
                                let rh = crate::table_builder::compute_action_hash(&ra);
                                sim_entries.push(crate::cross_chain::CrossChainExecutionEntry {
                                    state_deltas: vec![], action_hash: rh, next_action: ra,
                                });
                                // CHILD entries: same format as the initial L2 sim placeholders.
                                // build_l2_to_l1_call_entries.l2_table_entries produces entries
                                // that the L2 CCM can consume via executeCrossChainCall.
                                for child in all_calls.iter() {
                                    if child.parent_call_index != Some(ci) { continue; }
                                    // Use 32-byte zero placeholder if return_data is empty
                                    // (avoids ABI decode failure for uint256 returns)
                                    let ret_data = if child.return_data.is_empty() {
                                        vec![0u8; 32]
                                    } else {
                                        child.return_data.clone()
                                    };
                                    let placeholder = crate::cross_chain::build_l2_to_l1_call_entries(
                                        child.destination,
                                        child.calldata.clone(),
                                        child.value,
                                        child.source_address,
                                        rollup_id,
                                        vec![0xc0], // placeholder rlp
                                        ret_data,
                                        !child.call_success,
                                    );
                                    sim_entries.extend(placeholder.l2_table_entries);
                                }
                                // Exec calldata for chained sim
                                let sa = crate::cross_chain::CrossChainAction {
                                    action_type: crate::cross_chain::CrossChainActionType::Call,
                                    rollup_id: our_rollup,
                                    destination: c.destination, value: c.value,
                                    data: c.calldata.clone(), failed: false,
                                    source_address: c.source_address,
                                    source_rollup: U256::ZERO, scope: vec![],
                                };
                                let ec = crate::cross_chain::encode_execute_incoming_call_calldata(&sa);
                                exec_calldatas.push((ec.to_vec(), c.value));
                            }

                            // Re-simulate ALL L1→L2 parents via one chained bundle.
                            // Use DIRECT calls (not executeIncomingCrossChainCall) to avoid
                            // the circular RESULT hash dependency. Direct calls bypass the
                            // CCM's _consumeExecution for the RESULT, while still using
                            // child entries loaded via loadExecutionTable.
                            if sys.is_some() {
                                let load_cd = crate::cross_chain::encode_load_execution_table_calldata(&sim_entries);
                                let load_data = format!("0x{}", hex::encode(load_cd.as_ref()));
                                let mut txs = vec![serde_json::json!({
                                    "from": &sys_hex, "to": &ccm_hex,
                                    "data": load_data, "gas": "0x1c9c380"
                                })];
                                // Direct calls: from=SYSTEM to=destination, data=calldata
                                for (ci, c) in all_calls.iter().enumerate() {
                                    if c.parent_call_index.is_some() { continue; }
                                    let dest_hex = format!("{}", c.destination);
                                    let cd_hex = format!("0x{}", hex::encode(&c.calldata));
                                    txs.push(serde_json::json!({
                                        "from": &sys_hex, "to": dest_hex,
                                        "data": cd_hex,
                                        "value": format!("0x{:x}", c.value),
                                        "gas": "0x2faf080"
                                    }));
                                }
                                let trace_req = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "debug_traceCallMany",
                                    "params": [[{"transactions": txs}], null, {"tracer": "callTracer"}],
                                    "id": 99950
                                });
                                if let Ok(resp) = client.post(l2_rpc_url).json(&trace_req).send().await {
                                    if let Ok(body) = resp.json::<Value>().await {
                                        let has_error = body.get("error").is_some();
                                        let traces = body.get("result")
                                            .and_then(|r| r.get(0))
                                            .and_then(|b| b.as_array());
                                        let trace_count = traces.as_ref().map(|a| a.len()).unwrap_or(0);
                                        tracing::info!(
                                            target: "based_rollup::l1_proxy",
                                            has_error, trace_count,
                                            sim_entries = sim_entries.len(),
                                            exec_calls = exec_calldatas.len(),
                                            "post-convergence: chained L2 sim response"
                                        );
                                        if let Some(arr) = traces {
                                            // Remove the debug trace status logs to reduce noise
                                            for (ti, trace) in arr.iter().enumerate() {
                                                let ok = trace.get("error").is_none();
                                                let out_len = trace.get("output").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0);
                                                tracing::info!(
                                                    target: "based_rollup::l1_proxy",
                                                    ti, ok, out_len,
                                                    "post-convergence: trace[{}] status", ti
                                                );
                                            }
                                            // Skip tx[0] (loadExecutionTable). tx[1..N] = exec calls.
                                            let mut l1_to_l2_idx = 0;
                                            for (ti, trace) in arr.iter().enumerate().skip(1) {
                                                // Find the corresponding L1→L2 call
                                                while l1_to_l2_idx < all_calls.len()
                                                    && all_calls[l1_to_l2_idx].parent_call_index.is_some()
                                                {
                                                    l1_to_l2_idx += 1;
                                                }
                                                if l1_to_l2_idx >= all_calls.len() { break; }
                                                let dest = all_calls[l1_to_l2_idx].destination;
                                                let success = trace.get("error").is_none();
                                                if success {
                                                    // Direct call: output is the raw return data
                                                    let output_hex = trace.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                                                    let output_clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
                                                    let data = hex::decode(output_clean).unwrap_or_default();
                                                    if !data.is_empty() && all_calls[l1_to_l2_idx].return_data.is_empty() {
                                                        tracing::info!(
                                                            target: "based_rollup::l1_proxy",
                                                            dest = %dest,
                                                            data_len = data.len(),
                                                            trace_idx = ti,
                                                            "post-convergence: enriched L2 return data via direct chained sim"
                                                        );
                                                        all_calls[l1_to_l2_idx].return_data = data;
                                                        all_calls[l1_to_l2_idx].call_success = true;
                                                    }
                                                }
                                                l1_to_l2_idx += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }

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
                    let mut iter_child_calls: Vec<(usize, DetectedInternalCall)> = Vec::new();
                    if !cross_chain_manager_address.is_zero() {
                        // Build RESULT entries and exec calldatas from ALL existing
                        // calls (already enriched) for chained simulation.
                        let sys_addr = {
                            let sys_calldata = super::common::encode_system_address_calldata();
                            let sys_result = super::common::eth_call_view(
                                client,
                                l2_rpc_url,
                                cross_chain_manager_address,
                                &sys_calldata,
                            )
                            .await;
                            sys_result
                                .and_then(|s| super::common::parse_address_from_abi_return(&s))
                        };
                        let mut prior_result_entries: Vec<
                            crate::cross_chain::CrossChainExecutionEntry,
                        > = Vec::new();
                        let mut prior_exec_calldatas: Vec<(Vec<u8>, U256)> = Vec::new();

                        // Accumulate prior entries from all_calls (already enriched).
                        for prior in all_calls.iter() {
                            // Only L1→L2 calls contribute to L2 state chaining.
                            // L2→L1 children (target_rollup_id=0) don't execute on L2.
                            if prior.parent_call_index.is_some() {
                                continue;
                            }
                            let result_action = crate::cross_chain::CrossChainAction {
                                action_type: crate::cross_chain::CrossChainActionType::Result,
                                rollup_id: U256::from(rollup_id),
                                destination: Address::ZERO,
                                value: U256::ZERO,
                                data: prior.return_data.clone(),
                                failed: !prior.call_success,
                                source_address: Address::ZERO,
                                source_rollup: U256::ZERO,
                                scope: vec![],
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
                                rollup_id: U256::from(rollup_id),
                                destination: prior.destination,
                                value: prior.value,
                                data: prior.calldata.clone(),
                                failed: false,
                                source_address: prior.source_address,
                                source_rollup: U256::ZERO,
                                scope: vec![],
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

                            call.return_data = final_ret_data.clone();
                            call.call_success = final_success;

                            // Accumulate this call's RESULT for future chaining.
                            let result_action = crate::cross_chain::CrossChainAction {
                                action_type: crate::cross_chain::CrossChainActionType::Result,
                                rollup_id: U256::from(rollup_id),
                                destination: Address::ZERO,
                                value: U256::ZERO,
                                data: final_ret_data,
                                failed: !final_success,
                                source_address: Address::ZERO,
                                source_rollup: U256::ZERO,
                                scope: vec![],
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
                                rollup_id: U256::from(rollup_id),
                                destination: call.destination,
                                value: call.value,
                                data: call.calldata.clone(),
                                failed: false,
                                source_address: call.source_address,
                                source_rollup: U256::ZERO,
                                scope: vec![],
                            };
                            let exec_cd = crate::cross_chain::encode_execute_incoming_call_calldata(
                                &sim_action,
                            );
                            prior_exec_calldatas.push((exec_cd.to_vec(), call.value));

                            // Convert child L2→L1 proxy calls to
                            // DetectedInternalCall with parent linkage.
                            // The parent_call_index will be set after extending
                            // all_calls (when we know the final index).
                            for child in &child_calls {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    parent_dest = %call.destination,
                                    child_dest = %child.original_address,
                                    child_source = %child.source_address,
                                    child_data_len = child.data.len(),
                                    child_value = %child.value,
                                    "discovered child L2→L1 call in iterative enrichment"
                                );
                                // Use a placeholder parent index (0) — we'll fix it
                                // below once we know where this call lands in all_calls.
                                iter_child_calls.push((
                                    0, // placeholder — updated below
                                    DetectedInternalCall {
                                        destination: child.original_address,
                                        target_rollup_id: 0, // L2→L1: child targets L1
                                        calldata: child.data.clone(),
                                        value: child.value,
                                        source_address: child.source_address,
                                        call_success: true,
                                        return_data: vec![],
                                        parent_call_index: None, // set below
                                    },
                                ));
                            }
                        }
                    }

                    // Enrich child L2→L1 calls with L1 delivery return data.
                    // Children discovered during L2 simulation have empty return_data.
                    // Simulate their delivery on L1 in a chained bundle that includes
                    // ALL prior L2→L1 children (from previous iterations) as preceding
                    // transactions. This ensures state accumulates across iterations:
                    // e.g., 2 Counter.increment() children from different iterations
                    // return (1, 2) not (1, 1).
                    if !iter_child_calls.is_empty() {
                        // Collect prior L2→L1 children from all_calls for state
                        // accumulation. These were enriched in previous iterations
                        // and their delivery must execute first in the bundle.
                        let prior_children: Vec<&DetectedInternalCall> = all_calls
                            .iter()
                            .filter(|c| c.parent_call_index.is_some() && c.target_rollup_id == 0)
                            .collect();

                        // Group ALL children (prior + new) by source_address.
                        // Each group shares an L1 proxy and needs one chained bundle.
                        let mut children_by_source: std::collections::HashMap<
                            Address,
                            (Vec<&DetectedInternalCall>, Vec<usize>),
                        > = std::collections::HashMap::new();
                        for prior in &prior_children {
                            children_by_source
                                .entry(prior.source_address)
                                .or_insert_with(|| (Vec::new(), Vec::new()))
                                .0
                                .push(prior);
                        }
                        for (idx, (_, child)) in iter_child_calls.iter().enumerate() {
                            children_by_source
                                .entry(child.source_address)
                                .or_insert_with(|| (Vec::new(), Vec::new()))
                                .1
                                .push(idx);
                        }

                        for (source_addr, (prior, new_indices)) in &children_by_source {
                            if new_indices.is_empty() {
                                continue;
                            }

                            // Compute L1 proxy address for this source.
                            let proxy_from = match super::l2_to_l1::compute_proxy_address_on_l1(
                                client,
                                l1_rpc_url,
                                rollups_address,
                                *source_addr,
                                rollup_id,
                            )
                            .await
                            {
                                Ok(addr) => addr,
                                Err(e) => {
                                    tracing::warn!(
                                        target: "based_rollup::l1_proxy",
                                        %e,
                                        source = %source_addr,
                                        "failed to compute L1 proxy for child enrichment — children keep empty return_data"
                                    );
                                    continue;
                                }
                            };

                            let block_ctx = get_l1_block_context(client, l1_rpc_url).await;
                            let (trace_block_num, trace_block_ts) = match block_ctx {
                                Ok((bn, _bh, _ph)) => {
                                    let ts = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs();
                                    (bn + 1, ts)
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        target: "based_rollup::l1_proxy",
                                        %e,
                                        "failed to get L1 block context for child enrichment"
                                    );
                                    continue;
                                }
                            };

                            // Build chained bundle: prior children first (for state
                            // accumulation), then new children.
                            let prior_count = prior.len();
                            let mut transactions: Vec<Value> = Vec::new();
                            for p in prior {
                                transactions.push(serde_json::json!({
                                    "from": format!("{proxy_from}"),
                                    "to": format!("{}", p.destination),
                                    "data": format!("0x{}", hex::encode(&p.calldata)),
                                    "value": format!("0x{:x}", p.value),
                                    "gas": "0x2faf080"
                                }));
                            }
                            for &idx in new_indices {
                                let (_, child) = &iter_child_calls[idx];
                                transactions.push(serde_json::json!({
                                    "from": format!("{proxy_from}"),
                                    "to": format!("{}", child.destination),
                                    "data": format!("0x{}", hex::encode(&child.calldata)),
                                    "value": format!("0x{:x}", child.value),
                                    "gas": "0x2faf080"
                                }));
                            }

                            let trace_req = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "debug_traceCallMany",
                                "params": [
                                    [{
                                        "transactions": transactions,
                                        "blockOverride": {
                                            "number": format!("{:#x}", trace_block_num),
                                            "time": format!("{:#x}", trace_block_ts)
                                        }
                                    }],
                                    null,
                                    { "tracer": "callTracer" }
                                ],
                                "id": 99935
                            });

                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                num_new = new_indices.len(),
                                num_prior = prior_count,
                                proxy = %proxy_from,
                                source = %source_addr,
                                "enriching child L2→L1 calls via chained L1 delivery simulation"
                            );

                            let resp = match client
                                .post(l1_rpc_url)
                                .json(&trace_req)
                                .send()
                                .await
                            {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::warn!(
                                        target: "based_rollup::l1_proxy",
                                        %e,
                                        "child L1 enrichment traceCallMany failed"
                                    );
                                    continue;
                                }
                            };
                            let body: Value = match resp.json().await {
                                Ok(b) => b,
                                Err(e) => {
                                    tracing::warn!(
                                        target: "based_rollup::l1_proxy",
                                        %e,
                                        "child L1 enrichment response parse failed"
                                    );
                                    continue;
                                }
                            };

                            // Parse results: result[0] is array of (prior + new) traces.
                            let total_expected = prior_count + new_indices.len();
                            let traces = match body
                                .get("result")
                                .and_then(|r| r.get(0))
                                .and_then(|b| b.as_array())
                            {
                                Some(arr) if arr.len() == total_expected => arr,
                                _ => {
                                    tracing::warn!(
                                        target: "based_rollup::l1_proxy",
                                        expected = total_expected,
                                        "child L1 enrichment: unexpected trace count"
                                    );
                                    continue;
                                }
                            };

                            // Skip prior traces (indices 0..prior_count), extract
                            // return data only for new children.
                            for (new_offset, &child_idx) in new_indices.iter().enumerate() {
                                let trace_idx = prior_count + new_offset;
                                let trace = &traces[trace_idx];
                                let has_error = trace.get("error").is_some()
                                    || trace.get("revertReason").is_some();
                                let output_hex =
                                    trace.get("output").and_then(|v| v.as_str()).unwrap_or("0x");
                                let hex_clean =
                                    output_hex.strip_prefix("0x").unwrap_or(output_hex);
                                let output_bytes = hex::decode(hex_clean).unwrap_or_default();

                                let (_, child) = &mut iter_child_calls[child_idx];
                                if has_error {
                                    // For multi-call continuations (flash-loan pattern), child
                                    // delivery simulation fails because it runs in isolation
                                    // without state effects from earlier calls. In real execution
                                    // the delivery succeeds. Treat as void/success.
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        child_idx,
                                        revert_data_len = output_bytes.len(),
                                        dest = %child.destination,
                                        "child delivery reverted — treating as void/success"
                                    );
                                } else {
                                    child.return_data = output_bytes.clone();
                                    child.call_success = true;
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        child_idx,
                                        return_data_len = output_bytes.len(),
                                        dest = %child.destination,
                                        prior_count,
                                        "enriched child L2→L1 call with L1 delivery return data"
                                    );
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
                                if child.parent_call_index.is_none() {
                                    child.parent_call_index = Some(parent_idx);
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
                    detected_calls = all_calls;
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

    tracing::info!(
        target: "based_rollup::composer_rpc::l1_to_l2",
        count = detected_calls.len(),
        "detected internal cross-chain calls — routing to buildExecutionTable"
    );

    let effective_gas_price = extract_gas_price_from_raw_tx(raw_tx).unwrap_or(0);

    queue_execution_table(
        client,
        l2_rpc_url,
        raw_tx,
        &detected_calls,
        effective_gas_price,
    )
    .await
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
    let calldata = super::common::encode_authorized_proxies_calldata(address);
    let hex_data = match eth_call_view(client, l1_rpc_url, rollups_address, &calldata).await {
        Some(hex) => hex,
        None => return false,
    };
    parse_address_from_abi_return(&hex_data).is_some()
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
async fn handle_estimate_gas_for_proxy(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    params: Option<&Vec<Value>>,
    rollups_address: Address,
    json: &Value,
) -> Option<Response<Full<HyperBytes>>> {
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

    Some(cors_response(
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Full::new(HyperBytes::from(response_body.to_string())))
            .expect("valid response"),
    ))
}

/// Decode a raw signed transaction into a JSON object suitable for tracing.
fn decode_raw_tx_for_trace(raw_tx: &str) -> eyre::Result<Value> {
    let raw_hex = raw_tx.strip_prefix("0x").unwrap_or(raw_tx);
    let raw_bytes =
        hex_decode(raw_hex).ok_or_else(|| eyre::eyre!("invalid hex in raw transaction"))?;

    use alloy_consensus::Transaction;
    use alloy_consensus::transaction::TxEnvelope;
    use alloy_rlp::Decodable;
    use reth_primitives_traits::SignerRecoverable;

    let tx_envelope = TxEnvelope::decode(&mut raw_bytes.as_slice())
        .map_err(|e| eyre::eyre!("failed to decode transaction: {e}"))?;

    let from = tx_envelope
        .recover_signer()
        .map_err(|e| eyre::eyre!("failed to recover signer: {e}"))?;

    let to = tx_envelope.to();
    let value = tx_envelope.value();
    let input = tx_envelope.input();
    let gas = tx_envelope.gas_limit();

    let mut obj = serde_json::json!({
        "from": format!("{from}"),
        "value": format!("{value:#x}"),
        "data": format!("0x{}", hex::encode(input)),
        "gas": format!("{gas:#x}")
    });

    if let Some(to_addr) = to {
        obj["to"] = Value::String(format!("{to_addr}"));
    }

    Ok(obj)
}

// eth_call_view is in super::common (imported above).

/// Thin wrapper for backward compatibility (used by tests via `use super::*`).
/// Returns `eyre::Result` and does NOT reject the zero address.
#[cfg(test)]
fn parse_address_from_return(hex_str: &str) -> eyre::Result<Address> {
    let clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex_decode(clean).ok_or_else(|| eyre::eyre!("invalid hex in eth_call return"))?;
    if bytes.len() < 32 {
        return Err(eyre::eyre!("return data too short for address"));
    }
    Ok(Address::from_slice(&bytes[12..32]))
}

/// Parse a U256 from a 32-byte ABI-encoded return value.
#[allow(dead_code)]
fn parse_u256_from_return(hex_str: &str) -> eyre::Result<u64> {
    let hex = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex_decode(hex).ok_or_else(|| eyre::eyre!("invalid hex in eth_call return"))?;
    if bytes.len() < 32 {
        return Err(eyre::eyre!("return data too short for uint256"));
    }
    Ok(u256_from_be_bytes(&bytes[0..32]))
}

/// Decode a hex string to bytes.
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16).ok()?;
        bytes.push(byte);
    }
    Some(bytes)
}

/// Read a big-endian uint256 as u64 (truncating high bytes).
#[allow(dead_code)]
fn u256_from_be_bytes(bytes: &[u8]) -> u64 {
    let len = bytes.len().min(32);
    let mut val: u64 = 0;
    let start = len.saturating_sub(8);
    for b in &bytes[start..len] {
        val = (val << 8) | (*b as u64);
    }
    val
}

// extract_methods, cors_response, error_response are in super::common (imported above).

/// Extract the effective gas price from a raw signed transaction.
/// For EIP-1559 txs, uses `max_fee_per_gas` (the worst-case ordering price).
/// For legacy/EIP-2930 txs, uses `gas_price`.
fn extract_gas_price_from_raw_tx(raw_tx: &str) -> eyre::Result<u128> {
    let raw_hex = raw_tx.strip_prefix("0x").unwrap_or(raw_tx);
    let raw_bytes =
        hex_decode(raw_hex).ok_or_else(|| eyre::eyre!("invalid hex in raw transaction"))?;

    use alloy_consensus::Transaction;
    use alloy_consensus::transaction::TxEnvelope;
    use alloy_rlp::Decodable;

    let tx_envelope = TxEnvelope::decode(&mut raw_bytes.as_slice())
        .map_err(|e| eyre::eyre!("failed to decode transaction: {e}"))?;

    let gas_price = match &tx_envelope {
        TxEnvelope::Legacy(signed) => signed.tx().gas_price,
        TxEnvelope::Eip2930(signed) => signed.tx().gas_price,
        TxEnvelope::Eip1559(signed) => signed.tx().max_fee_per_gas,
        TxEnvelope::Eip4844(signed) => signed.tx().max_fee_per_gas(),
        TxEnvelope::Eip7702(signed) => signed.tx().max_fee_per_gas,
    };

    Ok(gas_price)
}

// compute_tx_hash is in super::common. Local alias for the old name.
use super::common::compute_tx_hash as compute_tx_hash_from_raw;

// get_l1_block_context and get_verification_key are in super::common (imported above).

// Use hex crate for encoding (already in dependency tree via alloy)
mod hex {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(HEX_CHARS[(b >> 4) as usize] as char);
            s.push(HEX_CHARS[(b & 0xf) as usize] as char);
        }
        s
    }

    pub fn decode(hex: &str) -> Result<Vec<u8>, ()> {
        if hex.len() % 2 != 0 {
            return Err(());
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for i in (0..hex.len()).step_by(2) {
            let byte = u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| ())?;
            bytes.push(byte);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
#[path = "l1_to_l2_tests.rs"]
mod tests;
