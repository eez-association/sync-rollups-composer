//! Lightweight JSON-RPC reverse proxy embedded in the rollup binary.
//!
//! Sits in front of reth's RPC server and transparently forwards all requests.
//! Intercepts `eth_sendRawTransaction` to additionally call
//! `syncrollups_simulateTransaction`, enabling the execution planner to build
//! state deltas and L1 submission entries without a separate proxy process.
//!
//! Detects ALL cross-chain L2->L1 calls via a single generic trace path using
//! `trace::walk_trace_tree`. Detection uses the protocol-level
//! `executeCrossChainCall` child pattern — no contract-specific selectors.
//! Works for bridgeEther, bridgeTokens, direct proxy calls, wrapper contracts,
//! multi-call continuations, and any future cross-chain pattern. The proxy queues execution
//! entries BEFORE forwarding the user's tx (hold-then-forward pattern).
//!
//! Intercepts `eth_estimateGas` targeting CrossChainProxy addresses to return
//! a conservative gas estimate, since the actual on-chain execution depends on
//! loaded execution table entries.
//!
//! The proxy listens on a configurable port (default: disabled) and forwards
//! to `127.0.0.1:{reth_rpc_port}` (default: 8545).

use crate::cross_chain::{RollupId, ScopePath, filter_new_by_count};
use alloy_primitives::{Address, U256};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::net::SocketAddr;
use tokio::net::TcpListener;

// Shared items from common — used internally in this module. l1_to_l2 imports
// directly from super::common.
use super::common::detect_cross_chain_proxy_on_l2;

// Bring shared helpers into scope for internal use and `use super::*` in tests.
use super::common::{
    compute_tx_hash as compute_l2_tx_hash, cors_response, error_response, extract_methods,
};
use super::model::{DiscoveredCall, L2ProxyLookup, ReturnEdge};

