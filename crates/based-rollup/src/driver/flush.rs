//! L1 submission pipeline: `flush_to_l1` and its helpers.
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1k. This module
//! owns the five methods that handle submitting pending blocks and
//! cross-chain entries to L1:
//!
//! - [`Driver::flush_to_l1`] — main submission entry point. Drains
//!   pending blocks and cross-chain entries, constructs a `FlushPlan`,
//!   arms the entry verification hold, submits via the proposer, then
//!   handles receipt outcomes (confirmation, revert, timeout).
//! - [`Driver::verify_trigger_receipts`] — wait for L2→L1 trigger tx
//!   receipts and classify the outcome as `AllConfirmed` or `Reverted`.
//! - [`Driver::send_l2_to_l1_triggers`] — send `executeL2TX` trigger
//!   txs with explicit nonces. Resets nonce cache on failure.
//! - [`Driver::forward_queued_l1_txs`] — forward raw L1 user txs
//!   queued by the composer RPC after successful postBatch.
//! - [`Driver::compute_gas_overbid`] — peek at queued L1 user txs
//!   and compute a gas price hint that overbids them so the builder's
//!   postBatch tx is ordered first within the same L1 block.

use super::Driver;
use super::flush_plan::{Collected, FlushPlan, NoEntries, SendResult};
use super::pending_queue::TriggerMetadata;
use super::types::{
    L1ConfirmedAnchor, MAX_BATCH_SIZE, REORG_SAFETY_THRESHOLD, SUBMISSION_COOLDOWN_SECS,
    SiblingReorgRequest, TriggerExecutionResult, find_rightmost_sibling_reorg_target,
    reorg_depth_exceeded,
};
use crate::proposer::{GasPriceHint, PendingBlock};
use alloy_primitives::{B256, Bytes, U256};

