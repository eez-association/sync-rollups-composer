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

use crate::cross_chain::{IBridgeView, filter_new_by_count};
use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
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

/// The MAINNET_ROLLUP_ID — L1 source rollup is always 0.
const MAINNET_ROLLUP_ID: u64 = 0;

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
    bridge_l2_address: Address,
    bridge_l1_address: Address,
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
                    bridge_l2_address,
                    bridge_l1_address,
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
    bridge_l2_address: Address,
    bridge_l1_address: Address,
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
                        bridge_l2_address,
                        bridge_l1_address,
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
    bridge_l2_address: Address,
    bridge_l1_address: Address,
    cross_chain_manager_address: Address,
) -> eyre::Result<Option<String>> {
    // Decode the raw transaction
    let tx_obj = decode_raw_tx_for_trace(raw_tx)?;

    let to_addr = match tx_obj.get("to").and_then(|v| v.as_str()) {
        Some(s) => match s.parse::<Address>() {
            Ok(a) => a,
            Err(_) => return Ok(None),
        },
        None => return Ok(None), // Contract creation, not cross-chain
    };

    let from_addr = tx_obj
        .get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Address>().ok())
        .unwrap_or(Address::ZERO);

    let user_calldata = tx_obj.get("data").and_then(|v| v.as_str()).unwrap_or("0x");
    let user_calldata_bytes =
        hex_decode(user_calldata.strip_prefix("0x").unwrap_or(user_calldata)).unwrap_or_default();

    // Extract tx value (ETH amount). For bridgeEther, this is the deposited ETH.
    // The value MUST be included in the CALL action for the action hash to match
    // what Rollups.sol computes during executeCrossChainCall.
    let tx_value = tx_obj
        .get("value")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            alloy_primitives::U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok()
        })
        .unwrap_or(alloy_primitives::U256::ZERO);

    // ── Pass 1: Detect — check if `to` is a CrossChainProxy or Bridge ──
    // Returns (destination, rollup_id, inner_calldata, source_address).
    // For direct proxy calls, source_address = from_addr (the tx sender).
    // For bridge calls, source_address = bridge contract (which calls the proxy).
    let (destination, dest_rollup_id, inner_calldata_bytes, source_address) =
        match detect_cross_chain_proxy(client, l1_rpc_url, to_addr, rollups_address).await {
            Ok(Some((dest, rid))) => (dest, rid, user_calldata_bytes.clone(), from_addr),
            Ok(None) => {
                // Not a proxy — check if it's a Bridge contract.
                match detect_bridge_call(
                    client,
                    l1_rpc_url,
                    to_addr,
                    rollups_address,
                    from_addr,
                    &user_calldata_bytes,
                )
                .await
                {
                    Ok(Some(info)) => {
                        // bridgeEther is fine with the fast path — inner calldata is
                        // empty (pure ETH transfer via value), so the action hash matches.
                        //
                        // bridgeTokens MUST use the slow path because Bridge internally
                        // queries token metadata (name, symbol, decimals) and constructs
                        // a receiveTokens(...) calldata that differs from the raw
                        // bridgeTokens params. The fast path returns bridgeTokens params
                        // as inner calldata, producing a wrong action hash that causes
                        // ExecutionNotFound on L2.
                        if user_calldata_bytes.len() >= 4
                            && user_calldata_bytes[..4] == BRIDGE_TOKENS_SELECTOR
                        {
                            tracing::info!(
                                target: "based_rollup::l1_proxy",
                                bridge = %to_addr,
                                sender = %from_addr,
                                "bridgeTokens detected — falling through to trace \
                                 for accurate receiveTokens calldata"
                            );
                            return trace_and_detect_internal_calls(
                                client,
                                l1_rpc_url,
                                l2_rpc_url,
                                raw_tx,
                                &tx_obj,
                                rollups_address,
                                builder_private_key,
                                rollup_id,
                                bridge_l2_address,
                                bridge_l1_address,
                                cross_chain_manager_address,
                            )
                            .await;
                        }
                        info
                    }
                    Ok(None) => {
                        // ── Slow path: trace the tx to find internal cross-chain calls ──
                        return trace_and_detect_internal_calls(
                            client,
                            l1_rpc_url,
                            l2_rpc_url,
                            raw_tx,
                            &tx_obj,
                            rollups_address,
                            builder_private_key,
                            rollup_id,
                            bridge_l2_address,
                            bridge_l1_address,
                            cross_chain_manager_address,
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::debug!(
                            target: "based_rollup::l1_proxy",
                            %e, %to_addr,
                            "bridge detection failed"
                        );
                        return Ok(None);
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    target: "based_rollup::l1_proxy",
                    %e, %to_addr,
                    "proxy detection failed"
                );
                return Ok(None);
            }
        };

    let user_calldata_bytes = inner_calldata_bytes;

    tracing::info!(
        target: "based_rollup::l1_proxy",
        proxy = %to_addr,
        %destination,
        %dest_rollup_id,
        source = %from_addr,
        calldata_len = user_calldata_bytes.len(),
        "detected cross-chain proxy call"
    );

    // Extract effective gas price from the decoded tx for state delta ordering.
    // The L1 miner orders txs by gas price descending — our chained state deltas
    // must match that order, otherwise `_findAndApplyExecution` reverts with
    // `ExecutionNotFound` because `currentState` doesn't match the on-chain root.
    let effective_gas_price = extract_gas_price_from_raw_tx(raw_tx).unwrap_or(0);

    tracing::info!(
        target: "based_rollup::l1_proxy",
        %destination,
        source = %source_address,
        effective_gas_price,
        "queuing cross-chain call with gas price for ordering"
    );

    // ── Pass 2: Queue entries + raw L1 tx atomically via initiateCrossChainCall ──
    queue_single_cross_chain_call(
        client,
        l2_rpc_url,
        raw_tx,
        &destination,
        &user_calldata_bytes,
        tx_value,
        &source_address,
        effective_gas_price,
    )
    .await
}

/// Queue a single cross-chain call entry via `syncrollups_initiateCrossChainCall`.
///
/// This is the common path for both fast-path (direct proxy/bridge) and slow-path
/// (trace-detected internal calls). Returns `Ok(Some(tx_hash))` on success.
///
/// `raw_l1_tx` should be the raw signed L1 tx for the FIRST entry from a given
/// user tx, and an empty string for subsequent entries from the same tx.
async fn queue_single_cross_chain_call(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    raw_l1_tx: &str,
    destination: &Address,
    calldata: &[u8],
    value: U256,
    source_address: &Address,
    effective_gas_price: u128,
) -> eyre::Result<Option<String>> {
    let initiate_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "syncrollups_initiateCrossChainCall",
        "params": [{
            "destination": format!("{destination}"),
            "data": format!("0x{}", hex::encode(calldata)),
            "value": format!("{value}"),
            "sourceAddress": format!("{source_address}"),
            "sourceRollup": format!("{}", U256::from(MAINNET_ROLLUP_ID)),
            "gasPrice": effective_gas_price,
            "rawL1Tx": raw_l1_tx
        }],
        "id": 99990
    });

    let resp = client
        .post(l2_rpc_url)
        .json(&initiate_req)
        .send()
        .await?
        .json::<Value>()
        .await?;

    if let Some(error) = resp.get("error") {
        return Err(eyre::eyre!("initiateCrossChainCall failed: {error}"));
    }

    let call_id = resp
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0000000000000000000000000000000000000000000000000000000000000000")
        .to_string();

    tracing::info!(
        target: "based_rollup::l1_proxy",
        %call_id,
        "cross-chain call + L1 tx queued atomically — driver will sort by gas price"
    );

    // Compute user tx hash for the JSON-RPC response
    let tx_hash = compute_tx_hash_from_raw(raw_l1_tx).unwrap_or(call_id);

    Ok(Some(tx_hash))
}

