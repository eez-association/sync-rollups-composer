//! L1 composer bundle manager — deferred-batch simulation window.
//!
//! Accumulates user txs that the L1 composer intercepts during a fixed-duration
//! window (aligned to L1 block time), then finalizes the batch atomically by:
//!
//! 1. Sorting by `effective_gas_price` descending (matches reth mempool ordering).
//! 2. Filtering out txs already mined on L1.
//! 3. Simulating all txs via `debug_traceCallMany` in that order — each tx's trace
//!    sees state effects of its predecessors, so `keccak256(abi.encode(Action))`
//!    matches runtime.
//! 4. Building entries per user with chained state deltas.
//! 5. Committing atomically to the driver via `syncrollups_commitBundle`.
//!
//! The invariant this module enforces is docs/DERIVATION.md §15.1:
//! *for every user tx processed by the composer, if all txs preceding it in the
//! same L1 block also passed through the composer, the composer-logged actionHash
//! equals the runtime actionHash byte-for-byte.*
//!
//! ## Fire-and-forget semantics
//!
//! `handle_request` returns `tx_hash` immediately after [`BundleManager::submit`].
//! Finalization runs in the background. If finalization fails after all retries,
//! the txs are dropped with an ERROR log and a `finalize_failures_total` metric
//! bump — bots see a 60s wait-for-receipt timeout (same UX as any drop today).
//!
//! ## Retry policy
//!
//! No silent fallback. On transient `debug_traceCallMany` failure, retry 3× with
//! 100ms / 250ms / 500ms backoff. Drop on persistent failure. See issue #41 and
//! [`super::simulation`] for the anti-pattern this avoids.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use tokio::time::sleep;

/// Configuration for the bundle manager.
#[derive(Clone, Debug)]
pub struct BundleConfig {
    /// L1 block time in milliseconds. Used to size the bundle window.
    pub l1_block_time_ms: u64,
    /// Fraction of the L1 block time during which new txs join the CURRENT
    /// bundle. After this, new txs join the NEXT bundle.
    pub close_fraction: f64,
}

impl BundleConfig {
    /// Window duration for the "current" bundle (before new txs flip to "next").
    pub fn window_ms(&self) -> u64 {
        ((self.l1_block_time_ms as f64) * self.close_fraction).round() as u64
    }

    /// Duration of the grace period after close_deadline and before rotation.
    pub fn grace_ms(&self) -> u64 {
        self.l1_block_time_ms.saturating_sub(self.window_ms())
    }
}

/// One user transaction queued for the current or next bundle.
#[derive(Clone, Debug)]
pub struct PendingUserTx {
    /// Signed raw tx bytes.
    pub raw_tx: Bytes,
    /// Keccak of `raw_tx` — the tx hash the bot sees.
    pub tx_hash: B256,
    /// Sender — decoded from the signature (ecrecover).
    pub from: Address,
    /// Target contract (`None` only for CREATE txs, which we never bundle).
    pub to: Address,
    /// Call data.
    pub data: Bytes,
    /// Value in wei.
    pub value: U256,
    /// Effective gas price used for bundle ordering. Matches reth mempool semantics:
    /// legacy / EIP-2930 → `gasPrice`; EIP-1559 → `maxPriorityFeePerGas` (tip).
    pub effective_gas_price: u128,
    /// Lightweight request-time hint: `true` when the initial standalone trace
    /// already looked like a cross-chain tx. Used only to choose the safest
    /// fallback on bundle-finalization errors; the final classification still
    /// happens during finalization.
    pub cross_chain_hint: bool,
    /// When this tx landed in the bundle (wall-clock ms).
    pub arrived_at_ms: u64,
}

/// Bundle snapshot passed from the manager to the finalizer.
#[derive(Debug)]
pub struct DrainedBundle {
    pub bundle_id: B256,
    pub txs: Vec<PendingUserTx>,
}

/// Mutable state behind a single lock.
struct BundleState {
    current: Vec<PendingUserTx>,
    next: Vec<PendingUserTx>,
    /// When the current cycle started (wall-clock ms).
    cycle_start_ms: u64,
    /// After this instant (ms), new submissions target `next` instead of `current`.
    close_deadline_ms: u64,
}