/// Outcome of the flush pre-check phase (refactor PLAN step 2.6).
///
/// `flush_precheck` runs the first ~90 lines of `flush_to_l1`'s early-return
/// sequence and produces one of three terminal states:
///
/// - `Proceed` — all preconditions met, caller should continue with block
///   collection and submission. `on_chain_root` is the latest L1 state root.
/// - `Skip` — one of the preconditions failed but no rewind is needed
///   (e.g., nothing to submit, hold active, cooldown, proposer absent).
/// - `Rewind` — the on-chain root check triggered a rewind (already applied).
enum FlushPrecheckResult {
    Proceed { on_chain_root: B256 },
    Skip,
}
use alloy_provider::Provider;
use alloy_sol_types::SolCall;
use eyre::Result;
use reth_provider::{
    BlockHashReader, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use tracing::{error, info, warn};

impl<P, Pool> Driver<P, Pool>
where
    P: DatabaseProviderFactory
        + StageCheckpointReader
        + BlockNumReader
        + BlockHashReader
        + HeaderProvider<Header = alloy_consensus::Header>
        + TransactionsProvider<Transaction = reth_ethereum_primitives::TransactionSigned>
        + StateProviderFactory
        + Send
        + Sync,
    P::ProviderRW: StageCheckpointWriter,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_transaction_pool::PoolTransaction<
                Consensus = reth_ethereum_primitives::TransactionSigned,
            >,
        > + reth_transaction_pool::TransactionPoolExt
        + Send
        + Sync,
{
    /// Run the flush precondition checks: proposer availability, queue
    /// emptiness, hold state, submission cooldown, wallet balance, and
    /// on-chain root deduplication. Returns `Proceed { on_chain_root }`
    /// when all checks pass, `Skip` when the flush should be silently
    /// skipped this cycle. Any rewind triggered by the on-chain root
    /// check is applied inline before returning `Skip`.
    ///
    /// Refactor PLAN step 2.6 — makes the early-return sequence in
    /// `flush_to_l1` explicit and testable in isolation.
    async fn flush_precheck(&mut self) -> FlushPrecheckResult {
        let Some(proposer) = &self.proposer else {
            if !self.pending_submissions.is_empty() {
                warn!(
                    target: "based_rollup::driver",
                    count = self.pending_submissions.len(),
                    "discarding pending blocks — proposer not available (no private key?)"
                );
                self.pending_submissions.clear();
            }
            self.pending_l1.clear();
            return FlushPrecheckResult::Skip;
        };

        if self.pending_submissions.is_empty() && self.pending_l1.is_empty() {
            return FlushPrecheckResult::Skip;
        }

        // Entry verification hold (§4f nonce safety)
        if let Some(entry_block) = self.hold.armed_for() {
            info!(
                target: "based_rollup::driver",
                entry_block,
                pending_blocks = self.pending_submissions.len(),
                "holding submissions — awaiting derivation verification of entry-bearing block"
            );
            return FlushPrecheckResult::Skip;
        }

        // Submission cooldown
        if let Some(last_fail) = self.last_submission_failure {
            if last_fail.elapsed() < std::time::Duration::from_secs(SUBMISSION_COOLDOWN_SECS) {
                return FlushPrecheckResult::Skip;
            }
        }

        // Periodically check wallet balance (every 5 minutes)
        if self.last_balance_check.elapsed() > std::time::Duration::from_secs(300) {
            let _ = proposer.check_wallet_balance().await;
            self.last_balance_check = std::time::Instant::now();
        }

        // Skip blocks already submitted on-chain by comparing state roots.
        let on_chain_root = match proposer.last_submitted_state_root().await {
            Ok(root) => {
                if root != B256::ZERO {
                    if let Some(pos) = self.pending_submissions.iter().rposition(|b| {
                        b.state_root == root
                            || b.clean_state_root.as_b256() == root
                            || b.intermediate_roots.contains(&root)
                    }) {
                        // Issue #36 detection: scan blocks about to be drained
                        // for the speculative-vs-clean divergence signature. If
                        // a block's `clean_state_root == on_chain_root` but
                        // `state_root != root`, reth canonicalized the
                        // speculative (pre-§4f-filter) version while L1
                        // confirmed the clean version. Reth cannot unwind
                        // committed blocks via FCU (silent no-op per Engine
                        // API spec), so queue a sibling reorg for the
                        // divergent block — the subsequent re-derivation will
                        // produce the canonical tx set and `step_sync` will
                        // swap it in via `rebuild_block_as_sibling`.
                        //
                        // Record BEFORE draining so we retain the evidence
                        // even after the blocks are popped.
                        //
                        // M4 (second-pass review): delegate to
                        // `find_rightmost_sibling_reorg_target`, which scans
                        // the window in REVERSE so the block at `pos` (the
                        // rightmost `rposition` match above) is tried first.
                        // Earlier blocks can coincidentally have
                        // `clean_state_root == on_chain_root` (e.g. an empty
                        // block whose clean root matches a later entry
                        // block's on-chain root); a forward scan would pick
                        // the first such match and hijack the decision.
                        if self.pending_sibling_reorg.is_none() {
                            if let Some(req) = find_rightmost_sibling_reorg_target(
                                &self.pending_submissions,
                                root,
                                u64::from(self.consecutive_rewind_cycles),
                                REORG_SAFETY_THRESHOLD,
                                pos + 1,
                            ) {
                                let speculative_root = self
                                    .pending_submissions
                                    .iter()
                                    .find(|b| b.l2_block_number == req.target_l2_block)
                                    .map(|b| b.state_root)
                                    .unwrap_or_default();
                                warn!(
                                    target: "based_rollup::driver",
                                    target_block = req.target_l2_block,
                                    %speculative_root,
                                    clean_root = %req.expected_root,
                                    on_chain_root = %root,
                                    "issue #36: speculative/clean divergence detected at drain — \
                                     queuing sibling reorg (reth canonicalized speculative \
                                     version; FCU-to-ancestor is a no-op per Engine API spec)"
                                );
                                self.pending_sibling_reorg = Some(req);
                            }
                        }

                        // Capture the trimmed block's L2 number BEFORE popping,
                        // so we can advance `l1_confirmed_anchor` to match.
                        let trimmed_l2_block =
                            self.pending_submissions[pos].l2_block_number;

                        for _ in 0..=pos {
                            self.pending_submissions.pop_front();
                        }

                        // Advance the L1-confirmed anchor to the block we just
                        // trimmed: the trim proves the corresponding postBatch
                        // confirmed on L1 (its state root is the current
                        // `rollup.stateRoot`). Without this, a postBatch whose
                        // `wait_for_l1_receipt` poll timed out — but whose tx
                        // actually confirmed in the background — would be
                        // detected here via the state-root trim, but the anchor
                        // would remain stale. On the next tick the driver's
                        // rewind logic compares `first_pre` (from post-trim
                        // pending, which starts *after* the confirmed block)
                        // to the stale anchor and enters a permanent
                        // `pre_state_root mismatch` rewind loop that reth
                        // cannot unwind through (Ethereum-engine FCU-to-ancestor
                        // is a silent no-op).
                        //
                        // `last_seen_l1_block` is the freshest L1 head the
                        // driver has observed — safe upper bound for the
                        // anchor's L1 block (the real containing L1 block is
                        // ≤ this).
                        let should_update = self
                            .l1_confirmed_anchor
                            .is_none_or(|a| a.l2_block_number < trimmed_l2_block);
                        if should_update {
                            let new_anchor = L1ConfirmedAnchor {
                                l2_block_number: trimmed_l2_block,
                                l1_block_number: self.last_seen_l1_block,
                            };
                            self.l1_confirmed_anchor = Some(new_anchor);
                            self.save_l1_confirmed_anchor();
                            info!(
                                target: "based_rollup::driver",
                                l2_block = trimmed_l2_block,
                                l1_block = self.last_seen_l1_block,
                                "advanced L1-confirmed anchor via flush-precheck \
                                 on-chain root trim (silent-confirmation recovery)"
                            );
                        }
                    }
                }
                root
            }
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    "failed to read last submitted state root from L1, will retry"
                );
                return FlushPrecheckResult::Skip;
            }
        };

        // Issue #36 flush-path dispatch: if we queued a sibling reorg (either
        // just above or in a previous cycle), force Sync mode so derivation
        // re-runs block `target_l2_block` from L1 calldata with §4f filtering
        // applied. `step_sync` will detect the re-derivation of a block whose
        // number is at or below the current head and call
        // `rebuild_block_as_sibling` to swap it in.
        //
        // The request must SURVIVE `clear_internal_state` — the M2 fix wipes
        // it deliberately, so we stash and reinstate.
        if let Some(req) = self.pending_sibling_reorg {
            info!(
                target: "based_rollup::driver",
                target = req.target_l2_block,
                expected_root = %req.expected_root,
                pending_depth = self
                    .l2_head_number
                    .saturating_sub(req.target_l2_block),
                "switching to Sync mode to re-derive block for sibling reorg"
            );
            let (rewind_target, rollback_l1_block) = if let Some(anchor) = self.l1_confirmed_anchor
            {
                (
                    req.target_l2_block
                        .saturating_sub(1)
                        .max(anchor.l2_block_number),
                    anchor.l1_block_number.saturating_sub(1),
                )
            } else {
                (
                    req.target_l2_block.saturating_sub(1),
                    self.config.deployment_l1_block,
                )
            };
            // M2: stash the request across `clear_internal_state`.
            //
            // Symmetry note (issue #36 third-pass review): unlike the verify
            // fast path (`apply_sibling_reorg_plan`), this flush-path dispatch
            // does NOT call `set_rewind_target`. This is intentional: control
            // returns from `flush_to_l1` here with `mode == Sync`, so the
            // subsequent `commit_batch` that would overwrite the derivation
            // rollback never runs (only `step_builder`'s trailing
            // `commit_batch` was the danger). If a future refactor moves
            // `commit_batch` or any rollback-invalidating call to run AFTER
            // this return, the rewind target MUST be set here to match
            // `apply_sibling_reorg_plan`.
            let saved_req = req;
            self.clear_internal_state();
            self.pending_sibling_reorg = Some(saved_req);
            self.derivation.set_last_derived_l2_block(rewind_target);
            self.derivation.rollback_to(rollback_l1_block);
            self.mode = super::DriverMode::Sync;
            self.synced
                .store(false, std::sync::atomic::Ordering::Relaxed);
            // Do NOT bump consecutive_rewind_cycles — sibling reorg is a
            // single productive recovery, not a rewind cycle.
            return FlushPrecheckResult::Skip;
        }

        // Stale-anchor detection: when the front of `pending_submissions` was
        // built against an L1 head that we have since observed advance past,
        // its `mix_hash` (= `l1_context_block`) commits the proof to a
        // `target_block = l1_context_block + 1` that is now in the past. Any
        // bundle submitted to a block-builder RPC against that target drops on
        // miss the moment it is relayed (the target slot has already been
        // produced). The block's `mix_hash` is a header field — we cannot
        // restamp it without rebuilding the block, so the only path forward is
        // to rebuild the block as a sibling with the *current* L1 head as
        // `mix_hash` and let reth wipe everything past it. See
        // `Driver::rebuild_with_fresh_l1_context`.
        //
        // Mutually exclusive with the issue-#36 sibling-reorg path above —
        // that one expects an `expected_root` derived from a confirmed L1
        // batch, while this path explicitly does not have one. If a
        // sibling-reorg request is already pending, defer to it.
        //
        // `pending_anchor_refresh` is consumed at the top of `step()` before
        // any mode-specific dispatch (analogous to `pending_rewind_target`).
        if self.pending_anchor_refresh.is_none() {
            if let Some(first) = self.pending_submissions.front() {
                let stale_target_l1 = first.l1_context_block.saturating_add(1);
                if stale_target_l1 <= self.last_seen_l1_block {
                    let depth = self
                        .l2_head_number
                        .saturating_sub(first.l2_block_number);
                    if reorg_depth_exceeded(depth, REORG_SAFETY_THRESHOLD) {
                        // Beyond `REORG_SAFETY_THRESHOLD` (75% of reth's
                        // `MAX_REORG_DEPTH = 64`) the wipe would push reth
                        // past its `CHANGESET_CACHE_RETENTION_BLOCKS`
                        // window, after which no recovery primitive
                        // (sibling reorg, anchor refresh, anything) can
                        // restore consensus. Surface the wedge with a
                        // structured ERROR for operator intervention; do
                        // NOT auto-rebuild.
                        error!(
                            target: "based_rollup::driver",
                            target_l2_block = first.l2_block_number,
                            stale_target_l1,
                            last_seen_l1 = self.last_seen_l1_block,
                            depth,
                            threshold = REORG_SAFETY_THRESHOLD,
                            "stale bundle anchor detected but rebuild depth exceeds \
                             safety threshold — bundle target is unreachable and \
                             auto-recovery would push reth past its changeset \
                             eviction window. Operator intervention required."
                        );
                        return FlushPrecheckResult::Skip;
                    }
                    warn!(
                        target: "based_rollup::driver",
                        target_l2_block = first.l2_block_number,
                        stale_l1_context = first.l1_context_block,
                        stale_target_l1,
                        last_seen_l1 = self.last_seen_l1_block,
                        depth,
                        "stale bundle anchor — front of pending_submissions targets \
                         a past L1 block; queuing anchor refresh to rebuild with \
                         current L1 head as mix_hash"
                    );
                    self.pending_anchor_refresh = Some(first.l2_block_number);
                    return FlushPrecheckResult::Skip;
                }
            }
        } else {
            // An anchor refresh is already queued — wait for `step()` to
            // consume it on the next iteration.
            return FlushPrecheckResult::Skip;
        }

        if self.pending_submissions.is_empty() && self.pending_l1.is_empty() {
            return FlushPrecheckResult::Skip;
        }

        FlushPrecheckResult::Proceed { on_chain_root }
    }

    /// Submit pending blocks and cross-chain entries to L1 via the proposer.
    ///
    /// Combines block submission and cross-chain entry posting into a single
    /// `submit_to_l1` call. Drains external cross-chain entries from the shared
    /// queue, collects pending blocks, and sends everything in one L1 transaction.
    pub(super) async fn flush_to_l1(&mut self) -> Result<()> {
        // Phase 2.6: precheck sequence extracted to a dedicated method.
        let on_chain_root = match self.flush_precheck().await {
            FlushPrecheckResult::Proceed { on_chain_root } => on_chain_root,
            FlushPrecheckResult::Skip => return Ok(()),
        };

        // Re-borrow proposer after precheck confirmed it exists. The precheck
        // already verified `self.proposer.is_some()` — this unwrap is safe.
        let proposer = self.proposer.as_ref().expect("precheck confirmed proposer");

        // Collect blocks to submit (up to MAX_BATCH_SIZE).
        // §4f nonce safety: when cross-chain entries are pending, limit the batch
        // to ONLY the blocks that were built WITH those entries. Subsequent blocks
        // have nonces that assume the entry protocol txs consumed nonces — if
        // derivation filters those txs (§4f), the nonces are wrong. By excluding
        // subsequent blocks from this batch, we ensure they are held until
        // derivation confirms the entry-bearing block.
        let has_pending_entries = !self.pending_l1.is_empty();

        // Per-anchor batch slicing: every block in a single bundle must share
        // the same `l1_context_block` (= `mix_hash`), because on-chain proof
        // verification compares each block's `mix_hash` to `blockhash(target_l1
        // - 1)` and there is exactly one `target_l1` per bundle. Pending
        // submissions accumulated across multiple builder ticks can carry
        // different `l1_context_block` values; mixing them in one bundle would
        // make the proof invalid for every block past the first anchor's run.
        //
        // Take only the contiguous run from the front whose `l1_context_block`
        // matches `pending.front().l1_context_block`. The rest stays in the
        // queue and lands in a subsequent bundle anchored at its own L1 block.
        let front_anchor = self
            .pending_submissions
            .front()
            .map(|b| b.l1_context_block);
        let same_anchor_run = match front_anchor {
            Some(a) => self
                .pending_submissions
                .iter()
                .take_while(|b| b.l1_context_block == a)
                .count(),
            None => 0,
        };
        let batch_size = if has_pending_entries {
            // Include ALL pending blocks when entries are present.
            // The entry block is the last one (just built). Earlier blocks are
            // non-entry blocks that accumulated during the hold or between cycles
            // (e.g., from complex-tx-sender generating L2 blocks).
            //
            // send_to_l1 builds a single aggregate immediate entry spanning
            // first_pre → last_clean, so the state delta chain works:
            //   Entry[0] immediate: pre_first → clean_last(=clean_entry_block)
            //   Entry[1..N] deferred: clean_entry_block → intermediates
            //
            // §4f nonce safety is preserved: entry protocol txs are only in the
            // LAST block, and earlier blocks don't depend on entry nonces.
            same_anchor_run.min(MAX_BATCH_SIZE)
        } else {
            same_anchor_run.min(MAX_BATCH_SIZE)
        };
        let blocks: Vec<PendingBlock> = self.pending_submissions.drain(..batch_size).collect();

        // Verify the first block's pre_state_root matches the on-chain state.
        // If they mismatch, the postBatch will revert (StateDelta.currentState
        // must equal on-chain stateRoot).
        //
        // With protocol tx filtering (§4f), rewind is productive: re-derivation
        // filters unconsumed executeRemoteCall txs from callData, producing the
        // correct root. No state alignment is needed.
        //
        // Retry a few times for transient mismatches (previous submission pending
        // or L1 reorg), then force rewind to re-derive from L1.
        if let Some(first) = blocks.first() {
            if first.pre_state_root != on_chain_root {
                let first_pre = first.pre_state_root;
                self.consecutive_flush_mismatches += 1;

                const MAX_FLUSH_MISMATCHES: u32 = 2;
                if self.consecutive_flush_mismatches >= MAX_FLUSH_MISMATCHES {
                    if self.consecutive_rewind_cycles > 0 {
                        // Already rewound at least once and the mismatch persists —
                        // rewinding is futile. The divergence is permanent (e.g., an
                        // entry-bearing block's bridge tx reverted on L1, so §4f
                        // NEVER override pre_state_root — it masks real bugs.
                        // If we get here, there is a genuine derivation or filtering
                        // issue that needs investigation. Log the evidence and keep
                        // retrying the rewind. The builder will be stuck but that is
                        // better than submitting blocks with fabricated pre_state_roots
                        // that fullnodes cannot reproduce.
                        error!(
                            target: "based_rollup::driver",
                            first_pre = %first_pre,
                            on_chain = %on_chain_root,
                            rewind_cycles = self.consecutive_rewind_cycles,
                            mismatches = self.consecutive_flush_mismatches,
                            l2_block = first.l2_block_number,
                            "persistent pre_state_root mismatch after rewind — \
                             NOT overriding (this indicates a bug in derivation/filtering). \
                             Builder will keep retrying rewind until the root cause is fixed."
                        );
                        // Rewind again — each attempt re-derives with latest L1 data.
                        let earliest_block = first.l2_block_number;
                        let (rewind_target, rollback_l1_block) =
                            if let Some(anchor) = self.l1_confirmed_anchor {
                                let target = earliest_block.saturating_sub(1);
                                (target, anchor.l1_block_number.saturating_sub(1))
                            } else {
                                (0, self.config.deployment_l1_block)
                            };
                        self.rewind_to_re_derive(rewind_target, rollback_l1_block);
                        return Ok(());
                    } else {
                        // First time hitting persistent mismatch. Before falling
                        // through to bare rewind, attempt to detect the
                        // post-commit anchor divergence pattern: the anchor
                        // block's LOCAL reth state root disagrees with what
                        // `rollups(rollupId).stateRoot` contains on L1. This
                        // happens when the entry-bearing block's trigger tx
                        // reverted on L1 AFTER postBatch confirmed the
                        // §4f-filtered root — reth has the speculative block
                        // canonicalised, but the confirmed root is the filtered
                        // one. A bare rewind to `earliest_block - 1` is floored
                        // at `anchor.l2_block_number` to avoid losing the
                        // confirmed anchor, so it can never rebuild the anchor
                        // itself — the mismatch repeats forever.
                        //
                        // The fix queues a sibling reorg for the anchor block,
                        // mirroring the drain-time detection at lines 134–184
                        // (which only works while the divergent block still
                        // sits in `pending_submissions`). Here the divergent
                        // block is long-confirmed and only lives in reth's
                        // canonical DB, so we target it directly.
                        if self.pending_sibling_reorg.is_none() {
                            if let Some(anchor) = self.l1_confirmed_anchor {
                                let local_anchor_root = self
                                    .l2_provider
                                    .sealed_header(anchor.l2_block_number)
                                    .ok()
                                    .flatten()
                                    .map(|h| h.state_root);
                                if let Some(local_root) = local_anchor_root {
                                    if local_root != on_chain_root {
                                        let reorg_depth = self
                                            .l2_head_number
                                            .saturating_sub(anchor.l2_block_number);
                                        if !reorg_depth_exceeded(
                                            reorg_depth,
                                            REORG_SAFETY_THRESHOLD,
                                        ) {
                                            warn!(
                                                target: "based_rollup::driver",
                                                anchor_l2 = anchor.l2_block_number,
                                                anchor_l1 = anchor.l1_block_number,
                                                local_anchor_root = %local_root,
                                                %on_chain_root,
                                                reorg_depth,
                                                l2_head = self.l2_head_number,
                                                "anchor-block post-commit divergence detected — \
                                                 queuing sibling reorg (reth canonicalized speculative \
                                                 version; confirmed root diverges from local header)"
                                            );
                                            self.pending_sibling_reorg =
                                                Some(SiblingReorgRequest {
                                                    target_l2_block: anchor.l2_block_number,
                                                    expected_root: on_chain_root,
                                                });
                                            // Preserve batched blocks for the
                                            // next attempt (same pattern as the
                                            // transient-mismatch branch below).
                                            for block in blocks.into_iter().rev() {
                                                self.pending_submissions.push_front(block);
                                            }
                                            self.consecutive_flush_mismatches = 0;
                                            // `flush_precheck` dispatch on the
                                            // next tick consumes
                                            // `pending_sibling_reorg` and
                                            // transitions to Sync mode cleanly.
                                            // Do NOT bump
                                            // `consecutive_rewind_cycles` —
                                            // sibling reorg is a productive
                                            // recovery, not a rewind cycle.
                                            return Ok(());
                                        } else {
                                            error!(
                                                target: "based_rollup::driver",
                                                anchor_l2 = anchor.l2_block_number,
                                                anchor_l1 = anchor.l1_block_number,
                                                local_anchor_root = %local_root,
                                                %on_chain_root,
                                                reorg_depth,
                                                threshold = REORG_SAFETY_THRESHOLD,
                                                l2_head = self.l2_head_number,
                                                "anchor-block post-commit divergence beyond \
                                                 safety threshold — halting; manual operator \
                                                 recovery required (reth changeset eviction \
                                                 window would be crossed)"
                                            );
                                            // Preserve batched blocks so an
                                            // operator can inspect them.
                                            for block in blocks.into_iter().rev() {
                                                self.pending_submissions.push_front(block);
                                            }
                                            self.consecutive_flush_mismatches = 0;
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                        }

                        // Fallback: bare rewind to re-derive. Retained for
                        // non-anchor-divergence cases (L1 reorg, transient
                        // context mismatches with `derived.filtering = None`,
                        // etc.).
                        let earliest_block = first.l2_block_number;

                        let (rewind_target, rollback_l1_block) =
                            if let Some(anchor) = self.l1_confirmed_anchor {
                                let target =
                                    earliest_block.saturating_sub(1).max(anchor.l2_block_number);
                                let l1_rollback = anchor.l1_block_number.saturating_sub(1);
                                info!(
                                    target: "based_rollup::driver",
                                    anchor_l2 = anchor.l2_block_number,
                                    anchor_l1 = anchor.l1_block_number,
                                    "using L1-confirmed anchor for rollback"
                                );
                                (target, l1_rollback)
                            } else {
                                // No batch has ever been confirmed on L1: on-chain state
                                // is still the genesis root, so NOTHING local below
                                // `earliest_block` has been committed. Rewind all the
                                // way to genesis — otherwise we retain local blocks
                                // whose post-state doesn't correspond to anything on
                                // L1 and the next flush loops on the same mismatch.
                                // Matches the sibling branches at :358-364 and :1028-1037.
                                (0, self.config.deployment_l1_block)
                            };

                        error!(
                            target: "based_rollup::driver",
                            first_pre = %first_pre,
                            on_chain = %on_chain_root,
                            mismatches = self.consecutive_flush_mismatches,
                            rewind_target,
                            rollback_l1_block,
                            pending_blocks = blocks.len() + self.pending_submissions.len(),
                            "persistent pre_state_root mismatch — rewind to re-derive \
                             (protocol tx filtering §4f will produce correct root)"
                        );
                        self.consecutive_flush_mismatches = 0;
                        self.rewind_to_re_derive(rewind_target, rollback_l1_block);
                        // Do NOT forward L1 txs during rewind — the entries
                        // are not on L1 yet, so the user's tx would revert
                        // with ExecutionNotFound, wasting gas. The L1 txs
                        // remain in the queue and will be forwarded after the
                        // next successful postBatch.
                        return Ok(());
                    }
                } else {
                    // Transient mismatch — re-queue and retry next cycle
                    for block in blocks.into_iter().rev() {
                        self.pending_submissions.push_front(block);
                    }
                    warn!(
                        target: "based_rollup::driver",
                        first_pre = %first_pre,
                        on_chain = %on_chain_root,
                        mismatches = self.consecutive_flush_mismatches,
                        "pre_state_root mismatch — a previous submission may be pending, \
                         re-queuing"
                    );
                    return Ok(());
                }
            } else {
                // Mismatch resolved — reset counters
                self.consecutive_flush_mismatches = 0;
                self.consecutive_rewind_cycles = 0;
            }
        }

        // Drain L1 entry queue for submission. The whole
        // `PendingL1SubmissionQueue` moves by value, preserving the
        // invariant that (entries, groups) stay in lock-step.
        let pending_l1_owned = std::mem::take(&mut self.pending_l1);

        info!(
            target: "based_rollup::driver",
            l1_entries = pending_l1_owned.len_entries(),
            groups = pending_l1_owned.num_groups(),
            entry_mix = ?pending_l1_owned.entry_mix(),
            pending_blocks = blocks.len(),
            "flush_to_l1: drained entries and blocks for submission"
        );

        // Extract the per-group trigger metadata the rest of this
        // function needs (for `send_l2_to_l1_triggers`). Lives
        // outside the `FlushPlan` because the plan is consumed by
        // `submit_via` but the trigger send happens AFTER the
        // submit. Step 2.7 (FlushAssembly → FlushPlan consolidation)
        // will fold this into the plan.
        let trigger_metadata: Vec<Option<TriggerMetadata>> = pending_l1_owned
            .groups
            .iter()
            .map(|g| g.trigger.clone())
            .collect();

        // Clone the entries separately from the plan. The plan will
        // own the authoritative copy and return it via
        // `SendResult::Failed { rollback }` if the submit fails; this
        // `l1_entries` clone is what the POST-send logic uses for
        // consumption-event filtering, logging, and entry counting.
        let l1_entries = pending_l1_owned.entries.clone();
        let has_entries = !l1_entries.is_empty();

        // Clone the block numbers we need for logging and anchor
        // updates after the submit consumes the plan. Blocks
        // themselves still live inside the plan until either
        // success (dropped) or failure (returned via rollback).
        let block_l2_numbers: Vec<u64> = blocks.iter().map(|b| b.l2_block_number).collect();

        // Clone the full blocks + queue for the post-Ok receipt
        // failure path. That path (receipt timeout or RPC error
        // after a successful send) needs to restore the drained
        // state even though `SendResult` already dropped it.
        // Pre-1.7 this was cheap because `blocks` was kept around;
        // post-1.7 the plan owns them, so we retain a clone.
        let blocks_clone_for_receipt_failure = blocks.clone();
        let pending_l1_clone_for_receipt_failure = pending_l1_owned.clone();

        // Construct the `FlushPlan` typestate. The plan owns the
        // blocks and the L1 entry queue; moving them in here means
        // no borrow crosses the `.await` on `submit_via`. Invariant
        // #1 ("hold MUST be armed before send") is encoded as:
        //
        //   - `FlushPlan<Collected>` is NOT `Sendable`.
        //   - The only way to reach `FlushPlan<HoldArmed>` is via
        //     `arm_hold`, which physically calls `hold.arm(...)`.
        //   - `submit_via` requires `S: Sendable` — passing a
        //     `Collected` plan is a compile error.
        //
        // The `NoEntries` vs `Collected` branch exists so that
        // blocks-only submissions don't arm the hold at all (the
        // plan's `entry_block` is `None` and `arm_hold` is a no-op
        // on the Collected path; but splitting into two marker
        // types makes the no-hold case trivially sendable without
        // touching the hold field at all — invariant #1 only applies
        // when there are actual entries).
        let gas_hint = self.compute_gas_overbid();
        let send_result = if has_entries {
            let plan = FlushPlan::<Collected>::new_collected(blocks, pending_l1_owned)
                .arm_hold(&mut self.hold);
            info!(
                target: "based_rollup::driver",
                l2_block = ?plan.block_count(),
                entry_count = plan.entry_count(),
                "setting entry verification hold before L1 submission (§4f nonce safety, FlushPlan<HoldArmed>)"
            );
            plan.submit_via(proposer, gas_hint).await
        } else {
            let plan = FlushPlan::<NoEntries>::new_blocks_only(blocks);
            plan.submit_via(proposer, gas_hint).await
        };

        // Unpack the `SendResult` into the legacy `Result<B256>`
        // shape the rest of this function still uses. Step 2.7's
        // FlushStage pipeline will consume `SendResult` directly
        // via match arms.
        let (send_result, rollback) = match send_result {
            SendResult::Ok { tx_hash } => (Ok::<B256, eyre::Report>(tx_hash), None),
            SendResult::Failed { error, rollback } => {
                (Err::<B256, eyre::Report>(error), Some(rollback))
            }
        };
        match send_result {
            Ok(tx_hash) => {
                if let (Some(&first), Some(&last)) =
                    (block_l2_numbers.first(), block_l2_numbers.last())
                {
                    info!(
                        target: "based_rollup::driver",
                        block_count = block_l2_numbers.len(),
                        entry_count = l1_entries.len(),
                        l2_blocks = %format!("{first}..={last}"),
                        "submitted to L1 (awaiting confirmation)"
                    );
                } else {
                    info!(
                        target: "based_rollup::driver",
                        entry_count = l1_entries.len(),
                        "submitted cross-chain entries to L1 (awaiting confirmation)"
                    );
                }
                // Forward queued L1 txs BEFORE waiting for receipt — they must land
                // in the same L1 block as postBatch for consumption to work.
                if has_entries {
                    self.forward_queued_l1_txs().await?;
                }
                // Send L1 trigger txs (executeL2TX) BEFORE waiting for receipt —
                // they must land in the SAME L1 block as postBatch
                // (ExecutionNotInCurrentBlock). Filter out None entries (protocol-
                // triggered groups that don't need executeL2TX).
                let effective_trigger_metadata: Vec<TriggerMetadata> = trigger_metadata
                    .iter()
                    .filter_map(|opt| opt.clone())
                    .collect();
                let trigger_tx_hashes: Vec<B256> = if !effective_trigger_metadata.is_empty() {
                    match self
                        .send_l2_to_l1_triggers(&effective_trigger_metadata)
                        .await
                    {
                        Ok(hashes) => hashes,
                        Err(err) => {
                            error!(
                                target: "based_rollup::driver",
                                %err,
                                "L2→L1 trigger tx failed — rewinding to re-derive"
                            );
                            let (rewind_target, rollback_l1_block) =
                                if let Some(anchor) = self.l1_confirmed_anchor {
                                    (
                                        anchor.l2_block_number,
                                        anchor.l1_block_number.saturating_sub(1),
                                    )
                                } else {
                                    (0, self.config.deployment_l1_block)
                                };
                            self.rewind_to_re_derive(rewind_target, rollback_l1_block);
                            return Ok(());
                        }
                    }
                } else {
                    vec![]
                };
                // Now wait for the postBatch tx to be confirmed.
                let proposer = self.proposer.as_ref().expect("checked above");
                match proposer.wait_for_l1_receipt(tx_hash).await {
                    Ok(l1_block_number) => {
                        self.last_submission_failure = None;
                        if let Some(&last_l2_block) = block_l2_numbers.last() {
                            self.l1_confirmed_anchor = Some(L1ConfirmedAnchor {
                                l2_block_number: last_l2_block,
                                l1_block_number,
                            });
                            self.save_l1_confirmed_anchor();
                            self.prune_tx_journal(last_l2_block);
                        }
                        // Entry verification hold was set before send_to_l1 (above).

                        // Verify all L2→L1 trigger receipts. Triggers land in the
                        // same L1 block as postBatch, so receipts should be available
                        // immediately after the postBatch receipt.
                        //
                        // The `#[must_use]` attribute on `TriggerExecutionResult`
                        // (invariant #15) makes it impossible for future callers
                        // to silently drop this outcome — every new variant must
                        // be handled explicitly or the build fails under
                        // `-D warnings`.
                        match self.verify_trigger_receipts(&trigger_tx_hashes).await {
                            TriggerExecutionResult::AllConfirmed { .. } => {
                                // All triggers landed — fall through to entry
                                // consumption verification below.
                            }
                            TriggerExecutionResult::Reverted {
                                reverted_count,
                                total,
                            } => {
                                // With intermediate state roots, the on-chain stateRoot
                                // is at an intermediate root (partial consumption).
                                // Derivation can filter unconsumed L2→L1 txs to
                                // produce the matching root via §4f. Rewind to re-derive.
                                warn!(
                                    target: "based_rollup::driver",
                                    reverted_count,
                                    total,
                                    "one or more L2→L1 triggers reverted — \
                                     rewinding for re-derivation with filtered txs"
                                );
                                // The anchor was JUST updated (line ~2063) to the current
                                // batch's last block — which IS the entry block.
                                // We must rewind to anchor - 1 so the entry block
                                // itself gets re-derived with §4f filtering applied.
                                let (rewind_target, rollback_l1_block) =
                                    if let Some(anchor) = self.l1_confirmed_anchor {
                                        (
                                            anchor.l2_block_number.saturating_sub(1),
                                            anchor.l1_block_number.saturating_sub(1),
                                        )
                                    } else {
                                        (0, self.config.deployment_l1_block)
                                    };
                                self.rewind_to_re_derive(rewind_target, rollback_l1_block);
                                return Ok(());
                            }
                        }

                        // Immediate entry verification (§218): entries must be consumed
                        // in the SAME L1 block as postBatch (ExecutionNotInCurrentBlock
                        // constraint). Check ExecutionConsumed events right now — no need
                        // to wait for derivation.
                        if has_entries {
                            let consumed_filter = alloy_rpc_types::Filter::new()
                                .address(self.config.rollups_address)
                                .event_signature(
                                    crate::cross_chain::execution_consumed_signature_hash(),
                                )
                                .from_block(l1_block_number)
                                .to_block(l1_block_number);
                            let consumed_hashes =
                                match self.get_l1_provider().get_logs(&consumed_filter).await {
                                    Ok(logs) => {
                                        crate::cross_chain::parse_execution_consumed_logs(&logs)
                                    }
                                    Err(err) => {
                                        warn!(
                                            target: "based_rollup::driver",
                                            %err,
                                            "failed to query ExecutionConsumed events — \
                                             falling back to deferral verification"
                                        );
                                        std::collections::HashMap::new()
                                    }
                                };

                            // Common: precompute the real-entry set (entries
                            // expected to emit `ExecutionConsumed`). Used by
                            // both the partial-consumption branch below and
                            // the zero-consumption branch that follows.
                            //
                            // REVERT / REVERT_CONTINUE entries are consumed
                            // inside reverted scopes and their
                            // `ExecutionConsumed` events are reverted by
                            // `ScopeReverted`, so they never appear in
                            // `consumed_hashes`. Action-hash-zero entries are
                            // the immediate postBatch entry (state-delta
                            // carrier with no consumption event of its own).
                            let revert_continue_hash =
                                crate::cross_chain::compute_revert_continue_hash(
                                    crate::cross_chain::RollupId::new(
                                        alloy_primitives::U256::from(self.config.rollup_id),
                                    ),
                                );
                            let real_entry_count = l1_entries
                                .iter()
                                .filter(|e| {
                                    e.action_hash != crate::cross_chain::ActionHash::ZERO
                                        && e.next_action.action_type
                                            != crate::cross_chain::CrossChainActionType::Revert
                                        && e.action_hash != revert_continue_hash
                                })
                                .count();

                            if !consumed_hashes.is_empty() {
                                // Count how many entries we need per hash.
                                let mut entry_counts: std::collections::HashMap<
                                    crate::cross_chain::ActionHash,
                                    usize,
                                > = std::collections::HashMap::new();
                                for e in l1_entries.iter() {
                                    if e.action_hash == crate::cross_chain::ActionHash::ZERO {
                                        continue;
                                    }
                                    if e.next_action.action_type
                                        == crate::cross_chain::CrossChainActionType::Revert
                                    {
                                        continue;
                                    }
                                    if e.action_hash == revert_continue_hash {
                                        continue;
                                    }
                                    *entry_counts.entry(e.action_hash).or_default() += 1;
                                }
                                // Check that consumed count >= entry count for each hash
                                let all_consumed = entry_counts.iter().all(|(hash, &needed)| {
                                    consumed_hashes.get(hash).is_some_and(|v| v.len() >= needed)
                                });

                                let consumed_total: usize =
                                    consumed_hashes.values().map(|v| v.len()).sum();

                                if all_consumed {
                                    info!(
                                        target: "based_rollup::driver",
                                        l1_block_number,
                                        consumed = consumed_total,
                                        "all entries consumed in postBatch L1 block — \
                                         releasing hold immediately (no deferral needed)"
                                    );
                                    self.hold.clear();
                                } else {
                                    // Partial consumption — some entries reverted.
                                    // Rewind immediately to rebuild with filtered txs.
                                    warn!(
                                        target: "based_rollup::driver",
                                        l1_block_number,
                                        consumed = consumed_total,
                                        total = real_entry_count,
                                        "partial entry consumption — rewinding immediately"
                                    );
                                    let entry_block = self.hold.armed_for();
                                    let (rewind_target, rollback_l1_block) =
                                        if let Some(anchor) = self.l1_confirmed_anchor {
                                            let target = entry_block
                                                .unwrap_or(anchor.l2_block_number)
                                                .saturating_sub(1);
                                            (target, anchor.l1_block_number.saturating_sub(1))
                                        } else {
                                            (0, self.config.deployment_l1_block)
                                        };
                                    self.rewind_to_re_derive(rewind_target, rollback_l1_block);
                                    return Ok(());
                                }
                            } else if real_entry_count > 0 {
                                // Zero-consumption: `ExecutionConsumed` logs
                                // were queried successfully but NONE were
                                // emitted, despite the batch carrying real
                                // entries. This is the signature of the
                                // trigger tx reverting completely on L1 (e.g.
                                // the depth-0 post-commit divergence observed
                                // on testnet 2026-04-17 — see
                                // `project_testnet_stall_2026_04_17.md`).
                                //
                                // The anchor block is the block we just
                                // confirmed on L1 via postBatch; its
                                // speculative local root diverges from the
                                // §4f-filtered root that Rollups.sol now
                                // stores. We can queue a sibling reorg for
                                // the anchor IMMEDIATELY at depth 0, before
                                // any subsequent blocks build up and drive us
                                // toward the eviction window. The subsequent
                                // `flush_precheck` dispatch picks up the
                                // request and switches to Sync mode cleanly.
                                //
                                // Guard: we need the POST-postBatch on-chain
                                // root. `on_chain_root` captured by
                                // `flush_precheck` at the top of the function
                                // is pre-postBatch; re-query it here.
                                //
                                // Re-acquire the proposer reference locally
                                // (rather than reusing the outer `proposer`
                                // binding) so the immutable borrow doesn't
                                // conflict with the `&mut self` calls earlier
                                // in this arm (e.g. `prune_tx_journal`).
                                let refreshed_on_chain_root = match self
                                    .proposer
                                    .as_ref()
                                    .expect("precheck confirmed proposer")
                                    .last_submitted_state_root()
                                    .await
                                {
                                    Ok(r) => Some(r),
                                    Err(err) => {
                                        warn!(
                                            target: "based_rollup::driver",
                                            %err,
                                            l1_block_number,
                                            "zero-consumption detected but failed to \
                                             re-query on-chain root — Gap 2 \
                                             (anchor-divergence detection in flush \
                                             mismatch path) will catch it next cycle"
                                        );
                                        None
                                    }
                                };

                                if let Some(refreshed_root) = refreshed_on_chain_root {
                                    if let Some(anchor) = self.l1_confirmed_anchor {
                                        let reorg_depth = self
                                            .l2_head_number
                                            .saturating_sub(anchor.l2_block_number);
                                        if reorg_depth_exceeded(reorg_depth, REORG_SAFETY_THRESHOLD)
                                        {
                                            error!(
                                                target: "based_rollup::driver",
                                                anchor_l2 = anchor.l2_block_number,
                                                anchor_l1 = anchor.l1_block_number,
                                                %refreshed_root,
                                                reorg_depth,
                                                threshold = REORG_SAFETY_THRESHOLD,
                                                l2_head = self.l2_head_number,
                                                real_entry_count,
                                                l1_block_number,
                                                "zero-consumption (trigger fully reverted) \
                                                 AND sibling-reorg depth beyond safety \
                                                 threshold — halting; operator intervention \
                                                 required. Clearing hold so one cycle \
                                                 doesn't silently wedge."
                                            );
                                            // Clear the hold so
                                            // `flush_precheck` re-enters on
                                            // subsequent ticks (the hold
                                            // would otherwise block forever);
                                            // do NOT queue a sibling reorg
                                            // that cannot complete.
                                            self.hold.clear();
                                        } else if self.pending_sibling_reorg.is_none() {
                                            warn!(
                                                target: "based_rollup::driver",
                                                anchor_l2 = anchor.l2_block_number,
                                                anchor_l1 = anchor.l1_block_number,
                                                %refreshed_root,
                                                reorg_depth,
                                                l2_head = self.l2_head_number,
                                                real_entry_count,
                                                l1_block_number,
                                                "zero-consumption at postBatch-confirm: trigger \
                                                 fully reverted on L1. Queuing sibling reorg \
                                                 for anchor block (depth-0 recovery; \
                                                 flush_precheck dispatch will switch to Sync \
                                                 mode on next tick). See \
                                                 project_testnet_stall_2026_04_17."
                                            );
                                            self.pending_sibling_reorg =
                                                Some(SiblingReorgRequest {
                                                    target_l2_block: anchor.l2_block_number,
                                                    expected_root: refreshed_root,
                                                });
                                            // Release the hold — the
                                            // divergence is being handled via
                                            // sibling reorg; the subsequent
                                            // dispatch will clear internal
                                            // state anyway.
                                            self.hold.clear();
                                            return Ok(());
                                        } else {
                                            // A sibling reorg is already in
                                            // flight — let it complete before
                                            // queuing another. Fall through
                                            // to the deferral-mechanism
                                            // backup below.
                                            warn!(
                                                target: "based_rollup::driver",
                                                anchor_l2 = anchor.l2_block_number,
                                                l1_block_number,
                                                real_entry_count,
                                                "zero-consumption detected while a sibling \
                                                 reorg is already pending — falling through \
                                                 to deferral backup"
                                            );
                                        }
                                    } else {
                                        // No anchor yet (cold start). Fall
                                        // through to the deferral backup.
                                        warn!(
                                            target: "based_rollup::driver",
                                            l1_block_number,
                                            real_entry_count,
                                            "zero-consumption detected without an \
                                             L1-confirmed anchor — falling through to \
                                             deferral backup"
                                        );
                                    }
                                }
                            }
                            // If `consumed_hashes` is empty AND there are no
                            // real entries (or the refreshed-root re-query
                            // failed, or a sibling reorg is already in
                            // flight), fall through — the deferral mechanism
                            // in `verify_local_block_matches_l1` handles it
                            // as backup, and Gap 2 (anchor-divergence
                            // detection in the flush mismatch path) catches
                            // the post-commit case on the next cycle.
                        }
                    }
                    Err(err) => {
                        let err_str = err.to_string();
                        if err_str.contains("reverted") {
                            // postBatch REVERTED on L1 — the batch data is invalid
                            // (wrong pre_state_root, invalid proof, or stale state).
                            // Re-queuing produces the same revert forever. Rewind to
                            // rebuild from scratch with fresh state.
                            error!(
                                target: "based_rollup::driver",
                                %err,
                                "postBatch reverted on L1 — rewinding to rebuild batch"
                            );
                            let (rewind_target, rollback_l1_block) =
                                if let Some(anchor) = self.l1_confirmed_anchor {
                                    (
                                        anchor.l2_block_number,
                                        anchor.l1_block_number.saturating_sub(1),
                                    )
                                } else {
                                    (0, self.config.deployment_l1_block)
                                };
                            self.rewind_to_re_derive(rewind_target, rollback_l1_block);
                        } else {
                            // Receipt timeout or RPC error — re-queue for retry.
                            // Uses the pre-submit clone: the FlushPlan already
                            // consumed the authoritative blocks + queue on the
                            // Ok path, so we restore from the clone we kept.
                            //
                            // Clear the entry-verify hold so the next step_builder
                            // cycle can re-enter flush_to_l1 (step_builder returns
                            // early while the hold is armed, so no retry fires
                            // otherwise). Reset the proposer's nonce cache because
                            // the submitted tx may have been evicted by a
                            // replace-by-fee from a different signer using the
                            // same key, leaving alloy's cached nonce stale
                            // relative to L1. See docs/issue-29 for the incident.
                            warn!(target: "based_rollup::driver", %err, "L1 receipt failed — will retry");
                            self.last_submission_failure = Some(std::time::Instant::now());
                            self.hold.clear();
                            // Use reset_nonce_unsolicited: send returned a tx_hash
                            // (no NonceResetRequired token), but we still need to
                            // drop alloy's cached nonce since the tx may have been
                            // evicted from the L1 mempool.
                            if let Some(p) = self.proposer.as_mut() {
                                let _ = p.reset_nonce_unsolicited();
                            }
                            for block in blocks_clone_for_receipt_failure.into_iter().rev() {
                                self.pending_submissions.push_front(block);
                            }
                            self.pending_l1 = pending_l1_clone_for_receipt_failure;
                        }
                        return Ok(());
                    }
                }
            }
            Err(err) => {
                // Submission itself failed. Same hazard as the receipt-timeout
                // branch above: leaving the hold armed halts step_builder
                // permanently since there is no on-chain block to verify.
                warn!(target: "based_rollup::driver", %err, "L1 submission failed — will retry");
                self.last_submission_failure = Some(std::time::Instant::now());
                self.hold.clear();
                // Use reset_nonce_unsolicited: the send returned a tx_hash
                // (or failed at submit), so we don't have a NonceResetRequired
                // token, but we still need to drop alloy's cached nonce since
                // the tx may have been evicted from the L1 mempool.
                if let Some(p) = self.proposer.as_mut() {
                    let _ = p.reset_nonce_unsolicited();
                }
                // Restore from the rollback package returned by
                // `SendResult::Failed` — the plan owned the blocks
                // and queue and gives them back to us here.
                if let Some(rollback) = rollback {
                    for block in rollback.blocks.into_iter().rev() {
                        self.pending_submissions.push_front(block);
                    }
                    self.pending_l1 = rollback.pending_l1;
                }
            }
        }

        Ok(())
    }

    /// Send L1 trigger transactions for pending L2→L1 calls.
    ///
    /// For each trigger group, sends one or more `executeL2TX(rollupId, rlpTx)`
    /// calls to consume the L1 deferred entries posted in the same batch.
    ///
    /// Uses EXPLICIT nonces (queried from L1) instead of alloy's auto-nonce.
    /// This prevents nonce desynchronization when a tx fails — alloy's
    /// `CachedNonceManager` increments its cache even on failure, creating
    /// a permanent nonce gap. With explicit nonces, failures don't corrupt
    /// the nonce sequence for subsequent postBatch calls.
    ///
    /// On any failure, resets the proposer's nonce cache before returning
    /// the error, so the caller's next `send_to_l1` starts fresh.
    /// Verify L2→L1 trigger receipts and classify the outcome.
    ///
    /// Called by `flush_to_l1` after postBatch confirms, waiting synchronously
    /// for each trigger receipt and producing a `TriggerExecutionResult` that
    /// the caller MUST consume (`#[must_use]`) — see invariant #15.
    ///
    /// This function does NOT mutate driver state (no rewind, no hold change).
    /// The caller decides what to do with the result, but the `#[must_use]`
    /// attribute + `-D warnings` makes it impossible to silently drop.
    pub(super) async fn verify_trigger_receipts(
        &self,
        trigger_tx_hashes: &[B256],
    ) -> TriggerExecutionResult {
        if trigger_tx_hashes.is_empty() {
            return TriggerExecutionResult::AllConfirmed { count: 0 };
        }
        let Some(proposer) = self.proposer.as_ref() else {
            // No proposer means we could not have sent triggers in the first
            // place — treat as vacuous confirmation.
            return TriggerExecutionResult::AllConfirmed { count: 0 };
        };
        let mut reverted_count = 0usize;
        for trigger_hash in trigger_tx_hashes {
            match proposer.wait_for_l1_receipt(*trigger_hash).await {
                Ok(_) => {
                    // Trigger landed successfully — receipt status=1
                }
                Err(err) => {
                    warn!(
                        target: "based_rollup::driver",
                        %err, %trigger_hash,
                        "L2→L1 trigger reverted on L1 — will rewind to strip entries"
                    );
                    reverted_count += 1;
                }
            }
        }
        if reverted_count == 0 {
            TriggerExecutionResult::AllConfirmed {
                count: trigger_tx_hashes.len(),
            }
        } else {
            TriggerExecutionResult::Reverted {
                reverted_count,
                total: trigger_tx_hashes.len(),
            }
        }
    }

    pub(super) async fn send_l2_to_l1_triggers(
        &mut self,
        triggers: &[TriggerMetadata],
    ) -> Result<Vec<B256>> {
        let proposer = self
            .proposer
            .as_ref()
            .ok_or_else(|| eyre::eyre!("proposer required for trigger txs"))?;

        // Collect all trigger tx hashes for post-receipt verification.
        let mut trigger_tx_hashes: Vec<B256> = Vec::new();

        // Query the current pending nonce BEFORE sending any trigger txs.
        // postBatch was just sent (nonce K), so pending nonce should be K+1.
        let mut nonce = proposer.get_l1_nonce().await?;
        info!(
            target: "based_rollup::driver",
            nonce,
            trigger_count = triggers.len(),
            "starting L1 trigger txs with explicit nonce"
        );

        /// Gas limit for executeL2TX trigger txs. Must be generous to accommodate
        /// nested scope navigation (delivery + bridge return trips in multi-call patterns).
        /// The simpler single-call trigger uses ~50k, but multi-call with nested
        /// delivery (receiveTokens + claimAndBridgeBack + bridge back) needs ~1.5M+.
        const TRIGGER_GAS: u64 = 3_000_000;

        for w in triggers {
            // Encode executeL2TX(uint256 rollupId, bytes calldata rlpEncodedTx)
            // using typed ABI encoding via sol! macro (NEVER hardcode selectors).
            let execute_l2tx_calldata = crate::cross_chain::IRollups::executeL2TXCall {
                rollupId: U256::from(self.config.rollup_id),
                rlpEncodedTx: w.rlp_encoded_tx.clone().into(),
            }
            .abi_encode();

            // Send trigger_count executeL2TX calls. Multi-call patterns with N root
            // L2→L1 calls need N invocations since each _findAndApplyExecution on L1
            // consumes one entry via swap-and-pop.
            for trigger_idx in 0..w.trigger_count {
                info!(
                    target: "based_rollup::driver",
                    "trigger action will be: executeL2TX(rollupId={}, rlpTx_len={}, trigger {}/{})",
                    self.config.rollup_id, w.rlp_encoded_tx.len(),
                    trigger_idx + 1, w.trigger_count
                );

                let proposer = self.proposer.as_ref().expect("checked above");
                match proposer
                    .send_l1_tx_with_nonce(
                        self.config.rollups_address,
                        Bytes::from(execute_l2tx_calldata.clone()),
                        U256::ZERO,
                        nonce,
                        TRIGGER_GAS,
                    )
                    .await
                {
                    Ok(hash) => {
                        info!(
                            target: "based_rollup::driver",
                            %hash, nonce, user = %w.user,
                            amount = %w.amount,
                            rlp_tx_len = w.rlp_encoded_tx.len(),
                            trigger = trigger_idx + 1,
                            total_triggers = w.trigger_count,
                            "sent executeL2TX trigger for L2→L1 call"
                        );
                        trigger_tx_hashes.push(hash);
                        nonce += 1;
                    }
                    Err(nonce_err) => {
                        let source_display = nonce_err.source.to_string();
                        warn!(
                            target: "based_rollup::driver",
                            err = %source_display, nonce, user = %w.user,
                            "executeL2TX trigger failed — resetting nonce and aborting"
                        );
                        // Discharge the `NonceResetRequired` token by
                        // handing it to `reset_nonce`. This is the
                        // compile-time enforcement for invariant #2:
                        // the token is `#[must_use]` and can only be
                        // consumed by this call. Clippy + `-D warnings`
                        // makes it impossible to drop it silently.
                        if let Some(p) = self.proposer.as_mut() {
                            let _ = p.reset_nonce(nonce_err.reset_required);
                        }
                        return Err(nonce_err.source);
                    }
                }
            }
        }

        // After all triggers sent successfully, reset nonce cache so the next
        // postBatch picks up the correct nonce from L1 (includes the trigger txs).
        // This is the "unsolicited" path (no failure token) — all trigger
        // sends succeeded, but alloy's `CachedNonceManager` still needs a
        // fresh connection so the next postBatch sees the post-trigger
        // nonces. See `Proposer::reset_nonce_unsolicited`.
        if let Some(p) = self.proposer.as_mut() {
            let _ = p.reset_nonce_unsolicited();
        }

        Ok(trigger_tx_hashes)
    }

    /// Forward raw L1 transactions queued by the L1 proxy via the RPC.
    ///
    /// Called after successful L1 submission so that `postBatch` lands
    /// before the user's L1 tx (correct ordering, no nonce contention).
    /// These are pre-signed user txs — forwarded via `eth_sendRawTransaction`,
    /// which does not require the builder's wallet.
    pub(super) async fn forward_queued_l1_txs(&mut self) -> Result<()> {
        let txs: Vec<Bytes> = {
            let mut queue = self
                .pending_l1_forward_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if queue.is_empty() {
                return Ok(());
            }
            queue.drain(..).collect()
        };

        // Respect the same submission cooldown — if L1 is unreachable, don't spam.
        if let Some(last_fail) = self.last_submission_failure {
            if last_fail.elapsed().as_secs() < SUBMISSION_COOLDOWN_SECS {
                // Re-queue for next cycle
                let mut queue = self
                    .pending_l1_forward_txs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                queue.extend(txs);
                return Ok(());
            }
        }

        let provider = self.get_l1_provider().clone();
        for raw_tx in &txs {
            match provider.send_raw_transaction(raw_tx).await {
                Ok(pending) => {
                    info!(
                        target: "based_rollup::driver",
                        tx_hash = %pending.tx_hash(),
                        "forwarded queued L1 tx"
                    );
                }
                Err(err) => {
                    // Don't re-queue — user's tx is likely invalid or already submitted.
                    // Don't set submission_failure either since this is a user tx, not ours.
                    warn!(
                        target: "based_rollup::driver",
                        %err,
                        "failed to forward L1 tx — dropping"
                    );
                }
            }
        }

        Ok(())
    }

    /// Peek at queued L1 user txs and compute a gas price that overbids them
    /// by the configured percentage, so the builder's postBatch tx is ordered
    /// first within the same L1 block.
    ///
    /// Returns `None` if there are no queued txs (postBatch uses default gas).
    pub(super) fn compute_gas_overbid(&self) -> Option<GasPriceHint> {
        use alloy_consensus::Transaction;
        use alloy_consensus::transaction::TxEnvelope;
        use alloy_rlp::Decodable;

        // Check both unified queue (new path) and legacy queue (backward compat).
        let mut max_fee: u128 = 0;
        let mut max_priority_fee: u128 = 0;
        let mut has_txs = false;

        // Check unified queue first — gas prices are already extracted.
        {
            let queue = self
                .queued_cross_chain_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for call in queue.iter() {
                has_txs = true;
                // Unified queue stores max_fee_per_gas as effective_gas_price.
                // Use it for both fee and priority fee (conservative overbid).
                max_fee = max_fee.max(call.effective_gas_price());
                max_priority_fee = max_priority_fee.max(call.effective_gas_price());
            }
        }

        // Also check legacy forward tx queue.
        {
            let queue = self
                .pending_l1_forward_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            for raw_tx in queue.iter() {
                if let Ok(envelope) = TxEnvelope::decode(&mut raw_tx.as_ref()) {
                    has_txs = true;
                    let (fee, priority) = match &envelope {
                        TxEnvelope::Legacy(signed) => {
                            let gp = signed.tx().gas_price;
                            (gp, gp)
                        }
                        TxEnvelope::Eip2930(signed) => {
                            let gp = signed.tx().gas_price;
                            (gp, gp)
                        }
                        TxEnvelope::Eip1559(signed) => {
                            let tx = signed.tx();
                            (tx.max_fee_per_gas, tx.max_priority_fee_per_gas)
                        }
                        TxEnvelope::Eip4844(signed) => {
                            let tx = signed.tx();
                            (
                                tx.max_fee_per_gas(),
                                tx.max_priority_fee_per_gas().unwrap_or(0),
                            )
                        }
                        TxEnvelope::Eip7702(signed) => {
                            let tx = signed.tx();
                            (tx.max_fee_per_gas, tx.max_priority_fee_per_gas)
                        }
                    };
                    max_fee = max_fee.max(fee);
                    max_priority_fee = max_priority_fee.max(priority);
                }
            }
        }

        if !has_txs {
            return None;
        }

        if max_fee == 0 {
            return None;
        }

        // Apply the configured overbid percentage (can be negative for testing).
        let pct = self.config.l1_gas_overbid_pct;
        let apply_pct = |value: u128| -> u128 {
            if pct >= 0 {
                let bump = value.saturating_mul(pct as u128) / 100;
                // Ensure at least +1 when overbid is positive and value > 0,
                // otherwise integer truncation makes tiny values (e.g. 1 * 10/100 = 0)
                // produce no overbid at all.
                let bump = if bump == 0 && value > 0 { 1 } else { bump };
                value.saturating_add(bump)
            } else {
                let reduction = value.saturating_mul(pct.unsigned_abs() as u128) / 100;
                value.saturating_sub(reduction)
            }
        };

        let hint = GasPriceHint {
            max_fee_per_gas: apply_pct(max_fee),
            max_priority_fee_per_gas: apply_pct(max_priority_fee),
        };

        info!(
            target: "based_rollup::driver",
            user_max_fee = max_fee,
            user_priority_fee = max_priority_fee,
            overbid_max_fee = hint.max_fee_per_gas,
            overbid_priority_fee = hint.max_priority_fee_per_gas,
            overbid_pct = pct,
            "computed gas overbid from queued L1 txs"
        );

        Some(hint)
    }
}