/// Queue multiple detected calls as a single continuation execution table.
/// Used for flash loans and other multi-call patterns where entries must be
/// built atomically (with L2→L1 child call detection).
async fn queue_multi_call_execution_table(
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
                call_json["l2ReturnData"] = serde_json::json!(format!("0x{}", hex::encode(&c.return_data)));
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

/// Queue N identical cross-chain calls independently, each with its own
/// CALL+RESULT pair. Uses chained simulation so each call's return data
/// reflects state changes from previous calls.
/// Direction: L1→L2 (composer RPC for deposits).
///
/// For duplicate calls (e.g., `CallTwice` calling `counter.increment()` twice),
/// the continuation path produces chained RESULT→CALL entries with hashes that
/// depend on return data. Since return data is state-dependent (counter=1 after
/// first call, counter=2 after second), each call must be routed independently
/// with its own pre-computed return data.
async fn queue_independent_calls_l1_to_l2(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    raw_tx: &str,
    detected_calls: &[DetectedInternalCall],
    effective_gas_price: u128,
    cross_chain_manager_address: Address,
) -> eyre::Result<Option<String>> {
    // 1. Run chained simulation to get correct per-call return data.
    let chained_results = simulate_chained_delivery_l1_to_l2(
        client,
        l2_rpc_url,
        cross_chain_manager_address,
        detected_calls,
    )
    .await;

    tracing::info!(
        target: "based_rollup::composer_rpc::l1_to_l2",
        call_count = detected_calls.len(),
        chained_results_count = chained_results.len(),
        "routing {} identical calls independently with chained simulation",
        detected_calls.len(),
    );

    // 2. Queue each call via initiateCrossChainCall with pre-computed return data.
    for (i, call) in detected_calls.iter().enumerate() {
        let (return_data, call_success) = if i < chained_results.len() {
            chained_results[i].clone()
        } else {
            (vec![], true) // fallback: void return, success
        };

        // First call carries the raw L1 tx; subsequent calls use empty string
        // so the driver forwards the raw tx exactly once.
        let raw_l1_tx = if i == 0 { raw_tx } else { "" };

        // Build the RPC request WITH pre-computed return data.
        let initiate_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "syncrollups_initiateCrossChainCall",
            "params": [{
                "destination": format!("{}", call.destination),
                "data": format!("0x{}", hex::encode(&call.calldata)),
                "value": format!("{}", call.value),
                "sourceAddress": format!("{}", call.source_address),
                "sourceRollup": format!("{}", U256::from(MAINNET_ROLLUP_ID)),
                "gasPrice": effective_gas_price,
                "rawL1Tx": raw_l1_tx,
                "l2ReturnData": format!("0x{}", hex::encode(&return_data)),
                "l2CallSuccess": call_success
            }],
            "id": 99960 + i as u64
        });

        let resp = match client.post(l2_rpc_url).json(&initiate_req).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::composer_rpc::l1_to_l2",
                    call_idx = i,
                    %e,
                    "initiateCrossChainCall request failed for independent call"
                );
                continue;
            }
        };
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::composer_rpc::l1_to_l2",
                    call_idx = i,
                    %e,
                    "initiateCrossChainCall response parse failed for independent call"
                );
                continue;
            }
        };

        if let Some(error) = body.get("error") {
            tracing::warn!(
                target: "based_rollup::composer_rpc::l1_to_l2",
                call_idx = i,
                ?error,
                "initiateCrossChainCall failed for independent call"
            );
            continue;
        }

        let call_id = body
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("0x");

        tracing::info!(
            target: "based_rollup::composer_rpc::l1_to_l2",
            call_idx = i,
            %call_id,
            return_data_len = return_data.len(),
            call_success,
            "independent call queued successfully"
        );
    }

    // Return the tx hash computed from the raw L1 tx (same as queue_single_cross_chain_call).
    let tx_hash = compute_tx_hash_from_raw(raw_tx)
        .unwrap_or_else(|_| "0x".to_string());

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
    /// the RESULT hash must include it (docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md §C.2).
    /// Empty when the call returns void or when simulation was not performed.
    return_data: Vec<u8>,
}