/// In-memory counters exported via `/health` (see `health.rs`).
#[derive(Default)]
pub struct BundleMetrics {
    pub tx_accepted_total: AtomicU64,
    pub tx_deduped_total: AtomicU64,
    pub cycles_total: AtomicU64,
    pub finalize_success_total: AtomicU64,
    pub finalize_failures_total: AtomicU64,
    pub finalize_retries_total: AtomicU64,
    /// Cumulative tx count across finalized bundles (for avg size calc by consumers).
    pub tx_finalized_total: AtomicU64,
}

/// L1 composer bundle manager. See module docs.
pub struct BundleManager {
    config: BundleConfig,
    state: Mutex<BundleState>,
    pub metrics: Arc<BundleMetrics>,
}

impl BundleManager {
    /// Construct a new manager. Call [`Self::run_cycle_loop`] in a
    /// spawned task to start the rotation clock.
    pub fn new(config: BundleConfig) -> Self {
        let now = now_ms();
        let window_ms = config.window_ms();
        Self {
            state: Mutex::new(BundleState {
                current: Vec::new(),
                next: Vec::new(),
                cycle_start_ms: now,
                close_deadline_ms: now.saturating_add(window_ms),
            }),
            config,
            metrics: Arc::new(BundleMetrics::default()),
        }
    }

