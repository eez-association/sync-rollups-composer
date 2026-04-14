//! Builder-mode orchestration: block building + L1 submission loop.
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1l. This module
//! owns the main builder loop and its direct helpers:
//!
//! - [`Driver::step_builder`] — the builder's per-step entry point:
//!   derive from L1, build blocks up to the target, queue for L1
//!   submission, then flush.
//! - [`Driver::recover_builder_l2_nonce`] — read the builder's L2
//!   nonce from chain state after a Sync→Builder transition.
//! - [`Driver::collect_reverted_user_transactions`] — salvage user
//!   txs from blocks about to be unwound so they can be re-injected.
//! - [`Driver::inject_held_l2_txs`] — push held L2 txs (from the
//!   hold-then-forward composer RPC pattern) into the mempool just
//!   before block building.
//! - [`Driver::reinject_pending_transactions`] — deferred re-injection
//!   of transactions saved during a previous rewind.

use super::Driver;
use super::TriggerMetadata;
use super::types::{DriverMode, MAX_PENDING_CROSS_CHAIN_ENTRIES, MAX_PENDING_SUBMISSIONS};
use crate::cross_chain::CrossChainExecutionEntry;
use crate::proposer::PendingBlock;
use alloy_primitives::{B256, Bytes};
use alloy_provider::Provider;
use eyre::Result;
use reth_primitives_traits::{Recovered, SignerRecoverable};
use reth_provider::{
    BlockHashReader, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use tracing::{debug, error, info, warn};

/// Maximum number of blocks to build in one tick during catch-up.
const MAX_CATCHUP_BLOCKS: u64 = 10_000;

/// Pre-computed context for a single builder tick, determined before
/// queue drains and the block building loop.
pub(super) struct BuilderTickContext {
    /// Effective target L2 block number for this tick (catch-up capped).
    pub effective_target: u64,
    /// Current L1 block number providing context for this tick.
    pub current_l1_block: u64,
    /// Hash of `current_l1_block`.
    pub l1_hash: B256,
}

/// Result of draining the cross-chain call queues before the block
/// building loop.
pub(super) struct QueueDrainResult {
    /// Combined L2 execution entries (L1-fetched at front, RPC at back).
    pub builder_execution_entries: Vec<CrossChainExecutionEntry>,
    /// Number of RPC entries in `builder_execution_entries` (counted from back).
    pub rpc_entry_count: usize,
    /// L1 transactions to forward after successful postBatch.
    pub queued_l1_txs: Vec<Bytes>,
    /// State needed to undo the drain if block building fails.
    pub rollback: QueueDrainRollback,
}

/// Snapshot for undoing a queue drain on build failure (issue #237).
///
/// If block building or protocol tx construction fails, the drained
/// entries must be re-pushed to the shared queues before
/// `clear_internal_state()` runs during the Sync transition.
pub(super) struct QueueDrainRollback {
    pre_drain_l1_len: usize,
    pre_drain_l1_groups: usize,
    calls_for_repush: Vec<crate::rpc::QueuedCrossChainCall>,
    l2_to_l1_for_repush: Vec<crate::rpc::QueuedL2ToL1Call>,
}

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
    /// Builder mode: build blocks from the mempool and submit to L1.
    ///
    /// The builder:
    /// 1. Catches up with L1 (derive, verify, commit)
    /// 2. Computes tick context (target, L1 hash, hold guard)
    /// 3. Drains cross-chain queues
    /// 4. Builds new blocks up to the target
    /// 5. Submits pending blocks to L1 in batches
    pub(super) async fn step_builder(&mut self, latest_l1_block: u64) -> Result<()> {
        // Phase 1: Catch up with L1 — derive, verify, commit.
        self.derive_and_verify_from_l1(latest_l1_block).await?;
        if self.pending_rewind_target.is_some() {
            return Ok(());
        }

        // Phase 2: Compute tick context (target, L1 hash, hold check).
        let tick = match self.compute_tick_context(latest_l1_block).await? {
            Some(ctx) => ctx,
            None => return Ok(()),
        };
        let mut current_l1_block = tick.current_l1_block;
        let mut l1_hash = tick.l1_hash;
        let provider = self.get_l1_provider().clone();

        // Phase 3: Drain cross-chain queues, fetch L1 entries, inject held L2 txs.
        let drain = self.drain_rpc_queues(current_l1_block).await?;
        let mut builder_execution_entries = drain.builder_execution_entries;
        let mut rpc_entry_count_in_builder = drain.rpc_entry_count;
        let queued_l1_txs_for_block = drain.queued_l1_txs;
        let drain_rollback = drain.rollback;

        // During catch-up, refresh L1 context every N blocks to avoid all catch-up
        // blocks sharing the same L1 context (which causes mass rewind if the batch
        // submission lands in a different L1 block).
        const L1_REFRESH_INTERVAL: u64 = 100;
        let mut blocks_since_l1_refresh: u64 = 0;

        while self.l2_head_number < tick.effective_target {
            // Periodically refresh L1 context during catch-up to reduce blast radius
            // of L1 context mismatches (each batch of ~100 blocks gets fresh context).
            blocks_since_l1_refresh = blocks_since_l1_refresh.saturating_add(1);
            if blocks_since_l1_refresh > L1_REFRESH_INTERVAL {
                if let Ok(new_l1_block) = provider.get_block_number().await {
                    if new_l1_block > current_l1_block {
                        if let Ok(Some(block)) =
                            provider.get_block_by_number(new_l1_block.into()).await
                        {
                            current_l1_block = new_l1_block;
                            l1_hash = block.header.hash;
                            // Fetch any new execution entries in the extended range
                            match self
                                .derivation
                                .fetch_execution_entries_for_builder(current_l1_block, &provider)
                                .await
                            {
                                Ok(new_entries) => {
                                    builder_execution_entries = new_entries;
                                    // New entries are all L1-fetched — RPC entries
                                    // from the pre-loop merge are gone.
                                    rpc_entry_count_in_builder = 0;
                                }
                                Err(err) => {
                                    warn!(
                                        target: "based_rollup::driver",
                                        %err,
                                        l1_block = current_l1_block,
                                        "failed to fetch execution entries during L1 refresh — \
                                         entries from this range may be delayed"
                                    );
                                }
                            }
                            blocks_since_l1_refresh = 0;
                        }
                    }
                }
            }

            let next_l2_block = self.l2_head_number.saturating_add(1);
            let next_timestamp = self
                .config
                .l2_timestamp_checked(next_l2_block)
                .ok_or_else(|| eyre::eyre!("timestamp overflow for L2 block {next_l2_block}"))?;

            // Assign pending deposits/entries to the last block before an L1 context
            // change or the final block in the catch-up batch. This matches derivation
            // semantics: all blocks sharing the same L1 context have the same
            // deposit_cutoff, and the first *submitted* block claims the deposits.
            // By assigning to the last block, we avoid submitting an otherwise-empty
            // first block just because it carries deposits.
            let is_last_block = next_l2_block >= tick.effective_target;
            let is_last_before_refresh =
                blocks_since_l1_refresh.saturating_add(1) > L1_REFRESH_INTERVAL;
            let assign_entries = is_last_block || is_last_before_refresh;

            let execution_entries = if assign_entries {
                std::mem::take(&mut builder_execution_entries)
            } else {
                vec![]
            };
            let had_execution_entries = !execution_entries.is_empty();

            // Separate L1-fetched (base) entries from RPC (chained) entries.
            // L1-fetched entries are at the front, RPC entries at the back.
            // Base entries came FROM L1 — they don't need chained deltas.
            // RPC entries are speculative — they need chained deltas for L1 submission.
            let block_rpc_count = if had_execution_entries {
                rpc_entry_count_in_builder.min(execution_entries.len())
            } else {
                0
            };
            let base_count = execution_entries.len() - block_rpc_count;
            let rpc_entries_for_block = execution_entries[base_count..].to_vec();
            if assign_entries {
                rpc_entry_count_in_builder = 0;
            }

            // Construct builder-signed protocol transactions
            let derived_transactions = match self.build_builder_protocol_txs(
                next_l2_block,
                next_timestamp,
                l1_hash,
                current_l1_block,
                &execution_entries,
                usize::MAX, // builder mode: generate all triggers
            ) {
                Ok(txs) => txs,
                Err(err) => {
                    warn!(
                        target: "based_rollup::driver",
                        %err, l2_block = next_l2_block,
                        "failed to construct builder protocol txs — switching to sync mode"
                    );
                    self.rollback_queue_drain(drain_rollback);
                    self.mode = DriverMode::Sync;
                    self.synced
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
            };

            let built = match self
                .build_and_insert_block(
                    next_l2_block,
                    next_timestamp,
                    l1_hash,
                    current_l1_block,
                    &derived_transactions,
                )
                .await
            {
                Ok(b) => b,
                Err(err) => {
                    warn!(
                        target: "based_rollup::driver",
                        err = format!("{err:#}"),
                        l2_block = next_l2_block,
                        nonce = self.builder_l2_nonce,
                        head = self.l2_head_number,
                        head_hash = %self.head_hash,
                        "block building failed — switching to sync mode for recovery"
                    );
                    self.rollback_queue_drain(drain_rollback);
                    self.mode = DriverMode::Sync;
                    self.synced
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
            };

            // A block is "non-empty" if it has user transactions or
            // cross-chain execution entries. Execution entries modify L2 state
            // (loadExecutionTable writes to CrossChainManagerL2), so the block
            // must be submitted to L1 for fullnodes to assign the same entries
            // to the same L2 block number. Without this, the builder could
            // assign entries to block N (is_last_before_refresh) while fullnodes
            // assign them to a later submitted block, causing state root divergence.
            let has_content = built.tx_count > 0 || had_execution_entries;

            info!(
                target: "based_rollup::driver",
                l2_block = next_l2_block,
                block_hash = %built.hash,
                txs = built.tx_count,
                has_content,
                "built and inserted builder block"
            );

            // Journal user transactions for crash recovery. Written BEFORE
            // flush_to_l1 can trigger a rewind, so the journal always has the
            // data even if clear_pending_state destroys pending_submissions.
            self.journal_block_transactions(next_l2_block, &derived_transactions);

            // Cross-chain entries for L1 submission come from external sources
            // (L1 proxy, RPC) and are added to pending_l1_entries via the shared
            // queue. We do NOT generate per-block entries here because the aggregate
            // block entry in flush_to_l1 already handles state root progression.
            // Per-block entries would conflict: Rollups.sol processes entries
            // sequentially, so after the aggregate entry updates the on-chain root,
            // per-block entries' currentState would mismatch.
            if self.pending_l1.len_entries() > MAX_PENDING_CROSS_CHAIN_ENTRIES {
                warn!(target: "based_rollup::driver",
                    count = self.pending_l1.len_entries(),
                    max = MAX_PENDING_CROSS_CHAIN_ENTRIES,
                    "pending cross-chain entries exceeded cap, dropping oldest"
                );
                self.pending_l1
                    .trim_entries_from_front(MAX_PENDING_CROSS_CHAIN_ENTRIES);
            }

            // Compute intermediate state roots and attach entry deltas.
            let (clean_state_root, intermediate_roots) = self.finalize_block_entries(
                next_l2_block,
                next_timestamp,
                l1_hash,
                current_l1_block,
                &built,
                &rpc_entries_for_block,
            );

            // Queue ALL blocks for L1 submission (including empty ones).
            // The aggregate state root entry spans the entire batch so empty
            // blocks add only callData cost (block number + empty tx bytes).
            // Submitting all blocks avoids gap-fill complexity and ensures
            // deterministic L1 context across builder/fullnodes.
            if self.pending_submissions.len() < MAX_PENDING_SUBMISSIONS {
                self.pending_submissions.push_back(PendingBlock {
                    l2_block_number: next_l2_block,
                    pre_state_root: built.pre_state_root,
                    state_root: built.state_root,
                    clean_state_root,
                    encoded_transactions: built.encoded_transactions,
                    intermediate_roots,
                });
            } else {
                warn!(
                    target: "based_rollup::driver",
                    l2_block = next_l2_block,
                    queue_size = self.pending_submissions.len(),
                    "pending submissions queue full, block will be backfilled later"
                );
            }

            // Note: the entry-hold mechanism is inside flush_to_l1 itself.
            // When flush_to_l1 submits a batch with cross-chain entries, it sets
            // pending_entry_verification_block to hold further submissions until
            // derivation confirms the entry block. See flush_to_l1 for details.
        }

        // Commit L1 forward txs to the legacy queue AFTER all blocks built
        // successfully. This ensures they are not orphaned if building fails.
        // See issue #237.
        if !queued_l1_txs_for_block.is_empty() {
            let mut fwd_queue = self
                .pending_l1_forward_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            fwd_queue.extend(queued_l1_txs_for_block);
        }

        // Submit pending blocks and cross-chain entries to L1
        self.flush_to_l1().await?;

        Ok(())
    }

    /// Compute intermediate state roots, attach entry deltas to pending L1
    /// entries, and return the clean state root for the pending submission.
    ///
    /// This is the single site where `CleanStateRoot::new` is called for builder
    /// blocks — invariant #3 (never fabricate pre_state_root) is enforced by
    /// the `pub(crate)` constructor.
    fn finalize_block_entries(
        &mut self,
        l2_block: u64,
        timestamp: u64,
        l1_hash: B256,
        l1_block: u64,
        built: &super::types::BuiltBlock,
        rpc_entries: &[CrossChainExecutionEntry],
    ) -> (crate::cross_chain::CleanStateRoot, Vec<B256>) {
        // Count true triggers (not continuation table entries).
        let our_rollup_id =
            crate::cross_chain::RollupId::new(alloy_primitives::U256::from(self.config.rollup_id));
        let num_protocol_triggers = rpc_entries
            .iter()
            .filter(|e| {
                let is_call_to_us = e.next_action.action_type
                    == crate::cross_chain::CrossChainActionType::Call
                    && e.next_action.rollup_id == our_rollup_id;
                if !is_call_to_us {
                    return false;
                }
                let next_hash = crate::table_builder::compute_action_hash(&e.next_action);
                next_hash == e.action_hash
            })
            .count();
        let num_user_triggers = self
            .pending_l1
            .groups
            .iter()
            .filter(|g| g.trigger.is_some())
            .count();
        let has_entries = !rpc_entries.is_empty() || num_user_triggers > 0;

        // Compute intermediate roots and derive clean state root.
        let mut intermediate_roots = Vec::new();
        let clean_state_root = if has_entries {
            match self.compute_intermediate_roots(
                l2_block.saturating_sub(1),
                timestamp,
                l1_hash,
                l1_block,
                built.state_root,
                &built.encoded_transactions,
            ) {
                Ok(roots) => {
                    let clean = roots[0];
                    info!(
                        target: "based_rollup::driver",
                        l2_block,
                        speculative = %built.state_root,
                        clean = %clean,
                        num_protocol_triggers,
                        num_user_triggers,
                        "computed unified intermediate state roots"
                    );
                    intermediate_roots = roots;
                    crate::cross_chain::CleanStateRoot::new(clean)
                }
                Err(err) => {
                    error!(
                        target: "based_rollup::driver",
                        l2_block,
                        %err,
                        "failed to compute intermediate state roots — \
                         discarding cross-chain entries for this block"
                    );
                    self.pending_l1.clear();
                    crate::cross_chain::CleanStateRoot::new(built.state_root)
                }
            }
        } else {
            crate::cross_chain::CleanStateRoot::new(built.state_root)
        };

        // Attach state deltas to pending L1 entries.
        if !self.pending_l1.is_empty() && !intermediate_roots.is_empty() {
            let group_starts: Vec<usize> = self.pending_l1.groups.iter().map(|g| g.start).collect();
            crate::cross_chain::attach_generic_state_deltas(
                &mut self.pending_l1.entries,
                &intermediate_roots,
                self.config.rollup_id,
                &group_starts,
            );
            info!(
                target: "based_rollup::driver",
                unified_entry_count = self.pending_l1.len_entries(),
                groups = self.pending_l1.num_groups(),
                roots = intermediate_roots.len(),
                entry_mix = ?self.pending_l1.entry_mix(),
                "attached generic state deltas to unified L1 entries"
            );

            // Override state deltas for independent groups (L1→L2 partial revert).
            let num_groups = self.pending_l1.num_groups();
            for k in 0..num_groups {
                if self.pending_l1.groups[k].mode.is_chained() {
                    continue;
                }
                if k >= intermediate_roots.len() {
                    break;
                }
                let pre_root = intermediate_roots[k];
                let start = self.pending_l1.groups[k].start;
                let end = if k + 1 < num_groups {
                    self.pending_l1.groups[k + 1].start
                } else {
                    self.pending_l1.len_entries()
                };
                for i in start..end {
                    if let Some(delta) = self.pending_l1.entries[i].state_deltas.first_mut() {
                        delta.current_state = pre_root;
                    }
                }
                debug!(
                    target: "based_rollup::driver",
                    group = k,
                    entries = end - start,
                    %pre_root,
                    "overrode currentState for independent group (partial revert)"
                );
            }

            // Log composite entry hashes for byte-level debugging.
            for (i, e) in self.pending_l1.entries.iter().enumerate() {
                use alloy_sol_types::SolType as _;
                let next_action_encoded =
                    crate::cross_chain::ICrossChainManagerL2::Action::abi_encode(
                        &e.next_action.to_sol_action(),
                    );
                let next_action_hash = alloy_primitives::keccak256(&next_action_encoded);
                let mut composite_input = Vec::with_capacity(64);
                composite_input.extend_from_slice(e.action_hash.as_b256().as_slice());
                composite_input.extend_from_slice(next_action_hash.as_slice());
                let composite = alloy_primitives::keccak256(&composite_input);
                debug!(
                    target: "based_rollup::driver",
                    idx = i,
                    action_hash = %e.action_hash,
                    next_action_type = ?e.next_action.action_type,
                    next_action_rollup_id = %e.next_action.rollup_id,
                    next_action_dest = %e.next_action.destination,
                    next_action_scope = ?e.next_action.scope.as_slice().iter().map(|s| format!("{s}")).collect::<Vec<_>>(),
                    next_action_data_hex = %format!("0x{}", hex::encode(&e.next_action.data)),
                    next_action_failed = e.next_action.failed,
                    current_state = %e.state_deltas.first().map(|d| format!("{}", d.current_state)).unwrap_or_default(),
                    new_state = %e.state_deltas.first().map(|d| format!("{}", d.new_state)).unwrap_or_default(),
                    composite_verify_hash = %composite,
                    "L1 entry [byte-level] for VerifyL1Batch comparison"
                );
            }
        }

        (clean_state_root, intermediate_roots)
    }

    /// Catch up with L1: derive the next batch, verify local blocks against
    /// derived blocks, apply any blocks we haven't seen yet, and commit the
    /// derivation cursor.
    ///
    /// Sets `self.pending_rewind_target` if verification detects a mismatch.
    /// Callers should check for a pending rewind and return early.
    async fn derive_and_verify_from_l1(&mut self, latest_l1_block: u64) -> Result<()> {
        let provider = self.get_l1_provider().clone();

        if self.derivation.last_processed_l1_block() < latest_l1_block {
            let batch = self
                .derivation
                .derive_next_batch(latest_l1_block, &provider)
                .await?;

            for block in &batch.blocks {
                // If a rewind was triggered by a previous block in this batch,
                // stop processing — remaining blocks will be re-derived after rewind.
                if self.pending_rewind_target.is_some() {
                    break;
                }

                if block.l2_block_number <= self.l2_head_number {
                    // We already built this block locally. Verify it matches L1.
                    self.verify_local_block_matches_l1(block)?;
                    continue;
                }
                debug!(
                    target: "based_rollup::driver",
                    l2_block = block.l2_block_number,
                    is_empty = block.is_empty,
                    "another builder submitted this block, applying"
                );
                // §4f deferred filtering: apply receipt-based filtering if needed.
                let effective_transactions = self.apply_deferred_filtering(block)?;
                let _ = self
                    .build_and_insert_block(
                        block.l2_block_number,
                        block.l2_timestamp,
                        block.l1_info.l1_block_hash,
                        block.l1_info.l1_block_number,
                        &effective_transactions,
                    )
                    .await?;
            }

            // If a rewind was triggered during verification, do NOT commit the
            // batch — the cursor must stay so blocks are re-derived after the
            // rewind completes.
            if self.pending_rewind_target.is_some() {
                return Ok(());
            }

            // All blocks processed successfully — commit the cursor state.
            self.derivation.commit_batch(&batch);
            self.maybe_save_checkpoint()?;
        }

        Ok(())
    }

    /// Compute the builder tick context: target L2 block, L1 block info.
    ///
    /// Returns `None` if building should be skipped this tick:
    /// - L1 has not advanced past the deployment block
    /// - Already at or past the target L2 block
    /// - Entry verification hold is active (invariant #14)
    async fn compute_tick_context(
        &self,
        latest_l1_block: u64,
    ) -> Result<Option<BuilderTickContext>> {
        // Wait for at least one L1 block after deployment before building.
        // The L1 context rule is: containing_l1_block - 1. The builder uses
        // latest_l1_block as context, so we need latest_l1_block > deployment
        // to ensure the submitted tx produces matching context.
        if latest_l1_block <= self.config.deployment_l1_block {
            debug!(
                target: "based_rollup::driver",
                latest_l1_block,
                deployment_l1_block = self.config.deployment_l1_block,
                "waiting for L1 to advance past deployment block before building"
            );
            return Ok(None);
        }

        // Derive the target L2 block deterministically from the L1 head.
        // l2_block_number(N) = N - deployment_l1_block. The builder targets
        // the next L1 block (latest + 1) for postBatch, so building up to
        // l2_block_number(latest) produces a block whose timestamp matches.
        let target_l2_block = self.config.l2_block_number(latest_l1_block);

        // Cap the catch-up gap to prevent runaway block production.
        if target_l2_block > self.l2_head_number.saturating_add(MAX_CATCHUP_BLOCKS) {
            error!(
                target: "based_rollup::driver",
                head = self.l2_head_number,
                target = target_l2_block,
                gap = target_l2_block.saturating_sub(self.l2_head_number),
                "catch-up gap exceeds {} blocks — building max {} this step",
                MAX_CATCHUP_BLOCKS,
                MAX_CATCHUP_BLOCKS
            );
        }
        let effective_target =
            target_l2_block.min(self.l2_head_number.saturating_add(MAX_CATCHUP_BLOCKS));

        // Nothing to build this tick.
        if self.l2_head_number >= effective_target {
            return Ok(None);
        }

        // Fetch L1 block hash for the current L1 head.
        let provider = self.get_l1_provider().clone();
        let l1_hash = provider
            .get_block_by_number(latest_l1_block.into())
            .await?
            .ok_or_else(|| eyre::eyre!("L1 block {latest_l1_block} not found"))?
            .header
            .hash;

        // Don't build new blocks while waiting for entry verification.
        // Building during hold accumulates blocks with advancing L1 context
        // that will mismatch after rewind, causing a double rewind cycle.
        // Check BEFORE draining queues so entries accumulate in the shared
        // queues until the hold clears.
        //
        // Closes invariant #14 — `is_blocking_build` is the typed gate.
        if self.hold.is_blocking_build() {
            return Ok(None);
        }

        Ok(Some(BuilderTickContext {
            effective_target,
            current_l1_block: latest_l1_block,
            l1_hash,
        }))
    }

    /// Drain cross-chain call queues (L1→L2 and L2→L1), fetch L1 execution
    /// entries, sort by gas price, merge into a single entry vector, and
    /// inject held L2 txs into the pool.
    ///
    /// Returns a [`QueueDrainResult`] whose `rollback` field must be
    /// consumed via [`rollback_queue_drain`] if block building fails.
    async fn drain_rpc_queues(&mut self, current_l1_block: u64) -> Result<QueueDrainResult> {
        let provider = self.get_l1_provider().clone();

        // Fetch cross-chain execution entries from L1 for builder blocks.
        let mut builder_execution_entries = self
            .derivation
            .fetch_execution_entries_for_builder(current_l1_block, &provider)
            .await?;

        let mut rpc_entry_count: usize = 0;

        // Snapshot for rollback on build failure (issue #237).
        let pre_drain_l1_len = self.pending_l1.len_entries();
        let pre_drain_l1_groups = self.pending_l1.num_groups();

        let mut queued_l1_txs: Vec<Bytes> = Vec::new();
        let mut calls_for_repush: Vec<crate::rpc::QueuedCrossChainCall> = Vec::new();

        // --- L1→L2 queue (deposits, cross-chain calls) ---
        {
            let mut queue = self
                .queued_cross_chain_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !queue.is_empty() {
                let mut calls: Vec<_> = queue.drain(..).collect();
                // Sort by gas price descending — matches L1 miner tx ordering.
                calls.sort_by_key(|b| std::cmp::Reverse(b.effective_gas_price()));

                info!(
                    target: "based_rollup::driver",
                    count = calls.len(),
                    gas_prices = ?calls.iter().map(|c| c.effective_gas_price()).collect::<Vec<_>>(),
                    "merging RPC cross-chain entries (sorted by gas price)"
                );

                // One continuation per cycle: only process the FIRST
                // continuation call to prevent mixing entries.
                let mut had_continuation = false;
                let mut rpc_entries: Vec<CrossChainExecutionEntry> = Vec::new();
                for call in calls {
                    let is_continuation = call.is_continuation();
                    if is_continuation && had_continuation {
                        queue.push(call);
                        continue;
                    }
                    if is_continuation {
                        had_continuation = true;
                    }

                    let group_mode = call.l1_independent_entries();
                    let raw_l1_tx_for_forward = call.raw_l1_tx().clone();
                    let group_l1_entries: Vec<CrossChainExecutionEntry> = match &call {
                        crate::rpc::QueuedCrossChainCall::Simple {
                            call_entry,
                            result_entry,
                            ..
                        } => {
                            let is_terminal_failure = result_entry.next_action.failed
                                && !crate::cross_chain::is_simulation_artifact(
                                    &result_entry.next_action.data,
                                );
                            if !is_terminal_failure {
                                rpc_entries.push(call_entry.clone());
                                rpc_entries.push(result_entry.clone());
                            } else {
                                tracing::debug!(
                                    target: "based_rollup::driver",
                                    call_id = %call_entry.action_hash,
                                    data_len = result_entry.next_action.data.len(),
                                    "terminal failure: skipping L2 entries (delivery always reverts)"
                                );
                            }

                            // Simple deposit: convert CALL+RESULT pair to L1 format
                            crate::cross_chain::convert_pairs_to_l1_entries(&[
                                call_entry.clone(),
                                result_entry.clone(),
                            ])
                        }
                        crate::rpc::QueuedCrossChainCall::WithContinuations {
                            l2_table_entries,
                            l1_entries,
                            ..
                        } => {
                            // Terminal failure check for continuation path:
                            // The L2 table entries are [CALL trigger, ...RESULT entries].
                            // Check the LAST entry (terminal RESULT): if failed=true
                            // with non-artifact data, the delivery always fails.
                            // L1 entries are still posted for state commitment.
                            let is_continuation_terminal = l2_table_entries
                                .last()
                                .map(|e| {
                                    e.next_action.failed
                                        && !e.next_action.data.is_empty()
                                        && !crate::cross_chain::is_simulation_artifact(
                                            &e.next_action.data,
                                        )
                                })
                                .unwrap_or(false);
                            if is_continuation_terminal {
                                tracing::info!(
                                    target: "based_rollup::driver",
                                    l2_entries = l2_table_entries.len(),
                                    "terminal failure (continuation): skipping L2 table entries"
                                );
                                // Skip L2 entries — no loadExecutionTable on L2.
                            } else {
                                rpc_entries.extend(l2_table_entries.iter().cloned());
                            }
                            // L1 entries always posted (state commitment).
                            l1_entries.clone()
                        }
                    };

                    if !raw_l1_tx_for_forward.is_empty() {
                        queued_l1_txs.push(raw_l1_tx_for_forward);
                    }
                    self.pending_l1
                        .append_group(group_l1_entries, group_mode, None);
                    calls_for_repush.push(call);
                }
                rpc_entry_count = rpc_entries.len();
                builder_execution_entries.extend(rpc_entries);
            }
        }

        // --- L2→L1 queue (withdrawals, cross-chain calls) ---
        let mut l2_to_l1_for_repush: Vec<crate::rpc::QueuedL2ToL1Call> = Vec::new();
        let mut held_l2_txs: Vec<Bytes> = Vec::new();
        {
            let mut queue = self
                .queued_l2_to_l1_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !queue.is_empty() {
                let l2_to_l1_calls: Vec<_> = queue.drain(..).collect();
                info!(
                    target: "based_rollup::driver",
                    count = l2_to_l1_calls.len(),
                    protocol_entries = rpc_entry_count,
                    "draining L2→L1 call queue (unified intermediate roots)"
                );
                l2_to_l1_for_repush = l2_to_l1_calls.clone();
                for w in l2_to_l1_calls {
                    if !w.raw_l2_tx.is_empty() {
                        held_l2_txs.push(w.raw_l2_tx);
                    }
                    let w_entry_count = w.l2_table_entries.len();
                    builder_execution_entries.extend(w.l2_table_entries.iter().cloned());
                    rpc_entry_count += w_entry_count;

                    self.pending_l1.append_group(
                        w.l1_deferred_entries.iter().cloned(),
                        crate::cross_chain::EntryGroupMode::Chained,
                        Some(TriggerMetadata {
                            user: w.user,
                            amount: w.amount,
                            rlp_encoded_tx: w.rlp_encoded_tx.clone(),
                            trigger_count: w.trigger_count,
                        }),
                    );
                }
            }
        }

        // Inject held L2 txs into the pool BEFORE block building.
        if !held_l2_txs.is_empty() {
            self.inject_held_l2_txs(&held_l2_txs).await;
        }

        Ok(QueueDrainResult {
            builder_execution_entries,
            rpc_entry_count,
            queued_l1_txs,
            rollback: QueueDrainRollback {
                pre_drain_l1_len,
                pre_drain_l1_groups,
                calls_for_repush,
                l2_to_l1_for_repush,
            },
        })
    }

    /// Undo a queue drain after a build failure: truncate pending_l1 to its
    /// pre-drain state and re-push drained calls to the shared queues.
    fn rollback_queue_drain(&mut self, rollback: QueueDrainRollback) {
        self.pending_l1
            .truncate_to(rollback.pre_drain_l1_len, rollback.pre_drain_l1_groups);
        if !rollback.calls_for_repush.is_empty() {
            let mut q = self
                .queued_cross_chain_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            warn!(
                target: "based_rollup::driver",
                count = rollback.calls_for_repush.len(),
                "re-pushing cross-chain calls to shared queue after build failure"
            );
            q.extend(rollback.calls_for_repush);
        }
        if !rollback.l2_to_l1_for_repush.is_empty() {
            let mut q = self
                .queued_l2_to_l1_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            warn!(
                target: "based_rollup::driver",
                count = rollback.l2_to_l1_for_repush.len(),
                "re-pushing L2→L1 calls to shared queue after build failure"
            );
            q.extend(rollback.l2_to_l1_for_repush);
        }
    }

    /// Read the builder's current L2 nonce from chain state.
    /// Called on Sync→Builder transitions to ensure correct nonce after reorgs.
    pub(super) fn recover_builder_l2_nonce(&mut self) {
        if self.config.builder_address.is_zero() {
            return;
        }
        // Use state_by_block_hash(head_hash) instead of latest() so the nonce
        // reflects the actual fork-choice head after rewinds.  latest() can
        // return stale (pre-rewind) state when reth hasn't fully unwound yet.
        match self.l2_provider.state_by_block_hash(self.head_hash) {
            Ok(state) => {
                use reth_provider::AccountReader;
                match state.basic_account(&self.config.builder_address) {
                    Ok(Some(account)) => {
                        self.builder_l2_nonce = account.nonce;
                        info!(
                            target: "based_rollup::driver",
                            nonce = account.nonce,
                            head_hash = %self.head_hash,
                            head_number = self.l2_head_number,
                            builder = %self.config.builder_address,
                            "recovered builder L2 nonce from state"
                        );
                    }
                    Ok(None) => {
                        self.builder_l2_nonce = 0;
                        debug!(
                            target: "based_rollup::driver",
                            "builder account not found in state, using nonce 0"
                        );
                    }
                    Err(err) => {
                        warn!(
                            target: "based_rollup::driver",
                            %err,
                            "failed to read builder account — using nonce 0"
                        );
                        self.builder_l2_nonce = 0;
                    }
                }
            }
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    head_hash = %self.head_hash,
                    "failed to get state provider for head — using nonce 0"
                );
                self.builder_l2_nonce = 0;
            }
        }
    }

    /// Collect user transactions from blocks that are about to be reverted.
    ///
    /// Reads block bodies from `from_block..=to_block` (inclusive) while they are
    /// still canonical (BEFORE the FCU rewind removes them). Filters out the
    /// builder's own protocol transactions (setContext, etc.) since those are
    /// rebuilt fresh for every block.
    ///
    /// Returns (sender, transaction) pairs with signers already recovered.
    pub(super) fn collect_reverted_user_transactions(
        &self,
        from_block: u64,
        to_block: u64,
    ) -> Vec<(
        alloy_primitives::Address,
        reth_ethereum_primitives::TransactionSigned,
    )> {
        if from_block > to_block {
            return Vec::new();
        }

        let block_range_txs = match self
            .l2_provider
            .transactions_by_block_range(from_block..=to_block)
        {
            Ok(txs) => txs,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    from_block,
                    to_block,
                    "failed to read block transactions for pool sync — \
                     pool may have stale state until maintenance catches up"
                );
                return Vec::new();
            }
        };

        let mut result = Vec::new();
        for block_txs in block_range_txs {
            for tx in block_txs {
                match tx.recover_signer() {
                    Ok(sender) => {
                        // Skip builder's protocol transactions — they are
                        // rebuilt fresh by build_builder_protocol_txs().
                        if sender == self.config.builder_address {
                            continue;
                        }
                        result.push((sender, tx));
                    }
                    Err(err) => {
                        warn!(
                            target: "based_rollup::driver",
                            %err,
                            "failed to recover signer during pool sync — skipping tx"
                        );
                    }
                }
            }
        }

        result
    }

    /// Inject held L2 transactions into the pool.
    ///
    /// These are user txs that were held by the L2 proxy (hold-then-forward pattern)
    /// to prevent the timing race where a tx enters the mempool before entries are
    /// loaded. The proxy computes the tx hash and returns it to the user immediately,
    /// while the raw tx is queued alongside the entries. The driver injects these
    /// into the pool right before block building, ensuring entries and txs land in
    /// the same block.
    ///
    /// Failures are non-fatal: if pool rejects a tx, entries still load and the
    /// user can resend. This matches L1 proxy behavior.
    pub(super) async fn inject_held_l2_txs(&self, held_txs: &[Bytes]) {
        use alloy_rlp::Decodable;

        let mut pool_txs: Vec<Pool::Transaction> = Vec::new();

        for raw in held_txs {
            // TransactionSigned = EthereumTxEnvelope<TxEip4844>, which implements
            // alloy_rlp::Decodable for the same EIP-2718 typed envelope format
            // that eth_sendRawTransaction uses. Decode directly — no roundtrip needed.
            let signed =
                match reth_ethereum_primitives::TransactionSigned::decode(&mut raw.as_ref()) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            target: "based_rollup::driver",
                            %e,
                            "failed to decode held L2 tx — skipping"
                        );
                        continue;
                    }
                };

            let tx_hash = *signed.tx_hash();
            let signer = match signed.recover_signer() {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        target: "based_rollup::driver",
                        %e,
                        ?tx_hash,
                        "failed to recover signer from held L2 tx — skipping"
                    );
                    continue;
                }
            };

            let recovered = Recovered::new_unchecked(signed, signer);
            match reth_transaction_pool::PoolTransaction::try_from_consensus(recovered) {
                Ok(pool_tx) => pool_txs.push(pool_tx),
                Err(_e) => {
                    warn!(
                        target: "based_rollup::driver",
                        ?tx_hash,
                        "failed to convert held L2 tx to pool tx — skipping"
                    );
                }
            }
        }

        if pool_txs.is_empty() {
            return;
        }

        let count = pool_txs.len();
        let results = self.pool.add_external_transactions(pool_txs).await;
        let accepted = results.iter().filter(|r| r.is_ok()).count();

        info!(
            target: "based_rollup::driver",
            count,
            accepted,
            rejected = count - accepted,
            "injected held L2 txs into pool (hold-then-forward)"
        );
    }

    /// Deferred re-injection: add transactions from a previous rewind back into
    /// the pool. Called at the top of step(), after reth's async pool maintenance
    /// has fully processed the CanonStateNotification from the FCU rewind.
    ///
    /// This eliminates the TOCTOU race in the old `sync_pool_after_rewind`:
    /// - OLD: update_accounts → .await add_external_transactions → reth's Commit
    ///   notification interleaves, overwrites nonces → tx rejected, permanently lost
    /// - NEW: defer re-injection by one full step() iteration (~12s). By then,
    ///   reth's Reorg notification has updated pool nonces. No race possible.
    pub(super) async fn reinject_pending_transactions(&mut self) {
        let txs = std::mem::take(&mut self.pending_reinjection);
        if txs.is_empty() {
            return;
        }

        let pool_txs: Vec<Pool::Transaction> = txs
            .iter()
            .filter_map(|(sender, tx)| {
                let recovered = Recovered::new_unchecked(tx.clone(), *sender);
                reth_transaction_pool::PoolTransaction::try_from_consensus(recovered).ok()
            })
            .collect();

        let tx_count = pool_txs.len();
        let results = self.pool.add_external_transactions(pool_txs).await;
        let accepted = results.iter().filter(|r| r.is_ok()).count();

        info!(
            target: "based_rollup::driver",
            tx_count,
            accepted,
            rejected = tx_count - accepted,
            "deferred pool re-injection after rewind"
        );
    }
}