/// Simulate an L1->L2 call on L2 to capture the actual return data.
///
/// For each detected L1->L2 cross-chain call, the L2 target contract may return
/// non-void data. The RESULT action hash includes this data (per
/// docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md §C.2), so we must simulate the call on L2 to
/// predict the correct return bytes.
///
/// The simulation runs as `debug_traceCallMany` from the L2 proxy address
/// (computed via `computeCrossChainProxyAddress` on the L2 CCM) to the
/// destination contract.
///
/// Returns `(return_data, call_success)`. On simulation failure, returns
/// `(vec![], true)` as a safe fallback (void return, success).
async fn simulate_l1_to_l2_call_on_l2(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    destination: Address,
    data: &[u8],
    value: U256,
    source_address: Address,
) -> (Vec<u8>, bool) {
    // Step 1: Compute L2 proxy address for the source (L1 contract).
    // computeCrossChainProxyAddress(originalAddress, originalRollupId=0)
    // Uses typed ABI encoding — NEVER hardcode selectors.
    let proxy_from = {
        use alloy_sol_types::SolCall;
        let compute_data = crate::cross_chain::IRollups::computeCrossChainProxyAddressCall {
            originalAddress: source_address,
            originalRollupId: alloy_primitives::U256::ZERO,
        }.abi_encode();
        let compute_hex = format!("0x{}", hex::encode(&compute_data));
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{
                "to": format!("{cross_chain_manager_address}"),
                "data": compute_hex
            }, "latest"],
            "id": 99960
        });
        let resp = match client.post(l2_rpc_url).json(&req).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    %e,
                    source = %source_address,
                    "L2 proxy address lookup failed — using void return fallback"
                );
                return (vec![], true);
            }
        };
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    %e,
                    "L2 proxy address response parse failed"
                );
                return (vec![], true);
            }
        };
        if body.get("error").is_some() {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                source = %source_address,
                "computeCrossChainProxyAddress failed on L2 CCM"
            );
            return (vec![], true);
        }
        match body.get("result").and_then(|v| v.as_str()) {
            Some(s) => {
                let clean = s.strip_prefix("0x").unwrap_or(s);
                if clean.len() >= 64 {
                    format!("0x{}", &clean[24..64])
                } else {
                    tracing::warn!(
                        target: "based_rollup::l1_proxy",
                        source = %source_address,
                        "L2 proxy address return too short"
                    );
                    return (vec![], true);
                }
            }
            None => return (vec![], true),
        }
    };

    // Step 2: Simulate the call on L2 via debug_traceCallMany.
    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCallMany",
        "params": [[{
            "transactions": [{
                "from": proxy_from,
                "to": format!("{destination}"),
                "data": format!("0x{}", hex::encode(data)),
                "value": format!("0x{:x}", value),
                "gas": "0x2faf080"
            }]
        }], null, { "tracer": "callTracer" }],
        "id": 99961
    });

    let resp = match client.post(l2_rpc_url).json(&trace_req).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                dest = %destination,
                "L2 call simulation (debug_traceCallMany) request failed"
            );
            return (vec![], true);
        }
    };
    let body: Value = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                dest = %destination,
                "L2 call simulation response parse failed"
            );
            return (vec![], true);
        }
    };

    // Extract output from trace: result[0][0].output
    let trace = match body
        .get("result")
        .and_then(|r| r.get(0))
        .and_then(|b| b.as_array())
        .and_then(|arr| arr.first())
    {
        Some(t) => t,
        None => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                dest = %destination,
                "L2 call simulation returned no trace"
            );
            return (vec![], true);
        }
    };

    // Save L2 simulation trace for debugging
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let _ = std::fs::write(
        format!("/tmp/trace_l2sim_{ts}.json"),
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    );
    tracing::info!(
        target: "based_rollup::l1_proxy::debug256",
        dest = %destination,
        source = %source_address,
        file = %format!("/tmp/trace_l2sim_{ts}.json"),
        has_error = trace.get("error").is_some(),
        output_len = trace.get("output").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0),
        "simulate_l1_to_l2_call_on_l2 trace saved"
    );

    // Check for revert.
    let has_error = trace.get("error").is_some() || trace.get("revertReason").is_some();
    if has_error {
        let error = trace
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        // For non-leaf L1→L2 calls (where the L2 target calls another cross-chain
        // proxy), the trace reverts with ExecutionNotFound because no entries are
        // loaded. Walk the trace to find failed proxy calls; if any are found, build
        // placeholder entries and retry with loadExecutionTable pre-loaded.
        if !cross_chain_manager_address.is_zero() {
            let rollup_id = 1u64; // L2 rollup ID (same as used elsewhere in l1_proxy)
            let mut proxy_cache: HashMap<Address, Option<(Address, u64)>> = HashMap::new();
            let mut discovered = Vec::new();
            crate::composer_rpc::l2_to_l1::find_failed_proxy_calls_in_l2_trace(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                trace,
                rollup_id,
                &mut proxy_cache,
                &mut discovered,
            )
            .await;

            let reverted_proxies: Vec<_> =
                discovered.into_iter().filter(|d| d.reverted).collect();

            if !reverted_proxies.is_empty() {
                tracing::info!(
                    target: "based_rollup::l1_proxy",
                    dest = %destination,
                    source = %source_address,
                    reverted_proxy_count = reverted_proxies.len(),
                    "L2 call simulation reverted with {} failed proxy call(s) — \
                     retrying with loadExecutionTable",
                    reverted_proxies.len()
                );

                // Build placeholder L2 entries for all reverted proxy calls.
                let mut all_placeholder_entries = Vec::new();
                for rp in &reverted_proxies {
                    let placeholder = crate::cross_chain::build_l2_to_l1_call_entries(
                        rp.original_address,
                        rp.data.clone(),
                        rp.value,
                        rp.source_address,
                        rp.source_address,
                        rollup_id,
                        Address::ZERO,
                        vec![],
                        false,
                    );
                    all_placeholder_entries.extend(placeholder.l2_table_entries);
                }

                if !all_placeholder_entries.is_empty() {
                    // Query SYSTEM_ADDRESS from the CCM.
                    let system_addr = {
                        let sys_req = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "eth_call",
                            "params": [{
                                "to": format!("{cross_chain_manager_address}"),
                                "data": "0xbe890557"
                            }, "latest"],
                            "id": 99970
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

                    if let Some(sys_addr) = system_addr {
                        let load_calldata =
                            crate::cross_chain::encode_load_execution_table_calldata(
                                &all_placeholder_entries,
                            );
                        let load_data =
                            format!("0x{}", hex::encode(load_calldata.as_ref()));
                        let ccm_hex = format!("{cross_chain_manager_address}");

                        let retry_req = serde_json::json!({
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
                                        "from": proxy_from,
                                        "to": format!("{destination}"),
                                        "data": format!("0x{}", hex::encode(data)),
                                        "value": format!("0x{:x}", value),
                                        "gas": "0x2faf080"
                                    }
                                ]
                            }], null, { "tracer": "callTracer" }],
                            "id": 99971
                        });

                        if let Ok(resp2) = client.post(l2_rpc_url).json(&retry_req).send().await {
                            if let Ok(body2) = resp2.json::<Value>().await {
                                if let Some(traces) = body2
                                    .get("result")
                                    .and_then(|r| r.get(0))
                                    .and_then(|b| b.as_array())
                                {
                                    if traces.len() >= 2 {
                                        let t2 = &traces[1];
                                        let t2_error = t2.get("error").is_some();
                                        if t2_error {
                                            tracing::info!(
                                                target: "based_rollup::l1_proxy",
                                                dest = %destination,
                                                "L2 call still reverts after loadExecutionTable — marking as failed"
                                            );
                                            let revert_data = t2
                                                .get("output")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| {
                                                    hex::decode(
                                                        s.strip_prefix("0x").unwrap_or(s),
                                                    )
                                                    .ok()
                                                })
                                                .unwrap_or_default();
                                            return (revert_data, false);
                                        }
                                        // Success — extract return data from the retry trace.
                                        let retry_output = t2
                                            .get("output")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("0x");
                                        let retry_data = hex::decode(
                                            retry_output
                                                .strip_prefix("0x")
                                                .unwrap_or(retry_output),
                                        )
                                        .unwrap_or_default();
                                        tracing::info!(
                                            target: "based_rollup::l1_proxy",
                                            dest = %destination,
                                            source = %source_address,
                                            return_data_len = retry_data.len(),
                                            "non-leaf L2 call succeeded after loadExecutionTable retry"
                                        );
                                        return (retry_data, true);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        tracing::info!(
            target: "based_rollup::l1_proxy",
            dest = %destination,
            source = %source_address,
            error,
            "L2 call simulation reverted — marking call as failed"
        );
        // Capture revert data from output.
        let revert_data = trace
            .get("output")
            .and_then(|v| v.as_str())
            .and_then(|s| hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok())
            .unwrap_or_default();
        return (revert_data, false);
    }

    // Extract return data from output.
    let output = trace
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap_or("0x");
    let return_data = hex::decode(output.strip_prefix("0x").unwrap_or(output)).unwrap_or_default();

    tracing::info!(
        target: "based_rollup::l1_proxy",
        dest = %destination,
        source = %source_address,
        return_data_len = return_data.len(),
        return_data_hex = %format!("0x{}", hex::encode(&return_data[..return_data.len().min(64)])),
        "L2 call simulation succeeded"
    );

    (return_data, true)
}

/// Simulate N identical cross-chain calls with state accumulation on L2.
///
/// Each call sees state effects from previous calls (e.g., counter=1 after
/// first increment). Uses `debug_traceCallMany` with N txs in one bundle.
/// Direction: L1->L2 (delivery happens on L2).
///
/// Returns `Vec<(return_data, call_success)>` with one entry per call in
/// the input slice. On any transport/parse failure, falls back to per-call
/// `simulate_l1_to_l2_call_on_l2` (existing function).
async fn simulate_chained_delivery_l1_to_l2(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    calls: &[DetectedInternalCall],
) -> Vec<(Vec<u8>, bool)> {
    if calls.is_empty() {
        return vec![];
    }

    // All identical calls share the same source_address, so compute the L2 proxy
    // address ONCE via computeCrossChainProxyAddress on the L2 CCM.
    let source_address = calls[0].source_address;
    let proxy_from = {
        use alloy_sol_types::SolCall;
        let compute_data = crate::cross_chain::IRollups::computeCrossChainProxyAddressCall {
            originalAddress: source_address,
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
            "id": 99950
        });
        let resp = match client.post(l2_rpc_url).json(&req).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    %e,
                    source = %source_address,
                    "chained L2 proxy address lookup failed — falling back to per-call simulation"
                );
                return fallback_per_call_simulation(client, l2_rpc_url, cross_chain_manager_address, calls).await;
            }
        };
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    %e,
                    "chained L2 proxy address response parse failed — falling back"
                );
                return fallback_per_call_simulation(client, l2_rpc_url, cross_chain_manager_address, calls).await;
            }
        };
        if body.get("error").is_some() {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                source = %source_address,
                "computeCrossChainProxyAddress failed on L2 CCM — falling back"
            );
            return fallback_per_call_simulation(client, l2_rpc_url, cross_chain_manager_address, calls).await;
        }
        match body.get("result").and_then(|v| v.as_str()) {
            Some(s) => {
                let clean = s.strip_prefix("0x").unwrap_or(s);
                if clean.len() >= 64 {
                    format!("0x{}", &clean[24..64])
                } else {
                    tracing::warn!(
                        target: "based_rollup::l1_proxy",
                        source = %source_address,
                        "chained L2 proxy address return too short — falling back"
                    );
                    return fallback_per_call_simulation(client, l2_rpc_url, cross_chain_manager_address, calls).await;
                }
            }
            None => {
                return fallback_per_call_simulation(client, l2_rpc_url, cross_chain_manager_address, calls).await;
            }
        }
    };

    // Build debug_traceCallMany request with ONE bundle containing N transactions.
    // Each tx in the bundle sees state effects from the previous ones.
    let transactions: Vec<Value> = calls
        .iter()
        .map(|call| {
            serde_json::json!({
                "from": &proxy_from,
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
        "params": [[{
            "transactions": transactions
        }], null, { "tracer": "callTracer" }],
        "id": 99951
    });

    tracing::info!(
        target: "based_rollup::l1_proxy",
        num_calls = calls.len(),
        proxy = %proxy_from,
        "chained L1→L2 delivery simulation: debug_traceCallMany with {} txs in one bundle",
        calls.len()
    );

    let resp = match client.post(l2_rpc_url).json(&trace_req).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                "chained L2 simulation (debug_traceCallMany) request failed — falling back"
            );
            return fallback_per_call_simulation(client, l2_rpc_url, cross_chain_manager_address, calls).await;
        }
    };
    let body: Value = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "based_rollup::l1_proxy",
                %e,
                "chained L2 simulation response parse failed — falling back"
            );
            return fallback_per_call_simulation(client, l2_rpc_url, cross_chain_manager_address, calls).await;
        }
    };

    // Save trace for debugging.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = std::fs::write(
        format!("/tmp/trace_chained_l1_to_l2_{ts}.json"),
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    );
    tracing::info!(
        target: "based_rollup::l1_proxy::debug256",
        file = %format!("/tmp/trace_chained_l1_to_l2_{ts}.json"),
        num_calls = calls.len(),
        "saved chained L1→L2 simulation trace"
    );

    // Parse the response. Structure: result[0] is an array of N trace objects
    // (one per tx in the bundle).
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
                target: "based_rollup::l1_proxy",
                expected = calls.len(),
                actual = actual_len,
                "chained L2 simulation returned unexpected trace count — falling back"
            );
            return fallback_per_call_simulation(client, l2_rpc_url, cross_chain_manager_address, calls).await;
        }
    };

    // Extract per-call results from each trace.
    let mut results = Vec::with_capacity(calls.len());
    for (i, trace) in traces.iter().enumerate() {
        let has_error = trace.get("error").is_some() || trace.get("revertReason").is_some();
        let output_hex = trace
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("0x");
        let hex_clean = output_hex.strip_prefix("0x").unwrap_or(output_hex);
        let output_bytes = hex::decode(hex_clean).unwrap_or_default();

        let success = !has_error;

        tracing::info!(
            target: "based_rollup::l1_proxy",
            idx = i,
            success,
            return_data_len = output_bytes.len(),
            "chained L1→L2 simulation: call {} result",
            i
        );

        results.push((output_bytes, success));
    }

    results
}

