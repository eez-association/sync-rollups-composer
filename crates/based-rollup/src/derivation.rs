//! L2 block derivation from L1 BatchPosted events.
//!
//! Reads `BatchPosted` events from the L1 Rollups contract, decodes the
//! `postBatch` calldata to extract block numbers and transactions, and
//! produces `DerivedBlock`s. Includes L1 reorg detection and rollback.

use crate::config::RollupConfig;
use crate::cross_chain::{self, ConsumedMap, CrossChainExecutionEntry};
use crate::payload_builder::L1BlockInfo;
use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types::Filter;
use eyre::{Result, WrapErr};
use reth_provider::{
    DBProvider, DatabaseProviderFactory, HeaderProvider, StageCheckpointReader,
    StageCheckpointWriter,
};
use reth_stages_types::{StageCheckpoint, StageId};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Maximum number of recent cursor entries to check for L1 reorgs.
const REORG_CHECK_DEPTH: usize = 64;

/// Maximum number of L1 blocks to query in a single `get_logs` call.
/// Prevents exceeding RPC provider limits after extended downtime.
const MAX_LOG_RANGE: u64 = 2000;

/// Maximum allowed gap between consecutive submitted block numbers.
/// Prevents DoS via a malicious L1 transaction that submits a far-future
/// block number, forcing all nodes to generate millions of gap-fill empty blocks.
const MAX_BLOCK_GAP: u64 = 1000;

/// Metadata stored per derived L2 block for reorg detection.
#[derive(Debug, Clone)]
pub struct DerivedBlockMeta {
    pub l2_block_number: u64,
    pub l1_block_number: u64,
    pub l1_block_hash: B256,
}

/// A block derived from L1 data, ready for execution.
#[derive(Debug, Clone)]
pub struct DerivedBlock {
    pub l2_block_number: u64,
    pub l2_timestamp: u64,
    pub l1_info: L1BlockInfo,
    /// State root submitted by the builder (for verification).
    pub state_root: B256,
    /// Transactions from the postBatch callData (may be unfiltered if
    /// `filtering` is `Some` — the driver applies §4f filtering using
    /// receipt-based L2→L1 tx identification before execution).
    pub transactions: Bytes,
    /// Whether this was an empty submission (no transactions).
    pub is_empty: bool,
    /// Cross-chain execution entries to load into CrossChainManagerL2.
    pub execution_entries: Vec<CrossChainExecutionEntry>,
    /// Deferred §4f filtering metadata — present when unconsumed entries
    /// require the driver to filter protocol txs before execution.
    /// `None` means no filtering needed (all entries consumed or no entries).
    pub filtering: Option<DeferredFiltering>,
}

/// Metadata for §4f protocol tx filtering, computed by derivation from L1 data.
///
/// The driver uses this with generic event-based filtering: trial-execute the
/// full block, identify trigger txs via `ExecutionConsumed` events from the CCM,
/// compute consumed trigger prefix using the L1 consumed map, and filter to keep
/// only consumed triggers. This is fully protocol-generic — no dependency on
/// entry type classification.
#[derive(Debug, Clone)]
pub struct DeferredFiltering {
    /// L1 consumed map snapshot: actionHash → remaining consumption count.
    ///
    /// The driver uses this for generic event-based §4f filtering:
    /// trial-execute + `ExecutionConsumed` events + prefix counting.
    ///
    /// The snapshot is taken BEFORE the current batch's entries are consumed
    /// from `remaining`, so the driver can independently determine which
    /// triggers were consumed using the same FIFO semantics as L1.
    pub l1_consumed_remaining: std::collections::HashMap<B256, usize>,

    /// All L2 execution entries from this batch (unfiltered).
    ///
    /// Used by the driver for generic event-based §4f filtering via
    /// `build_builder_protocol_txs`. When non-empty, the driver can rebuild
    /// the block from entries instead of parsing/filtering raw encoded
    /// transaction bytes. Falls back to `filter_block_by_trigger_prefix` on
    /// the raw `DerivedBlock.transactions` when empty or when a proposer
    /// (signer) is not available (fullnode/sync mode).
    pub all_l2_entries: Vec<crate::cross_chain::CrossChainExecutionEntry>,
}

/// A batch of derived blocks together with the cursor state that should be
/// committed after all blocks are successfully built and inserted.
///
/// The derivation pipeline returns this from `derive_next_batch` WITHOUT
/// advancing its own cursors. The caller must invoke `commit_batch` after
/// processing all blocks successfully.
#[derive(Debug, Clone)]
pub struct DerivedBatch {
    pub blocks: Vec<DerivedBlock>,
    /// Cursor snapshot to commit after successful processing.
    pub(crate) cursor_update: CursorUpdate,
}

/// Internal cursor state captured during `derive_next_batch` that should only
/// be applied to the pipeline after all blocks are successfully processed.
#[derive(Debug, Clone)]
pub(crate) struct CursorUpdate {
    pub last_processed_l1_block: u64,
    pub last_execution_l1_block: u64,
    pub last_derived_l2_block: u64,
    pub last_l1_info: Option<L1BlockInfo>,
    pub new_cursor_entries: Vec<DerivedBlockMeta>,
}

