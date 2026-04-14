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
    L1ConfirmedAnchor, MAX_BATCH_SIZE, SUBMISSION_COOLDOWN_SECS, TriggerExecutionResult,
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
                        for _ in 0..=pos {
                            self.pending_submissions.pop_front();
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
            // Previously, simple entries used batch_size=1, which sent the FIRST
            // pending block without entries but WITH entry state deltas computed
            // for the LAST block. This caused ExecutionNotFound when intermediate
            // blocks existed (the deferred entry's currentState didn't match the
            // on-chain stateRoot after the immediate entry for the wrong block).
            //
            // §4f nonce safety is preserved: entry protocol txs are only in the
            // LAST block, and earlier blocks don't depend on entry nonces.
            self.pending_submissions.len().min(MAX_BATCH_SIZE)
        } else {
            self.pending_submissions.len().min(MAX_BATCH_SIZE)
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
                        // First time hitting persistent mismatch — rewind to re-derive.
                        // §4f protocol tx filtering should produce the correct root.
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
                                (
                                    earliest_block.saturating_sub(1),
                                    self.config.deployment_l1_block,
                                )
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

                            if !consumed_hashes.is_empty() {
                                // Count how many entries we need per hash.
                                // Skip REVERT/REVERT_CONTINUE entries — they are consumed inside
                                // reverted scopes so their ExecutionConsumed events are reverted
                                // by ScopeReverted. We identify them by action_type (Revert) and
                                // by matching the REVERT_CONTINUE action hash.
                                let revert_continue_hash =
                                    crate::cross_chain::compute_revert_continue_hash(
                                        crate::cross_chain::RollupId::new(
                                            alloy_primitives::U256::from(self.config.rollup_id),
                                        ),
                                    );

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
                                        total = l1_entries.iter().filter(|e| e.action_hash != crate::cross_chain::ActionHash::ZERO).count(),
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
                            }
                            // If consumed_hashes is empty (query failed or no events),
                            // fall through — the deferral mechanism in
                            // verify_local_block_matches_l1 handles it as backup.
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
                            warn!(target: "based_rollup::driver", %err, "L1 receipt failed — will retry");
                            self.last_submission_failure = Some(std::time::Instant::now());
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
                warn!(target: "based_rollup::driver", %err, "L1 submission failed — will retry");
                self.last_submission_failure = Some(std::time::Instant::now());
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