/// Fallback: simulate each call independently using `simulate_l1_to_l2_call_on_l2`.
///
/// Used when the chained simulation fails (transport error, parse error, etc.).
async fn fallback_per_call_simulation(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    cross_chain_manager_address: Address,
    calls: &[DetectedInternalCall],
) -> Vec<(Vec<u8>, bool)> {
    tracing::info!(
        target: "based_rollup::l1_proxy",
        num_calls = calls.len(),
        "falling back to per-call L2 simulation"
    );
    let mut results = Vec::with_capacity(calls.len());
    for call in calls {
        let (data, success) = simulate_l1_to_l2_call_on_l2(
            client,
            l2_rpc_url,
            cross_chain_manager_address,
            call.destination,
            &call.calldata,
            call.value,
            call.source_address,
        )
        .await;
        results.push((data, success));
    }
    results
}

/// Trace a transaction using `debug_traceCall` with `callTracer` and detect
/// internal calls to CrossChainProxy or Bridge contracts.
///
/// This is the slow path, invoked only when the top-level `to` is neither a
/// proxy nor a bridge. It simulates the tx and walks the call tree recursively
/// to find ALL internal calls to proxies or bridges.
///
/// Returns `Ok(Some(tx_hash))` if internal cross-chain calls were found and queued.
/// Returns `Ok(None)` if no internal cross-chain calls were detected.
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
    bridge_l2_address: Address,
    bridge_l1_address: Address,
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
    // that would execute after entries are posted (flash loan pattern).
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
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
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
            .map(|s| s.starts_with("0x08c379a0")) // Error(string) selector
            .unwrap_or(false);

    // Walk the trace tree recursively to find cross-chain calls.
    // Cache proxy lookups to avoid repeated eth_call for the same address.
    let mut proxy_cache: HashMap<Address, Option<(Address, u64)>> = HashMap::new();
    let mut detected_calls: Vec<DetectedInternalCall> = Vec::new();

    walk_trace_tree(
        client,
        l1_rpc_url,
        rollups_address,
        &trace_result,
        &mut proxy_cache,
        &mut detected_calls,
    )
    .await;

    // Deduplicate: when both a bridge call and its proxy child are detected
    // for the same destination, keep the proxy detection (it has the correct
    // receiveTokens calldata, not bridgeTokens params).
    // A proxy detection is identified by having calldata that starts with the
    // receiveTokens selector (differs from bridgeTokens/bridgeEther selectors).
    if detected_calls.len() > 1 {
        let mut seen_destinations: std::collections::HashSet<Address> =
            std::collections::HashSet::new();
        let mut deduped: Vec<DetectedInternalCall> = Vec::new();
        // First pass: collect proxy-detected destinations
        for call in &detected_calls {
            if call.calldata.len() >= 4 {
                let sel: [u8; 4] = call.calldata[..4].try_into().unwrap_or([0; 4]);
                if sel != BRIDGE_ETHER_SELECTOR && sel != BRIDGE_TOKENS_SELECTOR {
                    // This is a proxy detection (not a bridge selector)
                    seen_destinations.insert(call.destination);
                }
            }
        }
        // Second pass: keep proxy detections, skip bridge detections for same destination
        for call in detected_calls.drain(..) {
            if call.calldata.len() >= 4 {
                let sel: [u8; 4] = call.calldata[..4].try_into().unwrap_or([0; 4]);
                if (sel == BRIDGE_ETHER_SELECTOR || sel == BRIDGE_TOKENS_SELECTOR)
                    && seen_destinations.contains(&call.destination)
                {
                    tracing::debug!(
                        target: "based_rollup::l1_proxy",
                        dest = %call.destination,
                        "deduplicating bridge detection (proxy detection preferred)"
                    );
                    continue;
                }
            }
            deduped.push(call);
        }
        detected_calls = deduped;
    }

    // Enrich detected calls with L2 return data by simulating each L1→L2 call
    // on L2. The RESULT action hash includes the exact return bytes from the
    // target contract (docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md §C.2).
    // Skip bridge receiveTokens calls — they always return void.
    let receive_tokens_selector: [u8; 4] = [0x6b, 0x39, 0x96, 0xb0];
    if !cross_chain_manager_address.is_zero() {
        for call in &mut detected_calls {
            let is_bridge_receive_tokens = call.calldata.len() >= 4
                && call.calldata[..4] == receive_tokens_selector
                && call.destination == bridge_l2_address;
            if is_bridge_receive_tokens {
                // receiveTokens always returns void — skip simulation.
                continue;
            }
            let (ret_data, success) = simulate_l1_to_l2_call_on_l2(
                client,
                l2_rpc_url,
                cross_chain_manager_address,
                call.destination,
                &call.calldata,
                call.value,
                call.source_address,
            )
            .await;
            tracing::info!(
                target: "based_rollup::l1_proxy::debug256",
                dest = %call.destination,
                source = %call.source_address,
                return_data_len = ret_data.len(),
                call_success = success,
                return_data_hex = %if ret_data.is_empty() { "EMPTY".to_string() } else { format!("0x{}", hex::encode(&ret_data[..std::cmp::min(ret_data.len(), 64)])) },
                "simulate_l1_to_l2_call_on_l2 result"
            );
            call.return_data = ret_data;
            call.call_success = success;
        }
    }

    // If initial trace found calls but tx reverts (flash loan pattern), iterate
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

                    let analyzed = crate::table_builder::analyze_continuation_calls(
                        &l1_detected,
                        rollup_id,
                        bridge_l2_address,
                        bridge_l1_address,
                    );

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
                    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
                    let _ = std::fs::write(format!("/tmp/trace_req_iter{iteration}_{ts}.json"), &req_json);

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
                    let _ = std::fs::write(format!("/tmp/trace_resp_iter{iteration}_{ts}.json"), &resp_json);
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
                            .unwrap_or_default().chars().take(3000).collect();
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
                    // Decode known Rollups.sol error selectors from output
                    let decoded_error = match user_output_raw.get(..10).or(user_output_raw.get(..))
                    {
                        Some("0xed6bc750") => "ExecutionNotFound",
                        Some("0xd4bae993") => "InvalidRevertData",
                        Some("0x622d0c4a") => "StateAlreadyUpdatedThisBlock",
                        Some("0x1b2075cd") => "StateRootMismatch",
                        Some("0xde315ee4") => "EtherDeltaMismatch",
                        Some("0x6b3b6576") => "CallExecutionFailed",
                        Some("0xe53dc94a") => "UnauthorizedProxy",
                        Some("0x096aa082") => "CallReverted(inner)",
                        _ => "unknown",
                    };
                    // If CallReverted, decode inner error
                    let inner_error = if user_output_raw.len() > 138 {
                        // Inner error selector at bytes 68..72 (hex chars 136..144, after 0x prefix = 138..146)
                        match user_output_raw.get(138..146) {
                            Some("ed6bc750") => "ExecutionNotFound",
                            Some("d4bae993") => "InvalidRevertData",
                            Some("622d0c4a") => "StateAlreadyUpdatedThisBlock",
                            Some("1b2075cd") => "StateRootMismatch",
                            Some("de315ee4") => "EtherDeltaMismatch",
                            Some("6b3b6576") => "CallExecutionFailed",
                            _ => "",
                        }
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
                    let mut new_detected: Vec<DetectedInternalCall> = Vec::new();
                    walk_trace_tree(
                        client,
                        l1_rpc_url,
                        rollups_address,
                        user_trace,
                        &mut proxy_cache,
                        &mut new_detected,
                    )
                    .await;

                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        new_detected_count = new_detected.len(),
                        "walked user tx trace for cross-chain calls"
                    );

                    // debug256: dump walk_trace_tree output
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

                    // Dedup new results (prefer proxy detections over bridge detections)
                    if new_detected.len() > 1 {
                        let mut seen: std::collections::HashSet<Address> =
                            std::collections::HashSet::new();
                        let mut deduped: Vec<DetectedInternalCall> = Vec::new();
                        for call in &new_detected {
                            if call.calldata.len() >= 4 {
                                let sel: [u8; 4] = call.calldata[..4].try_into().unwrap_or([0; 4]);
                                if sel != BRIDGE_ETHER_SELECTOR && sel != BRIDGE_TOKENS_SELECTOR {
                                    seen.insert(call.destination);
                                }
                            }
                        }
                        for call in new_detected.drain(..) {
                            if call.calldata.len() >= 4 {
                                let sel: [u8; 4] = call.calldata[..4].try_into().unwrap_or([0; 4]);
                                if (sel == BRIDGE_ETHER_SELECTOR || sel == BRIDGE_TOKENS_SELECTOR)
                                    && seen.contains(&call.destination)
                                {
                                    continue;
                                }
                            }
                            deduped.push(call);
                        }
                        new_detected = deduped;
                    }

                    // Find truly new calls using count-based comparison.
                    // A call is "new" only if new_detected has MORE of that
                    // (dest, calldata, value, source_address) tuple than all_calls —
                    // supports legitimate duplicate calls (e.g., CallTwice calling
                    // increment() twice). The CALL action hash includes value and
                    // sourceAddress, so two calls to the same proxy with different
                    // ETH values or from different sources are distinct.
                    let new_calls = filter_new_by_count(
                        new_detected,
                        &all_calls,
                        |a, b| {
                            a.destination == b.destination
                                && a.calldata == b.calldata
                                && a.value == b.value
                                && a.source_address == b.source_address
                        },
                    );

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
                    let mut enriched_new_calls = new_calls;
                    if !cross_chain_manager_address.is_zero() {
                        for call in &mut enriched_new_calls {
                            let is_bridge_receive_tokens = call.calldata.len() >= 4
                                && call.calldata[..4] == receive_tokens_selector
                                && call.destination == bridge_l2_address;
                            if is_bridge_receive_tokens {
                                continue;
                            }
                            let (ret_data, success) = simulate_l1_to_l2_call_on_l2(
                                client,
                                l2_rpc_url,
                                cross_chain_manager_address,
                                call.destination,
                                &call.calldata,
                                call.value,
                                call.source_address,
                            )
                            .await;
                            call.return_data = ret_data;
                            call.call_success = success;
                        }
                    }
                    all_calls.extend(enriched_new_calls);
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
        target: "based_rollup::l1_proxy",
        count = detected_calls.len(),
        "detected internal cross-chain calls via debug_traceCall"
    );

    let effective_gas_price = extract_gas_price_from_raw_tx(raw_tx).unwrap_or(0);

    // Check for duplicate calls (same action identity). Identical calls must
    // route independently — the continuation path produces chained RESULT→CALL
    // entries with return-data-dependent hashes that break for identical calls
    // because return data is state-dependent (#256).
    let has_duplicates = crate::cross_chain::has_duplicate_calls(
        &detected_calls
            .iter()
            .map(|c| (c.destination, c.calldata.as_slice(), c.value, c.source_address))
            .collect::<Vec<_>>(),
    );

    if detected_calls.len() >= 2 && has_duplicates {
        tracing::info!(
            target: "based_rollup::composer_rpc::l1_to_l2",
            count = detected_calls.len(),
            "duplicate calls detected — routing independently with chained simulation"
        );
        return queue_independent_calls_l1_to_l2(
            client,
            l2_rpc_url,
            raw_tx,
            &detected_calls,
            effective_gas_price,
            cross_chain_manager_address,
        )
        .await;
    }

    // Multi-call detection: if 2+ calls found, use buildExecutionTable for
    // continuation entry generation (flash loans). Single calls use legacy path.
    if detected_calls.len() >= 2 {
        tracing::info!(
            target: "based_rollup::l1_proxy",
            count = detected_calls.len(),
            "multi-call tx detected — using buildExecutionTable for continuation entries"
        );

        return queue_multi_call_execution_table(
            client,
            l2_rpc_url,
            raw_tx,
            &detected_calls,
            effective_gas_price,
        )
        .await;
    }

    let mut final_tx_hash: Option<String> = None;

    for (i, call) in detected_calls.iter().enumerate() {
        // Only the first entry carries the raw L1 tx; the rest have empty rawL1Tx
        // so the driver forwards the raw tx exactly once.
        let raw_l1_tx_for_entry = if i == 0 { raw_tx } else { "" };

        tracing::info!(
            target: "based_rollup::l1_proxy",
            destination = %call.destination,
            source = %call.source_address,
            value = %call.value,
            calldata_len = call.calldata.len(),
            index = i,
            "queuing internal cross-chain call"
        );

        match queue_single_cross_chain_call(
            client,
            l2_rpc_url,
            raw_l1_tx_for_entry,
            &call.destination,
            &call.calldata,
            call.value,
            &call.source_address,
            effective_gas_price,
        )
        .await
        {
            Ok(Some(hash)) => {
                if final_tx_hash.is_none() {
                    final_tx_hash = Some(hash);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    %e,
                    index = i,
                    "failed to queue internal cross-chain call"
                );
                // Continue queuing remaining calls — partial success is better
                // than total failure for multi-call traces.
            }
        }
    }

    Ok(final_tx_hash)
}

