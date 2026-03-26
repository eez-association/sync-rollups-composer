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
    /// Rollup ID of the target rollup.
    _rollup_id: u64,
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

    // Save trace for debugging.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = std::fs::write(
        format!("/tmp/trace_l2sim_{ts}.json"),
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    );

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

    let has_error = exec_trace.get("error").is_some() || exec_trace.get("revertReason").is_some();
    let success = !has_error;

    tracing::info!(
        target: "based_rollup::l1_proxy::debug256",
        file = %format!("/tmp/trace_l2sim_{ts}.json"),
        success,
        has_error,
        output_len = exec_trace.get("output").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0),
        "run_l2_sim_bundle trace saved"
    );

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
        walk_l2_simulation_trace(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            &trace,
            rollup_id,
        )
        .await
    } else {
        Vec::new()
    };

    // Step 5: Extract return data.
    let return_data = extract_return_data_from_trace(&trace);

    tracing::info!(
        target: "based_rollup::l1_proxy::debug256",
        dest = %destination,
        source = %source_address,
        return_data_len = return_data.len(),
        call_success = success,
        child_count = children.len(),
        has_error = trace.get("error").is_some(),
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
            let placeholder = crate::cross_chain::build_l2_to_l1_call_entries(
                child.original_address,
                child.data.clone(),
                child.value,
                child.source_address,
                rollup_id,
                vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
                vec![],     // delivery_return_data placeholder
                false,      // delivery_failed placeholder
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
            let retry_children = walk_l2_simulation_trace(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                &retry_trace,
                rollup_id,
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
                return (retry_data, true, retry_children);
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

    // No children — return the initial simulation result directly.
    if success {
        tracing::info!(
            target: "based_rollup::l1_proxy",
            dest = %destination,
            source = %source_address,
            return_data_len = return_data.len(),
            return_data_hex = %format!("0x{}", hex::encode(&return_data[..return_data.len().min(64)])),
            child_calls = children.len(),
            "L2 call simulation succeeded"
        );
    } else {
        let error = trace
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        tracing::info!(
            target: "based_rollup::l1_proxy",
            dest = %destination,
            source = %source_address,
            error,
            "L2 call simulation reverted — marking call as failed"
        );
    }

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
/// Returns detected calls as `DiscoveredProxyCall` for compatibility with
/// existing callers. Calls targeting our own rollup are filtered out (only
/// L2→L1 calls — those targeting rollup 0 — are returned).
async fn walk_l2_simulation_trace(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    ccm_address: Address,
    trace_node: &Value,
    our_rollup_id: u64,
) -> Vec<super::common::DiscoveredProxyCall> {
    let lookup = L2ProxyLookup {
        client,
        l2_rpc_url,
        ccm_address,
    };
    let mut proxy_cache: HashMap<Address, Option<super::trace::ProxyInfo>> = HashMap::new();
    let mut ephemeral_proxies = HashMap::new();
    let mut detected_calls = Vec::new();

    // The L2 CCM is the manager contract on L2.
    super::trace::walk_trace_tree(
        trace_node,
        &[ccm_address],
        &lookup,
        &mut proxy_cache,
        &mut ephemeral_proxies,
        &mut detected_calls,
    )
    .await;

    // Convert trace::DetectedCall to DiscoveredProxyCall, filtering out calls
    // that target our own rollup (we only want L2→L1 calls).
    detected_calls
        .into_iter()
        .filter_map(|c| {
            // Look up the proxy identity to get the original_rollup_id.
            // The walker already resolved proxy identity when detecting the
            // call — the destination IS the originalAddress. We need to find
            // the rollup ID from the proxy cache.
            //
            // The walker sets destination = proxy_info.original_address, so
            // to find the rollup ID we need to check what proxy was resolved.
            // However, walk_trace_tree doesn't expose the rollup ID in
            // DetectedCall. We recover it from the proxy_cache by looking up
            // the proxy address that resolved to this destination.
            //
            // Alternative: look for the proxy address in proxy_cache where
            // original_address matches c.destination. But multiple proxies
            // could map to different rollups.
            //
            // Simplest approach: check all cached entries for a proxy whose
            // original_address == c.destination and filter by rollup ID.
            let proxy_info = proxy_cache
                .values()
                .find_map(|opt| opt.filter(|info| info.original_address == c.destination));

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
        .collect()
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
    )
    .await;

    // Convert trace::DetectedCall to DetectedInternalCall.
    detected_calls
        .into_iter()
        .map(|c| DetectedInternalCall {
            destination: c.destination,
            _rollup_id: 0, // L1→L2: destination rollup determined by proxy identity
            calldata: c.calldata,
            value: c.value,
            source_address: c.source_address,
            call_success: true,
            return_data: vec![],
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

        // Save request to file for debugging (#256)
        let req_json = serde_json::to_string_pretty(&trace_req).unwrap_or_default();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let _ = std::fs::write(format!("/tmp/trace_req_initial_{ts}.json"), &req_json);

        let resp = client
            .post(l1_rpc_url)
            .json(&trace_req)
            .send()
            .await?
            .json::<Value>()
            .await?;

        // Save response to file for debugging (#256)
        let resp_json = serde_json::to_string_pretty(&resp).unwrap_or_default();
        let _ = std::fs::write(format!("/tmp/trace_resp_initial_{ts}.json"), &resp_json);
        tracing::info!(
            target: "based_rollup::l1_proxy::debug256",
            file = %format!("/tmp/trace_resp_initial_{ts}.json"),
            "saved initial debug_traceCall request+response to files"
        );

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

    tracing::info!(
        target: "based_rollup::l1_proxy::debug256",
        trace_json = %serde_json::to_string(&trace_result).unwrap_or_default().chars().take(3000).collect::<String>(),
        "initial debug_traceCall full trace"
    );

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
    // Also collect child L2→L1 proxy calls discovered in L2 simulation traces.
    // These represent the nested L1→L2→L1 pattern (the L2 target calls back to L1).
    let mut all_child_calls: Vec<DetectedInternalCall> = Vec::new();
    if !cross_chain_manager_address.is_zero() {
        for call in &mut detected_calls {
            let (ret_data, success, child_calls) = simulate_l1_to_l2_call_on_l2(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                call.destination,
                &call.calldata,
                call.value,
                call.source_address,
                rollup_id,
            )
            .await;
            tracing::info!(
                target: "based_rollup::l1_proxy::debug256",
                dest = %call.destination,
                source = %call.source_address,
                return_data_len = ret_data.len(),
                call_success = success,
                child_l2_to_l1_calls = child_calls.len(),
                return_data_hex = %if ret_data.is_empty() { "EMPTY".to_string() } else { format!("0x{}", hex::encode(&ret_data[..std::cmp::min(ret_data.len(), 64)])) },
                "simulate_l1_to_l2_call_on_l2 result"
            );
            call.return_data = ret_data;
            call.call_success = success;

            // Convert child L2→L1 proxy calls to DetectedInternalCall and
            // accumulate them. These will be added to detected_calls after
            // the enrichment loop completes (can't modify the vec during
            // iteration).
            for child in &child_calls {
                tracing::info!(
                    target: "based_rollup::l1_proxy",
                    parent_dest = %call.destination,
                    child_dest = %child.original_address,
                    child_source = %child.source_address,
                    child_data_len = child.data.len(),
                    child_value = %child.value,
                    child_reverted = child.reverted,
                    "discovered child L2→L1 call from L2 simulation (nested L1→L2→L1 pattern)"
                );
                all_child_calls.push(DetectedInternalCall {
                    destination: child.original_address,
                    _rollup_id: 0, // L2→L1: child targets L1
                    calldata: child.data.clone(),
                    value: child.value,
                    source_address: child.source_address,
                    call_success: true, // defaults to true; will be enriched later if needed
                    return_data: vec![], // will be enriched via L1 simulation
                });
            }
        }
    }

    // If child L2→L1 calls were discovered, they represent additional cross-chain
    // calls that need their own entries. Add them to detected_calls so the routing
    // logic below can handle them (multi-call path with continuation entries).
    if !all_child_calls.is_empty() {
        tracing::info!(
            target: "based_rollup::l1_proxy",
            parent_calls = detected_calls.len(),
            child_calls = all_child_calls.len(),
            "propagating child L2→L1 calls from L2 simulation to detected calls"
        );
        detected_calls.extend(all_child_calls);
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

                    let entries = if analyzed.is_empty() {
                        // Fallback: build simple CALL+RESULT pairs, then convert to L1 format.
                        // L2 format: [CALL trigger, RESULT table entry] per call
                        // L1 format: single entry with actionHash=hash(CALL), nextAction=RESULT
                        // Without convert_pairs_to_l1_entries, Rollups.sol would see
                        // nextAction=CALL and enter newScope(), causing InvalidRevertData.
                        let l2_pairs: Vec<_> = l1_detected
                            .iter()
                            .flat_map(|c| {
                                let (call_entry, result_entry) =
                                    crate::cross_chain::build_cross_chain_call_entries(
                                        alloy_primitives::U256::from(rollup_id),
                                        c.destination,
                                        c.data.clone(),
                                        c.value,
                                        c.source_address,
                                        alloy_primitives::U256::ZERO, // source_rollup = L1 = 0
                                        c.call_success,
                                        c.l2_return_data.clone(),
                                    );
                                vec![call_entry, result_entry]
                            })
                            .collect();
                        let l1_entries = crate::cross_chain::convert_pairs_to_l1_entries(&l2_pairs);
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            l2_pairs = l2_pairs.len(),
                            l1_entries = l1_entries.len(),
                            "built L1 entries from simple CALL+RESULT pairs"
                        );
                        l1_entries
                    } else {
                        let cont = crate::table_builder::build_continuation_entries(
                            &analyzed,
                            alloy_primitives::U256::from(rollup_id),
                        );
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            l2_entries = cont.l2_entries.len(),
                            l1_entries = cont.l1_entries.len(),
                            "built continuation entries"
                        );
                        // For traceCallMany we need L1 entries (posted via postBatch)
                        cont.l1_entries
                    };

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

                    // Save iterative request to file (#256 debug)
                    let req_json = serde_json::to_string_pretty(&trace_req).unwrap_or_default();
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    let _ = std::fs::write(
                        format!("/tmp/trace_req_iter{iteration}_{ts}.json"),
                        &req_json,
                    );

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

                    // Save iterative response to file (#256 debug)
                    let resp_json = serde_json::to_string_pretty(&resp).unwrap_or_default();
                    let _ = std::fs::write(
                        format!("/tmp/trace_resp_iter{iteration}_{ts}.json"),
                        &resp_json,
                    );
                    tracing::info!(
                        target: "based_rollup::l1_proxy::debug256",
                        file = %format!("/tmp/trace_resp_iter{iteration}_{ts}.json"),
                        iteration,
                        "saved iterative traceCallMany request+response to files"
                    );

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

                    // debug256: dump full user_trace JSON
                    {
                        let trace_str: String = serde_json::to_string(user_trace)
                            .unwrap_or_default()
                            .chars()
                            .take(3000)
                            .collect();
                        tracing::info!(
                            target: "based_rollup::l1_proxy::debug256",
                            iteration,
                            user_trace_json = %trace_str,
                            "iterative traceCallMany user_trace dump"
                        );
                    }

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
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        "╔══ traceCallMany Result ══"
                    );
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        "║ postBatch (tx1): {}",
                        if tx1_trace.get("error").is_some() { "REVERTED" } else { "SUCCESS" }
                    );
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        "║ userTx (tx2):   {} — {} {}",
                        if user_error == "none" { "SUCCESS" } else { "REVERTED" },
                        decoded_error,
                        if !inner_error.is_empty() { format!("(inner: {})", inner_error) } else { String::new() }
                    );
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        "║ userTx subcalls: {}",
                        user_calls_count
                    );
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        "╚═════════════════════════"
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

                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        new_detected_count = new_detected.len(),
                        "walked user tx trace for cross-chain calls"
                    );

                    // debug256: dump walk results
                    tracing::info!(
                        target: "based_rollup::l1_proxy::debug256",
                        new_detected_count = new_detected.len(),
                        all_calls_count = all_calls.len(),
                        "walk_trace_tree completed for re-trace iteration"
                    );
                    for (i, c) in new_detected.iter().enumerate() {
                        tracing::info!(
                            target: "based_rollup::l1_proxy::debug256",
                            index = i,
                            dest = %c.destination,
                            source = %c.source_address,
                            calldata_len = c.calldata.len(),
                            value = %c.value,
                            "new_detected call"
                        );
                    }

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

                    tracing::info!(
                        target: "based_rollup::l1_proxy::debug256",
                        new_calls_count = new_calls.len(),
                        "filter_new_by_count result"
                    );

                    tracing::info!(
                        target: "based_rollup::l1_proxy::debug256",
                        new_calls_count = new_calls.len(),
                        user_reverted = user_error != "none",
                        user_error = %user_error,
                        "convergence decision"
                    );

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

                    // Enrich new calls with L2 return data via simulation.
                    // Also collect any child L2→L1 calls discovered in the
                    // L2 simulation (nested L1→L2→L1 pattern).
                    let mut enriched_new_calls = new_calls;
                    let mut iter_child_calls: Vec<DetectedInternalCall> = Vec::new();
                    if !cross_chain_manager_address.is_zero() {
                        for call in &mut enriched_new_calls {
                            let (ret_data, success, child_calls) = simulate_l1_to_l2_call_on_l2(
                                client,
                                l2_rpc_url,
                                cross_chain_manager_address,
                                call.destination,
                                &call.calldata,
                                call.value,
                                call.source_address,
                                rollup_id,
                            )
                            .await;
                            call.return_data = ret_data;
                            call.call_success = success;

                            // Convert child L2→L1 proxy calls to
                            // DetectedInternalCall.
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
                                iter_child_calls.push(DetectedInternalCall {
                                    destination: child.original_address,
                                    _rollup_id: 0,
                                    calldata: child.data.clone(),
                                    value: child.value,
                                    source_address: child.source_address,
                                    call_success: true,
                                    return_data: vec![],
                                });
                            }
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
                        all_calls.extend(iter_child_calls);
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
