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
//!
//! See the `rewind_to_re_derive` doc comment for the full invariant
//! #9/#10 rationale.

use super::Driver;
use super::types::{DriverMode, FORK_CHOICE_DEPTH, compute_forkchoice_state};
use eyre::{Result, WrapErr};
use reth_provider::{
    BlockHashReader, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use std::collections::VecDeque;
use tracing::{info, warn};

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
    pub(super) fn clear_internal_state(&mut self) {
        self.preconfirmed_hashes.clear();
        self.pending_submissions.clear();
        self.pending_l1.clear();
        self.hold.clear();
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
        self.derivation.set_last_derived_l2_block(target_l2_block);
        self.derivation.rollback_to(rollback_l1_block);
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
}
