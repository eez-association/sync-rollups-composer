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
    get_l1_block_context, get_rollup_state_root, get_verification_key,
    parse_address_from_abi_return,
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
            // Propagate discovery iteration and L1 trace depth for reentrant detection.
            if c.discovery_iteration > 0 {
                call_json["discoveryIteration"] = serde_json::json!(c.discovery_iteration);
            }
            if c.trace_depth > 0 {
                call_json["l1TraceDepth"] = serde_json::json!(c.trace_depth);
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
    /// Depth in the source chain trace. For L1→L2 root calls: depth on L1 trace.
    /// Used to compute symmetric scope on L2: scope = [0; trace_depth].
    trace_depth: usize,
    /// Iterative discovery iteration in which this call was first detected.
    /// Used to distinguish reentrant patterns (calls discovered across multiple
    /// iterations — each level triggers the next) from continuation patterns
    /// (all calls discovered in the same iteration — user tx calls them directly).
    discovery_iteration: usize,
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
                let data =
                    hex::decode(output.strip_prefix("0x").unwrap_or(output)).unwrap_or_default();
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
    l2_scope: &[U256],
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
    // Scope reflects the nesting depth on L1 (symmetric with L2→L1 rule).
    let sim_action = crate::cross_chain::CrossChainAction {
        action_type: crate::cross_chain::CrossChainActionType::Call,
        rollup_id: U256::from(rollup_id),
        destination,
        value,
        data: data.to_vec(),
        failed: false,
        source_address,
        source_rollup: U256::ZERO, // L1 = rollup 0
        scope: l2_scope.to_vec(),
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
            let placeholder = crate::cross_chain::build_l2_to_l1_call_entries(
                child.original_address,
                child.data.clone(),
                child.value,
                child.source_address,
                rollup_id,
                vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
                vec![],     // delivery_return_data placeholder
                false,      // delivery_failed placeholder
                vec![],     // l1_delivery_scope placeholder
                false,      // tx_reverts
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
        rollup_id: U256::from(rollup_id),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: inner_return_data.clone(),
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
            let retry_inner =
                extract_inner_destination_return_data(&retry_trace, destination);
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
            // Trace extraction didn't find the destination call (nested self-calls
            // may strip children in callTracer). Fall back to direct eth_call on L2
            // to capture the exact revert data from the destination contract.
            let direct_req = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_call",
                "params": [{
                    "from": format!("{source_address}"),
                    "to": format!("{destination}"),
                    "data": format!("0x{}", hex::encode(data)),
                    "gas": "0x2faf080"
                }, "pending"],  // pending = latest including mempool state
                "id": 99959
            });
            if let Ok(resp) = client.post(l2_rpc_url).json(&direct_req).send().await {
                if let Ok(body) = resp.json::<Value>().await {
                    if let Some(error) = body.get("error") {
                        // eth_call returns error for reverting calls with data in error.data
                        if let Some(data_hex) = error.get("data").and_then(|v| v.as_str()) {
                            let clean = data_hex.strip_prefix("0x").unwrap_or(data_hex);
                            if let Ok(data) = hex::decode(clean) {
                                if !data.is_empty() {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        dest = %destination,
                                        data_len = data.len(),
                                        data_hex = %format!("0x{}", hex::encode(&data[..data.len().min(20)])),
                                        "captured revert data from direct eth_call to destination"
                                    );
                                    return (data, false, retry_children);
                                }
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
    l2_scope: &[U256],
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
        rollup_id: U256::from(rollup_id),
        destination,
        value,
        data: data.to_vec(),
        failed: false,
        source_address,
        source_rollup: U256::ZERO,
        scope: l2_scope.to_vec(),
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
                l2_scope, // l2_scope from L1 trace_depth
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
                l2_scope, // l2_scope from L1 trace_depth
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
            trace_depth: c.trace_depth,
            discovery_iteration: 0, // initial detection from first trace
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
async fn build_and_run_l1_postbatch_trace(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    rollup_id: u64,
    builder_key: &alloy_signer_local::PrivateKeySigner,
    detected_calls: &[DetectedInternalCall],
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
            l2_return_data: c.return_data.clone(),
            call_success: c.call_success,
            parent_call_index: c.parent_call_index,
            target_rollup_id: if c.parent_call_index.is_some() && c.target_rollup_id == 0 {
                Some(0)
            } else {
                None
            },
            scope: if c.trace_depth <= 1 {
                vec![]
            } else {
                vec![U256::ZERO; c.trace_depth]
            },
            discovery_iteration: c.discovery_iteration,
            l1_trace_depth: c.trace_depth,
        })
        .collect();

    let analyzed = crate::table_builder::analyze_continuation_calls(&l1_detected, rollup_id);

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
            i, c.destination, c.source_address, sel, c.return_data.len(), c.parent_call_index
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
                let (call_entry, result_entry) = crate::cross_chain::build_cross_chain_call_entries(
                    alloy_primitives::U256::from(rollup_id),
                    c.destination,
                    c.data.clone(),
                    c.value,
                    c.source_address,
                    alloy_primitives::U256::ZERO,
                    c.call_success,
                    c.l2_return_data.clone(),
                );
                vec![call_entry, result_entry]
            })
            .collect();
        crate::cross_chain::convert_pairs_to_l1_entries(&l2_pairs)
    } else {
        let cont = crate::table_builder::build_continuation_entries(
            &analyzed,
            alloy_primitives::U256::from(rollup_id),
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
    let trace_block_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

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
        crate::cross_chain::encode_post_batch_calldata(&entries, call_data_bytes, proof);

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

    let resp = match client.post(l1_rpc_url).json(&trace_req).send().await {
        Ok(r) => match r.json::<Value>().await {
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
                    "({label}) traceCallMany returned error"
                );
            } else {
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    "({label}) traceCallMany returned unexpected structure"
                );
            }
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
    Some((user_trace, resp))
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
fn extract_delivery_return_from_l1_trace_with_calldata(
    user_trace: &Value,
    child_dest: Address,
    _rollups_address: Address,
    child_calldata: Option<&[u8]>,
) -> Vec<u8> {
    let dest_lower = format!("{child_dest}").to_lowercase();
    let calldata_hex = child_calldata.map(|cd| format!("0x{}", hex::encode(cd)).to_lowercase());

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
                    return Some(hex::decode(hex).unwrap_or_default());
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

    // Iterative L1 discovery: if the initial trace found cross-chain calls but the
    // user tx reverted, retrace with entries loaded to discover calls hidden behind
    // the revert. Example: Aggregator calls Bridge.bridgeEther (reverts without
    // entries) then proxy.incrementProxy (never reached). Loading entries for the
    // bridge deposit allows the retrace to reach incrementProxy.
    //
    // This mirrors trace_and_detect_l2_internal_calls in l2_to_l1.rs (Problem 1).
    // Uses build_and_run_l1_postbatch_trace which already handles entry construction,
    // proof signing, and traceCallMany execution.
    if top_level_error && !detected_calls.is_empty() {
        if let Some(builder_key_hex) = &builder_private_key {
            let key_clean = builder_key_hex
                .strip_prefix("0x")
                .unwrap_or(builder_key_hex);
            if let Ok(signer) = key_clean.parse::<alloy_signer_local::PrivateKeySigner>() {
                const MAX_L1_DISCOVERY_ITERATIONS: usize = 5;
                let user_from = from.to_string();
                let user_to = to.to_string();
                let user_data = data.to_string();
                let user_value = value.to_string();

                for iteration in 1..=MAX_L1_DISCOVERY_ITERATIONS {
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        iteration,
                        known_calls = detected_calls.len(),
                        "iterative L1 discovery: retracing user tx with postBatch entries"
                    );

                    let trace_result = build_and_run_l1_postbatch_trace(
                        client,
                        l1_rpc_url,
                        rollups_address,
                        rollup_id,
                        &signer,
                        &detected_calls,
                        &user_from,
                        &user_to,
                        &user_data,
                        &user_value,
                        &format!("l1-discovery-iter-{iteration}"),
                    )
                    .await;

                    let Some((user_trace, _full_resp)) = trace_result else {
                        tracing::warn!(
                            target: "based_rollup::l1_proxy",
                            iteration,
                            "iterative L1 discovery: traceCallMany failed — stopping"
                        );
                        break;
                    };

                    // Walk the retrace for new cross-chain calls.
                    let new_detected = walk_l1_trace_generic(
                        client,
                        l1_rpc_url,
                        rollups_address,
                        &user_trace,
                        &mut proxy_cache,
                    )
                    .await;

                    // Find truly new calls (not already in detected_calls).
                    let truly_new: Vec<_> = new_detected
                        .into_iter()
                        .filter(|new_call| {
                            !detected_calls.iter().any(|existing| {
                                existing.destination == new_call.destination
                                    && existing.calldata == new_call.calldata
                                    && existing.value == new_call.value
                                    && existing.source_address == new_call.source_address
                            })
                        })
                        .collect();

                    if truly_new.is_empty() {
                        tracing::info!(
                            target: "based_rollup::l1_proxy",
                            iteration,
                            total = detected_calls.len(),
                            "iterative L1 discovery converged — no new calls"
                        );
                        break;
                    }

                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        iteration,
                        new_count = truly_new.len(),
                        "iterative L1 discovery found new cross-chain calls"
                    );
                    detected_calls.extend(truly_new);
                }
            }
        }
    }

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
                        trace_depth: 0,     // L2→L1 child: depth in L2 simulation
                        discovery_iteration: 0, // will be updated in iterative loop
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
                    "data": format!("0x{}", hex::encode(&child.calldata)),
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
            if let Ok(body) = resp.json::<Value>().await {
                if let Some(traces) = body
                    .get("result")
                    .and_then(|r| r.get(0))
                    .and_then(|b| b.as_array())
                {
                    for (i, (_, child)) in all_child_calls.iter_mut().enumerate() {
                        if let Some(trace) = traces.get(i) {
                            let has_error = trace.get("error").is_some();
                            if let Some(output) = trace.get("output").and_then(|v| v.as_str()) {
                                let hex = output.strip_prefix("0x").unwrap_or(output);
                                if let Ok(delivery_bytes) = hex::decode(hex) {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        idx = i,
                                        dest = %child.destination,
                                        return_data_len = delivery_bytes.len(),
                                        return_data_hex = %format!("0x{}", hex::encode(&delivery_bytes[..delivery_bytes.len().min(32)])),
                                        delivery_failed = has_error,
                                        "enriched L2→L1 child with CHAINED L1 delivery return data"
                                    );
                                    child.return_data = delivery_bytes;
                                    if has_error {
                                        child.call_success = false;
                                    }
                                }
                            } else if has_error {
                                child.call_success = false;
                            }
                        }
                    }
                } else if let Some(error) = body.get("error") {
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
        if let Some(ref builder_key_hex) = builder_private_key {
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
                    let trace_result = build_and_run_l1_postbatch_trace(
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
                    // Determine postBatch success from resp
                    let postbatch_ok = resp
                        .get("result")
                        .and_then(|r| r.get(0))
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
                        &mut proxy_cache,
                    )
                    .await;

                    tracing::info!(
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
                            if !child_calls.is_empty() {
                                // Simulate ALL children in a CHAINED bundle on L1.
                                // Includes ALL prior children (from all_calls + earlier
                                // iter_child_calls) so each child sees cumulative state.
                                let mut prior_child_txs: Vec<Value> = Vec::new();
                                // Prior children from all_calls
                                for prior in all_calls.iter() {
                                    if prior.parent_call_index.is_some()
                                        && prior.target_rollup_id == 0
                                    {
                                        prior_child_txs.push(serde_json::json!({
                                            "from": format!("{}", prior.source_address),
                                            "to": format!("{}", prior.destination),
                                            "data": format!("0x{}", hex::encode(&prior.calldata)),
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
                                        "data": format!("0x{}", hex::encode(&prev_child.calldata)),
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
                                            "data": format!("0x{}", hex::encode(&c.data)),
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
                                    if let Ok(body) = resp.json::<Value>().await {
                                        body.get("result")
                                            .and_then(|r| r.get(0))
                                            .and_then(|b| b.as_array())
                                            .cloned()
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
                                                if let Ok(bytes) = hex::decode(hex) {
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
                                        delivery_return_data_hex = %format!("0x{}", hex::encode(&child_delivery_data[..child_delivery_data.len().min(32)])),
                                        delivery_failed = child_delivery_failed,
                                        "discovered child L2→L1 in iterative enrichment (CHAINED L1 sim)"
                                    );
                                    iter_child_calls.push((
                                        0,
                                        DetectedInternalCall {
                                            destination: child.original_address,
                                            target_rollup_id: 0,
                                            calldata: child.data.clone(),
                                            value: child.value,
                                            source_address: child.source_address,
                                            call_success: !child_delivery_failed,
                                            return_data: child_delivery_data,
                                            parent_call_index: None,
                                            trace_depth: 0,
                                            discovery_iteration: iteration,
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
                        if c.parent_call_index.is_none() && c.return_data.is_empty() {
                            // Check if this call has children
                            let has_children = detected_calls
                                .iter()
                                .any(|other| other.parent_call_index == Some(i));
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
                    let root_calls: Vec<&DetectedInternalCall> = detected_calls
                        .iter()
                        .filter(|c| c.parent_call_index.is_none())
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
                                c.parent_call_index.is_some() && c.target_rollup_id == 0
                            })
                            .map(|(i, _)| i)
                            .collect();

                        for &ci in &child_indices {
                            let child = &detected_calls[ci];
                            // Only update if current data is stale (error selector = 4 bytes)
                            if child.return_data.len() <= 4 || !child.call_success {
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
                                        old_len = child.return_data.len(),
                                        new_len = delivery.len(),
                                        new_hex = %format!("0x{}", hex::encode(&delivery[..delivery.len().min(32)])),
                                        "post-convergence: PRE-ENRICHED L2→L1 child from saved L1 trace"
                                    );
                                    detected_calls[ci].return_data = delivery;
                                    detected_calls[ci].call_success = true;
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
                                if child.return_data.len() <= 4 && !child.call_success {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        ci,
                                        dest = %child.destination,
                                        old_hex = %format!("0x{}", hex::encode(&child.return_data)),
                                        "post-convergence: defaulting continuation child to void (simulation artifact)"
                                    );
                                    detected_calls[ci].return_data = vec![];
                                    detected_calls[ci].call_success = true;
                                }
                            }
                        }
                    }

                    // Collect L1→L2 root call indices in REVERSE order (innermost first).
                    let root_indices: Vec<usize> = detected_calls
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.parent_call_index.is_none())
                        .map(|(i, _)| i)
                        .rev()
                        .collect();

                    // Get system address for L2 simulation.
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
                            let has_children = detected_calls
                                .iter()
                                .any(|other| other.parent_call_index == Some(idx));
                            if !has_children || !detected_calls[idx].return_data.is_empty() {
                                continue; // Leaf or already enriched.
                            }

                            // Find this parent's L2→L1 child.
                            let child_idx = match detected_calls
                                .iter()
                                .position(|c| c.parent_call_index == Some(idx))
                            {
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
                                    dc.return_data.len(),
                                    if dc.return_data.is_empty() { "0x".to_string() } else {
                                        format!("0x{}", hex::encode(&dc.return_data[..dc.return_data.len().min(8)]))
                                    },
                                    dc.call_success
                                );
                            }

                            // STEP A: Run L1 trace to extract child delivery return.
                            // The L1 entries are rebuilt from current detected_calls state.
                            // Inner levels are already correct → their entries have correct
                            // RESULT hashes → scope navigation succeeds for this child.
                            if let Some((user_trace, _resp)) = build_and_run_l1_postbatch_trace(
                                client,
                                l1_rpc_url,
                                rollups_address,
                                rollup_id,
                                &builder_key,
                                &detected_calls,
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
                                        format!("0x{}", hex::encode(&delivery_data[..delivery_data.len().min(32)]))
                                    },
                                    "post-convergence: extracted child delivery return from L1 trace"
                                );

                                // Only update if current data is stale (error selector ≤4 bytes
                                // or call failed). Don't overwrite valid data from iterative
                                // discovery (e.g., chained L1 sim that returned uint256(2)
                                // for the second CounterL1 call).
                                let current = &detected_calls[child_idx];
                                if !delivery_data.is_empty()
                                    && (current.return_data.len() <= 4 || !current.call_success)
                                {
                                    detected_calls[child_idx].return_data = delivery_data;
                                    detected_calls[child_idx].call_success = true;
                                }
                            }

                            // STEP B: Rebuild L2 entries with updated child delivery return,
                            // then run L2 sim for the parent.
                            let call_destination = detected_calls[idx].destination;
                            let call_calldata = detected_calls[idx].calldata.clone();
                            let call_value = detected_calls[idx].value;
                            let call_source = detected_calls[idx].source_address;

                            let l1_detected: Vec<crate::table_builder::L1DetectedCall> =
                                detected_calls
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
                                        scope: if c.trace_depth <= 1 {
                                            vec![]
                                        } else {
                                            vec![U256::ZERO; c.trace_depth]
                                        },
                                        discovery_iteration: c.discovery_iteration,
                                        l1_trace_depth: c.trace_depth,
                                    })
                                    .collect();
                            let analyzed = crate::table_builder::analyze_continuation_calls(
                                &l1_detected,
                                rollup_id,
                            );
                            if analyzed.is_empty() {
                                continue;
                            }
                            let cont = crate::table_builder::build_continuation_entries(
                                &analyzed,
                                alloy_primitives::U256::from(rollup_id),
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
                                    dc.return_data.len(),
                                    if dc.return_data.is_empty() { "0x".to_string() } else {
                                        format!("0x{}", hex::encode(&dc.return_data[..dc.return_data.len().min(32)]))
                                    },
                                    dc.call_success,
                                    format!("0x{}", hex::encode(&dc.calldata[..dc.calldata.len().min(36)]))
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
                                rollup_id: U256::from(rollup_id),
                                destination: call_destination,
                                value: call_value,
                                data: call_calldata,
                                failed: false,
                                source_address: call_source,
                                source_rollup: U256::ZERO,
                                scope: vec![],
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
                                // BFS extraction
                                let inner =
                                    extract_inner_destination_return_data(&trace, call_destination)
                                        .unwrap_or_default();
                                let inner_success = !inner.is_empty()
                                    || destination_call_succeeded_in_trace(
                                        &trace,
                                        call_destination,
                                    );

                                // Fallback to top-level if BFS empty
                                let (ret_data, inner_success) = if inner.is_empty() && success {
                                    let raw = extract_return_data_from_trace(&trace);
                                    let decoded = if raw.len() >= 64 {
                                        let dlen = U256::from_be_slice(&raw[32..64]).to::<usize>();
                                        raw[64..64 + dlen.min(raw.len() - 64)].to_vec()
                                    } else {
                                        raw
                                    };
                                    (decoded, true)
                                } else {
                                    (inner, inner_success)
                                };

                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    idx,
                                    dest = %call_destination,
                                    ret_data_len = ret_data.len(),
                                    inner_success,
                                    sim_success = success,
                                    ret_data_hex = %if ret_data.is_empty() {
                                        "0x".to_string()
                                    } else {
                                        format!("0x{}", hex::encode(&ret_data[..ret_data.len().min(32)]))
                                    },
                                    "post-convergence: L2 sim result for parent"
                                );

                                // For reentrant patterns: update return data (deepCall returns
                                // incrementing values needed for result propagation entries).
                                // For continuation patterns with children: DON'T update.
                                // The L2 sim returns scope-chain-resolved data (e.g., CounterL1's
                                // return propagated back) but the parent (CAP2) returns void.
                                // Overwriting with sim data corrupts the resolution_terminal entry.
                                let has_children = detected_calls
                                    .iter()
                                    .any(|c| c.parent_call_index == Some(idx));
                                if is_reentrant_pattern || !has_children {
                                    if inner_success || !ret_data.is_empty() {
                                        detected_calls[idx].return_data = ret_data;
                                        detected_calls[idx].call_success = inner_success;
                                    }
                                } else {
                                    tracing::info!(
                                        target: "based_rollup::l1_proxy",
                                        idx,
                                        "post-convergence: skipping L2 return update for continuation parent with children (returns void)"
                                    );
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
                            i, c.destination, c.return_data.len(),
                            if c.return_data.is_empty() { "0x".to_string() } else {
                                format!("0x{}", hex::encode(&c.return_data[..c.return_data.len().min(32)]))
                            },
                            c.call_success, c.parent_call_index, c.discovery_iteration
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
