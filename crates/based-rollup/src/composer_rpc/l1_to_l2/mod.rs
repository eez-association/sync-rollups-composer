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

mod process;
mod simulation;

use alloy_primitives::{Address, Bytes};
#[cfg(test)]
use alloy_primitives::U256;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes as HyperBytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

// Shared helpers from the common module.
use super::common::{cors_response, error_response, extract_methods};
use super::model::L1ProxyLookup;

// Re-export process items that trace_and_detect_internal_calls needs.
use process::process_l1_to_l2_calls;
use process::walk_l1_trace_generic;

// Re-export items moved to process.rs so that the test module (which does
// `use super::*`) can still access them.
#[cfg(test)]
use process::parse_address_from_return;
#[cfg(test)]
use process::parse_u256_from_return;

/// Run the L1 RPC proxy server.
///
/// Creates a `BundleManager` and spawns its cycle loop to enforce the
/// sim-runtime determinism invariant (docs/DERIVATION.md §15.1).
///
/// Cross-chain user txs are intercepted by `handle_request`, decoded into
/// [`PendingUserTx`] entries, and submitted to the bundler. The bundler's
/// background cycle loop closes the window every
/// `l1_block_time_ms * bundle_close_fraction` ms and dispatches the drained
/// bundle to the finalizer, which processes each tx with prior-bundle context
/// (Phase 3.C: each tx's trace sees effects of prior txs in the bundle).
///
/// `queued_cross_chain_calls` is shared with the driver; the finalizer
/// snapshots `queue.len()` before each tx and reads the delta after, so it
/// can use each prior tx's produced L1 entries as the postBatch for the next
/// tx's simulation bundle.
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
    pending_l1_forward_txs: Arc<Mutex<Vec<Bytes>>>,
    queued_cross_chain_calls: Arc<Mutex<Vec<crate::rpc::QueuedCrossChainCall>>>,
    l1_block_time_ms: u64,
    bundle_close_fraction: f64,
) -> eyre::Result<()> {
    // `pending_l1_forward_txs` is retained for a future phase where the composer
    // may want to observe already-queued txs (e.g. if the bundler restarts and
    // we want to carry state forward). Phase 3 doesn't read it.
    let _ = &pending_l1_forward_txs;

    let addr = SocketAddr::from(([0, 0, 0, 0], l1_proxy_port));
    let listener = TcpListener::bind(addr).await?;

    tracing::info!(
        target: "based_rollup::l1_proxy",
        %l1_proxy_port,
        %l1_rpc_url,
        %l2_rpc_url,
        %rollups_address,
        %builder_address,
        l1_block_time_ms,
        bundle_close_fraction,
        "L1 RPC proxy listening"
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build reqwest client");

    // ── BundleManager wiring ────────────────────────────────────────────────
    let bundle_manager = Arc::new(super::bundle_manager::BundleManager::new(
        super::bundle_manager::BundleConfig {
            l1_block_time_ms,
            close_fraction: bundle_close_fraction,
        },
    ));

    // Spawn the cycle loop with the **bundle-aware** finalizer (Phase 3.C):
    // each drained tx is processed sequentially, but bot_i's initial trace
    // runs inside a `debug_traceCallMany` bundle that prepends prior bot
    // txs (with their produced L1 entries preloaded via postBatch). This
    // ensures bot_i's actionHash reflects the post-prior state — the core
    // sim==runtime invariant (§15.1).
    {
        let client = client.clone();
        let l1_rpc_url = l1_rpc_url.clone();
        let l2_rpc_url = l2_rpc_url.clone();
        let builder_private_key = builder_private_key.clone();
        let bundle_manager_clone = bundle_manager.clone();
        let queued_calls = queued_cross_chain_calls.clone();

        let poll_client = client.clone();
        let poll_url = Some(l1_rpc_url.clone());
        tokio::spawn(async move {
            bundle_manager_clone
                .run_cycle_loop(poll_url, poll_client, move |mgr, drained| {
                    finalize_bundle_with_context(
                        mgr,
                        drained,
                        client.clone(),
                        l1_rpc_url.clone(),
                        l2_rpc_url.clone(),
                        rollups_address,
                        builder_address,
                        builder_private_key.clone(),
                        rollup_id,
                        cross_chain_manager_address,
                        queued_calls.clone(),
                    )
                })
                .await;
        });
    }

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "based_rollup::l1_proxy", %e, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };

        let client = client.clone();
        let l1_rpc_url = l1_rpc_url.clone();
        let l2_rpc_url = l2_rpc_url.clone();
        let builder_private_key = builder_private_key.clone();
        let bundle_manager = bundle_manager.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let client = client.clone();
                let l1_rpc_url = l1_rpc_url.clone();
                let l2_rpc_url = l2_rpc_url.clone();
                let builder_private_key = builder_private_key.clone();
                let bundle_manager = bundle_manager.clone();
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
                    bundle_manager,
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


/// Bundle-aware finalizer (Phase 3.C): process each queued user tx sequentially
/// with **prior-bundle context** so each tx's `actionHash` matches runtime.
///
/// Algorithm:
/// 1. Sort drained bundle by `effective_gas_price` DESC (matches reth mempool).
/// 2. For each tx in order:
///    a. Snapshot `queued_cross_chain_calls.len()` BEFORE.
///    b. Call `handle_cross_chain_tx(..., prior_entries, prior_raw_txs)` with
///       the accumulated context. The initial `debug_traceCall` becomes a
///       `debug_traceCallMany([postBatch(prior_entries), prior_raw_txs..., tx])`
///       so tx's trace sees prior txs' state effects.
///    c. After: new items `queue[before_len..]` are THIS tx's produced entries.
///       Extract L1 entries from each (Simple: [call,result]; WithContinuations:
///       l1_entries) and append to `prior_entries`. Append raw tx to
///       `prior_raw_txs`.
/// 3. On per-tx error: log ERROR, skip the tx, continue with remaining — that
///    tx's bot sees a 60s timeout but subsequent txs still benefit from
///    prior-bundle context of the preceding successful ones.
///
/// The `sim == runtime` invariant (§15.1) holds for every tx whose priors
/// also went through the composer. Bot-vs-external-tx races remain out of
/// scope (documented in §15).
#[allow(clippy::too_many_arguments)]
async fn finalize_bundle_with_context(
    mgr: Arc<super::bundle_manager::BundleManager>,
    drained: super::bundle_manager::DrainedBundle,
    client: reqwest::Client,
    l1_rpc_url: String,
    l2_rpc_url: String,
    rollups_address: Address,
    builder_address: Address,
    builder_private_key: Option<String>,
    rollup_id: u64,
    cross_chain_manager_address: Address,
    queued_cross_chain_calls: Arc<Mutex<Vec<crate::rpc::QueuedCrossChainCall>>>,
) -> eyre::Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let bundle_id = drained.bundle_id;
    let mut txs = drained.txs;

    // Sort by gas price descending to match reth mempool ordering.
    super::bundle_manager::sort_bundle_by_gas_desc(&mut txs);

    // Inter-bundle races are handled NATURALLY by timing: by the time bundle
    // N+1 opens its window, bundle N's postBatch + raw txs have already
    // landed in the most recent L1 block, so the L1 "latest" state seen by
    // bundle N+1's debug_traceCall reflects bundle N's effects automatically.
    // No carry-over of prior_entries/prior_raw_txs from the driver queue is
    // required. See docs/DERIVATION.md §15.1 ("deterministic timing model").
    //
    // Intra-bundle (this finalize): accumulates via the loop below.
    let mut prior_entries: Vec<crate::cross_chain::CrossChainExecutionEntry> = Vec::new();
    let mut prior_raw_txs: Vec<Bytes> = Vec::new();

    let start = Instant::now();
    let sim_source = if txs.len() > 1 { "bundle" } else { "standalone" };
    tracing::info!(
        target: "based_rollup::composer_bundle",
        %bundle_id,
        tx_count = txs.len(),
        sim_source,
        "bundle_finalize_start"
    );

    for tx in &txs {
        let raw_tx_hex = format!("0x{}", alloy_primitives::hex::encode(&tx.raw_tx));

        // Snapshot queue length BEFORE — so we can diff afterwards.
        let before_len = queued_cross_chain_calls
            .lock()
            .map(|q| q.len())
            .unwrap_or(0);

        match handle_cross_chain_tx(
            &client,
            &l1_rpc_url,
            &l2_rpc_url,
            &raw_tx_hex,
            rollups_address,
            builder_address,
            builder_private_key.clone(),
            rollup_id,
            cross_chain_manager_address,
            &prior_entries,
            &prior_raw_txs,
        )
        .await
        {
            Ok(Some(_hash)) => {
                // Harvest the new entries that were added to the queue by this
                // tx's processing. They become priors for subsequent txs.
                let guard = match queued_cross_chain_calls.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                let new_items = guard.iter().skip(before_len);
                for item in new_items {
                    prior_entries.extend(extract_l1_entries_for_call(item));
                }
                drop(guard);
                prior_raw_txs.push(tx.raw_tx.clone());
            }
            Ok(None) => {
                tracing::warn!(
                    target: "based_rollup::composer_bundle",
                    %bundle_id,
                    tx_hash = %tx.tx_hash,
                    "bundle_tx_not_cross_chain — detection mismatch, skipping"
                );
            }
            Err(e) => {
                tracing::error!(
                    target: "based_rollup::composer_bundle",
                    %bundle_id,
                    tx_hash = %tx.tx_hash,
                    %e,
                    "bundle_tx_finalize_error"
                );
            }
        }
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    tracing::info!(
        target: "based_rollup::composer_bundle",
        %bundle_id,
        tx_count = txs.len(),
        elapsed_ms,
        prior_entries_final = prior_entries.len(),
        "bundle_finalize_success"
    );
    mgr.metrics
        .finalize_success_total
        .fetch_add(1, Ordering::Relaxed);
    mgr.metrics
        .tx_finalized_total
        .fetch_add(txs.len() as u64, Ordering::Relaxed);

    Ok(())
}

/// Extract the L1 deferred entries stored inside a `QueuedCrossChainCall`.
///
/// `Simple` deposits carry `[call_entry, result_entry]` (the L2 table pair;
/// their L1 format is converted by the driver). For the purposes of
/// pre-loading on a simulation postBatch we need the L1-shaped entries —
/// the driver's `convert_pairs_to_l1_entries` does this conversion, but for
/// the simulation we can use them as-is since the trace only needs the
/// actionHash + state delta to line up.
///
/// `WithContinuations` carries `l1_entries` directly — those are the L1
/// deferred entries pushed into the combined postBatch.
fn extract_l1_entries_for_call(
    call: &crate::rpc::QueuedCrossChainCall,
) -> Vec<crate::cross_chain::CrossChainExecutionEntry> {
    match call {
        crate::rpc::QueuedCrossChainCall::Simple {
            call_entry,
            result_entry,
            ..
        } => {
            // Convert the L2 pair to L1 format via the same conversion the
            // driver uses at flush time.
            let pairs = vec![call_entry.clone(), result_entry.clone()];
            super::entry_builder::pairs_to_l1_format(&pairs)
        }
        crate::rpc::QueuedCrossChainCall::WithContinuations { l1_entries, .. } => {
            l1_entries.clone()
        }
    }
}

/// Quick detection: does this tx make any cross-chain calls?
///
/// Runs `debug_traceCall` once on the tx, walks the call tree for
/// `executeCrossChainCall` children on the Rollups contract. Same logic
/// `trace_and_detect_internal_calls` uses to decide whether to process,
/// but stops immediately after the first trace — no iterative discovery.
///
/// Returns `false` on any RPC error (conservative: fall through to
/// regular forwarding; no cross-chain processing attempted).
async fn quick_detect_cross_chain(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    tx_obj: &Value,
    rollups_address: Address,
) -> bool {
    let from = tx_obj
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0000000000000000000000000000000000000000");
    let to = match tx_obj.get("to").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return false,
    };
    let data = tx_obj.get("data").and_then(|v| v.as_str()).unwrap_or("0x");
    let value = tx_obj
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0");

    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCall",
        "params": [
            {"from": from, "to": to, "data": data, "value": value, "gas": "0x2faf080"},
            "latest",
            { "tracer": "callTracer" }
        ],
        "id": 1
    });

    let resp = match client.post(l1_rpc_url).json(&trace_req).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };
    let body: super::common::JsonRpcResponse = match resp.json().await {
        Ok(b) => b,
        Err(_) => return false,
    };
    let trace = match body.into_result() {
        Ok(v) => v,
        Err(_) => return false,
    };

    let mut proxy_cache: HashMap<Address, Option<super::trace::ProxyInfo>> = HashMap::new();
    let detected = walk_l1_trace_generic(
        client,
        l1_rpc_url,
        rollups_address,
        &trace,
        &mut proxy_cache,
    )
    .await;
    !detected.is_empty()
}

