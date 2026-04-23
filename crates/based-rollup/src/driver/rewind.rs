//! Driver rewind paths.
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1g. This module
//! owns the four methods that together implement the driver's
//! rewind-and-re-derive machinery:
//!
//! - [`Driver::clear_internal_state`] — drop all pending state
//!   (submissions, entries, hold, forward-tx queue).
//! - [`Driver::set_rewind_target`] — record the earliest rewind target
//!   in a batch; idempotent when called multiple times.
//! - [`Driver::rewind_to_re_derive`] — the canonical 8-step **hard**
//!   rewind sequence used by every mismatch path (invariants #9/#10).
//! - [`Driver::rewind_l2_chain`] — the async FCU-based unwind of the
//!   L2 chain tip in reth after derivation detects a mismatch.
//! - [`Driver::rebuild_block_as_sibling`] — issue #36 sibling-reorg
//!   primitive (newPayloadV3 + forkchoiceUpdatedV3 on a sibling hash).
//! - [`Driver::apply_sibling_reorg_plan`] — issue #36 state-transition
//!   helper invoked by the verify fast-path.
//!
//! See the `rewind_to_re_derive` doc comment for the full invariant
//! #9/#10 rationale.

use super::Driver;
use super::types::{
    BuiltBlock, DriverMode, DriverRecoveryFields, FORK_CHOICE_DEPTH, SiblingReorgVerifyPlan,
    apply_sibling_reorg_plan_fields, check_sibling_state_root_matches, clear_recovery_state,
    compute_forkchoice_state, submit_sibling_payload,
};
use alloy_primitives::{B256, Bytes};
use eyre::{Result, WrapErr};
use reth_provider::{
    BlockHashReader, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use std::collections::VecDeque;
use tracing::{debug, info, warn};

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
    /// Drop all pending state before a rewind — submissions, L1 entry queue,
    /// hold, and the queued forward-tx buffer. Called from every rewind path
    /// so the rebuilt state is fresh after the re-derive.
    ///
    /// ## M2 (issue #36 second-pass review) — also clears `pending_sibling_reorg`
    ///
    /// `pending_sibling_reorg` is recovery state that targets a specific
    /// `(target_l2_block, expected_root)` derived from a particular L1 view. If
    /// a caller invokes `clear_internal_state` (e.g. an L1 reorg detected
    /// upstream, or a generic "wipe pending state and restart derivation"
    /// action), the expected_root may be obsolete — committing a sibling
    /// against it would overwrite the canonical block with the wrong root and
    /// cause silent drift.
    ///
    /// Call sites that legitimately need the request to survive (the two
    /// sibling-reorg dispatch paths in `flush_to_l1` and
    /// `verify_local_block_matches_l1`) explicitly save + reinstate the
    /// request around this call.
    ///
    /// The `pending_sibling_reorg` + `hold` clearing is centralized in
    /// [`clear_recovery_state`] so tests exercise the same production helper
    /// production uses. Removing or skipping any one field in the helper
    /// breaks `test_clear_recovery_state_wipes_all_fields`. Removing the CALL
    /// itself is caught by
    /// `test_clear_internal_state_via_real_driver_clears_pending_sibling_reorg`.
    pub(super) fn clear_internal_state(&mut self) {
        self.preconfirmed_hashes.clear();
        self.pending_submissions.clear();
        self.arb_trace_by_l2_block.clear();
        self.pending_l1.clear();
        clear_recovery_state(&mut self.pending_sibling_reorg, &mut self.hold);
        {
            let mut fwd = self
                .pending_l1_forward_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            fwd.clear();
        }
    }

    /// Set the pending rewind target to the EARLIEST (minimum) mismatch point.
    ///
    /// When multiple blocks in the same derivation batch have L1 context mismatches
    /// (e.g. a run of gap-fill blocks followed by a submitted block), we must rewind
    /// to the earliest one so all are re-derived with the correct context.
    pub(super) fn set_rewind_target(&mut self, target: u64) {
        self.pending_rewind_target =
            Some(self.pending_rewind_target.map_or(target, |t| t.min(target)));
    }

    /// Execute the canonical "hard rewind" sequence used by every mismatch path
    /// (flush pre-state mismatch, trigger revert, partial consumption, deferral
    /// exhaustion, postBatch revert, generic verification mismatch).
    ///
    /// The sequence is:
    /// 1. Clear all internal pending state (submissions, entries, hold, etc.)
    /// 2. Reset the derivation cursor's last derived L2 block to `target_l2_block`
    /// 3. Roll the L1 derivation scan back to `rollback_l1_block`
    /// 4. Switch to `DriverMode::Sync` so derivation catches up before building
    /// 5. Mark the node as not synced (clears WS preconfirmations etc.)
    /// 6. Increment `consecutive_rewind_cycles` for backoff dampening
    /// 7. Record the rewind target (takes min with any prior target in the batch)
    ///
    /// **Invariant #10**: when the caller rewinds because an entry block failed
    /// to verify, the target MUST be `entry_block.saturating_sub(1)` so the block
    /// containing the entry is itself re-derived, not skipped. Callers compute
    /// the target; this helper does not second-guess it.
    ///
    /// This is the *hard* rewind variant. The L1-context mismatch path in
    /// `verify_local_block_matches_l1` uses a lighter sequence (no mode switch,
    /// no rewind-cycle increment) and does not call this helper.
    pub(super) fn rewind_to_re_derive(&mut self, target_l2_block: u64, rollback_l1_block: u64) {
        self.clear_internal_state();
        // Order matters. `rollback_to` sets `last_derived_l2_block` as a
        // side-effect based on the cursor contents (`last_valid_l2.unwrap_or(0)`).
        // When the cursor has been evicted below `rollback_l1_block` (size
        // cap at derivation.rs:801), it resets the derivation head to 0 —
        // which, combined with an L2 head far ahead of 0, wedges derivation
        // via `MAX_BLOCK_GAP`. Call `rollback_to` FIRST so its side-effect
        // runs, THEN authoritatively overwrite with the intended target.
        // Regression: `test_rewind_sequence_leaves_derivation_head_at_target_when_cursor_empty`.
        // Root-cause incident: testnet-eez 2026-04-16, 32 min of rewind
        // cycles followed by a permanent `expected next block 1` wedge.
        self.derivation.rollback_to(rollback_l1_block);
        self.derivation.set_last_derived_l2_block(target_l2_block);
        self.mode = DriverMode::Sync;
        self.synced
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.consecutive_rewind_cycles = self.consecutive_rewind_cycles.saturating_add(1);
        self.set_rewind_target(target_l2_block);
    }

    /// Async FCU-based unwind of the L2 chain tip in reth.
    ///
    /// Called after derivation detects a reorg. Rebuilds the block-hashes
    /// deque from DB, issues a fork-choice update with the target as the
    /// new head, and — if reth refuses to unwind committed blocks — accepts
    /// reth's canonical tip and marks the surviving blocks as immutable so
    /// they never trigger a further rewind attempt.
    pub(super) async fn rewind_l2_chain(&mut self, target_l2_block: u64) -> Result<()> {
        info!(
            target: "based_rollup::driver",
            current_head = self.l2_head_number,
            target = target_l2_block,
            "rewinding L2 chain after L1 reorg"
        );

        let target_hash = if target_l2_block == 0 {
            // Genesis hash — stored at init
            self.l2_provider
                .block_hash(0)
                .wrap_err("failed to read genesis block hash")?
                .ok_or_else(|| eyre::eyre!("genesis block has no hash in DB"))?
        } else {
            self.l2_provider
                .block_hash(target_l2_block)
                .wrap_err("failed to read target block hash for rewind")?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "target block {target_l2_block} has no hash in DB — possible DB corruption"
                    )
                })?
        };

        // Rebuild block_hashes deque from DB (same pattern as recover_chain_state)
        let mut new_hashes = VecDeque::new();
        let start = target_l2_block.saturating_sub(FORK_CHOICE_DEPTH as u64);
        for n in start..=target_l2_block {
            if let Ok(Some(hash)) = self.l2_provider.block_hash(n) {
                new_hashes.push_back(hash);
            }
        }

        let fcs = compute_forkchoice_state(target_hash, &new_hashes);

        let fcu = self.fork_choice_updated_with_retry(fcs, None).await?;

        if fcu.is_invalid() {
            eyre::bail!(
                "rewind fork choice update rejected: {:?}",
                fcu.payload_status
            );
        }

        // Verify the rewind actually took effect. Reth's FCU with a backward
        // head_block_hash may return VALID without unwinding committed canonical
        // blocks. If the canonical tip is still ahead of our target, the FCU only
        // moved the fork-choice pointer without removing blocks. In that case,
        // accept reth's actual canonical state to avoid a permanent desync where
        // the driver thinks it's at `target` while reth is still at the old tip.
        let actual_tip = self
            .l2_provider
            .last_block_number()
            .wrap_err("failed to read actual tip after rewind")?;

        if actual_tip > target_l2_block {
            warn!(
                target: "based_rollup::driver",
                requested = target_l2_block,
                actual_tip,
                "FCU rewind did not unwind committed blocks — accepting reth canonical tip"
            );
            // Mark all blocks up to the actual tip as immutable — they can't be
            // unwound via FCU and must not trigger further rewind attempts.
            self.immutable_block_ceiling = actual_tip;
            // Re-read actual chain state from reth so the driver stays in sync
            // with the execution engine. Derivation will re-verify these blocks
            // against L1 and detect any genuine mismatches.
            self.recover_chain_state()?;
        } else {
            self.block_hashes = new_hashes;
            self.head_hash = target_hash;
            self.l2_head_number = target_l2_block;
        }

        info!(
            target: "based_rollup::driver",
            requested_target = target_l2_block,
            actual_head = self.l2_head_number,
            head_hash = %self.head_hash,
            "L2 chain rewind completed"
        );

        Ok(())
    }

    /// Apply the sibling-reorg recovery state transition computed by
    /// [`super::types::plan_sibling_reorg_from_verify`] (issue #36).
    ///
    /// Centralizes the state mutation so:
    /// (a) the `verify_local_block_matches_l1` fast path and any future caller
    ///     can't accidentally omit one of the fields (C1 regression), and
    /// (b) tests can construct a plan independently and assert the driver's
    ///     post-state against it without spinning up a full engine.
    ///
    /// Fields mutated:
    /// - `pending_sibling_reorg` ← the planned request (survives the
    ///   `clear_internal_state` wipe via save+reinstate; see M2).
    /// - `pending_rewind_target` ← `plan.rewind_target_l2` via the
    ///   `set_rewind_target` min-op (C1: required so `step_builder`
    ///   early-returns and skips `commit_batch`).
    /// - `mode` ← `Sync`.
    /// - `hold` ← cleared (entry-verification hold released).
    /// - Derivation pipeline: `set_last_derived_l2_block` +
    ///   `rollback_to` per the plan.
    ///
    /// Fields INTENTIONALLY NOT mutated:
    /// - `consecutive_rewind_cycles` — sibling reorg is a productive recovery,
    ///   not a rewind cycle. The safety gate counts unresolved recovery
    ///   attempts; a successful first-time queue should not advance it.
    pub(crate) fn apply_sibling_reorg_plan(&mut self, plan: SiblingReorgVerifyPlan) {
        // Save the planned request across `clear_internal_state` (which — per
        // the M2 fix — deliberately wipes `pending_sibling_reorg`).
        let saved_req = plan.request;
        self.clear_internal_state();
        // The remaining state mutations are factored into
        // `apply_sibling_reorg_plan_fields` so a test can assert all field
        // mutations on a `DriverRecoveryFields` instance without instantiating
        // a full driver. C1 regression: `pending_rewind_target` MUST be set.
        let mut fields = DriverRecoveryFields {
            pending_sibling_reorg: self.pending_sibling_reorg,
            pending_rewind_target: self.pending_rewind_target,
            hold: self.hold,
            mode: self.mode,
        };
        apply_sibling_reorg_plan_fields(&mut fields, saved_req, plan, &mut self.derivation);
        self.pending_sibling_reorg = fields.pending_sibling_reorg;
        self.pending_rewind_target = fields.pending_rewind_target;
        self.hold = fields.hold;
        self.mode = fields.mode;
        self.synced
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Rebuild block `target` as a sibling of the existing canonical block and
    /// swap it in via reth's first-class `newPayloadV3 + forkchoiceUpdatedV3`
    /// reorg path (issue #36).
    ///
    /// This exists because `forkchoiceUpdatedV3(head=ancestor)` on plain
    /// Ethereum engine kind is a silent no-op per the Engine API spec — reth
    /// refuses to unwind committed canonical blocks. The only way to replace
    /// a committed block is to present a sibling at the same height with a
    /// different hash and then issue FCU pointing at the sibling.
    ///
    /// Reference: reth's own `test_testsuite_deep_reorg` at
    /// `crates/e2e-test-utils/tests/e2e-testsuite/main.rs`. The same pattern
    /// is used by op-node (`consolidateNextSafeAttributes`) and Taiko.
    ///
    /// Semantics:
    /// - Parent is `target - 1` (must exist in reth).
    /// - `derived_transactions` is the exact tx set that the rebuilt block
    ///   must contain (already §4f-filtered by the caller).
    /// - `expected_root` is the state root the rebuilt block MUST produce
    ///   (C2 guard). If `apply_deferred_filtering` has any defect we bail
    ///   BEFORE any engine call is made.
    /// - On success the driver's `head_hash`, `l2_head_number`, and
    ///   `block_hashes` deque are updated to reflect reth's new canonical tip.
    /// - On success, a `PreconfirmedMessage::BlockInvalidated` broadcast is
    ///   emitted via `sibling_reorg_broadcast_tx` (when wired) so subscribed
    ///   fullnodes can evict any cached hash for `target`.
    ///
    /// Failure modes:
    /// - C2 guard fails (`built.state_root != expected_root`) → bail with a
    ///   structured error BEFORE the engine is touched.
    /// - `newPayload` returns INVALID → bail with a structured error.
    /// - FCU returns INVALID → bail; driver state is untouched.
    /// - FCU returns SYNCING → `submit_fork_choice_with_retry` handles it.
    pub(crate) async fn rebuild_block_as_sibling(
        &mut self,
        target: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        derived_transactions: &Bytes,
        expected_root: B256,
    ) -> Result<BuiltBlock> {
        if target == 0 {
            eyre::bail!("cannot rebuild genesis block (target=0) as sibling");
        }
        let parent_block_number = target - 1;

        let old_hash = self.head_hash;
        let old_head = self.l2_head_number;

        let (built, execution_data) = self
            .build_derived_block(
                parent_block_number,
                timestamp,
                l1_block_hash,
                l1_block_number,
                derived_transactions,
            )
            .wrap_err_with(|| {
                format!(
                    "sibling rebuild: build_derived_block failed for L2 block {target} \
                     (parent={parent_block_number})"
                )
            })?;

        // C2 guard (issue #36 second-pass review): assert the rebuilt block's
        // state root equals the `expected_root` we promised L1. If filtering
        // has any defect, committing the sibling anyway silently drifts from
        // L1 canon — and the next flush_to_l1 cycle queues ANOTHER reorg,
        // repeating indefinitely.
        //
        // Per CLAUDE.md cardinal rule: "If roots don't match, there is a real
        // bug in derivation or filtering. The builder must keep rewinding
        // until the root cause is fixed." So we bail loud here instead of
        // submitting the wrong root. No engine call is made, no driver state
        // is mutated. The caller returns Err upward and the sibling-reorg
        // request stays in place for the next retry.
        check_sibling_state_root_matches(built.state_root, expected_root, target)?;

        let sibling_hash = built.hash;

        if sibling_hash == old_hash && target == old_head {
            // Bit-identical payload already canonical — nothing to do.
            debug!(
                target: "based_rollup::driver",
                target,
                %sibling_hash,
                "sibling rebuild produced the same hash as current head — no reorg needed"
            );
            return Ok(built);
        }

        info!(
            target: "based_rollup::driver",
            target,
            parent = parent_block_number,
            %old_hash,
            %sibling_hash,
            tx_count = built.tx_count,
            "submitting sibling payload to engine (reorg via newPayload+FCU)"
        );

        // Pre-populate the hash deque with hashes up to and including
        // `target - 1`. `submit_sibling_payload` appends the sibling hash and
        // caps the deque depth, so it returns the final deque we should adopt
        // on success.
        let mut parent_hashes: VecDeque<B256> = VecDeque::new();
        let start = parent_block_number.saturating_sub(FORK_CHOICE_DEPTH as u64);
        for n in start..=parent_block_number {
            if let Ok(Some(h)) = self.l2_provider.block_hash(n) {
                parent_hashes.push_back(h);
            }
        }

        let outcome =
            submit_sibling_payload(&self.engine, execution_data, sibling_hash, &parent_hashes)
                .await?;

        // Only after reth confirms the reorg do we mutate driver state.
        self.block_hashes = outcome.new_hashes;
        self.head_hash = sibling_hash;
        self.l2_head_number = target;

        info!(
            target: "based_rollup::driver",
            target,
            old_head,
            %old_hash,
            new_hash = %sibling_hash,
            "sibling reorg completed — reth swapped canonical head"
        );

        self.broadcast_sibling_reorg(target, sibling_hash);

        Ok(built)
    }
}