    /// Submit a user tx. Adds to `current` if the cycle is still open, otherwise
    /// to `next`. Returns `true` if accepted, `false` if deduplicated (same
    /// `tx_hash` already queued).
    pub fn submit(&self, tx: PendingUserTx) -> bool {
        let mut s = self.state.lock().expect("bundle state poisoned");

        // Dedup: tx_hash already in current or next → idempotent no-op.
        if s.current.iter().any(|p| p.tx_hash == tx.tx_hash)
            || s.next.iter().any(|p| p.tx_hash == tx.tx_hash)
        {
            self.metrics
                .tx_deduped_total
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }

        let target = if now_ms() < s.close_deadline_ms {
            "current"
        } else {
            "next"
        };

        tracing::info!(
            target: "based_rollup::composer_bundle",
            tx_hash = %tx.tx_hash,
            from = %tx.from,
            gas_price = tx.effective_gas_price,
            queue = target,
            "bundle_tx_accepted"
        );

        if target == "current" {
            s.current.push(tx);
        } else {
            s.next.push(tx);
        }
        self.metrics
            .tx_accepted_total
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Atomically drain `current` and return as a `DrainedBundle` with a
    /// deterministic bundle_id. Caller drives the finalize outside the lock.
    pub fn drain_current(&self) -> DrainedBundle {
        let mut s = self.state.lock().expect("bundle state poisoned");
        let txs = std::mem::take(&mut s.current);
        let bundle_id = compute_bundle_id(s.cycle_start_ms, &txs);
        DrainedBundle { bundle_id, txs }
    }

    /// Rotate: `current` ← `next`, `next` ← empty. Resets cycle timing.
    /// Call after [`Self::drain_current`] has started finalization.
    pub fn rotate(&self) {
        let mut s = self.state.lock().expect("bundle state poisoned");
        s.current = std::mem::take(&mut s.next);
        s.cycle_start_ms = now_ms();
        s.close_deadline_ms = s.cycle_start_ms.saturating_add(self.config.window_ms());
        self.metrics.cycles_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Current cycle start time in wall-clock ms. For instrumentation.
    pub fn cycle_start_ms(&self) -> u64 {
        self.state
            .lock()
            .expect("bundle state poisoned")
            .cycle_start_ms
    }

    /// Main cycle loop — drives rotation event-driven on L1 block arrival.
    ///
    /// Per docs/DERIVATION.md §15.1 the bundle N+1 MUST be rooted in a fresh L1
    /// block (the one that potentially carries bundle N's postBatch), so we
    /// poll L1 during the grace period and rotate on block advance, not on a
    /// timer. This guarantees that bundle N+1's very first simulation reads an
    /// L1 "latest" state already containing bundle N's effects.
    ///
    /// Flow per cycle:
    /// 1. Wait `window_ms` (accept txs into `current`).
    /// 2. Drain `current`, spawn `finalize` in a sibling task (so we can start
    ///    polling while the finalize runs concurrently).
    /// 3. If the bundle is non-empty, wait for finalize completion and then for
    ///    every drained tx hash to have an L1 receipt before rotating. This
    ///    prevents inter-bundle drift from earlier composer txs that are
    ///    forwarded but not yet reflected in `latest`.
    /// 4. If the bundle is empty, poll L1 `eth_blockNumber` every
    ///    `L1_POLL_INTERVAL_MS` (default 200ms) until the number advances, or
    ///    until `grace_ms` elapses as a hard timeout.
    /// 5. Rotate: `current` ← `next`; `cycle_start_ms = now`.
    ///
    /// Takes an `l1_poll_url` for the observation. If `None`, falls back to
    /// timer-based rotation (used by unit tests).
    pub async fn run_cycle_loop<F, Fut>(
        self: Arc<Self>,
        l1_poll_url: Option<String>,
        client: reqwest::Client,
        finalizer: F,
    ) where
        F: Fn(Arc<Self>, DrainedBundle) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = eyre::Result<()>> + Send + 'static,
    {
        const L1_POLL_INTERVAL_MS: u64 = 200;
        let finalizer = Arc::new(finalizer);

        // Sync the first close_deadline to wall-clock time.
        {
            let mut s = self.state.lock().expect("bundle state poisoned");
            let now = now_ms();
            s.cycle_start_ms = now;
            s.close_deadline_ms = now.saturating_add(self.config.window_ms());
        }

        let window = Duration::from_millis(self.config.window_ms());
        let grace_timeout = Duration::from_millis(self.config.grace_ms());

        // Baseline L1 block for the CURRENT cycle — the one bundle N is rooted
        // in. Initialized from RPC on the first iteration; updated on each
        // successful rotation (after observing advance). We track this across
        // iterations because L1 may advance DURING the window; the "advance"
        // we care about is "past cycle N's baseline", not "past current".
        let mut current_cycle_l1_baseline: Option<u64> = None;
        if let Some(url) = &l1_poll_url {
            if let Ok(n) = fetch_l1_block_number(&client, url).await {
                current_cycle_l1_baseline = Some(n);
                tracing::debug!(
                    target: "based_rollup::composer_bundle",
                    cycle_l1_baseline = n,
                    "cycle_rotation_init: observed initial L1 block"
                );
            }
        }

        loop {
            sleep(window).await;

            let drained = self.drain_current();
            let drained_tx_hashes: Vec<B256> = drained.txs.iter().map(|tx| tx.tx_hash).collect();
            let finalize_handle = if !drained.txs.is_empty() {
                tracing::info!(
                    target: "based_rollup::composer_bundle",
                    bundle_id = %drained.bundle_id,
                    tx_count = drained.txs.len(),
                    "bundle_closed"
                );
                let this = self.clone();
                let fin = finalizer.clone();
                Some(tokio::spawn(async move {
                    if let Err(e) = (fin)(this.clone(), drained).await {
                        tracing::error!(
                            target: "based_rollup::composer_bundle",
                            %e,
                            "bundle_finalize_loop_error"
                        );
                        this.metrics
                            .finalize_failures_total
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }))
            } else {
                None
            };

            let rotation_reason = if drained_tx_hashes.is_empty() {
                // Wait for L1 to advance PAST the cycle's baseline — not the
                // number observed at grace start. L1 may have advanced during
                // the window, so the baseline captured at cycle start is the
                // correct reference.
                match (&l1_poll_url, current_cycle_l1_baseline) {
                    (Some(url), Some(baseline)) => {
                        match wait_for_l1_block_past(
                            &client,
                            url,
                            baseline,
                            grace_timeout,
                            Duration::from_millis(L1_POLL_INTERVAL_MS),
                        )
                        .await
                        {
                            Ok(Some(new_block)) => {
                                tracing::info!(
                                    target: "based_rollup::composer_bundle",
                                    cycle_l1_baseline = baseline,
                                    new_l1_block = new_block,
                                    "cycle_rotation: new L1 block past baseline"
                                );
                                current_cycle_l1_baseline = Some(new_block);
                                "l1_block_advance"
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    target: "based_rollup::composer_bundle",
                                    cycle_l1_baseline = baseline,
                                    grace_timeout_ms = grace_timeout.as_millis() as u64,
                                    "cycle_rotation: grace timeout without L1 advance past baseline"
                                );
                                if let Ok(n) = fetch_l1_block_number(&client, url).await {
                                    current_cycle_l1_baseline = Some(n);
                                }
                                "grace_timeout"
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: "based_rollup::composer_bundle",
                                    %e,
                                    "cycle_rotation: L1 poll error — rotating by timer"
                                );
                                "poll_error"
                            }
                        }
                    }
                    (Some(url), None) => {
                        if let Ok(n) = fetch_l1_block_number(&client, url).await {
                            current_cycle_l1_baseline = Some(n);
                        }
                        sleep(grace_timeout).await;
                        "poll_not_initialized"
                    }
                    (None, _) => {
                        sleep(grace_timeout).await;
                        "timer_only"
                    }
                }
            } else {
                if let Some(handle) = finalize_handle {
                    let finalize_timeout =
                        Duration::from_millis(self.config.l1_block_time_ms.saturating_mul(6));
                    match tokio::time::timeout(finalize_timeout, handle).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(
                                target: "based_rollup::composer_bundle",
                                %e,
                                "cycle_rotation: finalize task join error"
                            );
                        }
                        Err(_) => {
                            tracing::warn!(
                                target: "based_rollup::composer_bundle",
                                timeout_ms = finalize_timeout.as_millis() as u64,
                                "cycle_rotation: finalize timeout before receipts"
                            );
                        }
                    }
                }

                match &l1_poll_url {
                    Some(url) => {
                        let receipt_timeout =
                            Duration::from_millis(self.config.l1_block_time_ms.saturating_mul(6));
                        match wait_for_l1_receipts(
                            &client,
                            url,
                            &drained_tx_hashes,
                            receipt_timeout,
                            Duration::from_millis(L1_POLL_INTERVAL_MS),
                        )
                        .await
                        {
                            Ok(true) => {
                                if let Ok(n) = fetch_l1_block_number(&client, url).await {
                                    current_cycle_l1_baseline = Some(n);
                                }
                                "bundle_receipts"
                            }
                            Ok(false) => {
                                tracing::warn!(
                                    target: "based_rollup::composer_bundle",
                                    tx_count = drained_tx_hashes.len(),
                                    timeout_ms = receipt_timeout.as_millis() as u64,
                                    "cycle_rotation: timed out waiting for drained bundle receipts"
                                );
                                if let Ok(n) = fetch_l1_block_number(&client, url).await {
                                    current_cycle_l1_baseline = Some(n);
                                }
                                "receipt_timeout"
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: "based_rollup::composer_bundle",
                                    %e,
                                    "cycle_rotation: receipt polling error — rotating anyway"
                                );
                                if let Ok(n) = fetch_l1_block_number(&client, url).await {
                                    current_cycle_l1_baseline = Some(n);
                                }
                                "receipt_poll_error"
                            }
                        }
                    }
                    None => {
                        sleep(grace_timeout).await;
                        "timer_only_nonempty"
                    }
                }
            };