/// Build a [`super::bundle_manager::PendingUserTx`] from a raw signed tx.
///
/// Decodes the envelope to extract the sender (via ecrecover), target,
/// calldata, value, and effective gas price. Computes `tx_hash` as
/// `keccak256(raw_bytes)` — the same hash the bot computes client-side.
///
/// Returns `None` for CREATE txs (no `to`) or any decode failure —
/// those txs fall through to regular forwarding, not the bundler.
fn build_pending_user_tx(raw_tx_hex: &str) -> Option<super::bundle_manager::PendingUserTx> {
    use alloy_consensus::Transaction;
    use alloy_consensus::transaction::TxEnvelope;
    use alloy_primitives::keccak256;
    use alloy_rlp::Decodable;
    use reth_primitives_traits::SignerRecoverable;

    let raw_hex = raw_tx_hex.strip_prefix("0x").unwrap_or(raw_tx_hex);
    let raw_bytes = hex_decode(raw_hex)?;

    let envelope = TxEnvelope::decode(&mut raw_bytes.as_slice()).ok()?;
    let from = envelope.recover_signer().ok()?;
    let to = envelope.to()?;

    let tx_hash = keccak256(&raw_bytes);

    Some(super::bundle_manager::PendingUserTx {
        raw_tx: Bytes::from(raw_bytes.clone()),
        tx_hash,
        from,
        to,
        data: Bytes::from(envelope.input().to_vec()),
        value: envelope.value(),
        effective_gas_price: super::bundle_manager::effective_gas_price(&raw_bytes),
        arrived_at_ms: super::bundle_manager::now_ms(),
    })
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
    bundle_manager: Arc<super::bundle_manager::BundleManager>,
    _peer: SocketAddr,
) -> Result<Response<Full<HyperBytes>>, hyper::Error> {
    // `l2_rpc_url`, `builder_private_key`, `builder_address`, `rollup_id`, and
    // `cross_chain_manager_address` are only consumed by the finalizer (via
    // the cycle loop closure). In `handle_request` we only need `client`,
    // `l1_rpc_url`, `rollups_address`, and `bundle_manager`. Silence the unused
    // warnings; removing these params would break backward compat with any
    // inline fallthrough path we might add in future.
    let _ = (
        &l2_rpc_url,
        &builder_private_key,
        builder_address,
        rollup_id,
        cross_chain_manager_address,
    );
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
                    tracing::info!(
                        target: "based_rollup::l1_proxy",
                        raw_tx_prefix = %&raw_tx[..raw_tx.len().min(42)],
                        raw_tx_len = raw_tx.len(),
                        "L1 compositor: intercepted eth_sendRawTransaction"
                    );

                    // Phase 3.B: quick-detect cross-chain, then submit to the
                    // bundler and return tx_hash immediately (fire-and-forget).
                    // Non-cross-chain txs fall through to direct L1 forwarding.
                    let is_cross_chain = match decode_raw_tx_for_trace(raw_tx) {
                        Ok(tx_obj) => {
                            quick_detect_cross_chain(&client, &l1_rpc_url, &tx_obj, rollups_address)
                                .await
                        }
                        Err(_) => false,
                    };

                    if is_cross_chain {
                        match build_pending_user_tx(raw_tx) {
                            Some(pending_tx) => {
                                let tx_hash_hex = format!("{:#x}", pending_tx.tx_hash);
                                bundle_manager.submit(pending_tx);
                                tracing::info!(
                                    target: "based_rollup::l1_proxy",
                                    tx_hash = %tx_hash_hex,
                                    "cross-chain tx submitted to bundler"
                                );
                                let json_id = json.get("id").cloned().unwrap_or(Value::Null);
                                let response_body = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "result": tx_hash_hex,
                                    "id": json_id
                                });
                                return Ok(cors_response(
                                    Response::builder()
                                        .status(StatusCode::OK)
                                        .header("Content-Type", "application/json")
                                        .body(Full::new(HyperBytes::from(
                                            response_body.to_string(),
                                        )))
                                        .expect("valid response"),
                                ));
                            }
                            None => {
                                tracing::warn!(
                                    target: "based_rollup::l1_proxy",
                                    "failed to build PendingUserTx from raw — forwarding to L1"
                                );
                            }
                        }
                    }
                    // Not a cross-chain tx (or decode/build failed), forward normally.
                }
            }

            // Intercept eth_estimateGas for cross-chain proxy addresses.
            // Wallets (MetaMask, Rabby) call this before showing the confirmation
            // dialog. For cross-chain proxy calls, L1 estimation always reverts
            // because the execution table isn't populated yet, causing wallets to
            // fall back to incorrect defaults (e.g. Rabby uses 2M gas).
            // We compute gas from calldata instead.
            if method == "eth_estimateGas" {
                if let Some(result) = process::handle_estimate_gas_for_proxy(
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
/// Single-tx initial trace — the legacy path used when there are no priors.
async fn run_standalone_initial_trace(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    from: &str,
    to: &str,
    data: &str,
    value: &str,
) -> eyre::Result<Option<Value>> {
    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCall",
        "params": [
            {"from": from, "to": to, "data": data, "value": value, "gas": "0x2faf080"},
            "latest",
            { "tracer": "callTracer" }
        ],
        "id": 1
    });
    let resp: super::common::JsonRpcResponse =
        client.post(l1_rpc_url).json(&trace_req).send().await?.json().await?;
    match resp.into_result() {
        Ok(t) => Ok(Some(t)),
        Err(e) => {
            tracing::debug!(
                target: "based_rollup::l1_proxy",
                %e,
                "debug_traceCall failed — forwarding tx without cross-chain detection"
            );
            Ok(None)
        }
    }
}