/// Stage ID used to persist the L1 derivation checkpoint in reth's database.
pub const L1_DERIVATION_STAGE_ID: StageId = StageId::Other("L1Derivation");

/// Stage ID for persisting the last L1 block from which execution entries were fetched.
pub const L1_EXECUTION_STAGE_ID: StageId = StageId::Other("L1Execution");

/// Stage ID for persisting the last L1-confirmed L2 block number (anchor).
pub const L1_CONFIRMED_L2_STAGE_ID: StageId = StageId::Other("L1ConfirmedL2");

/// Stage ID for persisting the L1 block number of the last confirmed batch (anchor).
pub const L1_CONFIRMED_L1_STAGE_ID: StageId = StageId::Other("L1ConfirmedL1");

/// Derives L2 blocks by reading BatchPosted events from L1.
///
/// For each `BatchPosted` event, the L1 transaction's `postBatch` calldata is
/// decoded to extract block numbers and transactions. Immediate entries
/// (actionHash == 0) provide state roots; deferred entries (actionHash != 0)
/// are cross-chain execution entries assigned to the first block in the batch.
pub struct DerivationPipeline {
    config: Arc<RollupConfig>,
    last_processed_l1_block: u64,
    /// Metadata for unfinalized derived blocks (for reorg detection).
    cursor: Vec<DerivedBlockMeta>,
    /// The highest L2 block number derived so far. Used to generate gap-fill
    /// empty blocks when submitted block numbers are non-sequential (empty
    /// blocks are not submitted to L1 to save gas).
    last_derived_l2_block: u64,
    /// L1 info from the most recent BatchPosted event, used as context for
    /// gap-filled empty blocks.
    last_l1_info: Option<L1BlockInfo>,
    /// Hash of the deployment L1 block, used as default L1 context for gap-fill
    /// blocks when no BatchPosted event has been seen yet.
    deployment_l1_block_hash: B256,
    /// The L1 block up to which execution entries have been fetched and assigned
    /// to L2 blocks.
    last_execution_l1_block: u64,
    /// Ephemeral execution entry cursor for builder mode (not persisted).
    builder_execution_l1_block: u64,
}

impl DerivationPipeline {
    pub fn new(config: Arc<RollupConfig>) -> Self {
        let start = config.deployment_l1_block;
        Self {
            config,
            last_processed_l1_block: start,
            cursor: Vec::new(),
            last_derived_l2_block: 0,
            last_l1_info: None,
            deployment_l1_block_hash: B256::ZERO,
            last_execution_l1_block: start,
            builder_execution_l1_block: 0,
        }
    }

    /// Set the deployment L1 block hash (fetched once at startup).
    pub fn set_deployment_l1_block_hash(&mut self, hash: B256) {
        self.deployment_l1_block_hash = hash;
    }

    /// Resume derivation from a known L1 block (e.g., after restart).
    pub fn resume_from(&mut self, l1_block: u64) {
        self.last_processed_l1_block = l1_block;
        self.last_execution_l1_block = l1_block;
    }

    /// Set the last derived L2 block number (for gap-fill tracking).
    /// Should be called after loading checkpoint with the current L2 head.
    pub fn set_last_derived_l2_block(&mut self, l2_block: u64) {
        self.last_derived_l2_block = l2_block;
    }