            tracing::debug!(
                target: "based_rollup::composer_bundle",
                rotation_reason,
                "cycle_rotation"
            );
            self.rotate();
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
//  L1 block observation
// ──────────────────────────────────────────────────────────────────────────────

/// Poll `eth_blockNumber` at `poll_interval` until it's strictly greater than
/// `baseline`, or until `timeout` elapses.
///
/// The baseline is provided by the caller — this is important because L1 may
/// have advanced significantly during the bundle window; the "advance" we care
/// about is relative to the cycle's baseline, not to whenever this function
/// was called.
///
/// - `Ok(Some(new_block))` — some block > baseline observed; returns its number.
/// - `Ok(None)` — timeout elapsed with no block > baseline.
/// - `Err(e)` — never returned from the polling loop (transient RPC errors are
///   swallowed during polling); reserved for future structural failure modes.
pub async fn wait_for_l1_block_past(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    baseline: u64,
    timeout: Duration,
    poll_interval: Duration,
) -> eyre::Result<Option<u64>> {
    let deadline = std::time::Instant::now() + timeout;
    // First poll immediately — if L1 already advanced during the window,
    // we catch it without sleeping.
    if let Ok(n) = fetch_l1_block_number(client, l1_rpc_url).await {
        if n > baseline {
            return Ok(Some(n));
        }
    }
    loop {
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(poll_interval).await;
        match fetch_l1_block_number(client, l1_rpc_url).await {
            Ok(n) if n > baseline => return Ok(Some(n)),
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
}

async fn fetch_l1_block_number(client: &reqwest::Client, l1_rpc_url: &str) -> eyre::Result<u64> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_blockNumber",
        "params": [],
        "id": 1
    });
    let resp = client.post(l1_rpc_url).json(&req).send().await?;
    let body: serde_json::Value = resp.json().await?;
    let hex = body
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("missing result"))?;
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    Ok(u64::from_str_radix(hex, 16)?)
}