/// Bundled initial trace — runs this tx INSIDE a `debug_traceCallMany` where
/// the first element is `postBatch(prior_entries)` (signed) and the next N
/// elements are the prior raw txs as call shapes, followed by THIS tx.
///
/// Returns:
/// - `Ok(Some(trace))` on success — the trace of THIS tx from the bundle's
///   last slot.
/// - `Ok(None)` if the response shape is wrong (bundle didn't produce as many
///   traces as expected) — caller falls back.
/// - `Err` on RPC / parsing / signing errors — caller falls back.
#[allow(clippy::too_many_arguments)]
async fn build_and_run_bundled_initial_trace(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    from: &str,
    to: &str,
    data: &str,
    value: &str,
    rollups_address: Address,
    builder_private_key: Option<&str>,
    rollup_id: u64,
    prior_entries: &[crate::cross_chain::CrossChainExecutionEntry],
    prior_raw_txs: &[Bytes],
) -> eyre::Result<Option<Value>> {
    use super::common::{get_l1_block_context, get_verification_key};
    use alloy_signer::SignerSync;

    // Parse builder signer key — required to sign postBatch proof.
    let key_hex = match builder_private_key {
        Some(k) => k,
        None => {
            return Err(eyre::eyre!(
                "builder private key missing — cannot sign postBatch for bundled trace"
            ));
        }
    };
    let key_clean = key_hex.strip_prefix("0x").unwrap_or(key_hex);
    let builder_key: alloy_signer_local::PrivateKeySigner = key_clean
        .parse()
        .map_err(|e| eyre::eyre!("bad builder key: {e}"))?;

    let (block_number, block_hash, _) =
        get_l1_block_context(client, l1_rpc_url).await?;
    let vk = get_verification_key(client, l1_rpc_url, rollups_address, rollup_id)
        .await?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let call_data_bytes = alloy_primitives::Bytes::new();
    let entry_hashes = crate::cross_chain::compute_entry_hashes(prior_entries, vk);
    let public_inputs_hash = crate::cross_chain::compute_public_inputs_hash(
        &entry_hashes,
        &call_data_bytes,
        block_hash,
        timestamp,
    );
    let sig = builder_key
        .sign_hash_sync(&public_inputs_hash)
        .map_err(|e| eyre::eyre!("sign failed: {e}"))?;
    let sig_bytes = sig.as_bytes();
    let mut proof_bytes = sig_bytes.to_vec();
    if proof_bytes.len() == 65 && proof_bytes[64] < 27 {
        proof_bytes[64] += 27;
    }
    let proof = alloy_primitives::Bytes::from(proof_bytes);

    let post_batch_calldata =
        crate::cross_chain::encode_post_batch_calldata(prior_entries, call_data_bytes, proof);
    let post_batch_hex = format!("0x{}", hex::encode(post_batch_calldata.as_ref()));
    let builder_addr_hex = format!("{}", builder_key.address());
    let rollups_hex = format!("{}", rollups_address);
    let next_block = format!("{:#x}", block_number + 1);

    // Assemble the bundle: [postBatch, prior_tx_0_call, ..., prior_tx_{N-1}_call, this_tx_call]
    let mut transactions: Vec<Value> = Vec::with_capacity(prior_raw_txs.len() + 2);
    transactions.push(serde_json::json!({
        "from": builder_addr_hex,
        "to": rollups_hex,
        "data": post_batch_hex,
        "gas": "0x1c9c380"
    }));
    for raw in prior_raw_txs {
        // Decode each prior raw tx to {from, to, data, value} call shape.
        let raw_hex = format!("0x{}", hex::encode(raw.as_ref()));
        let tx_obj = decode_raw_tx_for_trace(&raw_hex)?;
        let (p_from, p_to, p_data, p_value) = (
            tx_obj.get("from").and_then(|v| v.as_str()).unwrap_or(""),
            tx_obj.get("to").and_then(|v| v.as_str()).unwrap_or(""),
            tx_obj.get("data").and_then(|v| v.as_str()).unwrap_or("0x"),
            tx_obj.get("value").and_then(|v| v.as_str()).unwrap_or("0x0"),
        );
        transactions.push(serde_json::json!({
            "from": p_from,
            "to": p_to,
            "data": p_data,
            "value": p_value,
            "gas": "0x2faf080"
        }));
    }
    // The subject tx (last in bundle — its trace is what we return).
    transactions.push(serde_json::json!({
        "from": from,
        "to": to,
        "data": data,
        "value": value,
        "gas": "0x2faf080"
    }));

    // Second param is StateContext / block override. reth doesn't accept a
    // bare string like "latest" here — must be null or a struct. See existing
    // invocations in sim_client.rs (line 116-119).
    let trace_req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceCallMany",
        "params": [
            [{ "transactions": transactions }],
            Value::Null,
            { "tracer": "callTracer" }
        ],
        "id": 1
    });

    let resp: super::common::JsonRpcResponse =
        client.post(l1_rpc_url).json(&trace_req).send().await?.json().await?;
    let result_val = resp.into_result()?;

    // Expected shape: [[trace_postBatch, trace_prior_0, ..., trace_prior_N-1, trace_subject]]
    let bundle_traces = match result_val.get(0).and_then(|b| b.as_array()) {
        Some(arr) => arr,
        None => return Ok(None),
    };
    let expected = prior_raw_txs.len() + 2;
    if bundle_traces.len() != expected {
        tracing::warn!(
            target: "based_rollup::l1_proxy",
            got = bundle_traces.len(),
            expected,
            "bundled trace length mismatch"
        );
        return Ok(None);
    }

    // Return the LAST trace — this tx's behavior in the post-priors state.
    // Use `next_block` / `timestamp` block-override? debug_traceCallMany
    // already advances state per tx; no extra override needed here.
    let _ = (next_block, timestamp); // suppress unused
    Ok(Some(bundle_traces[bundle_traces.len() - 1].clone()))
}

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
    prior_entries: &[crate::cross_chain::CrossChainExecutionEntry],
    prior_raw_txs: &[Bytes],
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
        prior_entries,
        prior_raw_txs,
    )
    .await
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
    prior_entries: &[crate::cross_chain::CrossChainExecutionEntry],
    prior_raw_txs: &[Bytes],
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

    let has_priors = !prior_raw_txs.is_empty();
    tracing::info!(
        target: "based_rollup::l1_proxy",
        %to, %from,
        prior_tx_count = prior_raw_txs.len(),
        prior_entry_count = prior_entries.len(),
        "initial trace — {}",
        if has_priors { "debug_traceCallMany with prior-bundle context" } else { "debug_traceCall (no priors)" }
    );

    // Initial trace: in the bundled-context path, run a debug_traceCallMany
    // so the current tx sees prior txs' state effects (AMM swaps, balances,
    // etc.). The prior txs need a postBatch loaded so their own CCM lookups
    // don't revert — we build that postBatch from prior_entries.
    //
    // This is the heart of docs/DERIVATION.md §15.1.
    let trace_result = if has_priors {
        match build_and_run_bundled_initial_trace(
            client,
            l1_rpc_url,
            from,
            to,
            data,
            value,
            rollups_address,
            builder_private_key.as_deref(),
            rollup_id,
            prior_entries,
            prior_raw_txs,
        )
        .await
        {
            Ok(Some(trace)) => trace,
            Ok(None) => {
                // Bundle sim failed structurally (no response / wrong length).
                // Fall back to standalone trace — accepts correctness loss for
                // this one tx rather than dropping it entirely. Logged loud.
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    "bundled initial trace returned unexpected shape — falling back to standalone traceCall"
                );
                match run_standalone_initial_trace(client, l1_rpc_url, from, to, data, value).await? {
                    Some(t) => t,
                    None => return Ok(None),
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "based_rollup::l1_proxy",
                    %e,
                    "bundled initial trace error — falling back to standalone"
                );
                match run_standalone_initial_trace(client, l1_rpc_url, from, to, data, value).await? {
                    Some(t) => t,
                    None => return Ok(None),
                }
            }
        }
    } else {
        match run_standalone_initial_trace(client, l1_rpc_url, from, to, data, value).await? {
            Some(t) => t,
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

    // Iterative L1 discovery via the unified discover_until_stable engine.
    // Replaces the inline loop + in_reverted_frame correction block.
    // discover_until_stable handles both the iterative retrace and
    // correct_in_reverted_frame internally.
    if top_level_error && !detected_calls.is_empty() {
        use super::direction::{L1ToL2, UserTxContext};
        use super::sim_client::HttpSimClient;

        let direction = L1ToL2 {
            l2_ccm: cross_chain_manager_address,
            l1_ccm: rollups_address,
            rollup_id,
            builder_key: {
                let key_hex = builder_private_key.as_deref().unwrap_or("");
                let key_clean = key_hex.strip_prefix("0x").unwrap_or(key_hex);
                key_clean
                    .parse::<alloy_signer_local::PrivateKeySigner>()
                    .unwrap_or_else(|_| alloy_signer_local::PrivateKeySigner::random())
            },
            client: client.clone(),
            l1_rpc_url: l1_rpc_url.to_string(),
        };
        let sim = HttpSimClient::new(
            client.clone(),
            l1_rpc_url.to_string(),
            l2_rpc_url.to_string(),
        );
        let lookup = L1ProxyLookup {
            client,
            rpc_url: l1_rpc_url,
            rollups_address,
        };
        let user_tx = UserTxContext {
            from: from.to_string(),
            to: to.to_string(),
            data: data.to_string(),
            value: value.to_string(),
            raw_tx_bytes: vec![], // L1→L2 doesn't need raw tx bytes for enrichment
        };
        match super::discover::discover_until_stable(
            &direction,
            &sim,
            &trace_result,
            &user_tx,
            &lookup,
            &mut proxy_cache,
            Some(detected_calls.clone()),
        )
        .await
        {
            Ok(discovered) => {
                detected_calls = discovered.calls;
                // last_converged_walk stays empty — discover_until_stable handles
                // in_reverted_frame internally via correct_in_reverted_frame
            }
            Err(e) => {
                tracing::warn!(target: "based_rollup::l1_proxy", %e,
                    "discover_until_stable failed — proceeding with initial calls");
            }
        }
    }

    process_l1_to_l2_calls(
        client,
        l1_rpc_url,
        l2_rpc_url,
        raw_tx,
        rollups_address,
        &builder_private_key,
        rollup_id,
        cross_chain_manager_address,
        from,
        to,
        data,
        value,
        top_level_error,
        &mut detected_calls,
        &mut proxy_cache,
    )
    .await
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
// extract_methods, cors_response, error_response are in super::common (imported above).

/// Decode a hex string to bytes.
pub(super) fn hex_decode(hex: &str) -> Option<Vec<u8>> {
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

/// Extract the effective gas price from a raw signed transaction.
/// For EIP-1559 txs, uses `max_fee_per_gas` (the worst-case ordering price).
/// For legacy/EIP-2930 txs, uses `gas_price`.
pub(super) fn extract_gas_price_from_raw_tx(raw_tx: &str) -> eyre::Result<u128> {
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
#[path = "../l1_to_l2_tests.rs"]
mod tests;