    /// Fetch the next batch of derived blocks from L1.
    ///
    /// Scans L1 blocks for `BatchPosted` events, decodes the `postBatch`
    /// calldata to extract block numbers and transactions, and returns
    /// derived blocks sorted by L2 block number. Generates gap-fill empty
    /// blocks for L2 block numbers between submissions (empty blocks are
    /// not submitted to L1 to save gas).
    ///
    /// Immediate entries (actionHash == 0) provide per-block state roots.
    /// Deferred entries (actionHash != 0) are cross-chain execution entries
    /// assigned to the first block in the batch.
    ///
    /// **Important:** This method does NOT advance the pipeline's internal cursor.
    /// After successfully processing all blocks in the returned batch, the caller
    /// must invoke [`commit_batch`](Self::commit_batch) to advance the cursor.
    /// If block building fails, the cursor stays where it was and the same blocks
    /// will be re-derived on the next tick.
    pub async fn derive_next_batch(
        &mut self,
        latest_l1_block: u64,
        l1_provider: &impl Provider,
    ) -> Result<DerivedBatch> {
        let empty_batch = DerivedBatch {
            blocks: Vec::new(),
            cursor_update: CursorUpdate {
                last_processed_l1_block: self.last_processed_l1_block,
                last_execution_l1_block: self.last_execution_l1_block,
                last_derived_l2_block: self.last_derived_l2_block,
                last_l1_info: self.last_l1_info.clone(),
                new_cursor_entries: Vec::new(),
            },
        };

        if latest_l1_block <= self.last_processed_l1_block {
            return Ok(empty_batch);
        }

        let from_block = self.last_processed_l1_block.saturating_add(1);
        // Limit query range to MAX_LOG_RANGE blocks per call to avoid
        // exceeding RPC provider limits on eth_getLogs (ISSUE-215).
        let to_block = latest_l1_block.min(from_block.saturating_add(MAX_LOG_RANGE - 1));

        // Fetch BatchPosted events from Rollups contract
        let filter = Filter::new()
            .address(self.config.rollups_address)
            .event_signature(cross_chain::batch_posted_signature_hash())
            .from_block(from_block)
            .to_block(to_block);

        let logs = l1_provider.get_logs(&filter).await?;

        // Fetch ExecutionConsumed events to know which deferred entries were
        // actually consumed on L1. Use `to_block` (the full derivation window)
        // because the user's proxy call may land in a later L1 block than the
        // postBatch. Extra consumed hashes are harmless (they won't match any
        // deferred entries in this batch).
        //
        // See docs/DERIVATION.md §4e: deferred entries are only executed on L2 if
        // consumed on L1. No event = it didn't happen.
        let consumed_map: ConsumedMap = if !self.config.rollups_address.is_zero() {
            let consumed_filter = Filter::new()
                .address(self.config.rollups_address)
                .event_signature(cross_chain::execution_consumed_signature_hash())
                .from_block(from_block)
                .to_block(to_block);
            let consumed_logs = l1_provider.get_logs(&consumed_filter).await?;
            cross_chain::parse_execution_consumed_logs(&consumed_logs)
        } else {
            std::collections::HashMap::new()
        };

        let mut derived_blocks = Vec::new();
        let mut new_cursor_entries = Vec::new();

        // Local cursor variables — NOT applied to self until commit_batch().
        let mut local_execution_l1_block = self.last_execution_l1_block;
        let mut local_derived_l2_block = self.last_derived_l2_block;
        let mut local_l1_info = self.last_l1_info.clone();

        // Shared consumed-entry counter across ALL batches in this derivation window.
        // MUST be shared (not rebuilt per-batch) because the same actionHash can appear
        // in multiple batches. On L1, entries are consumed FIFO across batches, so
        // the remaining count must decrement across batches in order.
        let mut remaining: std::collections::HashMap<B256, usize> =
            consumed_map.iter().map(|(k, v)| (*k, v.len())).collect();

        for log in logs {
            let l1_block = match log.block_number {
                Some(n) => n,
                None => {
                    warn!(
                        target: "based_rollup::derivation",
                        "skipping BatchPosted log with no block_number (possibly pending)"
                    );
                    continue;
                }
            };

            let tx_hash = match log.transaction_hash {
                Some(h) => h,
                None => {
                    warn!(
                        target: "based_rollup::derivation",
                        %l1_block,
                        "skipping BatchPosted log with no transaction_hash"
                    );
                    continue;
                }
            };

            // Fetch the L1 transaction to get the postBatch calldata
            let l1_tx = match l1_provider.get_transaction_by_hash(tx_hash).await? {
                Some(tx) => tx,
                None => {
                    warn!(
                        target: "based_rollup::derivation",
                        %l1_block,
                        %tx_hash,
                        "L1 transaction not found for BatchPosted event"
                    );
                    continue;
                }
            };

            // Decode postBatch calldata
            let tx_input = l1_tx.inner.input();
            let (entries, call_data) = match cross_chain::decode_post_batch_calldata(tx_input) {
                Ok(decoded) => decoded,
                Err(err) => {
                    warn!(
                        target: "based_rollup::derivation",
                        %l1_block,
                        %tx_hash,
                        %err,
                        "failed to decode postBatch calldata"
                    );
                    continue;
                }
            };

            // Decode block data from callData (may be empty if only cross-chain entries)
            let (block_numbers, block_txs) = if !call_data.is_empty() {
                match cross_chain::decode_block_calldata(&call_data) {
                    Ok(decoded) => decoded,
                    Err(err) => {
                        warn!(
                            target: "based_rollup::derivation",
                            %l1_block,
                            %tx_hash,
                            %err,
                            "failed to decode block calldata"
                        );
                        continue;
                    }
                }
            } else {
                (vec![], vec![])
            };

            // Separate immediate entries (aggregate state root) from deferred entries (cross-chain).
            // The builder submits a single aggregate immediate entry per batch:
            // StateDelta(currentState=on-chain, newState=final_state_root).
            // This final state root is assigned to the last block in the batch.
            let mut batch_final_state_root = B256::ZERO;
            let mut deferred_entries: Vec<CrossChainExecutionEntry> = Vec::new();
            let mut has_unconsumed_entries = false;

            // Snapshot `remaining` BEFORE this batch's entries consume it.
            // The driver's generic §4f filtering needs the pre-batch state to
            // independently determine which triggers were consumed via trial
            // execution + ExecutionConsumed events + prefix counting.
            let remaining_snapshot_for_generic_filtering = remaining.clone();

            for entry in &entries {
                if entry.action_hash == B256::ZERO {
                    // Immediate entry — extract final state root from StateDelta
                    if let Some(delta) = entry.state_deltas.first() {
                        batch_final_state_root = delta.new_state;
                    }
                } else if let Some(count) = remaining.get_mut(&entry.action_hash) {
                    if *count > 0 {
                        *count -= 1;
                        // Deferred entry consumed on L1 — include for L2 execution
                        deferred_entries.push(entry.clone());
                    } else {
                        // All consumed occurrences exhausted for this hash — unconsumed.
                        has_unconsumed_entries = true;
                        warn!(
                            target: "based_rollup::derivation",
                            action_hash = %entry.action_hash,
                            %l1_block,
                            "skipping deferred entry — consumed count exhausted \
                             (more entries than ExecutionConsumed events for this hash)"
                        );
                    }
                } else {
                    // Deferred entry NOT consumed on L1 — skip.
                    // The user's L1 proxy call either reverted or hasn't landed yet.
                    // See docs/DERIVATION.md §4e: no ExecutionConsumed event = it didn't happen.
                    has_unconsumed_entries = true;
                    warn!(
                        target: "based_rollup::derivation",
                        action_hash = %entry.action_hash,
                        %l1_block,
                        "skipping deferred entry — not consumed on L1 (user tx likely reverted)"
                    );
                }
            }

            // Reconstruct L2-format entry pairs from L1-format entries + CALL actions.
            // L1 entries have (actionHash=hash(CALL), nextAction=RESULT) — fullnodes
            // need the full entry pairs for effective_state_root computation via
            // chained state deltas (§4e). The CALL actions come from
            // ExecutionConsumed events emitted when entries are consumed on L1.
            let call_actions: Vec<cross_chain::CrossChainAction> = consumed_map
                .values()
                .flat_map(|v| v.iter())
                .cloned()
                .collect();
            let deferred_entries = if !call_actions.is_empty() {
                let mut pairs =
                    cross_chain::convert_l1_entries_to_l2_pairs(&deferred_entries, &call_actions);
                // Append continuation entries for multi-call patterns (§4e).
                // Continuation L1 entries have nextAction.type == CALL instead of RESULT,
                // signaling a reentrant cross-chain call. The additional L2 entries are
                // needed for the CCM execution table to resolve the full call chain.
                let continuation = cross_chain::reconstruct_continuation_l2_entries(
                    &deferred_entries,
                    &call_actions,
                );
                info!(
                    target: "based_rollup::derivation",
                    deferred_count = deferred_entries.len(),
                    call_action_count = call_actions.len(),
                    continuation_count = continuation.len(),
                    pairs_count = pairs.len(),
                    %l1_block,
                    "continuation reconstruction result"
                );
                if !continuation.is_empty() {
                    info!(
                        target: "based_rollup::derivation",
                        count = continuation.len(),
                        %l1_block,
                        "appending multi-call continuation entries"
                    );
                    pairs.extend(continuation);
                }
                pairs
            } else {
                deferred_entries
            };

            // Compute effective state root by applying consumed entries' chained
            // state deltas to the clean batch_final_state_root. Each CALL entry's
            // StateDelta chains: Y → X₁ → X₂ → ... → X. The effective root
            // equals the state after the last consumed entry (see §3e, §4e).
            let mut effective_state_root = batch_final_state_root;
            let rollup_id_u256 = U256::from(self.config.rollup_id);
            for entry in &deferred_entries {
                for delta in &entry.state_deltas {
                    if delta.current_state == effective_state_root
                        && delta.rollup_id == rollup_id_u256
                    {
                        effective_state_root = delta.new_state;
                    }
                }
            }

            // L1 context = parent of the containing L1 block.
            // The builder uses latest_l1_block when building, and the tx lands in
            // latest_l1_block + 1, so containing_block - 1 = latest_l1_block.
            let l1_context_block = l1_block.saturating_sub(1);

            // Fetch containing L1 block (for reorg detection hash AND parent hash for context)
            let containing_block = l1_provider
                .get_block_by_number(l1_block.into())
                .await?
                .ok_or_else(|| eyre::eyre!("L1 block {l1_block} not found"))?;
            let l1_block_hash = containing_block.header.hash;
            let l1_context_hash = containing_block.header.parent_hash;

            // Build DerivedBlocks from block data
            for (i, (&l2_block_number, transactions)) in
                block_numbers.iter().zip(block_txs.iter()).enumerate()
            {
                // Skip stale blocks whose l2_block_number is at or below our
                // already-derived head.
                if l2_block_number <= self.last_derived_l2_block
                    && l2_block_number <= local_derived_l2_block
                {
                    warn!(
                        target: "based_rollup::derivation",
                        l2_block_number,
                        last_derived = self.last_derived_l2_block,
                        %l1_block,
                        "skipping stale block in BatchPosted — l2_block_number already derived"
                    );
                    continue;
                }

                let l2_timestamp = match self.config.l2_timestamp_checked(l2_block_number) {
                    Some(ts) => ts,
                    None => {
                        warn!(
                            target: "based_rollup::derivation",
                            l2_block_number,
                            "skipping block in BatchPosted — timestamp overflow"
                        );
                        continue;
                    }
                };

                // The batch has a single aggregate state root for the final block.
                // Intermediate blocks get B256::ZERO (fullnode recomputes locally).
                // We use effective_state_root which accounts for consumed cross-chain
                // entry deltas (see §3e, §4e).
                let is_last_in_batch = i == block_numbers.len() - 1;
                let state_root = if is_last_in_batch {
                    effective_state_root
                } else {
                    B256::ZERO
                };
                let is_empty = transactions.is_empty() || transactions.as_ref() == [0xc0];

                // Generate gap-fill empty blocks for any skipped L2 block numbers.
                // Empty blocks are not submitted to L1 (to save gas), so we need to
                // produce them locally. They use the L1 context from the previous
                // submission (deterministic for all nodes).
                let expected_next = local_derived_l2_block.saturating_add(1);
                if l2_block_number > expected_next {
                    let gap_size = l2_block_number - expected_next;
                    if gap_size > MAX_BLOCK_GAP {
                        return Err(eyre::eyre!(
                            "BatchPosted block {l2_block_number} exceeds \
                             MAX_BLOCK_GAP ({MAX_BLOCK_GAP}): expected next block {expected_next}, \
                             gap size {gap_size}. This indicates a state inconsistency — \
                             the node may need to be re-synced from genesis."
                        ));
                    }

                    let gap_l1_info = local_l1_info.clone().unwrap_or(L1BlockInfo {
                        l1_block_number: self.config.deployment_l1_block,
                        l1_block_hash: self.deployment_l1_block_hash,
                    });

                    for gap_block in expected_next..l2_block_number {
                        let gap_timestamp = match self.config.l2_timestamp_checked(gap_block) {
                            Some(ts) => ts,
                            None => {
                                warn!(
                                    target: "based_rollup::derivation",
                                    gap_block,
                                    "skipping gap-fill block — timestamp overflow"
                                );
                                break;
                            }
                        };

                        debug!(
                            target: "based_rollup::derivation",
                            gap_block,
                            l1_context = gap_l1_info.l1_block_number,
                            "generating gap-fill empty block"
                        );

                        derived_blocks.push(DerivedBlock {
                            l2_block_number: gap_block,
                            l2_timestamp: gap_timestamp,
                            l1_info: gap_l1_info.clone(),
                            state_root: B256::ZERO,
                            transactions: Bytes::new(),
                            is_empty: true,
                            execution_entries: vec![],
                            filtering: None,
                        });
                    }
                }

                let l1_info = L1BlockInfo {
                    l1_block_number: l1_context_block,
                    l1_block_hash: l1_context_hash,
                };

                // Assign deferred execution entries to the FIRST block in the batch only
                let execution_entries = if i == 0 {
                    deferred_entries.clone()
                } else {
                    vec![]
                };

                // §4f protocol tx filtering — deferred to the driver.
                //
                // When unconsumed entries exist, pass the L1 consumed map
                // snapshot to the driver. The driver handles ALL filtering
                // generically via trial-execution + ExecutionConsumed events +
                // prefix counting. No type-specific counting is needed here.
                let filtering = if has_unconsumed_entries && i == 0 {
                    info!(
                        target: "based_rollup::derivation",
                        l2_block_number,
                        %l1_block,
                        "§4f filtering deferred to driver (generic event-based)"
                    );

                    Some(DeferredFiltering {
                        l1_consumed_remaining: remaining_snapshot_for_generic_filtering.clone(),
                        all_l2_entries: execution_entries.clone(),
                    })
                } else {
                    None
                };
                let effective_block_state_root = state_root;

                derived_blocks.push(DerivedBlock {
                    l2_block_number,
                    l2_timestamp,
                    l1_info: l1_info.clone(),
                    state_root: effective_block_state_root,
                    transactions: transactions.clone(),
                    is_empty,
                    execution_entries,
                    filtering,
                });

                // Update local tracking for gap-fill and L1 context
                local_derived_l2_block = l2_block_number;
                local_l1_info = Some(l1_info);

                new_cursor_entries.push(DerivedBlockMeta {
                    l2_block_number,
                    l1_block_number: l1_block,
                    l1_block_hash,
                });
            }

            // Advance execution cursor past this L1 block
            if l1_block > local_execution_l1_block {
                local_execution_l1_block = l1_block;
            }
        }

        // Sort by L2 block number (events may come from different L1 blocks)
        // and deduplicate — multiple L1 txs could reference the same L2 block.
        derived_blocks.sort_by_key(|b| b.l2_block_number);
        // Collect duplicate L2 block numbers before dedup removes them
        let mut duplicate_l2_numbers: Vec<u64> = Vec::new();
        for window in derived_blocks.windows(2) {
            if window[0].l2_block_number == window[1].l2_block_number
                && !duplicate_l2_numbers.contains(&window[0].l2_block_number)
            {
                duplicate_l2_numbers.push(window[0].l2_block_number);
            }
        }
        let count_before_dedup = derived_blocks.len();
        derived_blocks.dedup_by_key(|b| b.l2_block_number);
        let duplicates_removed = count_before_dedup - derived_blocks.len();
        if duplicates_removed > 0 {
            warn!(
                target: "based_rollup::derivation",
                duplicates_removed,
                from_l1 = from_block,
                to_l1 = to_block,
                duplicate_l2_blocks = ?duplicate_l2_numbers,
                "removed duplicate derived blocks with same L2 block number in L1 range"
            );
        }

        // Keep cursor entries consistent with derived_blocks: sort + dedup.
        new_cursor_entries.sort_by_key(|e| e.l2_block_number);
        new_cursor_entries.dedup_by_key(|e| e.l2_block_number);

        // Update local_derived_l2_block from the sorted/deduped result so the
        // cursor reflects the actual highest block returned, not the last log
        // processed in L1-block order (which could be lower after dedup).
        if let Some(last) = derived_blocks.last() {
            local_derived_l2_block = last.l2_block_number;
        }

        if !derived_blocks.is_empty() {
            debug!(
                target: "based_rollup::derivation",
                from_l1 = from_block,
                to_l1 = to_block,
                blocks = derived_blocks.len(),
                "derived L2 blocks from L1"
            );
        }

        Ok(DerivedBatch {
            blocks: derived_blocks,
            cursor_update: CursorUpdate {
                last_processed_l1_block: to_block,
                last_execution_l1_block: local_execution_l1_block,
                last_derived_l2_block: local_derived_l2_block,
                last_l1_info: local_l1_info,
                new_cursor_entries,
            },
        })
    }