/// Recursively walk a `callTracer` trace tree, collecting any calls to
/// CrossChainProxy or Bridge contracts.
///
/// Visits nodes depth-first (matching L1 execution order). Only processes
/// nodes without an `error` field (successful calls).
///
/// Uses `proxy_cache` to avoid repeated `authorizedProxies` lookups.
async fn walk_trace_tree(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    node: &Value,
    proxy_cache: &mut HashMap<Address, Option<(Address, u64)>>,
    detected_calls: &mut Vec<DetectedInternalCall>,
) {
    // Do NOT skip failed calls. Even though they revert on-chain, the trace
    // shows what cross-chain calls the tx WOULD make. We need to detect these
    // to pre-populate the execution table. When the tx executes for real (with
    // entries loaded), the calls will succeed.

    let mut should_recurse = true;

    // --- debug256: log every node visited ---
    {
        let d256_to = node.get("to").and_then(|v| v.as_str()).unwrap_or("none");
        let d256_from = node.get("from").and_then(|v| v.as_str()).unwrap_or("none");
        let d256_calls_count = node.get("calls").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        let d256_error = node.get("error").and_then(|v| v.as_str()).unwrap_or("none");
        tracing::info!(
            target: "based_rollup::l1_proxy::debug256",
            to = %d256_to,
            from = %d256_from,
            calls_count = d256_calls_count,
            error = %d256_error,
            "walk_trace_tree visiting node"
        );
    }

    // Extract the call target
    if let Some(to_str) = node.get("to").and_then(|v| v.as_str()) {
        if let Ok(to_addr) = to_str.parse::<Address>() {
            let call_from = node
                .get("from")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Address>().ok())
                .unwrap_or(Address::ZERO);

            let input = node.get("input").and_then(|v| v.as_str()).unwrap_or("0x");
            let input_clean = input.strip_prefix("0x").unwrap_or(input);
            let input_bytes = hex_decode(input_clean).unwrap_or_default();

            let call_value = node
                .get("value")
                .and_then(|v| v.as_str())
                .and_then(|s| U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok())
                .unwrap_or(U256::ZERO);

            // Check proxy cache first, then query on miss
            let proxy_info = match proxy_cache.get(&to_addr) {
                Some(cached) => cached.clone(),
                None => {
                    let result =
                        detect_cross_chain_proxy(client, l1_rpc_url, to_addr, rollups_address)
                            .await
                            .unwrap_or(None);
                    proxy_cache.insert(to_addr, result);
                    result
                }
            };

            if let Some((destination, rollup_id)) = proxy_info {
                // This is a call to a CrossChainProxy.
                // The input is what the proxy forwards to executeCrossChainCall.
                // The from field is the caller (source_address).
                detected_calls.push(DetectedInternalCall {
                    destination,
                    _rollup_id: rollup_id,
                    calldata: input_bytes,
                    value: call_value,
                    source_address: call_from,
                    call_success: true,
                    return_data: vec![],
                });
                tracing::info!(
                    target: "based_rollup::l1_proxy::debug256",
                    proxy_addr = %to_addr,
                    original_addr = %destination,
                    rollup_id,
                    "walk_trace_tree: proxy detected — adding to detected_calls"
                );
                // Do NOT recurse into proxy children (executeL2Call etc.).
                should_recurse = false;
            } else {
                // Not a proxy — check if it's a Bridge call (bridgeEther/bridgeTokens).
                if input_bytes.len() >= 4 {
                    let selector: [u8; 4] = input_bytes[..4].try_into().unwrap_or([0; 4]);
                    if selector == BRIDGE_ETHER_SELECTOR || selector == BRIDGE_TOKENS_SELECTOR {
                        if let Ok(Some(info)) = detect_bridge_call(
                            client,
                            l1_rpc_url,
                            to_addr,
                            rollups_address,
                            call_from,
                            &input_bytes,
                        )
                        .await
                        {
                            let (destination, rollup_id, fallback_calldata, source_address) = info;

                            // Try to extract the actual proxy calldata from the trace
                            // tree instead of using detect_bridge_call's reconstructed
                            // data. Inside Bridge.bridgeTokens(), the trace looks like:
                            //
                            //   bridge.bridgeTokens()
                            //     +-- CREATE2 proxy (optional, if proxy didn't exist)
                            //     +-- proxy.fallback(data)     <-- proxy_node
                            //         +-- Rollups.executeCrossChainCall(source, data)
                            //
                            // The proxy node's `input` field contains the exact
                            // receiveTokens calldata that Rollups.sol will see.
                            // We identify the proxy node by checking if any of its
                            // children target rollups_address.
                            //
                            // This avoids querying authorizedProxies on-chain, which
                            // fails when the proxy was created inside a reverted tx
                            // (bridgeTokens reverts without a loaded execution table,
                            // so the CREATE2 proxy is never persisted).
                            let proxy_calldata =
                                extract_proxy_calldata_from_bridge_children(node, rollups_address);

                            let final_calldata = if let Some(ref trace_cd) = proxy_calldata {
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    trace_calldata_len = trace_cd.len(),
                                    fallback_calldata_len = fallback_calldata.len(),
                                    "using proxy calldata from trace instead of reconstructed bridge params"
                                );
                                trace_cd.clone()
                            } else {
                                fallback_calldata
                            };

                            detected_calls.push(DetectedInternalCall {
                                destination,
                                _rollup_id: rollup_id,
                                calldata: final_calldata,
                                value: call_value,
                                source_address,
                                call_success: true,
                                return_data: vec![],
                            });
                            // Already inspected bridge children for proxy data;
                            // do not recurse again.
                            should_recurse = false;
                        }
                    }
                }
            }
        }
    }

    // Recurse into child calls (depth-first to match L1 execution order).
    // Skip recursion for proxy nodes and bridge nodes (already handled above).
    if should_recurse {
        if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
            for child in calls {
                Box::pin(walk_trace_tree(
                    client,
                    l1_rpc_url,
                    rollups_address,
                    child,
                    proxy_cache,
                    detected_calls,
                ))
                .await;
            }
        }
    }
}