async fn has_l1_receipt(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    tx_hash: B256,
) -> eyre::Result<bool> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getTransactionReceipt",
        "params": [format!("{tx_hash:#x}")],
        "id": 1
    });
    let resp = client.post(l1_rpc_url).json(&req).send().await?;
    let body: serde_json::Value = resp.json().await?;
    Ok(!body
        .get("result")
        .unwrap_or(&serde_json::Value::Null)
        .is_null())
}

async fn wait_for_l1_receipts(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    tx_hashes: &[B256],
    timeout: Duration,
    poll_interval: Duration,
) -> eyre::Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut all_present = true;
        for tx_hash in tx_hashes {
            if !has_l1_receipt(client, l1_rpc_url, *tx_hash).await? {
                all_present = false;
                break;
            }
        }
        if all_present {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(poll_interval).await;
    }
}

// ──────────────────────────────────────────────────────────────────────────────
//  Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Wall-clock milliseconds since UNIX epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Deterministic bundle_id = keccak256(cycle_start_ms || sorted_tx_hashes).
pub fn compute_bundle_id(cycle_start_ms: u64, txs: &[PendingUserTx]) -> B256 {
    let mut buf = Vec::with_capacity(8 + txs.len() * 32);
    buf.extend_from_slice(&cycle_start_ms.to_be_bytes());
    let mut hashes: Vec<B256> = txs.iter().map(|t| t.tx_hash).collect();
    hashes.sort();
    // Dedup protects against accidental duplicate before the dedup fence hits.
    let unique: HashSet<B256> = hashes.iter().copied().collect();
    let mut sorted_unique: Vec<B256> = unique.into_iter().collect();
    sorted_unique.sort();
    for h in &sorted_unique {
        buf.extend_from_slice(h.as_slice());
    }
    keccak256(&buf)
}

/// Extract the effective_gas_price from a signed raw tx for mempool ordering.
///
/// Matches reth's mempool ordering:
/// - Legacy (type 0x00) / EIP-2930 (type 0x01): `tx.gas_price`.
/// - EIP-1559 (type 0x02): `tx.max_priority_fee_per_gas` — the tip, which is what
///   the miner actually bids against other txs with the same sender.
///
/// On decode failure (malformed tx) returns `0` so the tx lands at the tail of
/// any sort but is still forwardable.
pub fn effective_gas_price(raw_tx: &[u8]) -> u128 {
    use alloy_consensus::TxEnvelope;
    use alloy_eips::eip2718::Decodable2718;

    let Ok(envelope) = TxEnvelope::decode_2718(&mut &raw_tx[..]) else {
        return 0;
    };

    match envelope {
        TxEnvelope::Legacy(signed) => signed.tx().gas_price,
        TxEnvelope::Eip2930(signed) => signed.tx().gas_price,
        TxEnvelope::Eip1559(signed) => signed.tx().max_priority_fee_per_gas,
        TxEnvelope::Eip4844(signed) => signed.tx().tx().max_priority_fee_per_gas,
        TxEnvelope::Eip7702(signed) => signed.tx().max_priority_fee_per_gas,
    }
}

/// Sort a bundle in-place by effective_gas_price descending, with tx_hash as a
/// stable tiebreaker (ensures determinism for the bundle_id).
pub fn sort_bundle_by_gas_desc(bundle: &mut [PendingUserTx]) {
    bundle.sort_by(|a, b| {
        b.effective_gas_price
            .cmp(&a.effective_gas_price)
            .then_with(|| a.tx_hash.cmp(&b.tx_hash))
    });
}

#[cfg(test)]
mod tests;