    /// Commit the cursor state from a successfully processed batch.
    ///
    /// This advances the pipeline's internal cursors to reflect that all blocks
    /// in the batch have been successfully built and inserted. Must be called
    /// after all blocks from [`Self::derive_next_batch`] are processed.
    ///
    /// If block building fails partway through, do NOT call this method — the
    /// cursor stays where it was and the same blocks will be re-derived on the
    /// next tick.
    pub fn commit_batch(&mut self, batch: &DerivedBatch) {
        self.last_processed_l1_block = batch.cursor_update.last_processed_l1_block;
        self.last_execution_l1_block = batch.cursor_update.last_execution_l1_block;
        self.last_derived_l2_block = batch.cursor_update.last_derived_l2_block;
        self.last_l1_info = batch.cursor_update.last_l1_info.clone();
        self.cursor
            .extend(batch.cursor_update.new_cursor_entries.iter().cloned());

        // Cap cursor size to prevent unbounded growth during initial sync.
        // prune_finalized() only runs every 64 L1 blocks, so during long syncs
        // the cursor can grow much larger than needed.
        if self.cursor.len() > 2 * REORG_CHECK_DEPTH {
            let drain_count = self.cursor.len() - REORG_CHECK_DEPTH;
            self.cursor.drain(..drain_count);
        }
    }