/// Walk a bridge node's children to find a proxy call and extract its calldata.
///
/// When `Bridge.bridgeTokens()` executes, the trace contains:
/// ```text
/// bridge.bridgeTokens()            <-- bridge node (passed as `node`)
///   +-- CREATE2 proxy              <-- skip (proxy deployment)
///   +-- proxy.fallback(data)       <-- proxy call: input = receiveTokens calldata
///       +-- Rollups.executeCrossChainCall(source, data)  <-- confirms this is a proxy
/// ```
///
/// We identify the proxy call by: it is a non-CREATE child whose own child
/// targets `rollups_address` (i.e. calls `executeCrossChainCall`). The proxy
/// call's `input` field is the calldata that Rollups.sol will forward to the
/// destination contract on L2.
///
/// This approach works even when the proxy was dynamically created inside a
/// reverted tx (e.g. `_getOrDeployProxy` inside `bridgeTokens`), because
/// the trace captures the internal call regardless of revert status.
fn extract_proxy_calldata_from_bridge_children(
    node: &Value,
    rollups_address: Address,
) -> Option<Vec<u8>> {
    let children = node.get("calls").and_then(|v| v.as_array())?;

    for child in children {
        // Skip CREATE/CREATE2 nodes (proxy deployment)
        let call_type = child.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if call_type == "CREATE" || call_type == "CREATE2" {
            continue;
        }

        // Check if any of this child's sub-calls target rollups_address,
        // which indicates this child is a proxy forwarding to executeCrossChainCall.
        let grandchildren = match child.get("calls").and_then(|v| v.as_array()) {
            Some(gc) => gc,
            None => continue,
        };

        let targets_rollups = grandchildren.iter().any(|gc| {
            gc.get("to")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Address>().ok())
                .map(|addr| addr == rollups_address)
                .unwrap_or(false)
        });

        if !targets_rollups {
            continue;
        }

        // This child is the proxy call. Extract its input (the receiveTokens calldata).
        let input_str = child.get("input").and_then(|v| v.as_str()).unwrap_or("0x");
        let clean = input_str.strip_prefix("0x").unwrap_or(input_str);
        if let Some(bytes) = hex_decode(clean) {
            let proxy_addr = child
                .get("to")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            tracing::debug!(
                target: "based_rollup::l1_proxy",
                proxy = proxy_addr,
                calldata_len = bytes.len(),
                "found proxy call in bridge trace children"
            );
            return Some(bytes);
        }
    }

    None
}