/// Run the RPC proxy server.
///
/// Listens on `0.0.0.0:{proxy_port}` and forwards all JSON-RPC requests to
/// `http://127.0.0.1:{upstream_port}`. Intercepts `eth_sendRawTransaction`
/// to also call `syncrollups_simulateTransaction` in the background.
///
/// Detects cross-chain calls via a single generic trace path and queues entries
/// SYNCHRONOUSLY before forwarding the user's tx (hold-then-forward pattern).
/// Detection uses protocol-level `executeCrossChainCall` child pattern from
/// `trace::walk_trace_tree` — no contract-specific selectors. Works for
/// bridgeEther, bridgeTokens, direct proxy calls, wrapper contracts, multi-call
/// continuations, and any future cross-chain pattern.
#[allow(clippy::too_many_arguments)]
pub async fn run_rpc_proxy(
    proxy_port: u16,
    upstream_port: u16,
    bridge_l2_address: Address,
    cross_chain_manager_address: Address,
    rollup_id: u64,
    l1_rpc_url: String,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<String>,
) -> eyre::Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], proxy_port));
    let listener = TcpListener::bind(addr).await?;
    let upstream_url = format!("http://127.0.0.1:{upstream_port}");

    tracing::info!(
        target: "based_rollup::proxy",
        %proxy_port,
        %upstream_port,
        %bridge_l2_address,
        %cross_chain_manager_address,
        rollup_id,
        "RPC proxy listening"
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build reqwest client");

    let bridge_addr = bridge_l2_address;
    let ccm_addr = cross_chain_manager_address;
    let l1_rpc = l1_rpc_url;
    let rollups_addr = rollups_address;
    let builder_addr = builder_address;
    let builder_key = builder_private_key;

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "based_rollup::proxy", %e, "accept failed");
                // Brief backoff to prevent CPU-saturating spin on persistent errors
                // (e.g., file descriptor exhaustion).
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };

        let client = client.clone();
        let upstream = upstream_url.clone();
        let l1_rpc = l1_rpc.clone();
        let builder_key = builder_key.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let client = client.clone();
                let upstream = upstream.clone();
                let l1_rpc = l1_rpc.clone();
                let builder_key = builder_key.clone();
                handle_request(
                    req,
                    client,
                    upstream,
                    peer,
                    bridge_addr,
                    ccm_addr,
                    rollup_id,
                    l1_rpc,
                    rollups_addr,
                    builder_addr,
                    builder_key,
                )
            });

            let io = TokioIo::new(stream);
            if let Err(e) = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, service)
                .await
            {
                // Connection reset by peer is normal, don't log it as error
                if !e.is_incomplete_message() {
                    tracing::debug!(
                        target: "based_rollup::proxy",
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
    upstream_url: String,
    _peer: SocketAddr,
    _bridge_l2_address: Address,
    cross_chain_manager_address: Address,
    rollup_id: u64,
    l1_rpc_url: String,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<String>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Handle CORS preflight
    if req.method() == hyper::Method::OPTIONS {
        return Ok(cors_response(
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Full::new(Bytes::new()))
                .expect("valid response"),
        ));
    }

    // Only handle POST (JSON-RPC)
    if req.method() != hyper::Method::POST {
        return Ok(cors_response(
            Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(Full::new(Bytes::from("Method Not Allowed")))
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
            tracing::debug!(target: "based_rollup::proxy", %e, "failed to read request body");
            return Ok(error_response(StatusCode::BAD_REQUEST, "bad request body"));
        }
    };

    // Try to parse as JSON-RPC to check the method
    let maybe_json: Option<Value> = serde_json::from_slice(&body_bytes).ok();

    // Check if this is eth_sendRawTransaction or eth_estimateGas (single request or batch)
    if let Some(ref json) = maybe_json {
        let methods = extract_methods(json);
        for (method, params) in &methods {
            // Intercept eth_estimateGas targeting CrossChainProxy addresses.
            // The actual on-chain execution depends on loaded entries, so we
            // return a conservative estimate instead of forwarding to upstream
            // (which would fail or return wrong gas without entries loaded).
            if method == "eth_estimateGas" {
                if let Some(tx_obj) = params.and_then(|p| p.first()) {
                    if let Some(to_str) = tx_obj.get("to").and_then(|v| v.as_str()) {
                        if !cross_chain_manager_address.is_zero() {
                            if let Ok(to_addr) = to_str.parse::<Address>() {
                                // Check CrossChainProxy first (fast path)
                                // Check if target is a CrossChainProxy (conservative gas).
                                // The actual on-chain execution depends on loaded entries,
                                // so upstream gas estimation would fail or return wrong gas.
                                if detect_cross_chain_proxy_on_l2(
                                    &client,
                                    &upstream_url,
                                    to_addr,
                                    cross_chain_manager_address,
                                )
                                .await
                                .is_some()
                                {
                                    let gas_resp = build_gas_estimate_response(json, 500_000);
                                    return Ok(cors_response(
                                        Response::builder()
                                            .status(StatusCode::OK)
                                            .header("Content-Type", "application/json")
                                            .body(Full::new(Bytes::from(gas_resp)))
                                            .expect("valid response"),
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            if method == "eth_sendRawTransaction" {
                if let Some(raw_tx) = params.and_then(|p| p.first()).and_then(|v| v.as_str()) {
                    let mut cross_chain_detected = false;

                    // Compute tx hash early so ALL logs can reference it.
                    let user_tx_hash = compute_l2_tx_hash(raw_tx).unwrap_or_default();
                    tracing::info!(
                        target: "based_rollup::proxy",
                        %user_tx_hash,
                        "intercepted eth_sendRawTransaction — starting cross-chain detection"
                    );

                    // Single generic path: trace and detect ALL cross-chain calls
                    // via protocol-level detection (executeCrossChainCall child pattern).
                    // No contract-specific selectors. Works for bridgeEther, bridgeTokens,
                    // direct proxy calls, wrapper contracts, multi-call continuations — everything.
                    if !cross_chain_manager_address.is_zero() {
                        let detected = trace_and_detect_l2_internal_calls(
                            &client,
                            &upstream_url,
                            raw_tx,
                            cross_chain_manager_address,
                            rollup_id,
                            &l1_rpc_url,
                            rollups_address,
                            builder_address,
                            builder_private_key.as_deref(),
                            &user_tx_hash,
                        )
                        .await;
                        if detected {
                            cross_chain_detected = true;
                        }
                    }

                    // If cross-chain was detected, hold the tx and return computed hash.
                    // The driver will inject the held tx into the pool at block-build time,
                    // ensuring entries are loaded BEFORE the user's tx executes.
                    if cross_chain_detected {
                        if let Some(tx_hash) = compute_l2_tx_hash(raw_tx) {
                            let id = maybe_json
                                .as_ref()
                                .and_then(|j| j.get("id"))
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let resp_json = serde_json::json!({
                                "jsonrpc": "2.0",
                                "result": tx_hash,
                                "id": id
                            });
                            tracing::info!(
                                target: "based_rollup::proxy",
                                %tx_hash,
                                "hold-then-forward: returning computed hash, tx held for driver injection"
                            );
                            return Ok(cors_response(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("Content-Type", "application/json")
                                    .body(Full::new(Bytes::from(resp_json.to_string())))
                                    .expect("valid response"),
                            ));
                        }
                        // If hash computation fails, fall through to normal forwarding
                    }

                    // Fire-and-forget: also simulate the transaction (only for non-held txs)
                    if !cross_chain_detected {
                        let client_bg = client.clone();
                        let upstream_bg = upstream_url.clone();
                        let raw_tx_str = raw_tx.to_string();
                        tokio::spawn(async move {
                            simulate_in_background(&client_bg, &upstream_bg, &raw_tx_str).await;
                        });
                    }
                }
            }
        }
    }

    // Forward the original request to upstream as-is
    let resp = match client
        .post(&upstream_url)
        .header("Content-Type", "application/json")
        .body(body_bytes.to_vec())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "based_rollup::proxy", %e, "upstream request failed");
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream error: {e}"),
            ));
        }
    };

    let status = resp.status();
    let resp_bytes = resp.bytes().await.unwrap_or_default();

    Ok(cors_response(
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(resp_bytes.to_vec())))
            .expect("valid response"),
    ))
}

/// Call `syncrollups_simulateTransaction` in the background.
/// Logs the result but doesn't block the original request.
async fn simulate_in_background(client: &reqwest::Client, upstream_url: &str, raw_tx: &str) {
    let simulate_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "syncrollups_simulateTransaction",
        "params": [raw_tx],
        "id": 99999
    });

    match client.post(upstream_url).json(&simulate_req).send().await {
        Ok(resp) => {
            if let Ok(body) = resp.json::<Value>().await {
                if let Some(error) = body.get("error") {
                    tracing::debug!(
                        target: "based_rollup::proxy",
                        %error,
                        "simulation failed for intercepted tx"
                    );
                } else {
                    tracing::info!(
                        target: "based_rollup::proxy",
                        "simulation completed for intercepted tx"
                    );
                }
            }
        }
        Err(e) => {
            tracing::debug!(
                target: "based_rollup::proxy",
                %e,
                "simulation request failed for intercepted tx"
            );
        }
    }
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
    let mut proxy_cache: std::collections::HashMap<Address, Option<super::trace::ProxyInfo>> =
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
                                        crate::cross_chain::build_l2_to_l1_call_entries(
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
                                            super::common::encode_system_address_calldata();
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
                                        let load_calldata = crate::cross_chain::encode_load_execution_table_calldata(
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
    let mut proxy_cache: std::collections::HashMap<Address, Option<super::trace::ProxyInfo>> =
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
            let placeholder = crate::cross_chain::build_l2_to_l1_call_entries(
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
        let sys_calldata = super::common::encode_system_address_calldata();
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
        crate::cross_chain::encode_load_execution_table_calldata(&all_placeholder_entries);
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

// `simulate_l1_delivery` moved to `super::delivery::simulate_l1_delivery`.
// Re-export for backward compatibility with internal callers.
pub(crate) use super::delivery::simulate_l1_delivery;

// `simulate_chained_delivery_l2_to_l1`, `fallback_per_call_l2_to_l1_simulation`,
// and `simulate_l1_combined_delivery` moved to `super::delivery`.
use super::delivery::simulate_chained_delivery_l2_to_l1;
use super::delivery::simulate_l1_combined_delivery;

// `compute_proxy_address_on_l1` moved to `super::delivery`.

// `extract_delivery_output_from_trigger_trace`, `find_delivery_call`,
// `MAX_SIMULATION_ITERATIONS`, and `extract_l1_to_l2_return_calls`
// moved to `super::delivery`.

/// Build a JSON-RPC response with a gas estimate.
fn build_gas_estimate_response(request_json: &Value, gas: u64) -> String {
    let id = request_json.get("id").cloned().unwrap_or(Value::Null);
    serde_json::json!({
        "jsonrpc": "2.0",
        "result": format!("0x{:x}", gas),
        "id": id
    })
    .to_string()
}

// The direction-local `DetectedL2InternalCall` struct has been replaced by `DiscoveredCall`
// from `super::model`. The shared type has the same core fields plus `parent_call_index`,
// `discovery_iteration`, and `target_rollup_id` (set to defaults at construction sites).

/// Walk an L2 trace using the generic `trace::walk_trace_tree` and convert results
/// to `DiscoveredCall` format used by the rest of this module.
async fn walk_l2_trace_generic(
    client: &reqwest::Client,
    upstream_url: &str,
    ccm_address: Address,
    trace_node: &Value,
    proxy_cache: &mut std::collections::HashMap<Address, Option<super::trace::ProxyInfo>>,
) -> Vec<DiscoveredCall> {
    let lookup = L2ProxyLookup {
        client,
        rpc_url: upstream_url,
        ccm_address,
    };

    // Delegate to the shared walk function. walk_trace_to_discovered already returns
    // DiscoveredCall with the correct defaults (delivery_return_data=[], delivery_failed=false,
    // parent_call_index=Root, discovery_iteration=0, target_rollup_id=0).
    super::model::walk_trace_to_discovered(
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
async fn walk_l2_trace_for_discovered_proxy_calls(
    client: &reqwest::Client,
    upstream_url: &str,
    ccm_address: Address,
    trace_node: &Value,
    our_rollup_id: u64,
    proxy_cache: &mut std::collections::HashMap<Address, Option<super::trace::ProxyInfo>>,
) -> Vec<super::common::DiscoveredProxyCall> {
    let lookup = L2ProxyLookup {
        client,
        rpc_url: upstream_url,
        ccm_address,
    };
    let mut ephemeral_proxies = std::collections::HashMap::new();
    let mut detected_calls = Vec::new();

    super::trace::walk_trace_tree(
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
                    Some(super::common::DiscoveredProxyCall {
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

/// Trace a transaction on L2 using `debug_traceCall` with `callTracer` and detect
/// ALL cross-chain calls via protocol-level detection (executeCrossChainCall child
/// pattern). No contract-specific selectors — works for bridgeEther, bridgeTokens,
/// direct proxy calls, wrapper contracts, multi-call continuations, and any future pattern.
///
/// Returns `true` if cross-chain calls were found and queued.
#[allow(clippy::too_many_arguments)]
async fn trace_and_detect_l2_internal_calls(
    client: &reqwest::Client,
    upstream_url: &str,
    raw_tx_hex: &str,
    cross_chain_manager_address: Address,
    rollup_id: u64,
    l1_rpc_url: &str,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<&str>,
    user_tx_hash: &str,
) -> bool {
    use alloy_consensus::transaction::TxEnvelope;
    use alloy_rlp::Decodable;

    let hex_str = raw_tx_hex.strip_prefix("0x").unwrap_or(raw_tx_hex);
    let tx_bytes = match hex::decode(hex_str) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let envelope = match TxEnvelope::decode(&mut tx_bytes.as_slice()) {
        Ok(e) => e,
        Err(_) => return false,
    };

    use alloy_consensus::Transaction;
    let to_addr = match envelope.to() {
        Some(a) => a,
        None => return false, // Contract creation — no internal cross-chain calls
    };

    // Only trace if the target has code (is a contract).
    let code_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getCode",
        "params": [format!("{to_addr}"), "latest"],
        "id": 99990
    });
    let code_resp = match client.post(upstream_url).json(&code_req).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };
    let code_body: Value = match code_resp.json().await {
        Ok(v) => v,
        Err(_) => return false,
    };
    let code = code_body
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("0x");
    if code == "0x" || code.len() <= 2 {
        return false; // EOA — no internal calls possible
    }

    let value = envelope.value();
    let input = envelope.input();

    use reth_primitives_traits::SignerRecoverable;
    let sender = match envelope.recover_signer() {
        Ok(s) => s,
        Err(_) => return false,
    };

    tracing::info!(
        target: "based_rollup::proxy",
        %to_addr, %sender, user_tx = %user_tx_hash,
        "tracing L2 tx with debug_traceCall to detect cross-chain calls"
    );

    // Build debug_traceCall request against L2 upstream.
    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCall",
        "params": [
            {
                "from": format!("{sender}"),
                "to": format!("{to_addr}"),
                "data": format!("0x{}", hex::encode(input)),
                "value": format!("0x{:x}", value),
                "gas": "0x2faf080"
            },
            "latest",
            { "tracer": "callTracer" }
        ],
        "id": 99989
    });

    let trace_resp = match client.post(upstream_url).json(&trace_req).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                target: "based_rollup::proxy",
                %e,
                "cross-chain detection failed: debug_traceCall transport error on L2 — \
                 tx forwarded without cross-chain entry queuing"
            );
            return false;
        }
    };

    let trace_body: Value = match trace_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                target: "based_rollup::proxy",
                %e,
                "cross-chain detection failed: debug_traceCall response parse error — \
                 tx forwarded without cross-chain entry queuing"
            );
            return false;
        }
    };

    if trace_body.get("error").is_some() {
        let error = trace_body.get("error");
        tracing::error!(
            target: "based_rollup::proxy",
            ?error,
            "cross-chain detection failed: debug_traceCall returned RPC error — \
             tx forwarded without cross-chain entry queuing"
        );
        return false;
    }

    let trace_result = match trace_body.get("result") {
        Some(r) => r,
        None => return false,
    };

    // Walk the trace tree using the generic protocol-level walker.
    // Detection uses executeCrossChainCall child pattern — no contract-specific selectors.
    let mut proxy_cache: std::collections::HashMap<Address, Option<super::trace::ProxyInfo>> =
        std::collections::HashMap::new();
    let mut detected_calls = walk_l2_trace_generic(
        client,
        upstream_url,
        cross_chain_manager_address,
        trace_result,
        &mut proxy_cache,
    )
    .await;

    if detected_calls.is_empty() {
        // Log the trace tree summary so we can diagnose WHY no calls were detected.
        let top_error = trace_result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        let child_count = trace_result
            .get("calls")
            .and_then(|v| v.as_array())
            .map_or(0, |a| a.len());
        tracing::info!(
            target: "based_rollup::proxy",
            %to_addr, %sender, user_tx = %user_tx_hash,
            top_error,
            child_count,
            "no L2→L1 cross-chain calls detected in trace — forwarding tx as-is"
        );
        return false;
    }

    tracing::info!(
        target: "based_rollup::proxy",
        count = detected_calls.len(),
        user_tx = %user_tx_hash,
        "detected internal L2→L1 cross-chain calls via debug_traceCall"
    );

    for (i, call) in detected_calls.iter().enumerate() {
        tracing::info!(
            target: "based_rollup::proxy",
            idx = i,
            destination = %call.destination,
            source_address = %call.source_address,
            calldata_len = call.calldata.len(),
            calldata_prefix = %format!("0x{}", hex::encode(&call.calldata[..call.calldata.len().min(8)])),
            value = %call.value,
            "L2 detected call"
        );
    }

    // Check if the top-level call reverted (multi-call continuation pattern: L2→L1 calls
    // that need entries pre-loaded before the user tx can succeed).
    let top_level_error =
        trace_result.get("error").is_some() || trace_result.get("revertReason").is_some();

    // Run the unified iterative discovery loop via discover_until_stable.
    // This replaces the former inline loop that manually called simulate_l1_delivery,
    // built loadExecutionTable bundles, and deduped new calls. The L2ToL1 direction
    // hooks handle enrichment (two-step: direct L1 sim + full simulate_l1_delivery)
    // and retrace bundle construction (loadExecutionTable + userTx on L2).
    let mut early_return_calls: Vec<ReturnEdge> = Vec::new();
    let mut last_user_trace_had_error = false;

    {
        use super::direction::{L2ToL1, UserTxContext};
        use super::sim_client::HttpSimClient;

        let direction = L2ToL1 {
            l1_ccm: rollups_address,
            l2_ccm: cross_chain_manager_address,
            builder_address,
            builder_private_key: builder_private_key.map(|s| s.to_string()),
            rollup_id,
            client: client.clone(),
            l1_rpc_url: l1_rpc_url.to_string(),
            l2_rpc_url: upstream_url.to_string(),
        };
        let sim = HttpSimClient::new(
            client.clone(),
            l1_rpc_url.to_string(),
            upstream_url.to_string(),
        );
        let user_tx = UserTxContext {
            from: format!("{sender}"),
            to: format!("{to_addr}"),
            data: format!("0x{}", hex::encode(input)),
            raw_tx_bytes: tx_bytes.clone(),
            value: format!("0x{:x}", value),
        };
        let lookup = L2ProxyLookup {
            client,
            rpc_url: upstream_url,
            ccm_address: cross_chain_manager_address,
        };

        let discovery_result = super::discover::discover_until_stable(
            &direction,
            &sim,
            trace_result,
            &user_tx,
            &lookup,
            &mut proxy_cache,
            Some(detected_calls.clone()),
        )
        .await;

        match discovery_result {
            Ok(discovered_set) => {
                detected_calls = discovered_set.calls;
                early_return_calls = discovered_set.returns;
                last_user_trace_had_error = discovered_set.user_tx_reverted;

                if !early_return_calls.is_empty() {
                    tracing::info!(
                        target: "based_rollup::proxy",
                        early_return_call_count = early_return_calls.len(),
                        "iterative L2 discovery captured return calls from enrichment"
                    );
                    for (ri, rc) in early_return_calls.iter().enumerate() {
                        tracing::info!(
                            target: "based_rollup::proxy",
                            ri,
                            dest = %rc.destination,
                            source = %rc.source_address,
                            data_len = rc.data.len(),
                            scope_len = rc.scope.len(),
                            "early return call from L2 iterative discovery"
                        );
                    }
                }

                tracing::info!(
                    target: "based_rollup::proxy",
                    count = detected_calls.len(),
                    early_return_calls = early_return_calls.len(),
                    user_tx_reverted = last_user_trace_had_error,
                    "iterative L2 discovery complete (via discover_until_stable)"
                );

                for (i, call) in detected_calls.iter().enumerate() {
                    tracing::info!(
                        target: "based_rollup::proxy",
                        idx = i,
                        destination = %call.destination,
                        source_address = %call.source_address,
                        calldata_len = call.calldata.len(),
                        calldata_prefix = %format!("0x{}", hex::encode(&call.calldata[..call.calldata.len().min(8)])),
                        value = %call.value,
                        trace_depth = call.trace_depth,
                        in_reverted_frame = call.in_reverted_frame,
                        delivery_failed = call.delivery_failed,
                        delivery_return_data_len = call.delivery_return_data.len(),
                        delivery_return_data_hex = %format!("0x{}", hex::encode(&call.delivery_return_data)),
                        "iterative discovery final call (with revert frame status)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    %e,
                    "discover_until_stable failed — proceeding with initial detected calls"
                );
                // detected_calls remains as initially detected (no iterative expansion).
            }
        }
    }


    process_l2_to_l1_calls(
        client,
        upstream_url,
        raw_tx_hex,
        l1_rpc_url,
        rollups_address,
        builder_address,
        builder_private_key,
        rollup_id,
        cross_chain_manager_address,
        &mut detected_calls,
        &early_return_calls,
        &tx_bytes,
        sender,
        to_addr,
        value,
        input,
        top_level_error,
        last_user_trace_had_error,
    )
    .await
}

/// Post-discovery processing: compute tx outcome, verify return calls via
/// retrace, then route through the appropriate queuing path (partial revert,
/// duplicate, multi-call, single-call with depth, or simple single-call).
///
/// Separated from `trace_and_detect_l2_internal_calls` for readability.
/// The logic and behavior are identical — this is a purely mechanical extraction.
#[allow(clippy::too_many_arguments)]
async fn process_l2_to_l1_calls(
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
                    std::collections::HashMap::<Address, Option<super::trace::ProxyInfo>>::new();
                if let Ok(resp) = client.post(upstream_url).json(&retrace_req).send().await {
                    if let Ok(body) = resp.json::<Value>().await {
                        if let Some(traces) = body
                            .get("result")
                            .and_then(|r| r.get(0))
                            .and_then(|b| b.as_array())
                        {
                            if traces.len() >= 2 {
                                let new_detected = walk_l2_trace_generic(
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
            let new_l2_calls = simulate_l2_return_call_delivery(
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
            let nested_l2_calls = simulate_l2_return_call_delivery(
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
                                            Option<super::trace::ProxyInfo>,
                                        > = std::collections::HashMap::new();
                                        let discovered_in_phase_b =
                                            walk_l2_trace_for_discovered_proxy_calls(
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
            enrich_return_calls_via_l2_trace(
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
async fn simulate_l2_return_call_delivery(
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
    let mut proxy_cache: std::collections::HashMap<Address, Option<super::trace::ProxyInfo>> =
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
        let sys_calldata = super::common::encode_system_address_calldata();
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
        let placeholder_entries = crate::cross_chain::build_l2_to_l1_call_entries(
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
        let load_calldata = crate::cross_chain::encode_load_execution_table_calldata(
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

// cors_response and error_response are in super::common (re-exported above).

#[cfg(test)]
#[path = "l2_to_l1_tests.rs"]
mod tests;
