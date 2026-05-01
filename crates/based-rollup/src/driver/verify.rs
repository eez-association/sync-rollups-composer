//! Derivation verification + §4f deferred filtering.
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1i. This is the
//! biggest single impl-block extraction: ~750 lines covering the
//! post-derivation verify path and the four filtering helpers.
//!
//! - [`Driver::classify_and_apply_verification`] — the state machine
//!   behind `verify_local_block_matches_l1`. Returns a
//!   [`VerificationDecision`] after applying all side effects inline.
//!   Owns the invariant #9 + #10 closure.
//! - [`Driver::verify_local_block_matches_l1`] — thin wrapper that
//!   maps the decision to `Result<()>` and emits a trace-level log.
//! - [`Driver::apply_deferred_filtering`] — top-level router between
//!   the rebuild path (builder with signer) and the filter path.
//! - [`Driver::apply_generic_filtering`] — raw-byte prefix filter.
//! - [`Driver::apply_generic_filtering_via_rebuild`] — rebuild from
//!   entries with `max_trigger_count`.
//! - [`Driver::compute_state_root_with_entries`] — per-prefix state
//!   root computation via isolated clone of the EVM config.
//! - [`Driver::trial_execute_for_receipts`] — trial execution
//!   returning per-tx receipts for generic trigger identification.
//! - [`Driver::compute_intermediate_roots`] — generic N+1 root chain
//!   builder used by the unified D+W state-delta construction.