/// Check if an address is a CrossChainProxy by querying the `authorizedProxies`
/// mapping on Rollups.sol. This mapping stores `ProxyInfo(originalAddress, originalRollupId)`
/// for every proxy created via `createCrossChainProxy()`.
///
/// We query Rollups.sol instead of the proxy contract because CrossChainProxy's
/// `ORIGINAL_ADDRESS` and `ORIGINAL_ROLLUP_ID` are `internal immutable` — no public getters.
///
/// Returns `Some((destination, rollup_id))` if it's a proxy, `None` otherwise.
async fn detect_cross_chain_proxy(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    address: Address,
    rollups_address: Address,
) -> eyre::Result<Option<(Address, u64)>> {
    // authorizedProxies(address) — selector 0x360d95b6
    // ABI return: (address originalAddress, uint64 originalRollupId)
    let calldata = format!("0x360d95b6{:0>64}", hex::encode(address.as_slice()));

    let result = eth_call_view(client, l1_rpc_url, rollups_address, &calldata).await;

    let hex_data = match result {
        Ok(hex) => hex,
        Err(_) => return Ok(None),
    };

    let destination = parse_address_from_return(&hex_data)?;

    // If originalAddress is zero, the proxy isn't registered
    if destination.is_zero() {
        return Ok(None);
    }

    // Second 32 bytes = originalRollupId (uint64 ABI-encoded as uint256)
    let hex_clean = hex_data.strip_prefix("0x").unwrap_or(&hex_data);
    if hex_clean.len() < 128 {
        return Ok(None);
    }
    let rollup_id_hex = format!("0x{}", &hex_clean[64..128]);
    let rollup_id = parse_u256_from_return(&rollup_id_hex)?;

    Ok(Some((destination, rollup_id)))
}

/// Selectors for Bridge contract functions.
const BRIDGE_ETHER_SELECTOR: [u8; 4] = [0xf4, 0x02, 0xd9, 0xf3]; // bridgeEther(uint256,address)
const BRIDGE_TOKENS_SELECTOR: [u8; 4] = [0x33, 0xb1, 0x5a, 0xad]; // bridgeTokens(address,uint256,uint256,address)
/// Selector for the Bridge.manager() view function.
const MANAGER_SELECTOR: &str = "0x481c6a75";

/// Detect if a transaction targets a Bridge contract (bridgeEther or bridgeTokens).
///
/// Bridge contracts call `_getOrDeployProxy(sender, rollupId)` internally, which
/// creates a CrossChainProxy and calls it. Without pre-populating the execution
/// table, the proxy call reverts with `ExecutionNotFound`.
///
/// Detection: call `manager()` on the target address. If it returns the Rollups
/// address, it's a Bridge. Then parse the function selector to extract the
/// destination and rollupId.
///
/// Returns `Some((destination, rollup_id, inner_calldata, source_address))` if
/// this is a bridge call, `None` otherwise. `source_address` is the address that
/// will be `msg.sender` on the proxy call (the bridge contract, not the user).
async fn detect_bridge_call(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    to_addr: Address,
    rollups_address: Address,
    from_addr: Address,
    calldata: &[u8],
) -> eyre::Result<Option<(Address, u64, Vec<u8>, Address)>> {
    // Must have at least a 4-byte selector
    if calldata.len() < 4 {
        return Ok(None);
    }

    let selector: [u8; 4] = calldata[..4].try_into().unwrap();

    // Only handle bridgeEther and bridgeTokens
    if selector != BRIDGE_ETHER_SELECTOR && selector != BRIDGE_TOKENS_SELECTOR {
        return Ok(None);
    }

    // Check if to_addr.manager() returns the Rollups address
    let manager_result = eth_call_view(client, l1_rpc_url, to_addr, MANAGER_SELECTOR).await;
    let manager_hex = match manager_result {
        Ok(hex) => hex,
        Err(e) => {
            tracing::debug!(
                target: "based_rollup::l1_proxy",
                %e, %to_addr,
                "manager() call failed — not a bridge"
            );
            return Ok(None); // No manager() method — not a bridge
        }
    };
    let manager_addr = parse_address_from_return(&manager_hex)?;
    if manager_addr != rollups_address {
        tracing::debug!(
            target: "based_rollup::l1_proxy",
            %to_addr, %manager_addr, %rollups_address,
            "manager() doesn't match rollups address — not our bridge"
        );
        return Ok(None); // manager() doesn't point to Rollups — not our bridge
    }

    if selector == BRIDGE_ETHER_SELECTOR {
        // bridgeEther(uint256 _rollupId, address destinationAddress)
        // calldata: selector(4) + rollupId(32) + destinationAddress(32)
        if calldata.len() < 68 {
            return Ok(None);
        }
        let rollup_id_hex = format!("0x{}", hex::encode(&calldata[4..36]));
        let rollup_id = parse_u256_from_return(&rollup_id_hex)?;

        // Extract destinationAddress from bytes 36..68 (ABI-encoded: 12 zero bytes + 20 address bytes)
        let destination_address = Address::from_slice(&calldata[48..68]);

        // Bridge deploys proxy for (destinationAddress, _rollupId) and calls proxy.call{value}("")
        // The proxy's fallback calls executeCrossChainCall(sourceAddress=bridge, callData="")
        // Destination for the cross-chain call is destinationAddress on the target rollup.
        tracing::info!(
            target: "based_rollup::l1_proxy",
            bridge = %to_addr,
            sender = %from_addr,
            %destination_address,
            rollup_id,
            "detected bridgeEther call"
        );

        // The cross-chain call destination is the destinationAddress (they receive ETH on L2).
        // The inner calldata is empty (ETH transfer via value).
        // sourceAddress = bridge contract (to_addr), because the bridge calls the proxy.
        Ok(Some((destination_address, rollup_id, vec![], to_addr)))
    } else {
        // bridgeTokens(address token, uint256 amount, uint256 _rollupId, address destinationAddress)
        // calldata: selector(4) + token(32) + amount(32) + rollupId(32) + destinationAddress(32)
        if calldata.len() < 132 {
            return Ok(None);
        }
        let rollup_id_hex = format!("0x{}", hex::encode(&calldata[68..100]));
        let rollup_id = parse_u256_from_return(&rollup_id_hex)?;

        // For token bridging, the bridge calls _getOrDeployProxy(_bridgeAddress(), _rollupId)
        // and calls the proxy with receiveTokens calldata. The cross-chain destination
        // is the bridge's own address (canonicalBridgeAddress) on the target rollup.
        // We use the bridge address as destination — the execution table entry will
        // match because _bridgeAddress() resolves to the bridge's canonical address.
        //
        // Query canonicalBridgeAddress (or use bridge address if not set)
        let selector = hex::encode(&IBridgeView::canonicalBridgeAddressCall::SELECTOR);
        let bridge_addr =
            match eth_call_view(client, l1_rpc_url, to_addr, &format!("0x{selector}")).await {
                Ok(hex) => {
                    let addr = parse_address_from_return(&hex)?;
                    if addr.is_zero() { to_addr } else { addr }
                }
                Err(_) => to_addr,
            };

        tracing::info!(
            target: "based_rollup::l1_proxy",
            bridge = %to_addr,
            sender = %from_addr,
            rollup_id,
            bridge_dest = %bridge_addr,
            "detected bridgeTokens call"
        );

        // For bridgeTokens, the bridge creates a proxy for its own address (not the sender),
        // and calls it with the receiveTokens ABI-encoded calldata. However, we pass the full
        // original calldata — the L2 simulation in initiateCrossChainCall will use it to
        // produce the correct execution entry.
        // sourceAddress = bridge contract (to_addr), because the bridge calls the proxy.
        Ok(Some((
            bridge_addr,
            rollup_id,
            calldata[4..].to_vec(),
            to_addr,
        )))
    }
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

    // Check if the `to` address is a cross-chain proxy
    let is_proxy = detect_cross_chain_proxy(client, l1_rpc_url, to_addr, rollups_address)
        .await
        .ok()
        .flatten()
        .is_some();

    // If not a proxy, check if it's a bridge contract
    let is_bridge = if !is_proxy {
        let calldata_hex = tx_obj.get("data").and_then(|v| v.as_str()).unwrap_or("0x");
        let calldata_clean = calldata_hex.strip_prefix("0x").unwrap_or(calldata_hex);
        let calldata_bytes = hex_decode(calldata_clean).unwrap_or_default();
        let from_str = tx_obj
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("0x0000000000000000000000000000000000000000");
        let from_addr = from_str.parse::<Address>().unwrap_or(Address::ZERO);
        detect_bridge_call(
            client,
            l1_rpc_url,
            to_addr,
            rollups_address,
            from_addr,
            &calldata_bytes,
        )
        .await
        .ok()
        .flatten()
        .is_some()
    } else {
        false
    };

    if !is_proxy && !is_bridge {
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

/// Make a read-only `eth_call` to a contract and return the hex result.
async fn eth_call_view(
    client: &reqwest::Client,
    rpc_url: &str,
    to: Address,
    data: &str,
) -> eyre::Result<String> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": format!("{to}"), "data": data}, "latest"],
        "id": 99996
    });

    let resp = client
        .post(rpc_url)
        .json(&req)
        .send()
        .await?
        .json::<Value>()
        .await?;

    if let Some(error) = resp.get("error") {
        return Err(eyre::eyre!("eth_call failed: {error}"));
    }

    resp.get("result")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("eth_call returned no result"))
}

