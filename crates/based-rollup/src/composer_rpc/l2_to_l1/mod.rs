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

mod enrichment;
mod process;

// Re-export for cross-module callers
pub(crate) use enrichment::enrich_return_calls_via_l2_trace;

use alloy_primitives::Address;
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
use super::model::{L2ProxyLookup, ReturnEdge};


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
    let mut detected_calls = enrichment::walk_l2_trace_generic(
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


    process::process_l2_to_l1_calls(
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

// cors_response and error_response are in super::common (re-exported above).

#[cfg(test)]
#[path = "../l2_to_l1_tests.rs"]
mod tests;
