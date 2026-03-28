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

use crate::cross_chain::filter_new_by_count;
use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
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
use super::common::{detect_cross_chain_proxy_on_l2, find_failed_proxy_calls_in_l2_trace};

// Bring shared helpers into scope for internal use and `use super::*` in tests.
use super::common::{
    compute_tx_hash as compute_l2_tx_hash, cors_response, error_response, extract_methods,
    get_l1_block_context as get_l1_block_context_for_proxy,
    get_verification_key as get_verification_key_for_proxy,
};

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
/// 3 L1 deferred entries (continuation structure), queued as a single `QueuedWithdrawal`.
#[allow(clippy::too_many_arguments)]
async fn queue_l2_to_l1_multi_call_entries(
    client: &reqwest::Client,
    upstream_url: &str,
    raw_tx_hex: &str,
    detected_l2_calls: &[DetectedL2InternalCall],
    return_calls: &[DetectedReturnCall],
    _rollup_id: u64,
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
            serde_json::json!({
                "destination": format!("{}", call.destination),
                "data": format!("0x{}", hex::encode(&call.calldata)),
                "value": format!("{}", call.value),
                "sourceAddress": format!("{}", call.source_address),
                "deliveryReturnData": format!("0x{}", hex::encode(&call.delivery_return_data)),
                "deliveryFailed": call.delivery_failed
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
            if let Some(idx) = rc.parent_call_index {
                obj.as_object_mut().unwrap().insert(
                    "parentCallIndex".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(idx)),
                );
            }
            if !rc.l2_return_data.is_empty() {
                obj.as_object_mut().unwrap().insert(
                    "l2ReturnData".to_string(),
                    serde_json::Value::String(format!("0x{}", hex::encode(&rc.l2_return_data))),
                );
            }
            if rc.l2_delivery_failed {
                obj.as_object_mut().unwrap().insert(
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
            "rawL2Tx": raw_tx_hex
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
    )
    .await
}

/// Fallback: queue a simple L2→L1 call entry when the continuation RPC fails.
async fn queue_l2_to_l1_fallback(
    client: &reqwest::Client,
    upstream_url: &str,
    raw_tx_hex: &str,
    destination: Address,
    data: &[u8],
    value: U256,
    sender: Address,
) -> Option<()> {
    tracing::info!(
        target: "based_rollup::proxy",
        "falling back to initiateL2CrossChainCall for L2→L1 multi-call continuation"
    );
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
            "rawL2Tx": raw_tx_hex
        }],
        "id": 99991
    });

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
async fn queue_independent_calls_l2_to_l1(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    upstream_url: &str,
    raw_tx_hex: &str,
    detected_calls: &[DetectedL2InternalCall],
    rollups_address: Address,
    rollup_id: u64,
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
                "rawL2Tx": raw_l2_tx
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
/// return data. For each return call with empty `l2_return_data`:
/// 1. Compute L2 proxy address via `computeCrossChainProxyAddress`
/// 2. Run `debug_traceCallMany` on L2 with `[direct_call]`
/// 3. Extract return data from trace output
/// 4. Store in `rc.l2_return_data` / `rc.l2_delivery_failed`
///
/// This enrichment ensures that the next iteration of `simulate_l1_delivery` builds
/// inner RESULT entries with real data (instead of `data: vec![]`), so the L1
/// delivery function (e.g., Logger) receives the correct inner return value (#246).
async fn enrich_return_calls_via_l2_trace(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    return_calls: &mut [DetectedReturnCall],
    rollup_id: u64,
) {
    // Shared proxy cache across all return calls in this enrichment pass.
    let mut proxy_cache: std::collections::HashMap<Address, Option<(Address, u64)>> =
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
                return_calls[i].l2_return_data.is_empty() && !return_calls[i].l2_delivery_failed
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
        if !return_calls[i].l2_return_data.is_empty() || return_calls[i].l2_delivery_failed {
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
                            // Walk the trace tree using protocol-generic proxy detection
                            // (authorizedProxies query) instead of heuristic output matching.
                            // This finds all cross-chain proxy calls in the trace, whether
                            // they reverted with ExecutionNotFound, a wrapper's custom error,
                            // or any other revert reason.
                            let mut discovered = Vec::new();
                            find_failed_proxy_calls_in_l2_trace(
                                client,
                                l2_rpc_url,
                                cross_chain_manager_address,
                                trace,
                                rollup_id,
                                &mut proxy_cache,
                                &mut discovered,
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
                                                                    .l2_delivery_failed = true;
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
                                                                            .l2_return_data = bytes;
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
                                        return_calls[i].l2_delivery_failed = true;
                                    }
                                } else {
                                    // No placeholder entries could be built — mark as failed.
                                    return_calls[i].l2_delivery_failed = true;
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
                            return_calls[i].l2_delivery_failed = true;
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
                                    return_calls[i].l2_return_data = bytes;
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
/// **Transactional**: backs up existing `l2_return_data` and `l2_delivery_failed`
/// for all calls before attempting enrichment. On partial failure, reverted calls
/// get their backup restored.
///
/// **Phase 1** (simple chained trace): bundles all calls in one `debug_traceCallMany`.
/// If all succeed, keeps new data and returns `true`.
///
/// **Phase 2** (loadExecutionTable retry): if Phase 1 has any reverted calls, uses
/// `find_failed_proxy_calls_in_l2_trace` to discover inner proxy calls, builds
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
    return_calls: &mut [DetectedReturnCall],
    needs_enrichment: &[usize],
    proxy_addr_cache: &std::collections::HashMap<Address, Option<String>>,
) -> bool {
    // --- Backup existing data (transactional safety) ---
    let backups: Vec<(Vec<u8>, bool)> = needs_enrichment
        .iter()
        .map(|&idx| {
            (
                return_calls[idx].l2_return_data.clone(),
                return_calls[idx].l2_delivery_failed,
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
                        return_calls[idx].l2_return_data = bytes;
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
    let mut proxy_cache: std::collections::HashMap<Address, Option<(Address, u64)>> =
        std::collections::HashMap::new();
    let mut all_placeholder_entries = Vec::new();

    for &pos in &reverted_positions {
        let trace = &traces[pos];
        let mut discovered = Vec::new();
        find_failed_proxy_calls_in_l2_trace(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            trace,
            rollup_id,
            &mut proxy_cache,
            &mut discovered,
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
            );
            all_placeholder_entries.extend(placeholder.l2_table_entries);
        }
    }

    if all_placeholder_entries.is_empty() {
        // No proxy calls found in any reverted trace — restore backups for
        // reverted calls and report failure so per-call fallback handles them.
        for &pos in &reverted_positions {
            let idx = needs_enrichment[pos];
            return_calls[idx].l2_return_data = backups[pos].0.clone();
            return_calls[idx].l2_delivery_failed = backups[pos].1;
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
                return_calls[idx].l2_return_data = backups[pos].0.clone();
                return_calls[idx].l2_delivery_failed = backups[pos].1;
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
                return_calls[idx].l2_return_data = backups[pos].0.clone();
                return_calls[idx].l2_delivery_failed = backups[pos].1;
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
                return_calls[idx].l2_return_data = backups[pos].0.clone();
                return_calls[idx].l2_delivery_failed = backups[pos].1;
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
                return_calls[idx].l2_return_data = backups[pos].0.clone();
                return_calls[idx].l2_delivery_failed = backups[pos].1;
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
            return_calls[idx].l2_return_data = backups[pos].0.clone();
            return_calls[idx].l2_delivery_failed = backups[pos].1;
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
                    return_calls[idx].l2_return_data = bytes;
                }
            }
        }
    }

    all_enriched
}

/// Simulate L1 delivery via `debug_traceCallMany` to capture return data.
///
/// For EOA targets (no code on L1), returns `(vec![], false, vec![])` immediately.
/// For contract targets, builds preliminary L1 deferred entries, signs an ECDSA
/// proof, and simulates `[postBatch, createProxy, trigger]` on L1 via
/// `debug_traceCallMany`. Walks the trigger trace to extract the delivery CALL's
/// return data and success/failure status.
///
/// **Iterative discovery**: After each simulation, walks the trigger trace for
/// L1→L2 return calls (multi-call continuation pattern). If new return calls are found, rebuilds
/// entries incorporating the return calls as continuations and re-simulates until
/// convergence or MAX_SIMULATION_ITERATIONS is reached. This mirrors the L1 proxy's
/// iterative `traceCallMany` loop.
///
/// Returns `(return_data, failed, detected_return_calls)`.
/// Returns `None` if the simulation cannot be performed.
#[allow(clippy::too_many_arguments)]
async fn simulate_l1_delivery(
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
) -> Option<(Vec<u8>, bool, Vec<DetectedReturnCall>)> {
    // First check if destination has code on L1.
    // If it's an EOA, return data is empty and we skip simulation.
    let code_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getCode",
        "params": [format!("{destination}"), "latest"],
        "id": 1
    });
    let code_resp = client.post(l1_rpc_url).json(&code_req).send().await.ok()?;
    let code_body: Value = code_resp.json().await.ok()?;
    let code_hex = code_body.get("result")?.as_str()?;
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

    // Iterative discovery loop: simulate, extract return calls, rebuild entries, repeat.
    let mut all_return_calls: Vec<DetectedReturnCall> = Vec::new();
    let mut final_return_data: Vec<u8> = Vec::new();
    let mut prev_return_data: Vec<u8> = Vec::new();
    let mut final_delivery_failed = false;

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
            let withdrawal_entries = crate::cross_chain::build_l2_to_l1_call_entries(
                destination,
                data.to_vec(),
                value,
                _trigger_user,
                rollup_id,
                rlp_encoded_tx.to_vec(), // RLP-encoded L2 tx for L2TX trigger
                vec![],                  // placeholder delivery_return_data
                false,                   // placeholder delivery_failed
            );
            withdrawal_entries.l1_deferred_entries
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
                        l2_return_data: rc.l2_return_data.clone(),
                        l2_delivery_failed: rc.l2_delivery_failed,
                    })
                    .collect();

            let analyzed = crate::table_builder::analyze_l2_to_l1_continuation_calls(
                &[root_call],
                &return_calls_for_builder,
                rollup_id,
            );
            let continuation = crate::table_builder::build_l2_to_l1_continuation_entries(
                &analyzed,
                alloy_primitives::U256::from(rollup_id),
                rlp_encoded_tx,
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
            match get_l1_block_context_for_proxy(client, l1_rpc_url).await {
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
        let trace_block_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Get verification key from Rollups contract.
        let vk =
            match get_verification_key_for_proxy(client, l1_rpc_url, rollups_address, rollup_id)
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
            crate::cross_chain::encode_post_batch_calldata(&entries, call_data_bytes, proof);

        // Encode executeL2TX calldata using typed ABI encoding (NEVER hardcode selectors).
        let execute_l2tx_calldata = crate::cross_chain::IRollups::executeL2TXCall {
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

        let resp = match client.post(l1_rpc_url).json(&trace_req).send().await {
            Ok(r) => match r.json::<Value>().await {
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
        let bundle_traces = match resp
            .get("result")
            .and_then(|r| r.get(0))
            .and_then(|b| b.as_array())
        {
            Some(arr) if arr.len() >= 2 => arr,
            _ => {
                if let Some(error) = resp.get("error") {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        ?error,
                        "traceCallMany returned error"
                    );
                } else {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        "traceCallMany returned unexpected structure (expected 2 traces)"
                    );
                }
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

        // DEBUG: dump BOTH traces for depth-N investigation
        // Log tx0 root-level info without the full trace (too large)
        let tx0_error = bundle_traces[0].get("error").and_then(|v| v.as_str()).unwrap_or("NONE");
        let tx0_output = bundle_traces[0].get("output").and_then(|v| v.as_str()).unwrap_or("").chars().take(20).collect::<String>();
        let tx0_to = bundle_traces[0].get("to").and_then(|v| v.as_str()).unwrap_or("?");
        let tx0_input_sel = bundle_traces[0].get("input").and_then(|v| v.as_str()).unwrap_or("").chars().take(10).collect::<String>();
        let tx0_trace_str = format!("to={} sel={} error={} output={}", tx0_to, tx0_input_sel, tx0_error, tx0_output);
        let tx1_trace_str: String = serde_json::to_string(&bundle_traces[1])
            .unwrap_or_default()
            .chars()
            .take(8000)
            .collect();
        tracing::info!(
            target: "based_rollup::proxy",
            iteration,
            tx0_trace = %tx0_trace_str,
            "DEBUG: L1 delivery simulation postBatch trace (tx0)"
        );
        tracing::info!(
            target: "based_rollup::proxy",
            iteration,
            tx1_trace = %tx1_trace_str,
            "DEBUG: L1 delivery simulation trigger trace (tx1)"
        );

        // Extract delivery output from executeL2TX trace (tx1).
        let trigger_trace = &bundle_traces[1];
        let (return_data, _delivery_failed) =
            extract_delivery_output_from_trigger_trace(trigger_trace, destination);

        // Trigger simulation is unreliable for L2→L1 calls: entries have
        // placeholder state deltas and the ECDSA proof may not match real L1
        // state. Always assume delivery succeeds — §4f + rewind handles real
        // failures on L1.
        final_delivery_failed = false;

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
            // Log each return call's l2_return_data for hash comparison
            for (ri, rc) in all_return_calls.iter().enumerate() {
                tracing::info!(
                    target: "based_rollup::proxy",
                    ri,
                    dest = %rc.destination,
                    l2_return_data_hex = %format!("0x{}", hex::encode(&rc.l2_return_data)),
                    l2_return_data_len = rc.l2_return_data.len(),
                    l2_delivery_failed = rc.l2_delivery_failed,
                    "return call l2_return_data at convergence"
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
        enrich_return_calls_via_l2_trace(
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

/// Simulate N identical cross-chain calls with state accumulation on L1.
///
/// Each call sees state effects from previous calls (e.g., counter on L1 increments
/// from 0 to 1 to 2 when called twice). Uses `debug_traceCallMany` with N delivery
/// calls in one bundle. Direction: L2->L1 (delivery happens on L1).
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
async fn simulate_chained_delivery_l2_to_l1(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    _l2_rpc_url: &str,
    rollups_address: Address,
    rollup_id: u64,
    _builder_address: Address,
    _builder_private_key: &str,
    calls: &[DetectedL2InternalCall],
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
        let code_body: Value = match code_resp.json().await {
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
        let code_hex = code_body
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("0x");
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
        match get_l1_block_context_for_proxy(client, l1_rpc_url).await {
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
        .unwrap()
        .as_secs();

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
    let body: Value = match resp.json().await {
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
    let traces = match body
        .get("result")
        .and_then(|r| r.get(0))
        .and_then(|b| b.as_array())
    {
        Some(arr) if arr.len() == calls.len() => arr,
        _ => {
            let actual_len = body
                .get("result")
                .and_then(|r| r.get(0))
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
async fn fallback_per_call_l2_to_l1_simulation(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    calls: &[DetectedL2InternalCall],
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

        let resp = match client.post(l1_rpc_url).json(&trace_req).send().await {
            Ok(r) => match r.json::<Value>().await {
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

        let trace = resp
            .get("result")
            .and_then(|r| r.get(0))
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
async fn simulate_l1_combined_delivery(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<&str>,
    rollup_id: u64,
    calls: &[&DetectedL2InternalCall],
    rlp_encoded_tx: &[u8],
) -> Option<Vec<(Vec<u8>, bool, Vec<DetectedReturnCall>)>> {
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
        let code_body: Value = code_resp.json().await.ok()?;
        let code_hex = code_body.get("result")?.as_str()?;
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
    let mut all_return_calls: Vec<DetectedReturnCall> = Vec::new();
    // Per-call results: indexed by position in `calls` slice.
    let mut per_call_return_data: Vec<Vec<u8>> = vec![vec![]; calls.len()];
    let mut per_call_delivery_failed: Vec<bool> = vec![false; calls.len()];
    // Track previous return data for convergence (#254 item 7).
    let mut prev_per_call_return_data: Vec<Vec<u8>> = vec![vec![]; calls.len()];

    for iteration in 1..=MAX_SIMULATION_ITERATIONS {
        tracing::info!(
            target: "based_rollup::proxy",
            iteration,
            known_return_calls = all_return_calls.len(),
            "combined L1 delivery simulation iteration"
        );

        // Build combined L1 deferred entries for all calls.
        let mut combined_entries: Vec<crate::cross_chain::CrossChainExecutionEntry> = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            // Collect return calls belonging to this trigger call.
            let my_return_calls: Vec<&DetectedReturnCall> = all_return_calls
                .iter()
                .filter(|rc| rc.parent_call_index == Some(i))
                .collect();

            let entries = if my_return_calls.is_empty() {
                // Simple case: just this L2→L1 call.
                let withdrawal_entries = crate::cross_chain::build_l2_to_l1_call_entries(
                    call.destination,
                    call.calldata.to_vec(),
                    call.value,
                    call.source_address,
                    rollup_id,
                    rlp_encoded_tx.to_vec(),
                    per_call_return_data[i].clone(),
                    per_call_delivery_failed[i],
                );
                withdrawal_entries.l1_deferred_entries
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
                };

                let return_calls_for_builder: Vec<crate::table_builder::L2ReturnCall> =
                    my_return_calls
                        .iter()
                        .map(|rc| crate::table_builder::L2ReturnCall {
                            destination: rc.destination,
                            data: rc.data.clone(),
                            value: rc.value,
                            source_address: rc.source_address,
                            // parent_call_index in DetectedReturnCall refers to the
                            // combined simulation's call index. For the table builder,
                            // we're building entries for a single root call, so set to
                            // None (defaults to last L2→L1 call, which is the only one).
                            parent_call_index: None,
                            l2_return_data: rc.l2_return_data.clone(),
                            l2_delivery_failed: rc.l2_delivery_failed,
                        })
                        .collect();

                let analyzed = crate::table_builder::analyze_l2_to_l1_continuation_calls(
                    &[root_call],
                    &return_calls_for_builder,
                    rollup_id,
                );
                let continuation = crate::table_builder::build_l2_to_l1_continuation_entries(
                    &analyzed,
                    alloy_primitives::U256::from(rollup_id),
                    rlp_encoded_tx,
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
            match get_l1_block_context_for_proxy(client, l1_rpc_url).await {
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
            .unwrap()
            .as_secs();

        // Get verification key from Rollups contract.
        let vk =
            match get_verification_key_for_proxy(client, l1_rpc_url, rollups_address, rollup_id)
                .await
            {
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
        let entry_hashes = crate::cross_chain::compute_entry_hashes(&combined_entries, vk);
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
        let post_batch_calldata = crate::cross_chain::encode_post_batch_calldata(
            &combined_entries,
            call_data_bytes,
            proof,
        );

        // Build the traceCallMany bundle:
        //   tx0: postBatch(combined_entries)
        //   tx1: executeL2TX(rollupId, rlpTx)
        // One executeL2TX handles all entries via scope resolution.
        let builder_addr_hex = format!("{builder_address}");
        let rollups_hex = format!("{rollups_address}");
        let post_batch_data = format!("0x{}", hex::encode(post_batch_calldata.as_ref()));
        let next_block = format!("{:#x}", trace_block_number);

        // Encode executeL2TX calldata using typed ABI encoding (NEVER hardcode selectors).
        let execute_l2tx_calldata = crate::cross_chain::IRollups::executeL2TXCall {
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

        let resp = match client.post(l1_rpc_url).json(&trace_req).send().await {
            Ok(r) => match r.json::<Value>().await {
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
        let bundle_traces = match resp
            .get("result")
            .and_then(|r| r.get(0))
            .and_then(|b| b.as_array())
        {
            Some(arr) if arr.len() >= expected_trace_count => arr,
            _ => {
                if let Some(error) = resp.get("error") {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        ?error,
                        "combined traceCallMany returned error"
                    );
                } else {
                    let actual = resp
                        .get("result")
                        .and_then(|r| r.get(0))
                        .and_then(|b| b.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        expected = expected_trace_count,
                        actual,
                        "combined traceCallMany returned unexpected trace count"
                    );
                }
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
        let mut new_return_calls_this_iteration: Vec<DetectedReturnCall> = Vec::new();

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
            let new_returns = extract_l1_to_l2_return_calls(
                client,
                l1_rpc_url,
                rollups_address,
                trigger_trace,
                rollup_id,
            )
            .await;

            // Tag return calls with parent_call_index so we know which trigger produced them.
            for mut rc in new_returns {
                rc.parent_call_index = Some(call_idx);
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
        enrich_return_calls_via_l2_trace(
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
    let mut results: Vec<(Vec<u8>, bool, Vec<DetectedReturnCall>)> = Vec::new();
    for (i, _call) in calls.iter().enumerate() {
        let my_return_calls: Vec<DetectedReturnCall> = all_return_calls
            .iter()
            .filter(|rc| rc.parent_call_index == Some(i))
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

/// Compute the CrossChainProxy address on L1 for a given trigger user.
///
/// Calls `computeCrossChainProxyAddress(originalAddress, originalRollupId)`
/// on the Rollups contract.
async fn compute_proxy_address_on_l1(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    trigger_user: Address,
    rollup_id: u64,
) -> eyre::Result<Address> {
    // Encode computeCrossChainProxyAddress(address, uint256)
    use alloy_sol_types::SolCall;
    let compute_data = crate::cross_chain::IRollups::computeCrossChainProxyAddressCall {
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

    let resp = client
        .post(l1_rpc_url)
        .json(&req)
        .send()
        .await?
        .json::<Value>()
        .await?;

    if let Some(error) = resp.get("error") {
        return Err(eyre::eyre!("computeCrossChainProxyAddress failed: {error}"));
    }

    let result_hex = resp
        .get("result")
        .and_then(|v| v.as_str())
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
fn extract_delivery_output_from_trigger_trace(
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

/// A cross-chain call detected in the L1 delivery trace (multi-call continuation return trip).
///
/// When an L2→L1 call's L1 delivery triggers L1→L2 return calls (e.g., multi-call continuation
/// pattern), these are extracted from the trigger trace via `authorizedProxies` queries.
#[derive(Debug, Clone)]
struct DetectedReturnCall {
    /// Target address on L2 (originalAddress from L1 proxy).
    destination: Address,
    /// Calldata forwarded through the proxy to the destination.
    data: Vec<u8>,
    /// ETH value sent with the call.
    value: U256,
    /// The address that initiated the call on L1 (from field in trace).
    source_address: Address,
    /// Index of the trigger call (in the combined simulation bundle) that produced this
    /// return call. `None` for single-call simulations; `Some(i)` for combined simulation
    /// where call `i` in the `calls` slice triggered this return.
    parent_call_index: Option<usize>,
    /// Return data from simulating this call's execution on L2.
    /// Used for the L2 RESULT entry hash — must match the actual return data
    /// from _processCallAtScope on L2. Empty for void functions.
    l2_return_data: Vec<u8>,
    /// Whether the L2 simulation of this call reverted.
    /// Used for the L2 RESULT entry `failed` flag (#246 audit).
    l2_delivery_failed: bool,
}

/// Maximum number of iterative discovery rounds in `simulate_l1_delivery`.
const MAX_SIMULATION_ITERATIONS: usize = 10;

/// Extract L1→L2 return calls from the trigger trace by querying `authorizedProxies`
/// on the L1 Rollups contract.
///
/// Walks the trigger trace depth-first. For each CALL, checks if the target is a
/// registered CrossChainProxy on L1 via `authorizedProxies(address)` on Rollups.sol.
/// If so, extracts the destination (originalAddress), calldata, value, and source.
///
/// Uses a cache to avoid repeated `authorizedProxies` lookups for the same address.
async fn extract_l1_to_l2_return_calls(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    trigger_trace: &Value,
    our_rollup_id: u64,
) -> Vec<DetectedReturnCall> {
    let mut results = Vec::new();
    let mut proxy_cache: std::collections::HashMap<Address, Option<(Address, u64)>> =
        std::collections::HashMap::new();

    walk_trigger_trace_for_return_calls(
        client,
        l1_rpc_url,
        rollups_address,
        trigger_trace,
        our_rollup_id,
        &mut proxy_cache,
        &mut results,
        0, // depth — skip top-level (the trigger tx itself)
    )
    .await;

    results
}

/// Recursively walk the trigger trace to find L1→L2 return calls.
///
/// Checks each subcall to see if it targets a CrossChainProxy on L1 (via
/// `authorizedProxies` on Rollups.sol). If the proxy's `originalRollupId` matches
/// our rollup, it is an L1→L2 return call.
///
/// `depth` is used to skip the top-level trigger call itself (depth 0) — we only
/// look at subcalls within the delivery execution.
#[allow(clippy::too_many_arguments)]
async fn walk_trigger_trace_for_return_calls(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    node: &Value,
    our_rollup_id: u64,
    proxy_cache: &mut std::collections::HashMap<Address, Option<(Address, u64)>>,
    results: &mut Vec<DetectedReturnCall>,
    depth: usize,
) {
    let mut should_recurse = true;

    // At depth >= 2 (subcalls within the delivery), check if this call targets
    // a CrossChainProxy on L1. Depth 0 = trigger proxy, depth 1 = Rollups.executeCrossChainCall,
    // depth 2+ = delivery subcalls.
    if depth >= 2 {
        if let Some(to_str) = node.get("to").and_then(|v| v.as_str()) {
            if let Ok(to_addr) = to_str.parse::<Address>() {
                // Skip the Rollups contract itself — not a proxy
                if to_addr != rollups_address {
                    let proxy_info = match proxy_cache.get(&to_addr) {
                        Some(cached) => *cached,
                        None => {
                            let result = detect_cross_chain_proxy_on_l1(
                                client,
                                l1_rpc_url,
                                to_addr,
                                rollups_address,
                            )
                            .await;
                            proxy_cache.insert(to_addr, result);
                            result
                        }
                    };

                    if let Some((destination, rollup_id)) = proxy_info {
                        if rollup_id == our_rollup_id {
                            let call_from = node
                                .get("from")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<Address>().ok())
                                .unwrap_or(Address::ZERO);

                            // Skip forward delivery calls: when Rollups calls
                            // proxy.executeOnBehalf(dest, data) to deliver the
                            // original cross-chain call. Only calls FROM user
                            // contracts (not Rollups) are true return calls.
                            if call_from == rollups_address {
                                tracing::debug!(
                                    target: "based_rollup::proxy",
                                    proxy = %to_addr,
                                    %destination,
                                    "skipping forward delivery proxy call (from=Rollups) — recursing for return calls"
                                );
                                // Continue recursing to find the real return call
                                // deeper in the tree.
                            } else {
                                let input =
                                    node.get("input").and_then(|v| v.as_str()).unwrap_or("0x");
                                let input_clean = input.strip_prefix("0x").unwrap_or(input);
                                let mut input_bytes = hex::decode(input_clean).unwrap_or_default();

                                // Strip executeOnBehalf(address,bytes) wrapper if present.
                                // The proxy's callTracer node may capture the executeOnBehalf
                                // encoding instead of raw msg.data when the node is at the
                                // Rollups→proxy→executeCrossChainCall level rather than the
                                // caller→proxy level.
                                // Selector derived via sol! macro — NEVER hardcode.
                                if input_bytes.len() > 100
                                    && input_bytes[..4] == super::common::EXECUTE_ON_BEHALF_SELECTOR
                                {
                                    // ABI decode: executeOnBehalf(address dest, bytes data)
                                    // Skip: selector(4) + address(32) + offset(32) = 68
                                    // Read length at offset 68
                                    if input_bytes.len() >= 100 {
                                        let data_len = U256::from_be_slice(&input_bytes[68..100]);
                                        let data_start = 100usize;
                                        let data_end = data_start + data_len.to::<usize>();
                                        if data_end <= input_bytes.len() {
                                            tracing::info!(
                                                target: "based_rollup::proxy",
                                                original_len = input_bytes.len(),
                                                unwrapped_len = data_end - data_start,
                                                "stripped executeOnBehalf wrapper from return call data"
                                            );
                                            input_bytes =
                                                input_bytes[data_start..data_end].to_vec();
                                        }
                                    }
                                }

                                let call_value = node
                                    .get("value")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| {
                                        U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
                                            .ok()
                                    })
                                    .unwrap_or(U256::ZERO);

                                tracing::info!(
                                    target: "based_rollup::proxy",
                                    proxy = %to_addr,
                                    %destination,
                                    rollup_id,
                                    source = %call_from,
                                    data_len = input_bytes.len(),
                                    "detected L1->L2 return call in delivery trace"
                                );

                                results.push(DetectedReturnCall {
                                    destination,
                                    data: input_bytes,
                                    value: call_value,
                                    source_address: call_from,
                                    parent_call_index: None,
                                    l2_return_data: vec![],
                                    l2_delivery_failed: false,
                                });
                                // Do not recurse into proxy children
                                should_recurse = false;
                            } // end else (not forward delivery)
                        }
                    }
                }
            }
        }
    }

    if should_recurse {
        if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
            for child in calls {
                Box::pin(walk_trigger_trace_for_return_calls(
                    client,
                    l1_rpc_url,
                    rollups_address,
                    child,
                    our_rollup_id,
                    proxy_cache,
                    results,
                    depth + 1,
                ))
                .await;
            }
        }
    }
}

/// Query `authorizedProxies(address)` on the L1 Rollups contract.
///
/// Returns `Some((originalAddress, originalRollupId))` if the address is a
/// registered proxy, `None` otherwise.
async fn detect_cross_chain_proxy_on_l1(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    address: Address,
    rollups_address: Address,
) -> Option<(Address, u64)> {
    // authorizedProxies(address) — typed ABI encoding via sol! macro — NEVER hardcode selectors.
    let calldata = super::common::encode_authorized_proxies_calldata(address);

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": format!("{rollups_address}"), "data": calldata}, "latest"],
        "id": 99993
    });

    let resp = client.post(l1_rpc_url).json(&req).send().await.ok()?;
    let body: Value = resp.json().await.ok()?;

    if body.get("error").is_some() {
        return None;
    }

    let hex_data = body.get("result")?.as_str()?;
    let hex_clean = hex_data.strip_prefix("0x").unwrap_or(hex_data);

    if hex_clean.len() < 128 {
        return None;
    }

    let addr_bytes = hex::decode(&hex_clean[..64]).ok()?;
    if addr_bytes.len() < 32 {
        return None;
    }
    let original_address = Address::from_slice(&addr_bytes[12..32]);

    if original_address.is_zero() {
        return None;
    }

    let rid_bytes = hex::decode(&hex_clean[64..128]).ok()?;
    if rid_bytes.len() < 32 {
        return None;
    }
    let mut val: u64 = 0;
    let start = rid_bytes.len().saturating_sub(8);
    for b in &rid_bytes[start..] {
        val = (val << 8) | (*b as u64);
    }

    Some((original_address, val))
}

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

/// Information about an internal L2→L1 cross-chain call detected via trace.
#[derive(Clone)]
struct DetectedL2InternalCall {
    /// Cross-chain destination address on L1.
    destination: Address,
    /// Calldata forwarded to the destination.
    calldata: Vec<u8>,
    /// ETH value sent with the call.
    value: U256,
    /// The address that initiated the cross-chain call (from field in trace).
    source_address: Address,
    /// Return data from the L1 delivery simulation for this call.
    /// When non-empty, the L1 RESULT entry hash includes this data.
    delivery_return_data: Vec<u8>,
    /// Whether the L1 delivery simulation reverted.
    delivery_failed: bool,
}

/// L2 proxy lookup: queries `authorizedProxies(address)` on the L2 CCM.
///
/// Implements `trace::ProxyLookup` for use with the generic `walk_trace_tree`.
struct L2ProxyLookup<'a> {
    client: &'a reqwest::Client,
    upstream_url: &'a str,
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
                self.upstream_url,
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

/// Walk an L2 trace using the generic `trace::walk_trace_tree` and convert results
/// to `DetectedL2InternalCall` format used by the rest of this module.
async fn walk_l2_trace_generic(
    client: &reqwest::Client,
    upstream_url: &str,
    ccm_address: Address,
    trace_node: &Value,
    proxy_cache: &mut std::collections::HashMap<Address, Option<super::trace::ProxyInfo>>,
) -> Vec<DetectedL2InternalCall> {
    let lookup = L2ProxyLookup {
        client,
        upstream_url,
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
    )
    .await;

    // Convert trace::DetectedCall to DetectedL2InternalCall.
    detected_calls
        .into_iter()
        .map(|c| DetectedL2InternalCall {
            destination: c.destination,
            calldata: c.calldata,
            value: c.value,
            source_address: c.source_address,
            delivery_return_data: vec![],
            delivery_failed: false,
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
        %to_addr, %sender,
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
        return false;
    }

    tracing::info!(
        target: "based_rollup::proxy",
        count = detected_calls.len(),
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

    if top_level_error && !detected_calls.is_empty() {
        // Iterative discovery: simulate with loadExecutionTable to see ALL calls.
        // The initial debug_traceCall runs without execution table entries loaded, so
        // some L2→L1 calls may be hidden behind reverts that only resolve when earlier
        // entries are pre-loaded. We iterate: build entries for known calls, simulate
        // [loadExecutionTable, userTx] via debug_traceCallMany, walk the user tx trace
        // for new calls, and repeat until convergence.
        let mut all_calls = detected_calls.clone();
        let mut iteration = 0;
        const MAX_ITERATIONS: usize = 10;

        loop {
            iteration += 1;
            if iteration > MAX_ITERATIONS {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    MAX_ITERATIONS,
                    "iterative L2 discovery hit max iterations — proceeding with known calls"
                );
                break;
            }

            tracing::info!(
                target: "based_rollup::proxy",
                iteration,
                known_calls = all_calls.len(),
                "iterative L2 discovery: traceCallMany with loadExecutionTable pre-loading"
            );

            // Step 1: Build L2 table entries for all known calls.
            let mut l2_table_entries = Vec::new();
            for call in &all_calls {
                let withdrawal_entries = crate::cross_chain::build_l2_to_l1_call_entries(
                    call.destination,
                    call.calldata.clone(),
                    call.value,
                    call.source_address,
                    rollup_id,
                    tx_bytes.clone(), // rlp_encoded_tx for L2TX trigger
                    vec![],           // delivery_return_data (placeholder for discovery)
                    false,            // delivery_failed (placeholder for discovery)
                );
                l2_table_entries.extend(withdrawal_entries.l2_table_entries);
            }

            // Step 2: Encode loadExecutionTable calldata.
            let load_table_calldata =
                crate::cross_chain::encode_load_execution_table_calldata(&l2_table_entries);
            let load_table_data = format!("0x{}", hex::encode(load_table_calldata.as_ref()));
            let ccm_hex = format!("{cross_chain_manager_address}");
            let builder_hex = format!("{builder_address}");

            // Step 3: Build traceCallMany request.
            // reth's debug_traceCallMany format:
            //   params: [bundles, stateContext?, tracingOptions?]
            //   bundle: { transactions: [tx1, tx2, ...] }
            // Both txs in ONE bundle so tx1's state (loadExecutionTable) is visible to tx2.
            let trace_req = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "debug_traceCallMany",
                "params": [
                    [
                        {
                            "transactions": [
                                {
                                    "from": builder_hex,
                                    "to": ccm_hex,
                                    "data": load_table_data,
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
                        }
                    ],
                    null,
                    { "tracer": "callTracer" }
                ],
                "id": 99987
            });

            // Step 4: Execute traceCallMany.
            let resp = match client.post(upstream_url).json(&trace_req).send().await {
                Ok(r) => match r.json::<Value>().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            target: "based_rollup::proxy",
                            %e,
                            "L2 traceCallMany response parse failed"
                        );
                        break;
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        target: "based_rollup::proxy",
                        %e,
                        "L2 traceCallMany request failed"
                    );
                    break;
                }
            };

            // Step 5: Parse traces — result is an array of per-tx traces.
            // debug_traceCallMany returns result[bundle_idx][tx_idx].
            // We have 1 bundle with 2 txs: result[0][0]=loadTable, result[0][1]=userTx.
            let bundle_traces = match resp
                .get("result")
                .and_then(|r| r.get(0))
                .and_then(|b| b.as_array())
            {
                Some(arr) if arr.len() >= 2 => arr,
                _ => {
                    if let Some(error) = resp.get("error") {
                        tracing::warn!(
                            target: "based_rollup::proxy",
                            ?error,
                            "L2 traceCallMany returned error"
                        );
                    }
                    break;
                }
            };

            // Check loadTable result.
            let tx0 = &bundle_traces[0];
            if tx0.get("error").is_some() {
                tracing::warn!(
                    target: "based_rollup::proxy",
                    "loadExecutionTable reverted in L2 traceCallMany"
                );
                // Still try to walk user tx trace for new calls.
            }

            // Log user tx trace status for debugging.
            let user_trace = &bundle_traces[1];
            let user_error = user_trace
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            let user_output = user_trace
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            tracing::info!(
                target: "based_rollup::proxy",
                user_error,
                user_output_prefix = &user_output[..user_output.len().min(20)],
                "L2 traceCallMany: user tx trace status"
            );

            // Step 6: Walk user tx trace for new calls using generic walker.
            let new_detected = walk_l2_trace_generic(
                client,
                upstream_url,
                cross_chain_manager_address,
                user_trace,
                &mut proxy_cache,
            )
            .await;

            // Step 7: Find truly new calls using count-based comparison.
            // A call is "new" only if new_detected has MORE of that
            // (dest, calldata, value, source_address) tuple than all_calls —
            // supports legitimate duplicate calls (e.g., CallTwice calling
            // increment() twice). The CALL action hash includes value and
            // sourceAddress, so two calls with different ETH values or from
            // different sources are distinct.
            let new_calls = filter_new_by_count(new_detected, &all_calls, |a, b| {
                a.destination == b.destination
                    && a.calldata == b.calldata
                    && a.value == b.value
                    && a.source_address == b.source_address
            });

            if new_calls.is_empty() {
                tracing::info!(
                    target: "based_rollup::proxy",
                    iteration,
                    total = all_calls.len(),
                    "iterative L2 discovery converged — no new calls found"
                );
                break;
            }

            tracing::info!(
                target: "based_rollup::proxy",
                iteration,
                new = new_calls.len(),
                "discovered new L2→L1 calls via L2 traceCallMany"
            );

            all_calls.extend(new_calls);
        }

        // Update detected_calls with the full set from iterative discovery.
        detected_calls = all_calls;

        tracing::info!(
            target: "based_rollup::proxy",
            count = detected_calls.len(),
            "iterative L2 discovery complete"
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
                "iterative discovery final call"
            );
        }
    }

    // Route through the appropriate queuing path based on call count.
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
        tracing::info!(
            target: "based_rollup::composer_rpc::l2_to_l1",
            count = detected_calls.len(),
            "duplicate calls detected — routing independently with chained simulation"
        );
        return queue_independent_calls_l2_to_l1(
            client,
            l1_rpc_url,
            upstream_url,
            raw_tx_hex,
            &detected_calls,
            rollups_address,
            rollup_id,
        )
        .await;
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
        let mut all_return_calls: Vec<DetectedReturnCall> = Vec::new();
        let mut current_l2_calls = detected_calls.clone();

        for depth in 0..MAX_RECURSIVE_DEPTH {
            // Phase A: Simulate current L2→L1 calls on L1.
            let call_refs: Vec<&DetectedL2InternalCall> = current_l2_calls.iter().collect();
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
                &tx_bytes,
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
            let new_return_calls: Vec<DetectedReturnCall> = sim_results_vec
                .into_iter()
                .flat_map(|(_data, _failed, rcs)| rcs)
                .map(|mut rc| {
                    if let Some(ref mut idx) = rc.parent_call_index {
                        *idx += global_offset;
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
        )
        .await
        .is_some();
    }

    // Single call path: simulate on L1 to get delivery data + return calls.
    let call = &detected_calls[0];
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
        &tx_bytes,
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
                let call_refs: Vec<&DetectedL2InternalCall> = nested_l2_calls.iter().collect();
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
                    &tx_bytes,
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
                let rcs: Vec<DetectedReturnCall> = sim_results_vec
                    .into_iter()
                    .flat_map(|(_data, _failed, rcs)| rcs)
                    .map(|mut rc| {
                        if let Some(ref mut idx) = rc.parent_call_index {
                            *idx += global_offset;
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
                if rc_clone.parent_call_index.is_none() {
                    rc_clone.parent_call_index = Some(0);
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
                if rc_clone.l2_return_data.is_empty() && !rc_clone.l2_delivery_failed {
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
                                        // Use protocol-generic proxy detection to check if the
                                        // revert involves a cross-chain proxy call. Walk the
                                        // trace with authorizedProxies queries instead of
                                        // matching ExecutionNotFound output heuristically.
                                        let mut discovered_in_phase_b = Vec::new();
                                        // find_failed_proxy_calls_in_l2_trace uses its own
                                        // cache type — separate from the trace::ProxyInfo cache.
                                        let mut fpc_cache: std::collections::HashMap<
                                            Address,
                                            Option<(Address, u64)>,
                                        > = std::collections::HashMap::new();
                                        find_failed_proxy_calls_in_l2_trace(
                                            client,
                                            upstream_url,
                                            cross_chain_manager_address,
                                            trace,
                                            rollup_id,
                                            &mut fpc_cache,
                                            &mut discovered_in_phase_b,
                                        )
                                        .await;

                                        let has_reverted_proxies =
                                            discovered_in_phase_b.iter().any(|d| d.reverted);

                                        if has_reverted_proxies {
                                            // Reverted proxy calls found — a wrapper contract
                                            // needs table entries loaded. Leave l2_return_data
                                            // empty and l2_delivery_failed false so
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
                                            rc_clone.l2_delivery_failed = true;
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
                                                rc_clone.l2_return_data = bytes;
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
            )
            .await
            .is_some();
        }
    }

    // Simple single-call path (no depth > 1): use initiateL2CrossChainCall
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
            "rawL2Tx": raw_tx_hex
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
    return_calls: &[DetectedReturnCall],
    rollup_id: u64,
) -> Vec<DetectedL2InternalCall> {
    if return_calls.is_empty() {
        return Vec::new();
    }

    tracing::info!(
        target: "based_rollup::proxy",
        return_call_count = return_calls.len(),
        "simulating L1→L2 return calls on L2 to detect depth > 1 L2→L1 calls"
    );

    let mut all_detected: Vec<DetectedL2InternalCall> = Vec::new();
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
            "simulating return call on L2 (simple trace: from=CCM, to=destination, data=calldata)"
        );

        // Phase 1: Simple trace WITHOUT loadExecutionTable to discover calls even in reverts.
        // We trace the return call's execution directly (from=CCM, to=destination, data=calldata).
        // _processCallAtScope uses try/catch, so the destination contract doesn't revert
        // even when the nested proxy call fails — callTracer shows the subcalls.
        //
        // Phase 2: If phase 1 finds nothing, try with loadExecutionTable pre-loading.
        // Build dummy entries for the return call so the execution doesn't revert,
        // then trace again.

        // Try simple trace first (fast path — works because _processCallAtScope try/catch).
        let simple_trace_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCall",
            "params": [
                {
                    "from": format!("{ccm_address}"),
                    "to": format!("{}", rc.destination),
                    "data": format!("0x{}", hex::encode(&rc.data)),
                    "value": format!("0x{:x}", rc.value),
                    "gas": "0x1c9c380"
                },
                "latest",
                { "tracer": "callTracer" }
            ],
            "id": 99970
        });

        let mut detected_for_call: Vec<DetectedL2InternalCall> = Vec::new();

        if let Ok(resp) = client.post(l2_rpc_url).json(&simple_trace_req).send().await {
            if let Ok(body) = resp.json::<Value>().await {
                if let Some(trace) = body.get("result") {
                    detected_for_call = walk_l2_trace_generic(
                        client,
                        l2_rpc_url,
                        ccm_address,
                        trace,
                        &mut proxy_cache,
                    )
                    .await;
                }
            }
        }

        // If simple trace found nothing and the call likely reverted, try with
        // loadExecutionTable pre-loading via traceCallMany.
        if detected_for_call.is_empty() {
            tracing::debug!(
                target: "based_rollup::proxy",
                idx = i,
                destination = %rc.destination,
                "simple trace found no nested calls — trying traceCallMany with loadExecutionTable"
            );

            // Build placeholder L2 entries. We don't know the exact nested call yet,
            // but we can build a generic CALL+RESULT entry for the return call itself.
            // This makes the top-level execution succeed (the return call's scope
            // navigation consumes the entry), revealing nested proxy calls.
            let placeholder_entries = crate::cross_chain::build_l2_to_l1_call_entries(
                rc.destination,
                rc.data.clone(),
                rc.value,
                rc.source_address,
                rollup_id,
                vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
                vec![],     // delivery_return_data placeholder
                false,      // delivery_failed placeholder
            );

            if !placeholder_entries.l2_table_entries.is_empty() {
                let load_calldata = crate::cross_chain::encode_load_execution_table_calldata(
                    &placeholder_entries.l2_table_entries,
                );
                let load_data = format!("0x{}", hex::encode(load_calldata.as_ref()));

                let bundle_trace_req = serde_json::json!({
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
                                        "from": format!("{ccm_address}"),
                                        "to": format!("{}", rc.destination),
                                        "data": format!("0x{}", hex::encode(&rc.data)),
                                        "value": format!("0x{:x}", rc.value),
                                        "gas": "0x2faf080"
                                    }
                                ]
                            }
                        ],
                        null,
                        { "tracer": "callTracer" }
                    ],
                    "id": 99971
                });

                if let Ok(resp) = client.post(l2_rpc_url).json(&bundle_trace_req).send().await {
                    if let Ok(body) = resp.json::<Value>().await {
                        // Extract tx1 trace (the return call execution with entries loaded)
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
