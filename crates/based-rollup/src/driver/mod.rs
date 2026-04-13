//! Main orchestration loop for the based rollup node.
//!
//! Manages transitions between Sync, Builder, and Fullnode modes, drives the
//! Engine API, and coordinates derivation, block building, and L1 submission.

use crate::builder_sync::{BuilderSync, PreconfirmedBlock};
use crate::config::RollupConfig;
use crate::derivation::DerivationPipeline;
use crate::evm_config::RollupEvmConfig;
use crate::health::HealthStatus;
use crate::proposer::{PendingBlock, Proposer};
use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use eyre::{Result, WrapErr};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_provider::{
    BlockHashReader, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

/// Orchestrates the rollup node's main loop.
///
/// Manages mode switching between sync and builder modes, drives the
/// derivation pipeline, triggers payload building via the engine API,
/// and coordinates the proposer.
pub struct Driver<P, Pool> {
    config: Arc<RollupConfig>,
    derivation: DerivationPipeline,
    proposer: Option<Proposer>,
    mode: DriverMode,
    /// High-level handle to the consensus engine (forkchoiceUpdated, newPayload).
    engine: ConsensusEngineHandle<EthEngineTypes>,
    /// Custom EVM config for direct block building.
    evm_config: RollupEvmConfig,
    /// Recent block hashes for fork choice tracking.
    block_hashes: VecDeque<B256>,
    /// The latest L2 block hash (head of the chain).
    head_hash: B256,
    /// The latest L2 block number (0 = genesis).
    l2_head_number: u64,
    /// Primary L1 provider connection.
    l1_provider: RootProvider,
    /// Optional fallback L1 provider (used when primary fails repeatedly).
    l1_fallback_provider: Option<RootProvider>,
    /// Counts consecutive failures on primary (to trigger fallback switch) or
    /// consecutive successes on fallback (to switch back to primary).
    l1_consecutive_counter: u32,
    /// Whether we are currently using the fallback provider.
    using_fallback: bool,
    /// Last time an L1 RPC call was made (for rate limiting).
    last_l1_call: tokio::time::Instant,
    /// L2 database provider for checkpoint persistence and chain state queries.
    l2_provider: P,
    /// Last L1 block at which we persisted a checkpoint (avoids writing on every step).
    last_checkpointed_l1_block: u64,
    /// Receiver for preconfirmed blocks from the builder WS subscription.
    preconfirmed_rx: Option<mpsc::Receiver<PreconfirmedBlock>>,
    /// Preconfirmed block hashes by block number (for L1 verification).
    preconfirmed_hashes: HashMap<u64, B256>,
    /// Blocks built locally but not yet submitted to L1 (builder mode).
    pending_submissions: VecDeque<PendingBlock>,
    /// Timestamp of last L1 submission failure (for cooldown).
    last_submission_failure: Option<std::time::Instant>,
    /// Sender for health status updates (consumed by the health HTTP server).
    health_status_tx: watch::Sender<HealthStatus>,
    /// Handle for the builder sync (WS preconfirmation) background task.
    builder_sync_handle: Option<tokio::task::JoinHandle<()>>,
    /// If set, the chain should be rewound to this L2 block before the next step.
    /// Set when `verify_local_block_matches_l1` detects a state root or L1 context mismatch.
    pending_rewind_target: Option<u64>,
    /// Entry verification hold — the state machine governing
    /// "builder halts + submissions pause while an entry-bearing
    /// block awaits derivation verification". See
    /// [`EntryVerificationHold`] for the full lifecycle. Closes
    /// invariants #1 (partial) and #14.
    hold: EntryVerificationHold,
    /// Last time we saw a new L1 block (for stall detection).
    last_new_l1_block_time: std::time::Instant,
    /// The most recent L1 block number we've seen.
    last_seen_l1_block: u64,
    /// Number of consecutive Builder→Sync rewind cycles (for cycle dampening).
    consecutive_rewind_cycles: u32,
    /// Last time the builder wallet balance was checked (avoids checking every flush).
    last_balance_check: std::time::Instant,
    /// Shared sync status flag (true when caught up, readable by RPC handlers).
    synced: Arc<std::sync::atomic::AtomicBool>,
    /// Unified queue for cross-chain calls (entry pairs + gas price + raw L1 tx).
    /// The RPC pushes here; the driver drains, sorts by gas price, then submits.
    queued_cross_chain_calls: crate::entry_queue::EntryQueue<crate::rpc::QueuedCrossChainCall>,
    /// Legacy queue for raw signed L1 transactions to forward after `postBatch`.
    /// Kept for backward compatibility with `queueL1ForwardTx` RPC method.
    pending_l1_forward_txs: Arc<std::sync::Mutex<Vec<Bytes>>>,
    /// Queue for L2→L1 calls. The RPC pushes here; the driver drains
    /// into builder_execution_entries alongside L1→L2 entries (unified intermediate roots).
    queued_l2_to_l1_calls: crate::entry_queue::EntryQueue<crate::rpc::QueuedL2ToL1Call>,
    /// Pending L1 deferred entries + their trigger groups, as a
    /// single atomic unit. See [`PendingL1SubmissionQueue`] for the
    /// structural rationale (closes invariant #11).
    pending_l1: PendingL1SubmissionQueue,
    /// The L2 head at the time of the last health status update (for staleness tracking).
    prev_health_l2_head: u64,
    /// Timestamp of the last time `l2_head` advanced (for health staleness check).
    last_l2_head_advance: std::time::Instant,
    /// Number of consecutive pre_state_root mismatches in flush_to_l1.
    /// When this exceeds a threshold, the builder forces a rewind to re-derive.
    consecutive_flush_mismatches: u32,
    /// Last L1-confirmed batch anchor for efficient rollback.
    l1_confirmed_anchor: Option<L1ConfirmedAnchor>,
    /// Builder's L2 nonce for signing protocol transactions.
    builder_l2_nonce: u64,
    // Intermediate root checking in the driver was removed — derivation handles
    // unconsumed entries via §4f protocol tx filtering. See docs/DERIVATION.md §4f.
    /// Blocks at or below this number are permanently committed in reth and cannot
    /// be unwound via FCU. Verification mismatches for these blocks are logged as
    /// warnings but do NOT trigger rewinding (which would be futile). Set when
    /// `rewind_l2_chain` detects that FCU didn't unwind committed blocks.
    immutable_block_ceiling: u64,
    /// Transaction pool for draining user transactions into builder blocks.
    pool: Pool,
    /// In-memory transaction replay journal. Written to DB after each block
    /// build, read on startup for crash recovery. Pruned after L1 confirmation.
    tx_journal: Vec<TxJournalEntry>,
    /// Transactions awaiting deferred re-injection into the pool after a rewind.
    /// Set during rewind, consumed on the next step() iteration. This avoids
    /// the TOCTOU race with reth's async CanonStateNotification processing.
    pending_reinjection: Vec<(
        alloy_primitives::Address,
        reth_ethereum_primitives::TransactionSigned,
    )>,
}

/// Result of building and inserting a block via the engine API.
mod build;
mod flush;
mod flush_plan;
mod hold;
mod journal;
mod pending_queue;
mod protocol_txs;
mod rewind;
mod step_builder;
mod types;
mod verify;
pub use flush_plan::{
    Collected, FlushPlan, HoldArmed, NoEntries, RollbackPackage, SendResult, Sendable,
};
pub use hold::{DeferralResult, EntryVerificationHold, MAX_ENTRY_VERIFY_DEFERRALS};
pub use pending_queue::{
    BlockEntryMix, PendingL1Group, PendingL1SubmissionQueue, TriggerMetadata,
};
pub use types::{BuiltBlock, DriverMode};
use types::{
    CHECKPOINT_INTERVAL, FORK_CHOICE_DEPTH, L1ConfirmedAnchor, MAX_BACKOFF_SECS,
    MAX_CONSECUTIVE_FAILURES, MIN_L1_CALL_INTERVAL, TxJournalEntry,
};

// Test-only re-exports: `driver_tests.rs` uses `use super::*;` and references
// these items directly. They are pulled into scope at the module level only
// under `#[cfg(test)]` so the release build does not flag them as unused.
#[cfg(test)]
use crate::cross_chain::CrossChainExecutionEntry;
#[cfg(test)]
use types::{
    DESIRED_GAS_LIMIT, FCU_SYNCING_INITIAL_BACKOFF_MS, FCU_SYNCING_MAX_RETRIES, MAX_BATCH_SIZE,
    MAX_PENDING_SUBMISSIONS, SUBMISSION_COOLDOWN_SECS, calc_gas_limit, compute_forkchoice_state,
    encode_block_transactions,
};

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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<RollupConfig>,
        engine: ConsensusEngineHandle<EthEngineTypes>,
        evm_config: RollupEvmConfig,
        genesis_hash: B256,
        l1_provider: RootProvider,
        l2_provider: P,
        pool: Pool,
        synced: Arc<std::sync::atomic::AtomicBool>,
        queued_cross_chain_calls: crate::entry_queue::EntryQueue<crate::rpc::QueuedCrossChainCall>,
        pending_l1_forward_txs: Arc<std::sync::Mutex<Vec<Bytes>>>,
        queued_l2_to_l1_calls: crate::entry_queue::EntryQueue<crate::rpc::QueuedL2ToL1Call>,
    ) -> (Self, watch::Receiver<HealthStatus>) {
        let derivation = DerivationPipeline::new(config.clone());
        let proposer = if config.builder_mode && config.builder_private_key.is_some() {
            match Proposer::new(config.clone()) {
                Ok(p) => Some(p),
                Err(err) => {
                    warn!(
                        target: "based_rollup::driver",
                        %err,
                        "failed to initialize proposer, blocks won't be submitted to L1"
                    );
                    None
                }
            }
        } else {
            None
        };

        let l1_fallback_provider =
            config
                .l1_rpc_url_fallback
                .as_ref()
                .and_then(|url| match url.parse() {
                    Ok(parsed) => {
                        info!(
                            target: "based_rollup::driver",
                            fallback_url = %url,
                            "fallback L1 RPC provider configured"
                        );
                        Some(RootProvider::new_http(parsed))
                    }
                    Err(err) => {
                        warn!(
                            target: "based_rollup::driver",
                            %err, fallback_url = %url,
                            "invalid fallback L1 RPC URL — ignoring fallback provider"
                        );
                        None
                    }
                });

        let mut block_hashes = VecDeque::with_capacity(FORK_CHOICE_DEPTH + 1);
        block_hashes.push_back(genesis_hash);

        let (health_status_tx, health_status_rx) = watch::channel(HealthStatus::default());

        let driver = Self {
            config,
            derivation,
            proposer,
            mode: DriverMode::Sync,
            engine,
            evm_config,
            block_hashes,
            head_hash: genesis_hash,
            l2_head_number: 0,
            l1_provider,
            l1_fallback_provider,
            l1_consecutive_counter: 0,
            using_fallback: false,
            last_l1_call: tokio::time::Instant::now(),
            l2_provider,
            last_checkpointed_l1_block: 0,
            preconfirmed_rx: None,
            preconfirmed_hashes: HashMap::new(),
            pending_submissions: VecDeque::new(),
            last_submission_failure: None,
            health_status_tx,
            builder_sync_handle: None,
            pending_rewind_target: None,
            hold: EntryVerificationHold::Clear,
            last_new_l1_block_time: std::time::Instant::now(),
            last_seen_l1_block: 0,
            consecutive_rewind_cycles: 0,
            last_balance_check: std::time::Instant::now(),
            synced,
            queued_cross_chain_calls,
            pending_l1_forward_txs,
            queued_l2_to_l1_calls,
            pending_l1: PendingL1SubmissionQueue::default(),
            prev_health_l2_head: 0,
            last_l2_head_advance: std::time::Instant::now(),
            consecutive_flush_mismatches: 0,
            l1_confirmed_anchor: None,
            builder_l2_nonce: 0,
            immutable_block_ceiling: 0,
            pool,
            tx_journal: Vec::new(),
            pending_reinjection: Vec::new(),
        };
        (driver, health_status_rx)
    }

    pub fn mode(&self) -> DriverMode {
        self.mode
    }

    /// Returns the active L1 provider, switching between primary and fallback
    /// based on consecutive failure counts.
    ///
    /// After `MAX_CONSECUTIVE_FAILURES` on the current provider, switches to
    /// the other one. If no fallback is configured, always returns primary.
    pub(super) fn get_l1_provider(&self) -> &RootProvider {
        if self.using_fallback {
            // Safety: using_fallback is only set to true when fallback exists
            self.l1_fallback_provider
                .as_ref()
                .unwrap_or(&self.l1_provider)
        } else {
            &self.l1_provider
        }
    }

    /// Record a successful L1 RPC call. After `MAX_CONSECUTIVE_FAILURES`
    /// successes on the fallback, switches back to the primary.
    fn record_l1_success(&mut self) {
        if self.using_fallback {
            self.l1_consecutive_counter = self.l1_consecutive_counter.saturating_add(1);
            if self.l1_consecutive_counter >= MAX_CONSECUTIVE_FAILURES {
                info!(
                    target: "based_rollup::driver",
                    "fallback L1 provider succeeded {} times, switching back to primary",
                    MAX_CONSECUTIVE_FAILURES
                );
                self.using_fallback = false;
                self.l1_consecutive_counter = 0;
                self.sync_proposer_l1_url();
            }
        } else {
            self.l1_consecutive_counter = 0;
        }
    }

    /// Record a failed L1 RPC call. After `MAX_CONSECUTIVE_FAILURES` failures
    /// on the primary, switches to the fallback (if configured).
    fn record_l1_failure(&mut self) {
        if self.using_fallback {
            // Already on fallback, reset success counter
            self.l1_consecutive_counter = 0;
        } else {
            self.l1_consecutive_counter = self.l1_consecutive_counter.saturating_add(1);
            if self.l1_consecutive_counter >= MAX_CONSECUTIVE_FAILURES
                && self.l1_fallback_provider.is_some()
            {
                warn!(
                    target: "based_rollup::driver",
                    consecutive_failures = self.l1_consecutive_counter,
                    "primary L1 RPC failed {} times, switching to fallback",
                    MAX_CONSECUTIVE_FAILURES
                );
                self.using_fallback = true;
                self.l1_consecutive_counter = 0;
                self.sync_proposer_l1_url();
            }
        }
    }

    /// Keep the proposer's L1 provider in sync with the driver's active
    /// provider (primary vs fallback). Without this, the proposer would
    /// keep submitting to a dead L1 endpoint while the driver reads from
    /// the healthy fallback.
    fn sync_proposer_l1_url(&mut self) {
        let target_url = if self.using_fallback {
            self.config
                .l1_rpc_url_fallback
                .as_deref()
                .unwrap_or(&self.config.l1_rpc_url)
        } else {
            &self.config.l1_rpc_url
        };

        if let Some(proposer) = &mut self.proposer {
            if let Err(err) = proposer.switch_l1_url(target_url) {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    "failed to switch proposer L1 URL"
                );
            } else {
                // Reset balance check timer so we verify the wallet balance
                // on the new RPC promptly (different network/gas prices).
                self.last_balance_check =
                    std::time::Instant::now() - std::time::Duration::from_secs(301);
            }
        }
    }

    /// Recover head_hash, l2_head_number, and block_hashes from the L2 chain on startup.
    ///
    /// Without this, after a restart the driver would think head is genesis,
    /// causing invalid forkchoice updates.
    fn recover_chain_state(&mut self) -> Result<()> {
        let tip = self
            .l2_provider
            .last_block_number()
            .wrap_err("failed to read last block number")?;

        if tip == 0 {
            return Ok(());
        }

        let tip_hash = self
            .l2_provider
            .block_hash(tip)
            .wrap_err("failed to read tip block hash")?
            .ok_or_else(|| {
                eyre::eyre!("tip block {tip} has no hash in DB — possible DB corruption")
            })?;

        self.head_hash = tip_hash;
        self.l2_head_number = tip;
        self.block_hashes.clear();

        let start = tip.saturating_sub(FORK_CHOICE_DEPTH as u64);
        for n in start..=tip {
            match self.l2_provider.block_hash(n) {
                Ok(Some(hash)) => self.block_hashes.push_back(hash),
                Ok(None) => {
                    warn!(
                        target: "based_rollup::driver",
                        block_number = n,
                        "missing block hash in DB during chain state recovery"
                    );
                }
                Err(e) => return Err(eyre::eyre!("failed to read block hash for {n}: {e}")),
            }
        }

        info!(
            target: "based_rollup::driver",
            tip,
            %tip_hash,
            tracked_hashes = self.block_hashes.len(),
            "recovered chain state from L2 DB"
        );

        Ok(())
    }

    /// Run the driver main loop.
    pub async fn run(&mut self) -> Result<()> {
        // Recover L2 chain state (head_hash, block_hashes) from DB
        self.recover_chain_state()?;

        // Load L1-confirmed anchor for efficient rollback
        self.load_l1_confirmed_anchor();

        // Load transaction replay journal for crash recovery
        self.load_tx_journal();

        // Load L1 derivation checkpoint from DB to resume where we left off
        self.derivation.load_checkpoint(&self.l2_provider)?;
        self.derivation
            .set_last_derived_l2_block(self.l2_head_number);
        self.last_checkpointed_l1_block = self.derivation.last_processed_l1_block();

        // Rebuild the reorg-detection cursor from L2 headers so we can detect
        // L1 reorgs that occurred while the node was offline (ISSUE-107).
        self.derivation
            .rebuild_cursor_from_headers(&self.l2_provider, self.l2_head_number)?;

        // Fetch deployment L1 block hash for gap-fill blocks (one-time at startup)
        if let Ok(Some(deploy_block)) = self
            .get_l1_provider()
            .get_block_by_number(self.config.deployment_l1_block.into())
            .await
        {
            self.derivation
                .set_deployment_l1_block_hash(deploy_block.header.hash);
        }

        // Spawn builder sync if configured (for fullnode preconfirmation)
        if let Some(ws_url) = &self.config.builder_ws_url {
            let (tx, rx) = mpsc::channel(64);
            self.preconfirmed_rx = Some(rx);

            let sync = BuilderSync::new(ws_url.clone());
            let handle = tokio::spawn(async move {
                if let Err(e) = sync.run(tx).await {
                    warn!(
                        target: "based_rollup::driver",
                        error = %e,
                        "builder sync task exited"
                    );
                }
            });
            self.builder_sync_handle = Some(handle);
        }

        info!(
            target: "based_rollup::driver",
            mode = ?self.mode,
            l2_head = self.l2_head_number,
            %self.head_hash,
            last_processed_l1 = self.derivation.last_processed_l1_block(),
            "starting driver"
        );

        // Verify Rollups contract is deployed before entering the main loop
        let rollups_code = self
            .get_l1_provider()
            .get_code_at(self.config.rollups_address)
            .await
            .wrap_err("failed to query Rollups contract code")?;
        if rollups_code.is_empty() && !self.config.rollups_address.is_zero() {
            return Err(eyre::eyre!(
                "no contract deployed at Rollups address {}",
                self.config.rollups_address
            ));
        }

        let mut interval = time::interval(Duration::from_secs(1));
        let mut consecutive_errors: u32 = 0;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match self.step().await {
                        Ok(()) => {
                            consecutive_errors = 0;
                            self.record_l1_success();
                        }
                        Err(err) => {
                            consecutive_errors = consecutive_errors.saturating_add(1);
                            self.record_l1_failure();
                            let backoff_secs = (1u64 << consecutive_errors.min(6)).min(MAX_BACKOFF_SECS);
                            error!(
                                target: "based_rollup::driver",
                                %err,
                                consecutive_errors,
                                backoff_secs,
                                "driver step failed, backing off"
                            );
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!(
                        target: "based_rollup::driver",
                        "received shutdown signal, saving checkpoint"
                    );
                    if let Some(handle) = self.builder_sync_handle.take() {
                        handle.abort();
                        debug!(
                            target: "based_rollup::driver",
                            "aborted builder sync task"
                        );
                    }
                    self.derivation.save_checkpoint(&self.l2_provider)?;
                    return Ok(());
                }
            }
        }
    }

    /// Execute a single driver step.
    async fn step(&mut self) -> Result<()> {
        // Rate-limit L1 RPC calls to avoid hammering during catchup.
        let elapsed = self.last_l1_call.elapsed();
        if elapsed < MIN_L1_CALL_INTERVAL {
            return Ok(());
        }

        // Deferred re-injection: process transactions saved from a previous rewind.
        // By now (~12s later), reth's pool maintenance has fully processed the
        // CanonStateNotification from the FCU rewind, so pool nonces are correct
        // and re-injection won't race with async maintenance.
        if !self.pending_reinjection.is_empty() {
            self.reinject_pending_transactions().await;
        }

        // Handle pending rewind from state root mismatch detection.
        // This must be async (rewind_l2_chain calls engine API) so it can't
        // happen inside the sync verify_local_block_matches_l1 method.
        if let Some(target) = self.pending_rewind_target.take() {
            if target < self.l2_head_number {
                let old_head = self.l2_head_number;
                info!(
                    target: "based_rollup::driver",
                    current_head = old_head,
                    rewind_target = target,
                    "rewinding L2 chain to re-derive with correct L1 context"
                );

                // Collect user transactions from blocks to be reverted BEFORE
                // the FCU rewind removes them from the canonical chain.
                let reverted_user_txs =
                    self.collect_reverted_user_transactions(target + 1, old_head);

                // Clear all pending state before rewind — entries from the
                // old fork are stale and must not be used during re-derivation.
                self.clear_internal_state();
                match self.rewind_l2_chain(target).await {
                    Ok(()) => {
                        // After rewind, the builder's L2 nonce must be re-read
                        // from chain state — the old nonce is stale.
                        self.recover_builder_l2_nonce();

                        // Defer pool re-injection to the next step() iteration.
                        // By then, reth's async CanonStateNotification::Reorg
                        // handler will have updated pool nonces, eliminating
                        // the TOCTOU race that causes transaction loss.
                        if self.l2_head_number <= target {
                            self.pending_reinjection.extend(reverted_user_txs);
                        }
                    }
                    Err(err) => {
                        // Restore the rewind target so it can be retried on the
                        // next step() call instead of being silently lost.
                        self.pending_rewind_target = Some(target);
                        return Err(err);
                    }
                }
            }
        }

        // Drain any preconfirmed blocks from the builder WS
        self.drain_preconfirmed_blocks();

        let l1_provider = self.get_l1_provider().clone();
        let latest_l1_block = l1_provider.get_block_number().await?;
        self.last_l1_call = tokio::time::Instant::now();

        // L1 stall detection
        if latest_l1_block > self.last_seen_l1_block {
            self.last_seen_l1_block = latest_l1_block;
            self.last_new_l1_block_time = std::time::Instant::now();
        } else if self.last_new_l1_block_time.elapsed().as_secs() > 60 {
            warn!(
                target: "based_rollup::driver",
                last_l1_block = self.last_seen_l1_block,
                stalled_secs = self.last_new_l1_block_time.elapsed().as_secs(),
                "L1 appears stalled — no new block in >60s"
            );
        }

        // Check for L1 reorgs
        if let Some(fork_point) = self.derivation.detect_reorg(&l1_provider).await? {
            warn!(
                target: "based_rollup::driver",
                fork_point,
                "L1 reorg detected, rolling back"
            );
            let last_valid_l2 = self.derivation.rollback_to(fork_point);
            // Persist the rolled-back state so we don't re-derive stale blocks on crash
            self.derivation.save_checkpoint(&self.l2_provider)?;
            self.last_checkpointed_l1_block = self.derivation.last_processed_l1_block();

            // Rewind the L2 chain so reth unwinds blocks built on the old fork
            let rewind_target = last_valid_l2.unwrap_or(0);
            if rewind_target < self.l2_head_number {
                let old_head = self.l2_head_number;
                // Collect user transactions BEFORE rewind while blocks are canonical
                let reverted_user_txs =
                    self.collect_reverted_user_transactions(rewind_target + 1, old_head);

                self.clear_internal_state();
                self.rewind_l2_chain(rewind_target).await?;

                if self.l2_head_number <= rewind_target {
                    self.pending_reinjection.extend(reverted_user_txs);
                }
            } else {
                self.clear_internal_state();
            }

            self.mode = DriverMode::Sync;
            self.synced
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }

        match self.mode {
            DriverMode::Sync => self.step_sync(latest_l1_block).await?,
            DriverMode::Builder => self.step_builder(latest_l1_block).await?,
            DriverMode::Fullnode => self.step_fullnode(latest_l1_block).await?,
        }

        // Track when l2_head advances for staleness detection
        if self.l2_head_number > self.prev_health_l2_head {
            self.last_l2_head_advance = std::time::Instant::now();
            self.prev_health_l2_head = self.l2_head_number;
        }

        // Update health status for the HTTP endpoint
        let _ = self.health_status_tx.send(HealthStatus {
            mode: format!("{:?}", self.mode),
            l2_head: self.l2_head_number,
            l1_derivation_head: self.derivation.last_processed_l1_block(),
            pending_submissions: self.pending_submissions.len(),
            consecutive_rewind_cycles: self.consecutive_rewind_cycles,
            last_l2_head_advance: Some(self.last_l2_head_advance),
        });

        Ok(())
    }

    /// Drain preconfirmed blocks from the builder WS channel.
    ///
    /// Also prunes entries that are far behind the current head to prevent
    /// unbounded memory growth (ISSUE-105).
    ///
    /// Design note: The current implementation only records block hashes for
    /// later verification, not for immediate block building. This is intentional
    /// — the fullnode builds blocks independently from L1 derivation and only
    /// uses preconfirmation hashes to detect divergence early (i.e., if the
    /// locally-derived block hash differs from the builder's preconfirmed hash,
    /// the fullnode knows something is wrong before waiting for L1 finality).
    fn drain_preconfirmed_blocks(&mut self) {
        let Some(rx) = &mut self.preconfirmed_rx else {
            return;
        };
        while let Ok(block) = rx.try_recv() {
            // Reject preconfirmations too far ahead of current head
            if block.block_number > self.l2_head_number.saturating_add(1000) {
                warn!(
                    target: "based_rollup::driver",
                    block_number = block.block_number,
                    head = self.l2_head_number,
                    "ignoring preconfirmation far ahead of head"
                );
                continue;
            }
            debug!(
                target: "based_rollup::driver",
                block_number = block.block_number,
                block_hash = %block.block_hash,
                "received preconfirmed block from builder"
            );
            self.preconfirmed_hashes
                .insert(block.block_number, block.block_hash);
        }

        // Prune stale entries more than 1000 blocks behind head
        if self.preconfirmed_hashes.len() > 1000 {
            let cutoff = self.l2_head_number.saturating_sub(1000);
            self.preconfirmed_hashes.retain(|&k, _| k >= cutoff);
        }
    }

    /// Conditionally save checkpoint if enough L1 blocks have been processed
    /// since the last save. Also prunes finalized cursor entries.
    fn maybe_save_checkpoint(&mut self) -> Result<()> {
        let current = self.derivation.last_processed_l1_block();
        if current.saturating_sub(self.last_checkpointed_l1_block) >= CHECKPOINT_INTERVAL {
            // Prune cursor entries that are finalized (>128 blocks behind tip).
            // Conservative: L1 finality is ~13 min (~64 blocks), use 128 for safety.
            let finalized_l1 = current.saturating_sub(128);
            self.derivation.prune_finalized(finalized_l1);

            self.derivation.save_checkpoint(&self.l2_provider)?;
            self.last_checkpointed_l1_block = current;
        }
        Ok(())
    }

    /// Sync mode: derive blocks from L1 until caught up.
    async fn step_sync(&mut self, latest_l1_block: u64) -> Result<()> {
        let provider = self.get_l1_provider().clone();
        let batch = self
            .derivation
            .derive_next_batch(latest_l1_block, &provider)
            .await?;

        if batch.blocks.is_empty() {
            // Only switch modes when the derivation pipeline has actually scanned
            // all L1 blocks up to latest_l1_block. With MAX_LOG_RANGE pagination,
            // an empty result may just mean no events in the current chunk — not
            // that the node is fully caught up.
            //
            // Even when no blocks were derived, we still need to commit the batch
            // to advance last_processed_l1_block past the scanned range. Without
            // this, the pipeline would re-scan the same empty range every tick.
            //
            // However, if a rewind is pending, do NOT advance the cursor — blocks
            // in this range need to be re-derived after the rewind completes.
            if self.pending_rewind_target.is_some() {
                return Ok(());
            }
            self.derivation.commit_batch(&batch);
            let fully_caught_up = self.derivation.last_processed_l1_block() >= latest_l1_block;
            if fully_caught_up && self.mode == DriverMode::Sync {
                if self.config.builder_mode {
                    // Dampen Builder→Sync→Builder cycles: if we've been rewinding
                    // repeatedly, delay re-entering Builder mode.
                    if self.consecutive_rewind_cycles > 0 {
                        let delay = (2u64 << self.consecutive_rewind_cycles.min(5)).min(60);
                        warn!(
                            target: "based_rollup::driver",
                            cycles = self.consecutive_rewind_cycles,
                            delay_secs = delay,
                            "delaying builder mode re-entry after repeated rewinds"
                        );
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                    }
                    // Do NOT reset consecutive_rewind_cycles here — flush_to_l1 needs
                    // this counter to detect futile rewind loops where the pre_state_root
                    // mismatch is permanent (entry bridge tx reverted on L1). The counter
                    // is reset here (successful sync→builder transition means the rewind
                    // episode is complete) and by flush_to_l1 when a successful flush
                    // confirms no mismatch.
                    info!(target: "based_rollup::driver", "caught up to L1, switching to builder mode");
                    self.clear_internal_state();
                    self.consecutive_rewind_cycles = 0;
                    self.recover_builder_l2_nonce();
                    self.mode = DriverMode::Builder;
                    self.synced
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                } else {
                    self.consecutive_rewind_cycles = 0;
                    info!(target: "based_rollup::driver", "caught up to L1, switching to fullnode mode");
                    self.clear_internal_state();
                    self.mode = DriverMode::Fullnode;
                    self.synced
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            return Ok(());
        }

        for block in &batch.blocks {
            // Skip blocks we already have
            if block.l2_block_number <= self.l2_head_number {
                continue;
            }

            // §4f deferred filtering: if derivation flagged this block as needing
            // filtering, apply it now using receipt-based L2→L1 tx identification.
            let effective_transactions = self.apply_deferred_filtering(block)?;

            let built = self
                .build_and_insert_block(
                    block.l2_block_number,
                    block.l2_timestamp,
                    block.l1_info.l1_block_hash,
                    block.l1_info.l1_block_number,
                    &effective_transactions,
                )
                .await?;

            info!(
                target: "based_rollup::driver",
                l2_block = block.l2_block_number,
                block_hash = %built.hash,
                l1_block = block.l1_info.l1_block_number,
                is_empty = block.is_empty,
                execution_entries = block.execution_entries.len(),
                "derived and inserted L2 block"
            );
        }

        // All blocks built successfully — commit the cursor state.
        // Do NOT commit if a rewind is pending — the cursor must stay so blocks
        // are re-derived after the rewind completes.
        if self.pending_rewind_target.is_none() {
            self.derivation.commit_batch(&batch);
            self.maybe_save_checkpoint()?;
        }

        Ok(())
    }

    /// Fullnode mode: derive from L1, verify against preconfirmed blocks.
    async fn step_fullnode(&mut self, latest_l1_block: u64) -> Result<()> {
        let provider = self.get_l1_provider().clone();
        let batch = self
            .derivation
            .derive_next_batch(latest_l1_block, &provider)
            .await?;

        if batch.blocks.is_empty() {
            // Commit even when empty to advance last_processed_l1_block.
            // But do NOT commit if a rewind is pending — the cursor must stay
            // so blocks are re-derived after the rewind completes.
            if self.pending_rewind_target.is_none() {
                self.derivation.commit_batch(&batch);
            }
            return Ok(());
        }

        for block in &batch.blocks {
            // We already have this block — verify it matches L1 before skipping.
            if block.l2_block_number <= self.l2_head_number {
                self.verify_local_block_matches_l1(block)?;
                continue;
            }

            // §4f deferred filtering: apply receipt-based filtering if needed.
            let effective_transactions = self.apply_deferred_filtering(block)?;

            let built = self
                .build_and_insert_block(
                    block.l2_block_number,
                    block.l2_timestamp,
                    block.l1_info.l1_block_hash,
                    block.l1_info.l1_block_number,
                    &effective_transactions,
                )
                .await?;

            // Check against preconfirmed block from builder WS
            if let Some(preconfirmed_hash) = self.preconfirmed_hashes.remove(&block.l2_block_number)
            {
                if preconfirmed_hash == built.hash {
                    // Hash match implies identical L1 context, since L1 block
                    // number/hash are embedded in the header (prev_randao /
                    // parent_beacon_block_root) and affect the block hash.
                    info!(
                        target: "based_rollup::driver",
                        l2_block = block.l2_block_number,
                        block_hash = %built.hash,
                        l1_context_block = block.l1_info.l1_block_number,
                        l1_context_hash = %block.l1_info.l1_block_hash,
                        "L1-confirmed: preconfirmed block matches L1 derivation (L1 context verified)"
                    );
                } else {
                    // Hash mismatch — the builder may have used a different L1
                    // context block. Log derived L1 context for diagnosis.
                    warn!(
                        target: "based_rollup::driver",
                        l2_block = block.l2_block_number,
                        l1_derived_hash = %built.hash,
                        preconfirmed_hash = %preconfirmed_hash,
                        derived_l1_context_block = block.l1_info.l1_block_number,
                        derived_l1_context_hash = %block.l1_info.l1_block_hash,
                        "preconfirmed block MISMATCH — L1 derivation takes precedence; builder may have used different L1 context"
                    );
                }
            } else {
                info!(
                    target: "based_rollup::driver",
                    l2_block = block.l2_block_number,
                    block_hash = %built.hash,
                    l1_block = block.l1_info.l1_block_number,
                    is_empty = block.is_empty,
                    "derived and inserted L2 block (no preconfirmation)"
                );
            }
        }

        // All blocks processed successfully — commit the cursor state.
        // Do NOT commit if a rewind is pending — the cursor must stay so blocks
        // are re-derived after the rewind completes.
        if self.pending_rewind_target.is_none() {
            self.derivation.commit_batch(&batch);
            self.maybe_save_checkpoint()?;
        }

        Ok(())
    }

    pub fn derivation(&self) -> &DerivationPipeline {
        &self.derivation
    }

    pub fn derivation_mut(&mut self) -> &mut DerivationPipeline {
        &mut self.derivation
    }
}

#[cfg(test)]
#[path = "../driver_tests.rs"]
mod tests;