use super::Driver;
use super::hold::{DeferralResult, MAX_ENTRY_VERIFY_DEFERRALS};
use super::types::{
    DESIRED_GAS_LIMIT, DriverMode, VerificationDecision, VerifyMismatchAction, calc_gas_limit,
    classify_verify_mismatch, plan_sibling_reorg_from_verify,
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes};
use eyre::{OptionExt, Result, WrapErr};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_primitives_traits::SignedTransaction;
use reth_provider::{
    BlockHashReader, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use reth_revm::database::StateProviderDatabase;
use revm::database::State;
use tracing::{debug, error, info, trace, warn};

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
    /// Verify that a locally built block matches what derivation produced for
    /// it and return a `VerificationDecision` describing the outcome.
    ///
    /// This method applies all side effects (rewind target, mode switch, hold
    /// transitions) inline before returning; the returned enum is the record
    /// of which branch fired. The thin `verify_local_block_matches_l1` wrapper
    /// maps the decision to `Result<()>` for callers.
    pub(super) fn classify_and_apply_verification(
        &mut self,
        derived: &crate::derivation::DerivedBlock,
    ) -> Result<VerificationDecision> {
        // Skip verification for blocks that are permanently committed in reth
        // and cannot be unwound via FCU. These were built during a prior session
        // or before a failed rewind. Re-triggering a rewind for them would be
        // futile (the rewind can't actually remove them) and cause an infinite
        // verify→rewind→recover→verify loop.
        if derived.l2_block_number <= self.immutable_block_ceiling {
            debug!(
                target: "based_rollup::driver",
                l2_block = derived.l2_block_number,
                ceiling = self.immutable_block_ceiling,
                "skipping verification for immutable block (cannot be unwound)"
            );
            return Ok(VerificationDecision::Skip);
        }

        let local_header = self
            .l2_provider
            .sealed_header(derived.l2_block_number)
            .wrap_err("failed to read local header for verification")?;

        let Some(local_header) = local_header else {
            warn!(
                target: "based_rollup::driver",
                l2_block = derived.l2_block_number,
                "cannot verify L1 match: local block not found"
            );
            return Ok(VerificationDecision::Skip);
        };

        let is_gap_fill = derived.state_root == B256::ZERO;

        // Check L1 context: the builder stored the L1 block number in prev_randao
        // (mix_hash) and the L1 block hash in parent_beacon_block_root. Compare
        // against what derivation produced from the containing L1 block.
        //
        // This check applies to BOTH gap-fill and submitted blocks. Gap-fill blocks
        // are built by the builder with `latest_l1_block` as context, but derivation
        // uses `last_l1_info` (from the previous submission). Since L2Context stores
        // per-block context in a mapping, different L1 context values produce different
        // state roots that never converge. The builder must rewind and re-derive with
        // the canonical context to stay in consensus.
        let local_mix_hash = local_header.mix_hash().unwrap_or_default();
        let local_l1_number: u64 = local_mix_hash.as_slice()[24..32]
            .try_into()
            .map(u64::from_be_bytes)
            .unwrap_or(0);
        let derived_l1_number = derived.l1_info.l1_block_number;

        if local_l1_number != derived_l1_number {
            // L1 context mismatch. For gap-fill blocks this happens when the builder
            // used a newer L1 block than derivation's `last_l1_info`. For submitted
            // blocks this happens when the tx landed in a later L1 block than expected.
            //
            // Use sibling-reorg (newPayloadV3 + FCU on a sibling hash) instead of
            // a bare FCU rewind. On reth Ethereum engine kind, FCU-to-ancestor is
            // a silent no-op — the canonical tip never unwinds. The sibling-reorg
            // path installs a new block at the divergent height with the correct
            // L1 context as a forked branch, then advances the head to it; reth
            // honors that as a normal reorg and wipes the speculative branch.
            //
            // `step_sync` consumes `pending_sibling_reorg` and runs
            // `rebuild_block_as_sibling` on the next tick. The rollback of the
            // derivation cursor + L1 context is done by `apply_sibling_reorg_plan`.
            info!(
                target: "based_rollup::driver",
                l2_block = derived.l2_block_number,
                local_l1_context = local_l1_number,
                derived_l1_context = derived_l1_number,
                is_gap_fill,
                "L1 context mismatch — queuing sibling reorg to install canonical \
                 version (bare FCU rewind is a silent no-op on Ethereum engine kind)"
            );
            let plan = plan_sibling_reorg_from_verify(
                derived.l2_block_number,
                derived.state_root,
                self.l1_confirmed_anchor,
                self.config.deployment_l1_block,
            );
            self.apply_sibling_reorg_plan(plan);
            return Ok(VerificationDecision::SiblingReorgQueued {
                target_l2_block: derived.l2_block_number,
                expected_root: derived.state_root,
            });
        }

        // For gap-fill blocks, L1 context match is sufficient — there's no L1 state
        // root to compare against (it's B256::ZERO). The block content is deterministic
        // (empty txs, no deposits), so matching L1 context guarantees identical state.
        if is_gap_fill {
            // If this is the pending entry verification block (state_root was set to
            // ZERO by derivation because entry txs were filtered), release the hold.
            // Without this, the hold would persist indefinitely since the root
            // comparison and hold release logic below are skipped for gap-fill blocks.
            if self.hold.is_armed_for(derived.l2_block_number) {
                info!(
                    target: "based_rollup::driver",
                    l2_block = derived.l2_block_number,
                    pending_blocks = self.pending_submissions.len(),
                    "entry-bearing block with filtered txs verified (state_root=ZERO) \
                     — releasing submission hold"
                );
                self.hold.clear();
            } else {
                debug!(
                    target: "based_rollup::driver",
                    l2_block = derived.l2_block_number,
                    l1_context = derived_l1_number,
                    "gap-fill block verified: L1 context matches"
                );
            }
            return Ok(VerificationDecision::GapFillVerified);
        }

        // With protocol tx filtering (§4f), derivation produces the correct root
        // for any consumption level. The derived root should match the header root
        // directly. If it doesn't, the builder's speculative block diverged from
        // the L1-derived block (e.g., entries were not consumed). Rewind is
        // productive — re-derivation with filtered txs produces the correct root.
        let header_root = local_header.state_root();
        if header_root != derived.state_root {
            // Dispatch is centralized in `classify_verify_mismatch` (pure fn)
            // so the branching is testable without instantiating a driver. The
            // classifier does NOT mutate state — the match arms below carry
            // the identical side-effects the old inline branching did.
            //
            // Issue #36 fast-path (C1 gate): when derivation flagged this
            // block as needing §4f filtering, the mismatch is provably NOT a
            // timing race — it's the speculative/clean divergence that the
            // deferral loop cannot resolve. Queue a sibling reorg immediately.
            let is_pending_entry_block = self.hold.is_armed_for(derived.l2_block_number);
            let action = classify_verify_mismatch(
                derived.filtering.is_some(),
                self.pending_sibling_reorg.is_some(),
                is_pending_entry_block,
                self.hold.deferrals(),
                MAX_ENTRY_VERIFY_DEFERRALS,
            );
            match action {
                VerifyMismatchAction::FastPathSiblingReorg => {
                    warn!(
                        target: "based_rollup::driver",
                        l2_block = derived.l2_block_number,
                        %header_root,
                        l1_state_root = %derived.state_root,
                        "issue #36: §4f-filtered divergence at verify — queuing sibling \
                         reorg immediately (skipping deferrals; L1 is already definitive)"
                    );
                    let plan = plan_sibling_reorg_from_verify(
                        derived.l2_block_number,
                        derived.state_root,
                        self.l1_confirmed_anchor,
                        self.config.deployment_l1_block,
                    );
                    self.apply_sibling_reorg_plan(plan);
                    return Ok(VerificationDecision::SiblingReorgQueued {
                        target_l2_block: derived.l2_block_number,
                        expected_root: derived.state_root,
                    });
                }
                VerifyMismatchAction::NoOpPendingSiblingReorg => {
                    // Production-critical: a sibling-reorg request is already
                    // queued for an earlier tick (by Fix 1 in flush_to_l1,
                    // Fix 2 in flush_precheck, or the verify fast-path above
                    // on a prior tick). `step_builder` / `step_sync` dispatch
                    // will complete the reorg on a subsequent tick. If we
                    // instead took the generic-rewind path we would
                    // `clear_internal_state()` — wiping `pending_sibling_reorg`
                    // — and `set_rewind_target()` — arming a bare FCU rewind.
                    // On reth `--dev` bare FCU-to-ancestor accidentally works
                    // because the auto-seal engine tolerates backward FCU; on
                    // production Ethereum-engine reth it's a silent no-op per
                    // Engine API spec, leaving the builder permanently
                    // divergent from fullnodes.
                    //
                    // Return `Err` so the main loop's backoff machinery
                    // engages, but do NOT mutate driver state: the queued
                    // reorg stays intact and completes on its own.
                    let queued_target = self.pending_sibling_reorg.map(|r| r.target_l2_block);
                    let queued_expected_root = self.pending_sibling_reorg.map(|r| r.expected_root);
                    warn!(
                        target: "based_rollup::driver",
                        l2_block = derived.l2_block_number,
                        %header_root,
                        l1_state_root = %derived.state_root,
                        ?queued_target,
                        ?queued_expected_root,
                        "§4f divergence at verify but sibling reorg already queued — \
                         returning Err to engage backoff; queued reorg will complete on a \
                         subsequent tick (do NOT wipe pending_sibling_reorg or set \
                         pending_rewind_target)"
                    );
                    let _decision = VerificationDecision::SiblingReorgAlreadyQueued {
                        target_l2_block: derived.l2_block_number,
                        queued_target,
                    };
                    return Err(eyre::eyre!(
                        "state root mismatch at L2 block {} deferred to queued sibling reorg",
                        derived.l2_block_number
                    ));
                }
                VerifyMismatchAction::DeferEntryVerify => {
                    // Existing hold-defer branch — the classifier identified
                    // `is_pending_entry_block && deferrals < MAX-1`.
                    match self.hold.defer() {
                        DeferralResult::Continue { deferrals } => {
                            warn!(
                                target: "based_rollup::driver",
                                l2_block = derived.l2_block_number,
                                deferrals,
                                max_deferrals = MAX_ENTRY_VERIFY_DEFERRALS,
                                %header_root,
                                l1_state_root = %derived.state_root,
                                "entry-bearing block state root mismatch — consumption event \
                                 may be in a later L1 block, deferring verification"
                            );
                            return Err(eyre::eyre!(
                                "entry verification deferred for block {} (attempt {}/{})",
                                derived.l2_block_number,
                                deferrals,
                                MAX_ENTRY_VERIFY_DEFERRALS
                            ));
                        }
                        DeferralResult::MustRewind {
                            target: rewind_target,
                        } => {
                            // Classifier promised `Continue`; this branch is
                            // only reached if the hold was mutated between
                            // the classify call and the `defer` call. Handle
                            // defensively by rewinding.
                            let rollback_l1_block = if let Some(anchor) = self.l1_confirmed_anchor {
                                anchor.l1_block_number.saturating_sub(1)
                            } else {
                                self.config.deployment_l1_block
                            };
                            self.rewind_to_re_derive(rewind_target, rollback_l1_block);
                            return Ok(VerificationDecision::MismatchDeferExhausted {
                                rewind_target,
                            });
                        }
                        DeferralResult::NotArmed => {
                            // Classifier said `is_pending_entry_block=true`;
                            // shouldn't happen. Fall through to generic rewind.
                        }
                    }
                }
                VerifyMismatchAction::ExhaustedDeferralRewind => {
                    // Exhausted deferrals — entry likely not consumed. Rewind
                    // to re-derive the block with §4f filtering.
                    let rewind_target = match self.hold.defer() {
                        DeferralResult::MustRewind { target } => target,
                        _ => derived.l2_block_number.saturating_sub(1),
                    };
                    warn!(
                        target: "based_rollup::driver",
                        l2_block = derived.l2_block_number,
                        deferrals = MAX_ENTRY_VERIFY_DEFERRALS,
                        %header_root,
                        l1_state_root = %derived.state_root,
                        "entry not consumed after max deferrals — rewinding to rebuild \
                         with §4f-filtered txs and correct nonces"
                    );
                    let rollback_l1_block = if let Some(anchor) = self.l1_confirmed_anchor {
                        anchor.l1_block_number.saturating_sub(1)
                    } else {
                        self.config.deployment_l1_block
                    };
                    self.rewind_to_re_derive(rewind_target, rollback_l1_block);
                    return Ok(VerificationDecision::MismatchDeferExhausted { rewind_target });
                }
                VerifyMismatchAction::GenericMismatchRewind => {
                    // Fall through to the generic rewind below.
                }
            }

            error!(
                target: "based_rollup::driver",
                l2_block = derived.l2_block_number,
                %header_root,
                l1_state_root = %derived.state_root,
                "builder state root MISMATCH — switching to sync mode \
                 (rewind productive via §4f protocol tx filtering)"
            );
            self.mode = DriverMode::Sync;
            self.synced
                .store(false, std::sync::atomic::Ordering::Relaxed);
            self.consecutive_rewind_cycles = self.consecutive_rewind_cycles.saturating_add(1);
            self.clear_internal_state();
            self.derivation.rollback_to(derived_l1_number);
            let rewind_target = derived.l2_block_number.saturating_sub(1);
            self.set_rewind_target(rewind_target);
            // Record the decision explicitly, then convert to Err so the main
            // loop's backoff machinery kicks in. The `rewind_target` in the
            // decision is for telemetry only — the side effects are already
            // applied above.
            let _decision = VerificationDecision::MismatchPermanent { rewind_target };
            return Err(eyre::eyre!(
                "state root mismatch at L2 block {}: header={header_root}, L1={}",
                derived.l2_block_number,
                derived.state_root
            ));
        }

        // Clear entry verification hold if this was the pending entry block.
        // Derivation confirmed the block matches — nonces are correct, builder
        // can resume posting accumulated pending blocks.
        if self.hold.is_armed_for(derived.l2_block_number) {
            info!(
                target: "based_rollup::driver",
                l2_block = derived.l2_block_number,
                pending_blocks = self.pending_submissions.len(),
                deferrals = self.hold.deferrals(),
                "entry-bearing block verified — releasing submission hold"
            );
            self.hold.clear();
        }

        debug!(
            target: "based_rollup::driver",
            l2_block = derived.l2_block_number,
            %header_root,
            l1_context = derived_l1_number,
            "builder block verified: L1 context and state root match"
        );

        Ok(VerificationDecision::Match)
    }

    /// Thin wrapper around `classify_and_apply_verification` that preserves
    /// the `Result<()>` API expected by callers. The outcome is logged at
    /// trace level so operators can correlate decisions with downstream
    /// state transitions.
    pub(super) fn verify_local_block_matches_l1(
        &mut self,
        derived: &crate::derivation::DerivedBlock,
    ) -> Result<()> {
        let decision = self.classify_and_apply_verification(derived)?;
        trace!(
            target: "based_rollup::driver",
            l2_block = derived.l2_block_number,
            ?decision,
            "verification decision"
        );
        Ok(())
    }

    /// Apply deferred §4f protocol tx filtering to a derived block.
    ///
    /// When derivation flags a block with `DeferredFiltering` metadata (unconsumed
    /// entries exist), this method filters the block's transactions to keep only
    /// the consumed trigger prefix.
    ///
    /// Two paths:
    /// - **Rebuild path** (preferred): when the filtering carries `all_l2_entries`
    ///   AND a proposer (signer) is available, rebuild the block from entries via
    ///   `build_builder_protocol_txs` with `max_trigger_count`. This uses the
    ///   same construction path as the builder and properly advances `builder_l2_nonce`.
    /// - **Filter path** (fallback): parse the raw encoded transactions from L1
    ///   calldata and filter via `filter_block_by_trigger_prefix`. Used by
    ///   fullnodes (no signer) or when `all_l2_entries` is empty.
    ///
    /// Returns the effective (filtered) transaction bytes. If no filtering is needed
    /// (`block.filtering` is `None`), returns the original transactions unchanged.
    pub(super) fn apply_deferred_filtering(
        &mut self,
        block: &crate::derivation::DerivedBlock,
    ) -> Result<Bytes> {
        let Some(ref filtering) = block.filtering else {
            return Ok(block.transactions.clone());
        };

        // Prefer rebuild path when entries are available and we have a signer.
        if !filtering.all_l2_entries.is_empty() && self.proposer.is_some() {
            return self.apply_generic_filtering_via_rebuild(block, filtering);
        }

        // Fallback: filter raw encoded transactions.
        self.apply_generic_filtering(block, filtering)
    }

    /// Generic §4f filtering using `ExecutionConsumed` events.
    ///
    /// Protocol-generic filtering that works uniformly for any cross-chain entry type:
    ///
    /// 1. Trial-executes the full block (with ALL triggers) to get receipts
    /// 2. Identifies trigger tx indices via `ExecutionConsumed` events from the CCM
    /// 3. Computes consumed trigger prefix using the L1 consumed map (FIFO counting)
    /// 4. Filters to keep only consumed triggers + all non-trigger txs
    ///
    /// The L1 consumed map (`filtering.l1_consumed_remaining`) is a snapshot taken
    /// by derivation BEFORE the current batch's entries consume it, ensuring the
    /// driver can independently match triggers against L1 consumption data.
    pub(super) fn apply_generic_filtering(
        &self,
        block: &crate::derivation::DerivedBlock,
        filtering: &crate::derivation::DeferredFiltering,
    ) -> Result<Bytes> {
        let parent_block_number = block.l2_block_number.saturating_sub(1);

        // Step 1: Trial-execute the full block to get receipts.
        let receipts = self
            .trial_execute_for_receipts(
                parent_block_number,
                block.l2_timestamp,
                block.l1_info.l1_block_hash,
                block.l1_info.l1_block_number,
                &block.transactions,
            )
            .wrap_err("failed to trial-execute block for generic §4f filtering")?;

        // Step 2: Identify trigger tx indices via ExecutionConsumed events.
        let trigger_indices = crate::cross_chain::identify_trigger_tx_indices(
            &receipts,
            self.config.cross_chain_manager_address,
        );

        if trigger_indices.is_empty() {
            // No triggers found — nothing to filter.
            return Ok(block.transactions.clone());
        }

        // Step 3: Compute consumed trigger prefix using L1 consumed map.
        // Clone the map because compute_consumed_trigger_prefix mutates it
        // (decrements counters as it walks), and we don't want to affect the
        // derivation's shared state.
        let mut l1_remaining = filtering.l1_consumed_remaining.clone();
        let consumed = crate::cross_chain::compute_consumed_trigger_prefix(
            &receipts,
            self.config.cross_chain_manager_address,
            &mut l1_remaining,
            &trigger_indices,
        );

        let total_triggers = trigger_indices.len();
        let consumed_count = consumed.as_usize();
        let unconsumed_count = total_triggers.saturating_sub(consumed_count);

        info!(
            target: "based_rollup::driver",
            l2_block = block.l2_block_number,
            total_triggers,
            consumed_count,
            unconsumed_count,
            "applying §4f filtering (generic event-based)"
        );

        if consumed_count >= total_triggers {
            // All triggers consumed — no filtering needed.
            return Ok(block.transactions.clone());
        }

        // Step 4: Filter to keep only consumed trigger prefix.
        match crate::cross_chain::filter_block_by_trigger_prefix(
            &block.transactions,
            &trigger_indices,
            consumed_count,
        ) {
            Ok(filtered) => Ok(filtered),
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    l2_block = block.l2_block_number,
                    "failed to apply generic §4f filtering — using original transactions"
                );
                Ok(block.transactions.clone())
            }
        }
    }

    /// Generic §4f filtering via block rebuild using `build_builder_protocol_txs`.
    ///
    /// Instead of parsing and filtering raw encoded transaction bytes, this method
    /// rebuilds the block from the L2 execution entries carried in `DeferredFiltering`.
    /// This uses the same construction path as the builder, which:
    /// - Ensures correct protocol tx construction (setContext, loadTable, triggers)
    /// - Properly advances `builder_l2_nonce` for builder mode nonce tracking
    /// - Uses `max_trigger_count` to limit triggers to the consumed prefix
    ///
    /// Requires a proposer (signer) — fullnodes must use the filter path instead.
    ///
    /// Steps:
    /// 1. Save `builder_l2_nonce` (will be restored if not all triggers are consumed)
    /// 2. Build full block with ALL triggers via `build_builder_protocol_txs(entries, MAX)`
    /// 3. Trial-execute to get receipts
    /// 4. Identify trigger tx indices via `ExecutionConsumed` events from the CCM
    /// 5. Compute consumed trigger prefix using the L1 consumed map (FIFO counting)
    /// 6. If all consumed, return full block (nonce already advanced correctly)
    /// 7. Otherwise, restore nonce and rebuild with `max_trigger_count = consumed_count`
    pub(super) fn apply_generic_filtering_via_rebuild(
        &mut self,
        block: &crate::derivation::DerivedBlock,
        filtering: &crate::derivation::DeferredFiltering,
    ) -> Result<Bytes> {
        let l2_block_number = block.l2_block_number;
        let timestamp = block.l2_timestamp;
        let l1_block_hash = block.l1_info.l1_block_hash;
        let l1_block_number = block.l1_info.l1_block_number;
        let parent_block_number = l2_block_number.saturating_sub(1);

        // Step 1: Save nonce so we can restore it if we need to rebuild.
        let saved_nonce = self.builder_l2_nonce;

        // Step 2: Build full block with ALL triggers.
        let full_txs = match self.build_builder_protocol_txs(
            l2_block_number,
            timestamp,
            l1_block_hash,
            l1_block_number,
            &filtering.all_l2_entries,
            usize::MAX,
        ) {
            Ok(txs) => txs,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    l2_block = l2_block_number,
                    "failed to rebuild block for §4f filtering — falling back to filter path"
                );
                self.builder_l2_nonce = saved_nonce;
                return self.apply_generic_filtering(block, filtering);
            }
        };

        // Step 3: Trial-execute the full block to get receipts.
        let receipts = match self.trial_execute_for_receipts(
            parent_block_number,
            timestamp,
            l1_block_hash,
            l1_block_number,
            &full_txs,
        ) {
            Ok(r) => r,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    l2_block = l2_block_number,
                    "failed to trial-execute rebuilt block for §4f filtering — falling back"
                );
                self.builder_l2_nonce = saved_nonce;
                return self.apply_generic_filtering(block, filtering);
            }
        };

        // Step 4: Identify trigger tx indices via ExecutionConsumed events.
        let trigger_indices = crate::cross_chain::identify_trigger_tx_indices(
            &receipts,
            self.config.cross_chain_manager_address,
        );

        if trigger_indices.is_empty() {
            // No triggers found — nothing to filter. Nonce is already advanced
            // past the protocol txs (setContext, loadTable, etc.) which is correct.
            return Ok(full_txs);
        }

        // Step 5: Compute consumed trigger prefix using L1 consumed map.
        let mut l1_remaining = filtering.l1_consumed_remaining.clone();
        let consumed = crate::cross_chain::compute_consumed_trigger_prefix(
            &receipts,
            self.config.cross_chain_manager_address,
            &mut l1_remaining,
            &trigger_indices,
        );

        let total_triggers = trigger_indices.len();
        let consumed_count = consumed.as_usize();
        let unconsumed_count = total_triggers.saturating_sub(consumed_count);

        info!(
            target: "based_rollup::driver",
            l2_block = l2_block_number,
            total_triggers,
            consumed_count,
            unconsumed_count,
            "applying §4f filtering (generic via rebuild)"
        );

        // Step 6: If all triggers consumed, full block is correct.
        if consumed_count >= total_triggers {
            // Nonce already advanced correctly past all protocol txs.
            return Ok(full_txs);
        }

        // Step 7: Not all consumed — restore nonce and rebuild with limited triggers.
        self.builder_l2_nonce = saved_nonce;
        let filtered_txs = match self.build_builder_protocol_txs(
            l2_block_number,
            timestamp,
            l1_block_hash,
            l1_block_number,
            &filtering.all_l2_entries,
            consumed_count,
        ) {
            Ok(txs) => txs,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    l2_block = l2_block_number,
                    consumed_count,
                    "failed to rebuild filtered block — falling back to filter path"
                );
                // Nonce was already restored above. Fall back to raw byte filtering.
                return self.apply_generic_filtering(block, filtering);
            }
        };

        Ok(filtered_txs)
    }

    /// Compute the state root for a block built with the given transactions.
    /// Uses an `isolated_clone` of the evm_config. The block is built on a fresh
    /// state snapshot of the parent with the same transactions as the speculative block.
    pub(super) fn compute_state_root_with_entries(
        &self,
        parent_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        encoded_transactions: &Bytes,
    ) -> Result<B256> {
        use reth_evm::execute::BlockBuilder;

        let parent_header = self
            .l2_provider
            .sealed_header(parent_block_number)
            .wrap_err("failed to get parent header for state root computation")?
            .ok_or_eyre("parent header not found for state root computation")?;

        let state_provider = self
            .l2_provider
            .state_by_block_hash(parent_header.hash())
            .wrap_err("failed to get state provider for state root computation")?;

        let state_db = StateProviderDatabase::new(state_provider.as_ref());
        let mut db = State::builder()
            .with_database(state_db)
            .with_bundle_update()
            .build();

        let prev_randao = B256::from(alloy_primitives::U256::from(l1_block_number));
        let attributes = NextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: self.config.builder_address,
            prev_randao,
            gas_limit: calc_gas_limit(parent_header.gas_limit(), DESIRED_GAS_LIMIT),
            parent_beacon_block_root: Some(l1_block_hash),
            withdrawals: Some(Default::default()),
            extra_data: Default::default(),
        };

        let sim_evm_config = self.evm_config.isolated_clone();

        let mut builder = sim_evm_config
            .builder_for_next_block(&mut db, &parent_header, attributes)
            .wrap_err("failed to create block builder for state root computation")?;

        builder
            .apply_pre_execution_changes()
            .wrap_err("pre-execution changes failed for state root computation")?;

        // Execute the same transactions as the speculative block
        if !encoded_transactions.is_empty() {
            let txs: Vec<reth_ethereum_primitives::TransactionSigned> =
                alloy_rlp::Decodable::decode(&mut encoded_transactions.as_ref())
                    .wrap_err("failed to RLP-decode transactions for state root computation")?;

            for tx in txs {
                let recovered = SignedTransaction::try_into_recovered(tx).map_err(|_| {
                    eyre::eyre!("failed to recover signer for state root computation tx")
                })?;
                builder
                    .execute_transaction(recovered)
                    .wrap_err("failed to execute tx for state root computation")?;
            }
        }

        let outcome = builder
            .finish(state_provider.as_ref())
            .wrap_err("block builder finish failed for state root computation")?;

        Ok(outcome.block.sealed_block().sealed_header().state_root())
    }

    /// Trial-execute a block and return receipts.
    ///
    /// Builds a block from the given encoded transactions using the same EVM config
    /// as the real builder, executes all transactions, and returns the per-transaction
    /// receipts. Used by `compute_intermediate_roots` for generic trigger detection.
    pub(super) fn trial_execute_for_receipts(
        &self,
        parent_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        encoded_transactions: &Bytes,
    ) -> Result<Vec<alloy_consensus::Receipt<alloy_primitives::Log>>> {
        use reth_evm::execute::BlockBuilder;

        if encoded_transactions.is_empty() {
            return Ok(Vec::new());
        }

        let parent_header = self
            .l2_provider
            .sealed_header(parent_block_number)
            .wrap_err("failed to get parent header for trial execution")?
            .ok_or_eyre("parent header not found for trial execution")?;

        let state_provider = self
            .l2_provider
            .state_by_block_hash(parent_header.hash())
            .wrap_err("failed to get state provider for trial execution")?;

        let state_db = StateProviderDatabase::new(state_provider.as_ref());
        let mut db = State::builder()
            .with_database(state_db)
            .with_bundle_update()
            .build();

        let prev_randao = B256::from(alloy_primitives::U256::from(l1_block_number));
        let attributes = NextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: self.config.builder_address,
            prev_randao,
            gas_limit: calc_gas_limit(parent_header.gas_limit(), DESIRED_GAS_LIMIT),
            parent_beacon_block_root: Some(l1_block_hash),
            withdrawals: Some(Default::default()),
            extra_data: Default::default(),
        };

        let sim_evm_config = self.evm_config.isolated_clone();

        let mut builder = sim_evm_config
            .builder_for_next_block(&mut db, &parent_header, attributes)
            .wrap_err("failed to create block builder for trial execution")?;

        builder
            .apply_pre_execution_changes()
            .wrap_err("pre-execution changes failed for trial execution")?;

        let txs: Vec<reth_ethereum_primitives::TransactionSigned> =
            alloy_rlp::Decodable::decode(&mut encoded_transactions.as_ref())
                .wrap_err("failed to RLP-decode transactions for trial execution")?;

        for tx in txs {
            let recovered = SignedTransaction::try_into_recovered(tx)
                .map_err(|_| eyre::eyre!("failed to recover signer for trial execution tx"))?;
            // Ignore execution errors — some txs may fail (e.g., reverts)
            // but we still need to process subsequent txs.
            let _ = builder.execute_transaction(recovered);
        }

        let outcome = builder
            .finish(state_provider.as_ref())
            .wrap_err("block builder finish failed for trial execution")?;

        // Convert reth's EthereumReceipt<TxType, Log> to alloy_consensus::Receipt<Log>
        // via the From impl so identify_trigger_tx_indices can consume them.
        let receipts: Vec<alloy_consensus::Receipt<alloy_primitives::Log>> = outcome
            .execution_result
            .receipts
            .into_iter()
            .map(Into::into)
            .collect();

        Ok(receipts)
    }

    /// Compute generic intermediate state roots for a block with cross-chain entries.
    ///
    /// Trial-executes the full block to identify trigger txs (any tx producing
    /// `ExecutionConsumed` events from the CCM). Then computes R(k) for k = 0..T
    /// by filtering trigger txs and re-executing.
    ///
    /// Returns T+1 roots where:
    ///   roots[0] = R(0) = state with loadTable but without any triggers
    ///   roots[k] = R(k) = state with loadTable + first k triggers
    ///   roots[T] = speculative = state with all triggers
    ///
    /// The function is protocol-generic: it doesn't distinguish between entry types
    /// (L1→L2 calls, L2→L1 calls, continuations). All trigger types are identified
    /// uniformly via `ExecutionConsumed` events.
    pub(super) fn compute_intermediate_roots(
        &self,
        parent_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        speculative_root: B256,
        block_encoded_txs: &Bytes,
    ) -> Result<Vec<B256>> {
        // Step 1: Trial-execute the full block to get receipts
        let receipts = self.trial_execute_for_receipts(
            parent_block_number,
            timestamp,
            l1_block_hash,
            l1_block_number,
            block_encoded_txs,
        )?;

        // Step 2: Identify trigger tx indices via ExecutionConsumed events
        let trigger_indices = crate::cross_chain::identify_trigger_tx_indices(
            &receipts,
            self.config.cross_chain_manager_address,
        );

        // No triggers → clean IS speculative. Return [clean, speculative] (2 roots)
        // so that attach_generic_state_deltas can assign identity deltas to any
        // pending deferred entries. This happens when the L2 protocol tx reverts
        // (no ExecutionConsumed events) but the L1 deferred entries still need
        // correct state deltas for _findAndApplyExecution to match.
        if trigger_indices.is_empty() {
            return Ok(vec![speculative_root, speculative_root]);
        }

        let num_triggers = trigger_indices.len();
        let mut roots = Vec::with_capacity(num_triggers + 1);

        // Step 3: Compute R(k) for k = 0..num_triggers-1
        // R(k) = state root with loadTable + first k triggers (rest removed)
        for k in 0..num_triggers {
            let filtered = crate::cross_chain::filter_block_by_trigger_prefix(
                block_encoded_txs,
                &trigger_indices,
                k,
            )?;

            let root = self.compute_state_root_with_entries(
                parent_block_number,
                timestamp,
                l1_block_hash,
                l1_block_number,
                &filtered,
            )?;
            roots.push(root);
        }

        // Step 4: R(T) = speculative = full block = already known
        roots.push(speculative_root);

        Ok(roots)
    }
}