    /// Convenience method: derive the next batch AND immediately commit the cursor.
    ///
    /// This is equivalent to calling [`derive_next_batch`](Self::derive_next_batch)
    /// followed by [`commit_batch`](Self::commit_batch). Useful in tests and
    /// contexts where the caller always wants to commit (no partial-failure
    /// recovery needed). Returns just the derived blocks.
    pub async fn derive_next_batch_and_commit(
        &mut self,
        latest_l1_block: u64,
        l1_provider: &impl Provider,
    ) -> Result<Vec<DerivedBlock>> {
        let batch = self.derive_next_batch(latest_l1_block, l1_provider).await?;
        self.commit_batch(&batch);
        Ok(batch.blocks)
    }

    /// Check for L1 reorgs by comparing stored L1 block hashes against the canonical chain.
    ///
    /// Only checks the most recent `REORG_CHECK_DEPTH` entries (64 by default).
    /// This bounds the number of RPC calls per check while covering well beyond
    /// typical L1 reorg depths (1-3 blocks).
    pub async fn detect_reorg(&self, l1_provider: &impl Provider) -> Result<Option<u64>> {
        if self.cursor.is_empty() {
            return Ok(None);
        }

        let check_depth = self.cursor.len().min(REORG_CHECK_DEPTH);
        let check_start = self.cursor.len() - check_depth;
        let entries = &self.cursor[check_start..];

        // Check the most recent entry first — fast path for no-reorg case
        let latest = entries.last().expect("entries is non-empty");
        let latest_canonical = l1_provider
            .get_block_by_number(latest.l1_block_number.into())
            .await?
            .ok_or_else(|| {
                eyre::eyre!(
                    "L1 block {} not found during reorg detection",
                    latest.l1_block_number
                )
            })?
            .header
            .hash;

        if latest_canonical == latest.l1_block_hash {
            return Ok(None);
        }

        // Reorg detected — walk backward to find the fork point
        info!(
            target: "based_rollup::derivation",
            l1_block = latest.l1_block_number,
            stored_hash = %latest.l1_block_hash,
            canonical_hash = %latest_canonical,
            "L1 reorg detected"
        );

        for meta in entries.iter().rev().skip(1) {
            let canonical_hash = l1_provider
                .get_block_by_number(meta.l1_block_number.into())
                .await?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "L1 block {} not found during reorg walk-back",
                        meta.l1_block_number
                    )
                })?
                .header
                .hash;

            if canonical_hash == meta.l1_block_hash {
                return Ok(Some(meta.l1_block_number));
            }
        }

        // All checked entries are mismatched — fork point is before our window
        Ok(Some(self.config.deployment_l1_block))
    }

    /// Roll back the derivation state to a given L1 block (the fork point).
    ///
    /// Returns the highest retained L2 block number, or `None` if the cursor
    /// is empty after rollback (deep reorg → rewind to genesis).
    pub fn rollback_to(&mut self, l1_block: u64) -> Option<u64> {
        self.cursor.retain(|m| m.l1_block_number <= l1_block);
        self.last_processed_l1_block = l1_block;
        // Execution cursor must never ADVANCE during rollback.
        // A rollback to L1 block M (e.g., from an L1 context mismatch) may be
        // AFTER the L1 block containing BatchPosted events that the re-derived
        // blocks still need. Using `min` ensures we only go backward,
        // preserving the ability to re-fetch events from earlier L1 blocks.
        self.last_execution_l1_block = l1_block.min(self.last_execution_l1_block);
        self.builder_execution_l1_block = l1_block.min(self.builder_execution_l1_block);
        let last_valid_l2 = self.cursor.last().map(|m| m.l2_block_number);
        // Reset L2 tracking for gap-fill to match the rollback point
        self.last_derived_l2_block = last_valid_l2.unwrap_or(0);
        // Restore last_l1_info from the last retained cursor entry so gap-fill
        // blocks after rollback use the correct L1 context instead of falling
        // back to deployment_l1_block (#125, #126).
        if let Some(last) = self.cursor.last() {
            self.last_l1_info = Some(L1BlockInfo {
                l1_block_number: last.l1_block_number,
                l1_block_hash: last.l1_block_hash,
            });
        } else {
            self.last_l1_info = None;
        }
        info!(
            target: "based_rollup::derivation",
            fork_point = l1_block,
            ?last_valid_l2,
            "rolled back derivation state"
        );
        last_valid_l2
    }

    /// Prune finalized blocks from the cursor (they can't reorg).
    pub fn prune_finalized(&mut self, finalized_l1_block: u64) {
        self.cursor
            .retain(|m| m.l1_block_number > finalized_l1_block);
    }

    pub fn last_processed_l1_block(&self) -> u64 {
        self.last_processed_l1_block
    }

    pub fn cursor_len(&self) -> usize {
        self.cursor.len()
    }

    #[doc(hidden)]
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cursor_push_for_test(&mut self, meta: DerivedBlockMeta) {
        self.cursor.push(meta);
    }

    /// Fetch cross-chain execution entries for the builder.
    ///
    /// Reads BatchPosted events from Rollups.sol and returns entries relevant
    /// to this rollup. Uses an ephemeral cursor (`builder_execution_l1_block`)
    /// that is not persisted.
    pub async fn fetch_execution_entries_for_builder(
        &mut self,
        up_to_l1_block: u64,
        l1_provider: &impl Provider,
    ) -> Result<Vec<CrossChainExecutionEntry>> {
        if self.config.rollups_address.is_zero() {
            return Ok(Vec::new());
        }
        let effective_cursor = self
            .builder_execution_l1_block
            .max(self.last_execution_l1_block);
        if up_to_l1_block <= effective_cursor {
            return Ok(Vec::new());
        }

        let from = effective_cursor.saturating_add(1);
        let to = up_to_l1_block.min(from.saturating_add(MAX_LOG_RANGE - 1));

        let filter = Filter::new()
            .address(self.config.rollups_address)
            .event_signature(cross_chain::batch_posted_signature_hash())
            .from_block(from)
            .to_block(to);

        let logs = l1_provider.get_logs(&filter).await?;
        let derived =
            cross_chain::parse_batch_posted_logs(&logs, U256::from(self.config.rollup_id));

        if !derived.is_empty() {
            debug!(
                target: "based_rollup::derivation",
                from_l1 = from,
                to_l1 = to,
                entry_count = derived.len(),
                "fetched execution entries for builder"
            );
        }

        self.builder_execution_l1_block = to;
        Ok(derived.into_iter().map(|d| d.entry).collect())
    }

    /// Save the current L1 derivation progress to the database.
    pub fn save_checkpoint<P>(&self, provider: &P) -> Result<()>
    where
        P: DatabaseProviderFactory,
        P::ProviderRW: StageCheckpointWriter,
    {
        let rw = provider
            .database_provider_rw()
            .wrap_err("database operation failed")?;
        rw.save_stage_checkpoint(
            L1_DERIVATION_STAGE_ID,
            StageCheckpoint::new(self.last_processed_l1_block),
        )
        .wrap_err("failed to save stage checkpoint")?;
        rw.save_stage_checkpoint(
            L1_EXECUTION_STAGE_ID,
            StageCheckpoint::new(self.last_execution_l1_block),
        )
        .wrap_err("failed to save execution checkpoint")?;
        rw.commit().wrap_err("failed to commit checkpoint")?;

        debug!(
            target: "based_rollup::derivation",
            l1_block = self.last_processed_l1_block,
            execution_l1_block = self.last_execution_l1_block,
            "saved L1 derivation checkpoint"
        );
        Ok(())
    }

    /// Load the L1 derivation checkpoint from the database and resume from it.
    pub fn load_checkpoint<P>(&mut self, provider: &P) -> Result<Option<u64>>
    where
        P: StageCheckpointReader,
    {
        let checkpoint = provider
            .get_stage_checkpoint(L1_DERIVATION_STAGE_ID)
            .wrap_err("database operation failed")?;

        // Also load the execution checkpoint
        let execution_checkpoint = provider
            .get_stage_checkpoint(L1_EXECUTION_STAGE_ID)
            .wrap_err("database operation failed")?;

        if let Some(cp) = checkpoint {
            let l1_block = cp.block_number;
            self.resume_from(l1_block);

            // Override execution cursor if a separate execution checkpoint exists
            if let Some(ecp) = execution_checkpoint {
                self.last_execution_l1_block = ecp.block_number;
            }

            info!(
                target: "based_rollup::derivation",
                l1_block,
                execution_l1_block = self.last_execution_l1_block,
                "resumed L1 derivation from checkpoint"
            );
            Ok(Some(l1_block))
        } else {
            Ok(None)
        }
    }

    /// Rebuild the reorg-detection cursor from L2 block headers stored in the DB.
    ///
    /// After a restart, the in-memory cursor is empty, making `detect_reorg` a no-op.
    /// This method reads the most recent L2 headers and extracts the L1 context that
    /// the driver encoded into `mix_hash` (L1 block number) and
    /// `parent_beacon_block_root` (L1 block hash).
    pub fn rebuild_cursor_from_headers<P>(&mut self, provider: &P, l2_head: u64) -> Result<()>
    where
        P: HeaderProvider<Header = alloy_consensus::Header>,
    {
        if l2_head == 0 {
            return Ok(());
        }

        self.cursor.clear();

        let start = l2_head.saturating_sub(REORG_CHECK_DEPTH as u64).max(1);

        for n in start..=l2_head {
            let header = match provider.sealed_header(n) {
                Ok(Some(h)) => h,
                Ok(None) => {
                    warn!(
                        target: "based_rollup::derivation",
                        block_number = n,
                        "missing header during cursor rebuild, skipping"
                    );
                    continue;
                }
                Err(e) => {
                    warn!(
                        target: "based_rollup::derivation",
                        block_number = n,
                        error = %e,
                        "failed to read header during cursor rebuild, skipping"
                    );
                    continue;
                }
            };

            // Decode L1 block number from mix_hash (last 8 bytes as u64, big-endian)
            // Same encoding as evm_config.rs:203-207
            let randao = match header.mix_hash() {
                Some(h) => h,
                None => {
                    warn!(
                        target: "based_rollup::derivation",
                        block_number = n,
                        "mix_hash is None during cursor rebuild, skipping"
                    );
                    continue;
                }
            };
            let l1_block_number: u64 = match randao.0[24..32].try_into().map(u64::from_be_bytes) {
                Ok(num) => num,
                Err(_) => {
                    warn!(
                        target: "based_rollup::derivation",
                        block_number = n,
                        "failed to decode L1 block number from mix_hash, skipping"
                    );
                    continue;
                }
            };

            // Validate extracted L1 block info: non-zero values expected for
            // blocks that were built with proper L1 context.
            if l1_block_number == 0 {
                warn!(
                    target: "based_rollup::derivation",
                    l2_block_number = n,
                    "L1 block number from mix_hash is zero during cursor rebuild"
                );
            }

            let l1_hash_candidate = header.parent_beacon_block_root().unwrap_or(B256::ZERO);
            if l1_hash_candidate.is_zero() {
                warn!(
                    target: "based_rollup::derivation",
                    l2_block_number = n,
                    "L1 block hash from parent_beacon_block_root is zero during cursor rebuild"
                );
            }

            // parent_beacon_block_root carries the L1 *context* block hash (containing - 1).
            // The cursor stores (l1_block_number, l1_block_hash) for reorg detection:
            // detect_reorg fetches the block at l1_block_number and compares its hash
            // to l1_block_hash. So we must store a CONSISTENT pair.
            //
            // derive_next_batch stores (containing_block_number, containing_block_hash).
            // We only have the context block number (from mix_hash) and context block hash
            // (from parent_beacon_block_root). Store the context pair — detect_reorg will
            // correctly verify the context block hash against the canonical chain.
            let l1_block_hash = header.parent_beacon_block_root().unwrap_or(B256::ZERO);

            self.cursor.push(DerivedBlockMeta {
                l2_block_number: n,
                l1_block_number,
                l1_block_hash,
            });
        }

        // Restore last_l1_info from the last cursor entry so gap-fill blocks
        // after restart use the same L1 context as during continuous operation.
        // Without this, gap-fill blocks default to deployment_l1_block context,
        // causing L2Context mapping divergence vs. nodes that synced from scratch.
        if let Some(last) = self.cursor.last() {
            self.last_l1_info = Some(L1BlockInfo {
                l1_block_number: last.l1_block_number,
                l1_block_hash: last.l1_block_hash,
            });
        }

        info!(
            target: "based_rollup::derivation",
            entries = self.cursor.len(),
            l2_head,
            "rebuilt reorg-detection cursor from L2 headers"
        );

        Ok(())
    }
}

#[cfg(test)]
#[path = "derivation_tests.rs"]
mod tests;