/// Parse an address from a 32-byte ABI-encoded return value.
fn parse_address_from_return(hex_str: &str) -> eyre::Result<Address> {
    let hex = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex_decode(hex).ok_or_else(|| eyre::eyre!("invalid hex in eth_call return"))?;
    if bytes.len() < 32 {
        return Err(eyre::eyre!("return data too short for address"));
    }
    Ok(Address::from_slice(&bytes[12..32]))
}

/// Parse a U256 from a 32-byte ABI-encoded return value.
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
fn u256_from_be_bytes(bytes: &[u8]) -> u64 {
    let len = bytes.len().min(32);
    let mut val: u64 = 0;
    let start = len.saturating_sub(8);
    for b in &bytes[start..len] {
        val = (val << 8) | (*b as u64);
    }
    val
}

/// Extract (method, params) pairs from a JSON-RPC request (single or batch).
fn extract_methods(json: &Value) -> Vec<(String, Option<&Vec<Value>>)> {
    let mut result = Vec::new();
    match json {
        Value::Object(obj) => {
            if let Some(Value::String(method)) = obj.get("method") {
                let params = obj.get("params").and_then(|p| p.as_array());
                result.push((method.clone(), params));
            }
        }
        Value::Array(arr) => {
            for item in arr {
                if let Value::Object(obj) = item {
                    if let Some(Value::String(method)) = obj.get("method") {
                        let params = obj.get("params").and_then(|p| p.as_array());
                        result.push((method.clone(), params));
                    }
                }
            }
        }
        _ => {}
    }
    result
}

/// Add CORS headers to a response.
fn cors_response(mut resp: Response<Full<HyperBytes>>) -> Response<Full<HyperBytes>> {
    let headers = resp.headers_mut();
    headers.insert(
        "Access-Control-Allow-Origin",
        "*".parse().expect("valid header"),
    );
    headers.insert(
        "Access-Control-Allow-Methods",
        "POST, OPTIONS".parse().expect("valid header"),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        "Content-Type".parse().expect("valid header"),
    );
    resp
}

/// Build a JSON-RPC error response.
fn error_response(status: StatusCode, message: &str) -> Response<Full<HyperBytes>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": -32603, "message": message },
        "id": null
    });
    cors_response(
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Full::new(HyperBytes::from(body.to_string())))
            .expect("valid response"),
    )
}

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

/// Compute the tx hash from a raw signed transaction hex string.
fn compute_tx_hash_from_raw(raw_tx: &str) -> eyre::Result<String> {
    let raw_hex = raw_tx.strip_prefix("0x").unwrap_or(raw_tx);
    let raw_bytes =
        hex_decode(raw_hex).ok_or_else(|| eyre::eyre!("invalid hex in raw transaction"))?;

    use alloy_consensus::transaction::TxEnvelope;
    use alloy_rlp::Decodable;

    let tx_envelope = TxEnvelope::decode(&mut raw_bytes.as_slice())
        .map_err(|e| eyre::eyre!("failed to decode transaction: {e}"))?;

    Ok(format!("{}", tx_envelope.tx_hash()))
}

/// Get the latest L1 block number, hash, and parent hash for proof computation.
///
/// Returns `(block_number, block_hash, parent_hash)`.
/// For real `postBatch`, use `(number + 1, hash)` as `(target_block, parent_hash)`.
/// For `traceCallMany` at "latest", use `(number, parent_hash)` since the trace
/// executes at the current block where `block.number = number` and
/// `blockhash(block.number - 1) = parent_hash`.
async fn get_l1_block_context(
    client: &reqwest::Client,
    l1_rpc_url: &str,
) -> eyre::Result<(u64, alloy_primitives::B256, alloy_primitives::B256)> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getBlockByNumber",
        "params": ["latest", false],
        "id": 99997
    });

    let resp = client
        .post(l1_rpc_url)
        .json(&req)
        .send()
        .await?
        .json::<Value>()
        .await?;

    let block = resp
        .get("result")
        .ok_or_else(|| eyre::eyre!("no result from eth_getBlockByNumber"))?;

    let number_hex = block
        .get("number")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no block number"))?;
    let number = u64::from_str_radix(number_hex.strip_prefix("0x").unwrap_or(number_hex), 16)
        .map_err(|e| eyre::eyre!("invalid block number: {e}"))?;

    let hash_hex = block
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no block hash"))?;
    let hash = hash_hex
        .parse::<alloy_primitives::B256>()
        .map_err(|e| eyre::eyre!("invalid block hash: {e}"))?;

    let parent_hash_hex = block
        .get("parentHash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no parent hash"))?;
    let parent_hash = parent_hash_hex
        .parse::<alloy_primitives::B256>()
        .map_err(|e| eyre::eyre!("invalid parent hash: {e}"))?;

    Ok((number, hash, parent_hash))
}

/// Query the verification key for a rollup from the Rollups contract.
async fn get_verification_key(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    rollup_id: u64,
) -> eyre::Result<alloy_primitives::B256> {
    // rollups(uint256) — ABI encode: selector + uint256(rollup_id)
    // Selector = keccak256("rollups(uint256)")[:4]
    let selector = &alloy_primitives::keccak256(b"rollups(uint256)")[..4];
    let calldata = format!("0x{}{:0>64x}", hex::encode(selector), rollup_id);

    let result_hex = eth_call_view(client, l1_rpc_url, rollups_address, &calldata).await?;

    // rollups() returns (address owner, bytes32 verificationKey, bytes32 stateRoot, uint256 etherBalance)
    // VK is at offset 32..64 (word 1, after address at word 0)
    let hex_clean = result_hex.strip_prefix("0x").unwrap_or(&result_hex);
    if hex_clean.len() < 128 {
        return Err(eyre::eyre!("rollups() return too short for VK"));
    }
    let vk_hex = &hex_clean[64..128];
    let vk_str = format!("0x{vk_hex}");
    vk_str
        .parse::<alloy_primitives::B256>()
        .map_err(|e| eyre::eyre!("invalid VK: {e}"))
}

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
