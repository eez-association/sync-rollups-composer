//! Main orchestration loop for the based rollup node.
//!
//! Manages transitions between Sync, Builder, and Fullnode modes, drives the
//! Engine API, and coordinates derivation, block building, and L1 submission.

use crate::builder_sync::{BuilderSync, PreconfirmedBlock};
use crate::config::RollupConfig;
use crate::cross_chain::CrossChainExecutionEntry;
use crate::derivation::{DerivationPipeline, L1_CONFIRMED_L1_STAGE_ID, L1_CONFIRMED_L2_STAGE_ID};
use crate::evm_config::RollupEvmConfig;
use crate::health::HealthStatus;
use crate::proposer::{GasPriceHint, PendingBlock, Proposer};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{
    ExecutionData, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes,
};
use alloy_sol_types::SolCall;
use eyre::{OptionExt, Result, WrapErr};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_payload_primitives::{EngineApiMessageVersion, PayloadTypes};
use reth_primitives_traits::{Recovered, SignedTransaction, SignerRecoverable};
use reth_provider::{
    BlockHashReader, BlockNumReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use reth_revm::database::StateProviderDatabase;
use reth_stages_types::{StageCheckpoint, StageId};
use revm::database::State;
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

/// The operating mode of the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverMode {
    /// Syncing from L1 events (catching up).
    Sync,
    /// Actively building blocks (caught up).
    Builder,
    /// Fullnode mode — sync only, never sequence.
    Fullnode,
}

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
    queued_cross_chain_calls: Arc<std::sync::Mutex<Vec<crate::rpc::QueuedCrossChainCall>>>,
    /// Legacy queue for raw signed L1 transactions to forward after `postBatch`.
    /// Kept for backward compatibility with `queueL1ForwardTx` RPC method.
    pending_l1_forward_txs: Arc<std::sync::Mutex<Vec<Bytes>>>,
    /// Queue for L2→L1 calls. The RPC pushes here; the driver drains
    /// into builder_execution_entries alongside L1→L2 entries (unified intermediate roots).
    queued_l2_to_l1_calls: Arc<std::sync::Mutex<Vec<crate::rpc::QueuedL2ToL1Call>>>,
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
pub struct BuiltBlock {
    /// The block hash.
    pub hash: B256,
    /// The parent's state root (pre-execution).
    pub pre_state_root: B256,
    /// The state root of the block (post-execution).
    pub state_root: B256,
    /// Number of transactions in the block.
    pub tx_count: usize,
    /// RLP-encoded transactions for L1 submission.
    pub encoded_transactions: Bytes,
}

/// Last L1-confirmed batch anchor — used for efficient rollback instead of genesis.
#[derive(Debug, Clone, Copy)]
struct L1ConfirmedAnchor {
    l2_block_number: u64,
    l1_block_number: u64,
}

/// Trigger metadata for L1 trigger groups. Groups that need L1 trigger txs
/// (`executeL2TX`) carry `Some(TriggerMetadata)`; protocol-triggered groups
/// (deposits) carry `None`.
#[derive(Debug, Clone)]
pub struct TriggerMetadata {
    /// User address (trigger initiator on L1).
    pub user: Address,
    /// Amount in wei (for logging / gas estimation).
    pub amount: U256,
    /// RLP-encoded L2 transaction for the L2TX trigger on L1.
    pub rlp_encoded_tx: Vec<u8>,
    /// Number of `executeL2TX` calls needed for this trigger group.
    pub trigger_count: usize,
}

// ──────────────────────────────────────────────
//  PendingL1SubmissionQueue (refactor PLAN step 1.5)
//
//  Consolidates four parallel vectors on `Driver` into a single
//  struct:
//
//      pending_l1_entries: Vec<CrossChainExecutionEntry>
//      pending_l1_group_starts: Vec<usize>
//      pending_l1_independent: Vec<EntryGroupMode>
//      pending_l1_trigger_metadata: Vec<Option<TriggerMetadata>>
//
//  Pre-1.5, every append, truncate, take, and clear had to touch
//  all four vectors together, and any site that forgot one of them
//  would silently desync the queue. The single-struct design makes
//  the 4-tuple an atomic unit: `append_group` grows all four at
//  once, `truncate_to` / `clear` drop them all at once, and
//  `std::mem::take(&mut self.pending_l1)` moves the whole unit by
//  value.
//
//  ## Closes invariant #11
//
//  PLAN §6 invariant #11 ("Deposits + withdrawals can coexist in
//  same block — removed mutual exclusion") is closed here by
//  exposing a typed [`BlockEntryMix`] classifier. The enum
//  documents the four possible mixes and makes `Mixed` a
//  first-class valid state, grep-able and discriminable by the
//  compiler. Any future code that wants to reintroduce a mutual
//  exclusion check will have to `match` on `BlockEntryMix` and
//  confront the `Mixed` variant explicitly.
// ──────────────────────────────────────────────

/// A single trigger group inside [`PendingL1SubmissionQueue`].
///
/// A group is a contiguous run of L1 deferred entries that share a
/// single trigger (either a protocol-initiated postBatch-only path,
/// or a user-initiated `executeL2TX` call). Groups are created by
/// the driver as it drains the `QueuedCrossChainCall` queue and by
/// the L2→L1 path as it drains withdrawal triggers.
#[derive(Debug, Clone)]
pub struct PendingL1Group {
    /// Index of the first entry in this group within
    /// [`PendingL1SubmissionQueue::entries`]. The last entry of
    /// group `k` is the one at index `groups[k+1].start - 1`
    /// (or `entries.len() - 1` for the last group).
    pub start: usize,
    /// Chaining mode for this group's state deltas.
    pub mode: crate::cross_chain::EntryGroupMode,
    /// Trigger metadata. `Some` for user-initiated L2→L1 groups
    /// (withdrawals, multi-call L2→L1); `None` for
    /// protocol-triggered groups (deposits, L1→L2 continuations).
    pub trigger: Option<TriggerMetadata>,
}

/// Classification of the entry mix in a
/// [`PendingL1SubmissionQueue`]. Returned by
/// [`PendingL1SubmissionQueue::entry_mix`]. Closes invariant #11
/// by making the "deposits + withdrawals coexist" state an
/// explicit enum variant (`Mixed`) rather than an implicit "no
/// mutual exclusion check" runtime invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockEntryMix {
    /// No entries pending.
    Empty,
    /// Only protocol-triggered groups (deposits, L1→L2
    /// continuations). No `executeL2TX` calls needed.
    /// Plan name (CLAUDE.md): "OnlyD".
    OnlyDeposits,
    /// Only user-initiated L2→L1 groups (withdrawals, multi-call
    /// L2→L1 continuations). Each group needs one `executeL2TX`
    /// call on L1. Plan name (CLAUDE.md): "OnlyW".
    OnlyWithdrawals,
    /// Mix of protocol-triggered and user-initiated groups. This
    /// is a **valid** state per invariant #11 — the unified
    /// intermediate-root chain handles mixed blocks.
    Mixed,
}

/// Pending L1 deferred entries awaiting the next `flush_to_l1`
/// call. See the module comment for the structural rationale.
#[derive(Debug, Clone, Default)]
pub struct PendingL1SubmissionQueue {
    /// All pending L1 deferred entries, in submission order.
    /// Entries are already in L1 format (no pair conversion
    /// needed).
    pub entries: Vec<CrossChainExecutionEntry>,
    /// Trigger groups, in submission order. `groups[k].start` is
    /// the index into `entries` where group `k` begins.
    pub groups: Vec<PendingL1Group>,
}

impl PendingL1SubmissionQueue {
    /// Total number of entries across all groups.
    pub fn len_entries(&self) -> usize {
        self.entries.len()
    }

    /// Total number of trigger groups.
    pub fn num_groups(&self) -> usize {
        self.groups.len()
    }

    /// `true` iff the queue has no entries pending.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append a new group atomically. The caller provides the
    /// entries that belong to the new group (they are appended to
    /// `entries`), the group's chaining mode, and its trigger
    /// metadata.
    pub fn append_group(
        &mut self,
        group_entries: impl IntoIterator<Item = CrossChainExecutionEntry>,
        mode: crate::cross_chain::EntryGroupMode,
        trigger: Option<TriggerMetadata>,
    ) {
        let start = self.entries.len();
        self.entries.extend(group_entries);
        self.groups.push(PendingL1Group {
            start,
            mode,
            trigger,
        });
    }

    /// Truncate the queue to the given entry count and group count.
    /// Used for rollback on build failure (preserves the invariant
    /// that both vectors stay in lock-step).
    pub fn truncate_to(&mut self, entry_len: usize, group_count: usize) {
        self.entries.truncate(entry_len);
        self.groups.truncate(group_count);
    }

    /// Clear all pending state.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.groups.clear();
    }

    /// Drain excess entries from the front of the queue when the
    /// entry count exceeds `max`. Leaves groups untouched — this
    /// mirrors the pre-1.5 behavior of the backpressure path in
    /// `step_builder` which trimmed entries without adjusting
    /// group boundaries (reasoning in that site: the trim is a
    /// safety valve, not a normal operational path).
    pub fn trim_entries_from_front(&mut self, max: usize) {
        if self.entries.len() > max {
            let excess = self.entries.len() - max;
            self.entries.drain(..excess);
        }
    }

    /// Classification of the current entry mix. Closes
    /// invariant #11 by surfacing the Mixed case as an explicit
    /// variant that future code must `match` on.
    pub fn entry_mix(&self) -> BlockEntryMix {
        if self.groups.is_empty() {
            return BlockEntryMix::Empty;
        }
        let mut any_proto = false;
        let mut any_user = false;
        for g in &self.groups {
            if g.trigger.is_some() {
                any_user = true;
            } else {
                any_proto = true;
            }
        }
        match (any_proto, any_user) {
            (true, false) => BlockEntryMix::OnlyDeposits,
            (false, true) => BlockEntryMix::OnlyWithdrawals,
            (true, true) => BlockEntryMix::Mixed,
            // Unreachable: if `groups` is non-empty, at least one
            // of `any_proto` / `any_user` must be true.
            (false, false) => BlockEntryMix::Empty,
        }
    }
}

// ──────────────────────────────────────────────
//  EntryVerificationHold (refactor PLAN step 1.6)
//
//  A tiny state machine that replaces the pair
//
//      pending_entry_verification_block: Option<u64>
//      entry_verify_deferrals: u32
//
//  on `Driver`. The hold is armed AFTER the proposer sends a
//  batch that carries cross-chain entries and BEFORE the receipt
//  lands (§4f nonce safety — see CLAUDE.md "Entry Verification
//  Hold"). While armed, two things happen:
//
//    1. `step_builder` HALTS block production (`is_blocking_build()
//        → true`) to avoid accumulating blocks with advancing L1
//        context that would mismatch after a rewind.
//    2. `flush_to_l1` holds off on submitting new batches until
//        derivation processes the entry-bearing block.
//
//  When derivation verifies the block, the hold is cleared. If the
//  state root mismatches, the hold defers up to
//  `MAX_ENTRY_VERIFY_DEFERRALS` times (the consumption event may
//  land 1–2 L1 blocks after postBatch due to hold-then-forward
//  timing). After exhaustion, `defer()` returns
//  `DeferralResult::MustRewind` with the rewind target pre-computed
//  (`entry_block - 1` per invariant #10).
//
//  ## Closes invariant #14
//
//  "Builder HALTS block production while hold is active." Pre-1.6,
//  this was enforced by a `pending_entry_verification_block.is_some()`
//  check at the top of `step_builder`. With the state machine, the
//  same check is `self.hold.is_blocking_build()` — a
//  self-documenting method call whose name makes the intent
//  obvious, and whose definition lives next to the `arm` / `defer`
//  / `clear` transitions so future readers see the whole lifecycle
//  in one place.
//
//  ## Closes invariant #1 (partial)
//
//  "Hold MUST be set BEFORE `send_to_l1`, not after." The armed
//  precondition for `flush_to_l1`'s "hold then send" path becomes
//  more enforceable via step 1.7's `FlushPlan<HoldArmed>`
//  typestate. Step 1.6 lays the groundwork by giving the hold an
//  `arm` method that is idempotent and takes the entry block
//  explicitly.
// ──────────────────────────────────────────────

/// Maximum number of times `defer()` can be called before
/// `MustRewind` is returned. Hold-then-forward timing means the
/// consumption event can land 1–2 L1 blocks after postBatch; 3
/// deferrals is enough slack for the L1 miner (exponential backoff
/// gives ~14 s of wall time).
pub const MAX_ENTRY_VERIFY_DEFERRALS: u32 = 3;

/// Outcome of [`EntryVerificationHold::defer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferralResult {
    /// The deferral was accepted; retry verification later. The
    /// payload is the deferral count AFTER the increment (so the
    /// first deferral yields `Continue(1)`).
    Continue { deferrals: u32 },
    /// The deferrals have been exhausted and the caller must
    /// rewind. `target` is the pre-computed rewind target —
    /// `entry_block - 1` per invariant #10 ("the entry block
    /// itself gets re-derived, not skipped").
    MustRewind { target: u64 },
    /// `defer()` was called on a `Clear` hold. This is a bug —
    /// deferral only makes sense when the hold is armed. The
    /// caller should log and treat it as a no-op.
    NotArmed,
}

/// Entry-verification hold. See the module comment for the
/// full lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryVerificationHold {
    /// The hold is not active. `step_builder` may build normally,
    /// `flush_to_l1` may submit freely.
    #[default]
    Clear,
    /// The hold is active for a specific entry-bearing L2 block.
    /// Builder production and new submissions are blocked until
    /// derivation verifies the block or the deferrals exhaust.
    Armed {
        /// L2 block number that carries the entries being held.
        entry_block: u64,
        /// Number of times `defer()` has been called for this
        /// armed state. When this reaches
        /// [`MAX_ENTRY_VERIFY_DEFERRALS`], the next `defer()`
        /// returns [`DeferralResult::MustRewind`].
        deferrals: u32,
    },
}

impl EntryVerificationHold {
    /// Arm the hold for a given entry block. **Idempotent**: arming
    /// with the same block is a no-op (so re-entering `flush_to_l1`
    /// after a partial failure does not double-arm). Arming with a
    /// *different* block overwrites the hold and resets the
    /// deferral counter — this mirrors the pre-1.6 behavior where
    /// `pending_entry_verification_block = Some(...)` was
    /// unconditionally assigned and `entry_verify_deferrals` was
    /// never reset on re-assign.
    pub fn arm(&mut self, entry_block: u64) {
        match *self {
            Self::Armed {
                entry_block: current,
                ..
            } if current == entry_block => {
                // Idempotent: same block, same hold, no reset.
            }
            _ => {
                *self = Self::Armed {
                    entry_block,
                    deferrals: 0,
                };
            }
        }
    }

    /// Record a deferral (the derivation check saw a mismatch but
    /// we want to give the consumption event more time to land).
    /// Returns [`DeferralResult::Continue`] while deferrals remain,
    /// [`DeferralResult::MustRewind`] once they are exhausted
    /// (with the target pre-computed as `entry_block - 1` per
    /// invariant #10), or [`DeferralResult::NotArmed`] if the hold
    /// is not armed.
    pub fn defer(&mut self) -> DeferralResult {
        match self {
            Self::Clear => DeferralResult::NotArmed,
            Self::Armed {
                entry_block,
                deferrals,
            } => {
                *deferrals += 1;
                if *deferrals < MAX_ENTRY_VERIFY_DEFERRALS {
                    DeferralResult::Continue {
                        deferrals: *deferrals,
                    }
                } else {
                    let target = entry_block.saturating_sub(1);
                    DeferralResult::MustRewind { target }
                }
            }
        }
    }

    /// Clear the hold. Called by `verify_local_block_matches_l1`
    /// when derivation confirms the entry-bearing block, and by
    /// `clear_internal_state` on rewind.
    pub fn clear(&mut self) {
        *self = Self::Clear;
    }

    /// `true` iff the hold is armed (any state other than
    /// `Clear`). Used by the flush_to_l1 "wait for verification"
    /// check.
    pub fn is_armed(&self) -> bool {
        matches!(self, Self::Armed { .. })
    }

    /// **Closes invariant #14.** `true` iff `step_builder` must
    /// halt block production. Currently identical to `is_armed()`,
    /// but exposed as a separate method so a future refactor that
    /// distinguishes "block production halt" from "submission halt"
    /// has a single site to change.
    pub fn is_blocking_build(&self) -> bool {
        self.is_armed()
    }

    /// The entry block the hold is armed for, if any.
    pub fn armed_for(&self) -> Option<u64> {
        match self {
            Self::Clear => None,
            Self::Armed { entry_block, .. } => Some(*entry_block),
        }
    }

    /// Current deferral count. `0` when clear or freshly armed.
    pub fn deferrals(&self) -> u32 {
        match self {
            Self::Clear => 0,
            Self::Armed { deferrals, .. } => *deferrals,
        }
    }

    /// `true` iff this hold is armed for exactly the given block.
    /// Convenience for the verify path which checks
    /// `pending_entry_verification_block == Some(block)`.
    pub fn is_armed_for(&self, block: u64) -> bool {
        matches!(self, Self::Armed { entry_block, .. } if *entry_block == block)
    }
}

// ──────────────────────────────────────────────
//  FlushPlan<S> typestate (refactor PLAN step 1.7)
//
//  **The central step of Phase 1.** Encodes the "arm the hold BEFORE
//  calling send_to_l1" rule (invariant #1) in the type system via a
//  three-state phantom-type state machine:
//
//      FlushPlan<NoEntries>   — bloques-only, no hold needed
//      FlushPlan<Collected>   — has entries, hold NOT armed yet
//      FlushPlan<HoldArmed>   — has entries, hold armed
//
//  A sealed `Sendable` trait is implemented for `NoEntries` and
//  `HoldArmed` only. `Collected` does NOT implement `Sendable`, so
//  any attempt to `submit_via(...)` a `Collected` plan is a
//  **compile error**. The only way to reach `HoldArmed` is via
//  `FlushPlan<Collected>::arm_hold(&mut EntryVerificationHold)`,
//  which is the function that physically calls `hold.arm(...)`
//  before returning the new phantom state.
//
//  ## Closes invariant #1
//
//  Pre-1.7, the arm-then-send sequence lived in a comment and a
//  hand-discipline rule at the top of `flush_to_l1`:
//
//      // §4f nonce safety: if this batch includes cross-chain
//      // entries, set the hold BEFORE sending to L1.
//      if !l1_entries.is_empty() {
//          self.hold.arm(...);
//      }
//      let send_result = proposer.send_to_l1(&blocks, &l1_entries, ...);
//
//  Any future refactor that moved the arm call after the send
//  (intentionally or by copy-paste error) would produce the bug
//  described in CLAUDE.md ("If set after and tx fails, hold is
//  active with no way to clear it"). With `FlushPlan<S>`, moving
//  the arm call after the send is **uncompilable**: the send path
//  takes `FlushPlan<S: Sendable>`, and `Collected` is not
//  `Sendable`.
//
//  ## Why typestate (and not a runtime check)
//
//  The same guarantee *could* be enforced at runtime with a
//  boolean field on the plan, but that's just moving the bug one
//  layer up: the caller has to remember to check the boolean. The
//  typestate pushes the check into the compiler, which never
//  forgets.
//
//  Three other benefits the typestate locks in:
//
//    1. **Ownership.** `FlushPlan` OWNS the blocks / entries /
//       groups. Nothing borrows across `.await`, which eliminates a
//       class of async-borrow bugs.
//
//    2. **entry_block travels inside the plan.** Pre-1.7, the arm
//       call did
//       `self.hold.arm(blocks.last().unwrap().l2_block_number)` at
//       the use site — fine today, but a future refactor that
//       computes `entry_block` from a different slice could arm
//       the hold for the wrong block. Now, `arm_hold` reads
//       `entry_block` from the plan itself, and `entry_block` is
//       set exactly once in `FlushPlan::new_collected`.
//
//    3. **`SendResult` is `#[must_use]`.** Ignoring the submit
//       result is a warning that `-D warnings` turns into an
//       error.
//
//  ## Scope decision
//
//  The plan (§8 1.7) sketches `Proposer::send_to_l1` taking
//  `FlushPlan<S: Sendable>` directly. That would push the typestate
//  all the way down into the proposer API and force every test
//  that calls `proposer.submit_to_l1(&blocks, &[])` to construct a
//  `FlushPlan`. There are ~10 such sites (`proposer_tests.rs` +
//  `e2e_anvil.rs`) that have no concept of the hold — they're pure
//  unit tests of the proposer's L1 sending logic, not of the
//  driver's flush sequence.
//
//  Per the same partial-migration discipline used in 1.2 (state
//  roots) and 1.4 (`EntryClass`), this step migrates the
//  **driver-facing** flush path to `FlushPlan` and leaves
//  `Proposer::send_to_l1` unchanged. The compile-time guarantee
//  lives on `FlushPlan::submit_via`: the driver's only way to
//  reach `proposer.send_to_l1` is through a `FlushPlan<S:
//  Sendable>`, so invariant #1 is closed at the driver level —
//  which is where the bug would live. Tests bypass `FlushPlan`
//  entirely and use the proposer's direct API; they do not arm
//  holds and are not subject to invariant #1.
// ──────────────────────────────────────────────

mod flush_plan_sealed {
    /// Sealed super-trait: only types inside this module can
    /// implement [`super::Sendable`]. Prevents downstream code
    /// from marking arbitrary phantom types as sendable.
    pub trait Sealed {}
    impl Sealed for super::NoEntries {}
    impl Sealed for super::HoldArmed {}
}

/// Phantom marker for `FlushPlan<NoEntries>` — the plan has only
/// pending blocks, no cross-chain entries. No hold is needed because
/// there are no entries whose consumption we await. `NoEntries` is
/// `Sendable`.
#[derive(Debug)]
pub enum NoEntries {}

/// Phantom marker for `FlushPlan<Collected>` — entries are present
/// and the hold is **NOT** yet armed. `Collected` is **NOT**
/// `Sendable`: the only way to make a `Collected` plan sendable is
/// to call [`FlushPlan::arm_hold`], which transitions to
/// [`HoldArmed`] by physically arming the `EntryVerificationHold`.
#[derive(Debug)]
pub enum Collected {}

/// Phantom marker for `FlushPlan<HoldArmed>` — entries are present
/// and the hold is armed. Only reachable via
/// `FlushPlan<Collected>::arm_hold`. `HoldArmed` is `Sendable`.
#[derive(Debug)]
pub enum HoldArmed {}

/// Sealed marker trait: types that `FlushPlan::submit_via` will
/// accept. Implemented only by [`NoEntries`] and [`HoldArmed`].
/// Because the trait is sealed, downstream code cannot add new
/// `Sendable` states to bypass the arm-then-send sequence.
pub trait Sendable: flush_plan_sealed::Sealed {}
impl Sendable for NoEntries {}
impl Sendable for HoldArmed {}

/// Result of `FlushPlan::submit_via`. `#[must_use]` — ignoring
/// the return value is a warning promoted to an error via
/// `-D warnings`. Carries enough information for the caller to
/// decide whether to rewind, cooldown, or clear the hold on
/// failure.
#[derive(Debug)]
#[must_use = "SendResult must be consumed — invariant #1 requires the caller \
              to decide whether to rewind, cooldown, or clear the hold on failure"]
pub enum SendResult {
    /// The L1 `postBatch` tx was submitted successfully. The
    /// carried `B256` is the tx hash; the caller must call
    /// `Proposer::wait_for_l1_receipt(tx_hash)` to confirm
    /// mining.
    Ok { tx_hash: B256 },
    /// The L1 submission failed. The caller must handle the
    /// failure by:
    ///
    /// - Clearing the hold (if it was armed) OR letting the
    ///   rewind path clear it.
    /// - Putting the blocks and entries back into their
    ///   respective queues (the caller still owns them via the
    ///   values returned in `rollback`).
    /// - Setting `last_submission_failure` so subsequent cycles
    ///   hit the cooldown.
    Failed {
        /// The error describing why the submission failed.
        error: eyre::Report,
        /// The rollback package — the blocks and entries the
        /// caller must restore to the driver state so they are
        /// not lost.
        rollback: RollbackPackage,
    },
}

/// Owned state returned by `SendResult::Failed` so the caller can
/// restore the driver's queues after a failed submit.
#[derive(Debug)]
pub struct RollbackPackage {
    /// The blocks that were drained from `pending_submissions` but
    /// not submitted.
    pub blocks: Vec<PendingBlock>,
    /// The L1 entry queue that was drained but not submitted.
    pub pending_l1: PendingL1SubmissionQueue,
}

/// Typestate flush plan. See the module comment for the full
/// rationale.
#[derive(Debug)]
pub struct FlushPlan<S> {
    /// Pending blocks drained from `pending_submissions`. May be
    /// empty for an entries-only submission (rare).
    blocks: Vec<PendingBlock>,
    /// L1 entry queue drained from the driver's
    /// `PendingL1SubmissionQueue`. Empty for `NoEntries`.
    pending_l1: PendingL1SubmissionQueue,
    /// The block number whose consumption the hold must be armed
    /// for. `None` when the plan has no entries; `Some` for
    /// `Collected` / `HoldArmed`. The plan computes this exactly
    /// once at construction time so it cannot drift.
    entry_block: Option<u64>,
    /// Phantom marker — zero-cost at runtime.
    _marker: PhantomData<fn() -> S>,
}

impl FlushPlan<NoEntries> {
    /// Construct a blocks-only plan. Used when the driver has
    /// `PendingBlock`s to submit but no cross-chain entries
    /// pending. Because there are no entries, the hold does not
    /// need to be armed — `NoEntries` is directly `Sendable`.
    pub fn new_blocks_only(blocks: Vec<PendingBlock>) -> Self {
        Self {
            blocks,
            pending_l1: PendingL1SubmissionQueue::default(),
            entry_block: None,
            _marker: PhantomData,
        }
    }
}

impl FlushPlan<Collected> {
    /// Construct a plan that holds both blocks and cross-chain
    /// entries. The `entry_block` is captured from `blocks.last()`
    /// exactly once, so `arm_hold` cannot be mis-armed for a
    /// different block. `Collected` is **NOT** `Sendable` —
    /// the only way to reach a sendable state is via
    /// [`FlushPlan::arm_hold`].
    pub fn new_collected(
        blocks: Vec<PendingBlock>,
        pending_l1: PendingL1SubmissionQueue,
    ) -> Self {
        // `entry_block` is the last block's number if any blocks
        // are present; otherwise `None`. The pre-1.7 code did
        // `self.hold.arm(blocks.last().unwrap().l2_block_number)`
        // at the use site with an `if` guard on `!l1_entries.is_empty()`;
        // we compute it once here so the arm call site is
        // trivially correct.
        let entry_block = blocks.last().map(|b| b.l2_block_number);
        Self {
            blocks,
            pending_l1,
            entry_block,
            _marker: PhantomData,
        }
    }

    /// Arm the hold for this plan's `entry_block` and transition
    /// to [`HoldArmed`]. This is the **only** way to turn a
    /// `Collected` plan into a `Sendable` one.
    ///
    /// Idempotent: if `hold` is already armed for the same block,
    /// the arm is a no-op (delegated to
    /// `EntryVerificationHold::arm`).
    ///
    /// If the plan has entries but no blocks (rare), the arm is
    /// skipped — there is no L2 block to associate the hold with,
    /// so the caller falls back to the pre-existing behavior of
    /// not arming.
    pub fn arm_hold(self, hold: &mut EntryVerificationHold) -> FlushPlan<HoldArmed> {
        if let Some(eb) = self.entry_block {
            hold.arm(eb);
        }
        FlushPlan {
            blocks: self.blocks,
            pending_l1: self.pending_l1,
            entry_block: self.entry_block,
            _marker: PhantomData,
        }
    }
}

impl<S: Sendable> FlushPlan<S> {
    /// Submit this plan via a proposer. Consumes the plan and
    /// returns a `SendResult` the caller must handle.
    ///
    /// **This is the compile-time gate for invariant #1.** The
    /// `S: Sendable` bound rejects `FlushPlan<Collected>` at
    /// compile time, so any code path that tries to submit
    /// without going through `arm_hold` produces a type error.
    ///
    /// On failure, the returned `SendResult::Failed { rollback,
    /// .. }` carries the drained state back to the caller so the
    /// driver can restore its queues without losing any blocks
    /// or entries.
    pub async fn submit_via(
        self,
        proposer: &Proposer,
        gas_hint: Option<GasPriceHint>,
    ) -> SendResult {
        let Self {
            blocks, pending_l1, ..
        } = self;
        let entries_slice: &[CrossChainExecutionEntry] = &pending_l1.entries;
        match proposer.send_to_l1(&blocks, entries_slice, gas_hint).await {
            Ok(tx_hash) => SendResult::Ok { tx_hash },
            Err(error) => SendResult::Failed {
                error,
                rollback: RollbackPackage { blocks, pending_l1 },
            },
        }
    }
}

impl<S> FlushPlan<S> {
    /// Number of pending blocks in this plan.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Number of L1 entries in this plan.
    pub fn entry_count(&self) -> usize {
        self.pending_l1.len_entries()
    }

    /// `true` iff there are neither blocks nor entries to submit.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.pending_l1.is_empty()
    }
}

/// Stage ID for the persistent transaction replay journal.
/// Stores user transaction bytes for recovery after rewinds and crashes.
const TX_JOURNAL_STAGE_ID: StageId = StageId::Other("TxJournal");

/// A single entry in the persistent transaction replay journal.
///
/// Stores the L2 block number and the full RLP-encoded transaction list for
/// that block. Written at block build time, pruned after L1 confirmation.
/// Used to recover user transactions after crashes (startup recovery).
#[derive(Clone)]
struct TxJournalEntry {
    l2_block_number: u64,
    /// Full encoded_transactions bytes (RLP-encoded list, includes protocol txs).
    /// Protocol txs are filtered out on recovery.
    block_txs: Vec<u8>,
}

impl TxJournalEntry {
    /// Serialize a list of journal entries to bytes.
    fn encode_all(entries: &[TxJournalEntry]) -> Vec<u8> {
        let mut buf = Vec::new();
        for entry in entries {
            buf.extend_from_slice(&entry.l2_block_number.to_le_bytes());
            let len = entry.block_txs.len() as u32;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&entry.block_txs);
        }
        buf
    }

    /// Deserialize a list of journal entries from bytes.
    fn decode_all(data: &[u8]) -> Vec<TxJournalEntry> {
        let mut entries = Vec::new();
        let mut pos = 0;
        while pos + 12 <= data.len() {
            let block_bytes: [u8; 8] = match data[pos..pos + 8].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let l2_block_number = u64::from_le_bytes(block_bytes);
            let len_bytes: [u8; 4] = match data[pos + 8..pos + 12].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let tx_len = u32::from_le_bytes(len_bytes) as usize;
            pos += 12;
            if pos + tx_len > data.len() {
                break;
            }
            let block_txs = data[pos..pos + tx_len].to_vec();
            pos += tx_len;
            entries.push(TxJournalEntry {
                l2_block_number,
                block_txs,
            });
        }
        entries
    }
}

/// RLP-encode a slice of transactions into a single bytes blob for L1 submission.
fn encode_block_transactions(txs: &[reth_ethereum_primitives::TransactionSigned]) -> Bytes {
    let mut buf = Vec::new();
    alloy_rlp::encode_list(txs, &mut buf);
    Bytes::from(buf)
}

/// Number of recent block hashes to keep for safe/finalized tracking.
const FORK_CHOICE_DEPTH: usize = 64;

/// Save L1 derivation checkpoint to DB every N L1 blocks during sync.
const CHECKPOINT_INTERVAL: u64 = 64;

/// Maximum backoff duration on repeated errors (seconds).
const MAX_BACKOFF_SECS: u64 = 60;

/// Cooldown after a failed L1 submission before retrying (seconds).
const SUBMISSION_COOLDOWN_SECS: u64 = 5;

/// Maximum number of blocks to submit in a single L1 batch transaction.
const MAX_BATCH_SIZE: usize = 100;

/// Maximum pending submissions queue size. Prevents unbounded memory growth
/// when L1 transactions are not confirming (e.g., gas too low, stuck nonce).
const MAX_PENDING_SUBMISSIONS: usize = 1000;

/// Maximum pending cross-chain entries queue size. Prevents unbounded memory
/// growth when L1 cross-chain submissions are failing or slow.
const MAX_PENDING_CROSS_CHAIN_ENTRIES: usize = 1000;

/// Number of consecutive L1 RPC failures before switching to the fallback provider.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Minimum interval between L1 RPC calls (rate limiting during catchup).
const MIN_L1_CALL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum retries when engine returns SYNCING for a fork choice update.
/// Total worst-case wait: 100+200+400+800+1600+3200 = ~6.3s.
const FCU_SYNCING_MAX_RETRIES: u32 = 6;

/// Initial backoff for SYNCING retries (doubles each attempt).
const FCU_SYNCING_INITIAL_BACKOFF_MS: u64 = 100;

/// Desired gas limit target for block building. Set to 60M to match Ethereum
/// mainnet's current gas limit. Must match the payload builder's default.
const DESIRED_GAS_LIMIT: u64 = 60_000_000;

/// Compute the gas limit for the next block, bounded by the EIP-1559 elasticity divisor (1024).
/// Mirrors `alloy_eips::eip1559::helpers::calculate_block_gas_limit` exactly — verified by
/// `test_calc_gas_limit_matches_reth`.
///
/// NOTE: The `saturating_sub(1)` is intentional and matches both alloy's canonical implementation
/// and go-ethereum's `core/block_validator.go`. This means: at parent_gas_limit <= 1024 the delta
/// is 0, effectively locking the gas limit (acceptable since real chains never have limits that low).
fn calc_gas_limit(parent_gas_limit: u64, desired_gas_limit: u64) -> u64 {
    let delta = (parent_gas_limit / 1024).saturating_sub(1);
    let min_limit = parent_gas_limit.saturating_sub(delta);
    let max_limit = parent_gas_limit.saturating_add(delta);
    desired_gas_limit.clamp(min_limit, max_limit)
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
        queued_cross_chain_calls: Arc<std::sync::Mutex<Vec<crate::rpc::QueuedCrossChainCall>>>,
        pending_l1_forward_txs: Arc<std::sync::Mutex<Vec<Bytes>>>,
        queued_l2_to_l1_calls: Arc<std::sync::Mutex<Vec<crate::rpc::QueuedL2ToL1Call>>>,
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
    fn get_l1_provider(&self) -> &RootProvider {
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

    /// Builder mode: build blocks from the mempool and submit to L1.
    ///
    /// The builder:
    /// 1. Checks L1 for any blocks submitted by others
    /// 2. Builds new blocks up to the current time
    /// 3. Submits pending blocks to L1 in batches
    async fn step_builder(&mut self, latest_l1_block: u64) -> Result<()> {
        let provider = self.get_l1_provider().clone();

        // Check if there are new L1 blocks we haven't processed
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
                continue;
            }

            // If a rewind was triggered during verification, do NOT commit the
            // batch — the cursor must stay so blocks are re-derived after the
            // rewind completes. Return early to avoid wasted block building and
            // L1 gas expenditure with incorrect state roots.
            if self.pending_rewind_target.is_some() {
                return Ok(());
            }

            // All blocks processed successfully — commit the cursor state.
            self.derivation.commit_batch(&batch);
            self.maybe_save_checkpoint()?;
        }

        // Wait for at least one L1 block after deployment before building.
        // The L1 context rule is: containing_l1_block - 1. The builder uses latest_l1_block
        // as context, so we need latest_l1_block > deployment_l1_block to ensure the
        // submitted tx (landing in latest_l1_block + 1) produces matching context.
        if latest_l1_block <= self.config.deployment_l1_block {
            debug!(
                target: "based_rollup::driver",
                latest_l1_block,
                deployment_l1_block = self.config.deployment_l1_block,
                "waiting for L1 to advance past deployment block before building"
            );
            return Ok(());
        }

        // Derive the target L2 block deterministically from the L1 head.
        // l2_block_number(N) = N - deployment_l1_block.  With the +1 offset in
        // l2_timestamp(), L2 block K has timestamp equal to L1 block (dep + K + 1).
        // The builder targets the next L1 block (latest + 1) for postBatch, so
        // building up to l2_block_number(latest) produces a block whose timestamp
        // matches that next L1 block exactly.  No wall-clock dependency.
        let target_l2_block = self.config.l2_block_number(latest_l1_block);

        // Sanity check: cap the catch-up gap to prevent runaway block production
        // (e.g., builder restarting far behind L1 head).
        const MAX_CATCHUP_BLOCKS: u64 = 10_000;
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

        // Early return if nothing to build
        if self.l2_head_number >= effective_target {
            return Ok(());
        }

        // Fetch L1 block hash for current L1 head
        let mut current_l1_block = latest_l1_block;
        let mut l1_hash = provider
            .get_block_by_number(current_l1_block.into())
            .await?
            .ok_or_else(|| eyre::eyre!("L1 block {current_l1_block} not found"))?
            .header
            .hash;

        // Don't build new blocks while waiting for entry verification.
        // Building during hold accumulates blocks with advancing L1 context
        // that will mismatch after rewind, causing a double rewind cycle.
        // Check BEFORE draining queues so entries accumulate in the shared
        // queues until the hold clears, avoiding the bug where drained entries
        // are lost on return and held L2 txs execute without loadExecutionTable.
        //
        // Closes invariant #14 — `is_blocking_build` is the typed gate
        // that replaces the pre-1.6 `.is_some()` check on
        // `pending_entry_verification_block`.
        if self.hold.is_blocking_build() {
            return Ok(());
        }

        // Fetch cross-chain execution entries for builder blocks.
        // These are L1-fetched entries (incoming calls from other rollups, already
        // consumed on L1). They are NOT submitted to L1 — they came FROM L1.
        let mut builder_execution_entries = self
            .derivation
            .fetch_execution_entries_for_builder(current_l1_block, &provider)
            .await?;

        // Track how many RPC entries are appended to builder_execution_entries.
        // L1-fetched entries are at the front, RPC entries at the back.
        // This counter is reset on L1 refresh (line below) or when entries are consumed.
        let mut rpc_entry_count_in_builder: usize = 0;

        // Drain unified cross-chain call queue, sort by gas price descending
        // (matching L1 miner ordering), then merge for same-block execution.
        // These entries are executed immediately in the next built block, then also
        // posted to L1 (via pending_l1_entries) so fullnodes can derive
        // identical blocks from L1 events.
        //
        // Sorting MUST happen before entries flow to `attach_chained_state_deltas`,
        // because chained deltas assume sequential consumption order. The L1 miner
        // orders user txs by gas price descending, so entries must match.
        //
        // NOTE: stale entry guard was removed — loadExecutionTable now deletes
        // existing entries per actionHash before pushing new ones, so stale entries
        // from prior blocks are automatically cleared on the next load.
        // Save pre-drain lengths so we can truncate self.pending_l1_* on build
        // failure and re-push drained entries back to the shared queues. Without
        // this, entries are permanently lost when clear_internal_state() runs
        // during the Sync transition. See issue #237.
        let pre_drain_l1_len = self.pending_l1.len_entries();
        let pre_drain_l1_groups = self.pending_l1.num_groups();

        let mut queued_l1_txs_for_block: Vec<Bytes> = Vec::new();
        // Saved originals for re-push on build failure. These are the
        // processed calls (after sort + continuation dedup) that would be
        // lost if block building fails before flush_to_l1 runs.
        let mut calls_for_repush: Vec<crate::rpc::QueuedCrossChainCall> = Vec::new();
        {
            let mut queue = self
                .queued_cross_chain_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !queue.is_empty() {
                let mut calls: Vec<_> = queue.drain(..).collect();
                // Sort by gas price descending — matches L1 miner tx ordering
                calls.sort_by(|a, b| {
                    b.effective_gas_price().cmp(&a.effective_gas_price())
                });

                info!(
                    target: "based_rollup::driver",
                    count = calls.len(),
                    gas_prices = ?calls.iter().map(|c| c.effective_gas_price()).collect::<Vec<_>>(),
                    "merging RPC cross-chain entries (sorted by gas price)"
                );

                // One continuation per cycle: if a call has non-empty l1_entries
                // (multi-call continuation), only process the FIRST such call.
                // Re-queue the rest to prevent multiple continuations'
                // entries from being mixed in pending_l1_entries.
                let mut had_continuation = false;
                let mut rpc_entries: Vec<CrossChainExecutionEntry> = Vec::new();
                for call in calls {
                    let is_continuation = call.is_continuation();
                    if is_continuation && had_continuation {
                        // Re-queue this continuation for the next cycle
                        queue.push(call);
                        continue;
                    }
                    if is_continuation {
                        had_continuation = true;
                    }

                    let group_mode = call.l1_independent_entries();
                    let raw_l1_tx_for_forward = call.raw_l1_tx().clone();
                    // Collect the L1 entries for this group locally first,
                    // then append them atomically via `append_group` so the
                    // (entries, groups) invariant stays intact.
                    let group_l1_entries: Vec<CrossChainExecutionEntry> = match &call {
                        crate::rpc::QueuedCrossChainCall::Simple {
                            call_entry,
                            result_entry,
                            ..
                        } => {
                            // Terminal failure: delivery ALWAYS fails (e.g., RevertCounter).
                            // RESULT(failed=true) with non-empty revert data after enrichment
                            // = true terminal failure. Skip L2 entries — protocol specifies no
                            // loadExecutionTable for terminal reverts.
                            // Simulation artifacts are protocol errors (ExecutionNotFound, etc.)
                            // that only occur when entries aren't loaded yet.
                            let is_terminal_failure = result_entry.next_action.failed
                                && !crate::cross_chain::is_simulation_artifact(
                                    &result_entry.next_action.data,
                                );
                            if !is_terminal_failure {
                                // Simple deposit: CALL trigger + RESULT table entry
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
                            // Multi-call continuation: continuation entries provide their
                            // own RESULT entries via the chain. Push the entire
                            // `l2_table_entries` sequence — invariant #6 ("never include
                            // a result_entry alongside continuations") is enforced by
                            // the type: this variant has no `result_entry` field at all.
                            rpc_entries.extend(l2_table_entries.iter().cloned());

                            // Continuation: use pre-built L1 entries directly
                            l1_entries.clone()
                        }
                    };

                    if !raw_l1_tx_for_forward.is_empty() {
                        queued_l1_txs_for_block.push(raw_l1_tx_for_forward);
                    }

                    // Atomic append: entries + group metadata grow together.
                    // protocol trigger group → no L1 executeL2TX needed (trigger=None).
                    self.pending_l1.append_group(group_l1_entries, group_mode, None);

                    // Save for re-push on build failure (call is consumed by value)
                    calls_for_repush.push(call);
                }
                rpc_entry_count_in_builder = rpc_entries.len();
                builder_execution_entries.extend(rpc_entries);
            }
        }
        // L1 forward txs are NOT committed to pending_l1_forward_txs yet.
        // They are committed after the block build loop succeeds to avoid
        // orphaned txs stuck in the queue on build failure. See issue #237.

        // Drain L2→L1 call queue — no mutual exclusion, deposits and L2→L1 calls
        // can coexist in the same block. The unified intermediate root chain
        // handles mixed blocks correctly.
        let mut held_l2_txs: Vec<Bytes> = Vec::new();
        // Saved originals for re-push on build failure. See issue #237.
        let mut l2_to_l1_for_repush: Vec<crate::rpc::QueuedL2ToL1Call> = Vec::new();
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
                    protocol_entries = rpc_entry_count_in_builder,
                    "draining L2→L1 call queue (unified intermediate roots)"
                );
                // Save a clone of the drained calls BEFORE processing
                // so they can be re-pushed to the shared queue if block
                // building fails. This prevents permanent entry loss.
                l2_to_l1_for_repush = l2_to_l1_calls.clone();
                for w in l2_to_l1_calls {
                    // Collect held L2 txs for pool injection (hold-then-forward)
                    if !w.raw_l2_tx.is_empty() {
                        held_l2_txs.push(w.raw_l2_tx);
                    }
                    // L2 table entries → loaded via loadExecutionTable in this block
                    let w_entry_count = w.l2_table_entries.len();
                    builder_execution_entries.extend(w.l2_table_entries.iter().cloned());
                    // Count L2→L1 entries as RPC entries so the position-based
                    // split (base vs RPC) correctly includes them in rpc_entries_for_block.
                    rpc_entry_count_in_builder += w_entry_count;

                    // Populate L1 entry queue — L2→L1 groups are always
                    // `Chained` and always carry user-trigger metadata
                    // (distinguishing them from protocol-triggered deposits
                    // in `entry_mix`).
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
        // This ensures entries are loaded (via protocol txs) in the same block
        // as the user's tx — eliminating the ExecutionNotFound timing race.
        if !held_l2_txs.is_empty() {
            self.inject_held_l2_txs(&held_l2_txs).await;
        }

        // During catch-up, refresh L1 context every N blocks to avoid all catch-up
        // blocks sharing the same L1 context (which causes mass rewind if the batch
        // submission lands in a different L1 block).
        const L1_REFRESH_INTERVAL: u64 = 100;
        let mut blocks_since_l1_refresh: u64 = 0;

        while self.l2_head_number < effective_target {
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
            let is_last_block = next_l2_block >= effective_target;
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
                    // Re-push drained entries back to shared queues so they are
                    // not lost when clear_internal_state() runs. See issue #237.
                    self.pending_l1.truncate_to(pre_drain_l1_len, pre_drain_l1_groups);
                    if !calls_for_repush.is_empty() {
                        let mut q = self
                            .queued_cross_chain_calls
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        warn!(
                            target: "based_rollup::driver",
                            count = calls_for_repush.len(),
                            "re-pushing cross-chain calls to shared queue after build failure"
                        );
                        q.extend(calls_for_repush.iter().cloned());
                    }
                    if !l2_to_l1_for_repush.is_empty() {
                        let mut q = self
                            .queued_l2_to_l1_calls
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        warn!(
                            target: "based_rollup::driver",
                            count = l2_to_l1_for_repush.len(),
                            "re-pushing L2→L1 calls to shared queue after build failure"
                        );
                        q.extend(l2_to_l1_for_repush.iter().cloned());
                    }
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
                    // Re-push drained entries back to shared queues so they are
                    // not lost when clear_internal_state() runs. See issue #237.
                    self.pending_l1.truncate_to(pre_drain_l1_len, pre_drain_l1_groups);
                    if !calls_for_repush.is_empty() {
                        let mut q = self
                            .queued_cross_chain_calls
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        warn!(
                            target: "based_rollup::driver",
                            count = calls_for_repush.len(),
                            "re-pushing cross-chain calls to shared queue after build failure"
                        );
                        q.extend(calls_for_repush.iter().cloned());
                    }
                    if !l2_to_l1_for_repush.is_empty() {
                        let mut q = self
                            .queued_l2_to_l1_calls
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        warn!(
                            target: "based_rollup::driver",
                            count = l2_to_l1_for_repush.len(),
                            "re-pushing L2→L1 calls to shared queue after build failure"
                        );
                        q.extend(l2_to_l1_for_repush.iter().cloned());
                    }
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

            // Compute unified intermediate state roots for chained cross-chain
            // entry deltas. All entry types (deposits, L2→L1 calls, continuations)
            // are handled in a single root chain via trigger group counting.
            let has_rpc_entries = !rpc_entries_for_block.is_empty();
            let our_rollup_id = crate::cross_chain::RollupId::new(alloy_primitives::U256::from(
                self.config.rollup_id,
            ));
            let num_protocol_triggers = rpc_entries_for_block
                .iter()
                .filter(|e| {
                    // Only count true triggers, NOT continuation table entries.
                    // Triggers have hash(next_action) == action_hash (same guard as
                    // partition_entries). Continuations have action_hash=hash(RESULT)
                    // but next_action=CALL_B, so hash(next_action) != action_hash.
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
            let has_entries = has_rpc_entries || num_user_triggers > 0;

            let mut intermediate_roots = Vec::new();
            // The clean state root is constructed exactly here. This is the
            // canonical (and currently only) call site that turns a raw
            // `B256` into a `CleanStateRoot`. `cross_chain::CleanStateRoot::new`
            // is `pub(crate)`, so any future attempt to fabricate a clean
            // root from a freshly-computed value somewhere else (the
            // anti-pattern that invariant #3 forbids) will not compile.
            let clean_state_root = if has_entries {
                match self.compute_intermediate_roots(
                    next_l2_block.saturating_sub(1),
                    next_timestamp,
                    l1_hash,
                    current_l1_block,
                    built.state_root,
                    &built.encoded_transactions,
                ) {
                    Ok(roots) => {
                        let clean = roots[0];
                        info!(
                            target: "based_rollup::driver",
                            l2_block = next_l2_block,
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
                            l2_block = next_l2_block,
                            %err,
                            "failed to compute intermediate state roots — \
                             discarding cross-chain entries for this block to prevent \
                             corrupt state root in IMMEDIATE entry"
                        );
                        // Clear entries to prevent submitting with wrong state deltas.
                        // Entries will be re-queued on the next builder cycle.
                        self.pending_l1.clear();
                        // No entries → speculative IS clean.
                        crate::cross_chain::CleanStateRoot::new(built.state_root)
                    }
                }
            } else {
                crate::cross_chain::CleanStateRoot::new(built.state_root)
            };

            // Attach correct state deltas to all pending L1 entries using the
            // intermediate roots from compute_intermediate_roots.
            if !self.pending_l1.is_empty() && !intermediate_roots.is_empty() {
                // `attach_generic_state_deltas` still takes a `&[usize]` of
                // group start indices — rebuild a temporary view rather than
                // changing the cross_chain API surface in 1.5.
                let group_starts: Vec<usize> =
                    self.pending_l1.groups.iter().map(|g| g.start).collect();
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
                // In independent groups, L1 try/catch rolls back the reverted call's
                // state, so all entries see the same pre-root. Override ALL entries
                // in the group to use intermediate_roots[k] as currentState.
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

                // Log composite entry hashes (VerifyL1Batch format) for byte-level debugging.
                // composite = keccak256(abi.encode(actionHash, keccak256(abi.encode(nextAction))))
                for (i, e) in self.pending_l1.entries.iter().enumerate() {
                    use alloy_sol_types::SolType as _;
                    let next_action_encoded =
                        crate::cross_chain::ICrossChainManagerL2::Action::abi_encode(
                            &e.next_action.to_sol_action(),
                        );
                    let next_action_hash = alloy_primitives::keccak256(&next_action_encoded);
                    // abi.encode(bytes32, bytes32) = 64 bytes concatenated
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

    /// Submit pending blocks and cross-chain entries to L1 via the proposer.
    ///
    /// Combines block submission and cross-chain entry posting into a single
    /// `submit_to_l1` call. Drains external cross-chain entries from the shared
    /// queue, collects pending blocks, and sends everything in one L1 transaction.
    async fn flush_to_l1(&mut self) -> Result<()> {
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
            return Ok(());
        };

        // NOTE: No secondary drain of queued_cross_chain_calls here.
        // Entries arriving after step_builder's drain wait in the shared queue
        // for the next step_builder tick (1 second). Draining here would pick up
        // entries that arrived AFTER the block was built — they'd get L1 entries
        // but no corresponding L2 block, causing orphaned entries with zero
        // state deltas.

        if self.pending_submissions.is_empty() && self.pending_l1.is_empty() {
            return Ok(());
        }

        // Entry verification hold (§4f nonce safety): if an entry-bearing batch
        // was flushed but derivation hasn't confirmed it yet, hold off on new
        // submissions. The builder keeps building blocks into the pending queue,
        // but doesn't post them until derivation verifies the entry block.
        // Once verified (by verify_local_block_matches_l1 or clear_pending_state),
        // the flag is cleared and accumulated blocks are posted normally.
        if let Some(entry_block) = self.hold.armed_for() {
            info!(
                target: "based_rollup::driver",
                entry_block,
                pending_blocks = self.pending_submissions.len(),
                "holding submissions — awaiting derivation verification of entry-bearing block"
            );
            return Ok(());
        }

        // Check submission cooldown
        if let Some(last_fail) = self.last_submission_failure {
            if last_fail.elapsed() < std::time::Duration::from_secs(SUBMISSION_COOLDOWN_SECS) {
                return Ok(());
            }
        }

        // Periodically check wallet balance (every 5 minutes)
        if self.last_balance_check.elapsed() > std::time::Duration::from_secs(300) {
            let _ = proposer.check_wallet_balance().await;
            self.last_balance_check = std::time::Instant::now();
        }

        // Skip blocks already submitted on-chain by comparing state roots.
        // last_submitted_state_root() returns the on-chain state root for our
        // rollup. We drain pending blocks whose state_root matches or precedes
        // the on-chain root (i.e., they are already submitted).
        //
        // With protocol tx filtering (§4f), derivation produces the correct root
        // for any consumption level. The on-chain root after postBatch is the
        // clean root Y' (with loadTable effects). If entries are consumed, it
        // evolves to X'_k. We check state_root (full consumption) and
        // clean_state_root (zero consumption). For partial consumption, the
        // on-chain root won't match either — the block stays in the queue,
        // flush_to_l1 detects the mismatch, and triggers a rewind. After
        // re-derivation with filtered txs, the correct root is produced.
        let on_chain_root = match proposer.last_submitted_state_root().await {
            Ok(root) => {
                if root != B256::ZERO {
                    if let Some(pos) = self.pending_submissions.iter().rposition(|b| {
                        b.state_root == root
                            || b.clean_state_root.as_b256() == root
                            || b.intermediate_roots.contains(&root)
                    }) {
                        // Drain blocks 0..=pos (already on-chain)
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
                return Ok(());
            }
        };

        if self.pending_submissions.is_empty() && self.pending_l1.is_empty() {
            return Ok(());
        }

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
                        self.clear_internal_state();
                        self.derivation.set_last_derived_l2_block(rewind_target);
                        self.derivation.rollback_to(rollback_l1_block);
                        self.mode = DriverMode::Sync;
                        self.synced
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        self.consecutive_rewind_cycles =
                            self.consecutive_rewind_cycles.saturating_add(1);
                        self.set_rewind_target(rewind_target);
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
                        self.clear_internal_state();
                        self.derivation.set_last_derived_l2_block(rewind_target);
                        self.derivation.rollback_to(rollback_l1_block);
                        self.mode = DriverMode::Sync;
                        self.synced
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        self.consecutive_rewind_cycles =
                            self.consecutive_rewind_cycles.saturating_add(1);
                        self.set_rewind_target(rewind_target);
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
        let block_l2_numbers: Vec<u64> =
            blocks.iter().map(|b| b.l2_block_number).collect();

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
                if !block_l2_numbers.is_empty() {
                    let first = *block_l2_numbers.first().unwrap();
                    let last = *block_l2_numbers.last().unwrap();
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
                            self.clear_internal_state();
                            self.derivation.set_last_derived_l2_block(rewind_target);
                            self.derivation.rollback_to(rollback_l1_block);
                            self.mode = DriverMode::Sync;
                            self.synced
                                .store(false, std::sync::atomic::Ordering::Relaxed);
                            self.consecutive_rewind_cycles =
                                self.consecutive_rewind_cycles.saturating_add(1);
                            self.set_rewind_target(rewind_target);
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
                        if !trigger_tx_hashes.is_empty() {
                            let proposer = self.proposer.as_ref().expect("checked above");
                            let mut any_trigger_failed = false;
                            for trigger_hash in &trigger_tx_hashes {
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
                                        any_trigger_failed = true;
                                    }
                                }
                            }
                            if any_trigger_failed {
                                // With intermediate state roots, the on-chain stateRoot
                                // is at an intermediate root (partial consumption).
                                // Derivation can filter unconsumed L2→L1 txs to
                                // produce the matching root via §4f. Rewind to re-derive.
                                warn!(
                                    target: "based_rollup::driver",
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
                                self.clear_internal_state();
                                self.derivation.set_last_derived_l2_block(rewind_target);
                                self.derivation.rollback_to(rollback_l1_block);
                                self.mode = DriverMode::Sync;
                                self.synced
                                    .store(false, std::sync::atomic::Ordering::Relaxed);
                                self.consecutive_rewind_cycles =
                                    self.consecutive_rewind_cycles.saturating_add(1);
                                self.set_rewind_target(rewind_target);
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
                                    self.clear_internal_state();
                                    self.derivation.set_last_derived_l2_block(rewind_target);
                                    self.derivation.rollback_to(rollback_l1_block);
                                    self.mode = DriverMode::Sync;
                                    self.synced
                                        .store(false, std::sync::atomic::Ordering::Relaxed);
                                    self.consecutive_rewind_cycles =
                                        self.consecutive_rewind_cycles.saturating_add(1);
                                    self.set_rewind_target(rewind_target);
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
                            self.clear_internal_state();
                            self.derivation.set_last_derived_l2_block(rewind_target);
                            self.derivation.rollback_to(rollback_l1_block);
                            self.mode = DriverMode::Sync;
                            self.synced
                                .store(false, std::sync::atomic::Ordering::Relaxed);
                            self.consecutive_rewind_cycles =
                                self.consecutive_rewind_cycles.saturating_add(1);
                            self.set_rewind_target(rewind_target);
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
    async fn send_l2_to_l1_triggers(&mut self, triggers: &[TriggerMetadata]) -> Result<Vec<B256>> {
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
    async fn forward_queued_l1_txs(&mut self) -> Result<()> {
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
    fn compute_gas_overbid(&self) -> Option<GasPriceHint> {
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

    /// Verify that a locally-built block was correctly submitted to L1.
    ///
    /// Compares the state root submitted to L1 against our local computation.
    /// The builder uses `latest_l1_block` as L1 context, and the tx should land in
    /// `latest_l1_block + 1`, so derivation computes context = `containing - 1` =
    /// `latest_l1_block` — matching. However, if the tx lands in a later block
    /// (due to L1 congestion or mempool delays), the contexts will differ, causing
    /// a state root or L1 context mismatch.
    ///
    /// On mismatch, returns an error to halt builder mode and force re-sync
    /// from L1 canonical data (ISSUE-101, ISSUE-99).
    ///
    /// The L1 context check is critical: the builder uses `latest_l1_block` when
    /// building, but the canonical L1 context is `containing_l1_block - 1` (derived
    /// from whichever L1 block the submission lands in). If the submission is
    /// delayed (batching, gas spikes), these differ, producing different state roots
    /// and block hashes. The builder must detect this and re-derive.
    /// Clear all pending state that becomes stale on L1 reorg or state root mismatch.
    ///
    /// This centralizes clearing of `preconfirmed_hashes` and `pending_submissions`
    /// so that every reorg/rewind path stays consistent.
    /// Read the builder's current L2 nonce from chain state.
    /// Called on Sync→Builder transitions to ensure correct nonce after reorgs.
    fn recover_builder_l2_nonce(&mut self) {
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

    /// Clear internal driver state (pending submissions, entries, hold).
    /// Preserves external queues (cross-chain calls, L2→L1 calls) because they
    /// represent user-initiated actions from the composer RPCs that must eventually
    /// be processed — silently discarding them loses user transactions.
    /// Also clears `pending_l1_forward_txs` as defense-in-depth: the normal path
    /// (step_builder) only commits L1 forward txs after successful block builds,
    /// but this ensures no orphaned txs survive a Sync transition. See issue #237.
    fn clear_internal_state(&mut self) {
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

    /// Collect user transactions from blocks that are about to be reverted.
    ///
    /// Reads block bodies from `from_block..=to_block` (inclusive) while they are
    /// still canonical (BEFORE the FCU rewind removes them). Filters out the
    /// builder's own protocol transactions (setContext, etc.) since those are
    /// rebuilt fresh for every block.
    ///
    /// Returns (sender, transaction) pairs with signers already recovered.
    fn collect_reverted_user_transactions(
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

    /// Deferred re-injection: add transactions from a previous rewind back into
    /// the pool. Called at the top of step(), after reth's async pool maintenance
    /// has fully processed the CanonStateNotification from the FCU rewind.
    ///
    /// This eliminates the TOCTOU race in the old `sync_pool_after_rewind`:
    /// - OLD: update_accounts → .await add_external_transactions → reth's Commit
    ///   notification interleaves, overwrites nonces → tx rejected, permanently lost
    /// - NEW: defer re-injection by one full step() iteration (~12s). By then,
    ///   reth's Reorg notification has updated pool nonces. No race possible.
    ///
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
    async fn inject_held_l2_txs(&self, held_txs: &[Bytes]) {
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

    async fn reinject_pending_transactions(&mut self) {
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

    /// Journal a block's transactions for crash recovery.
    ///
    /// Stores the full encoded transaction list in the persistent journal.
    /// On recovery, protocol transactions (builder address) are filtered out.
    fn journal_block_transactions(&mut self, l2_block_number: u64, encoded_transactions: &Bytes) {
        self.tx_journal.push(TxJournalEntry {
            l2_block_number,
            block_txs: encoded_transactions.to_vec(),
        });
        self.save_tx_journal();
    }

    /// Persist the transaction journal to the L2 database.
    fn save_tx_journal(&self) {
        let data = TxJournalEntry::encode_all(&self.tx_journal);
        let rw = match self.l2_provider.database_provider_rw() {
            Ok(rw) => rw,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    "failed to open DB for tx journal save"
                );
                return;
            }
        };
        if let Err(err) = rw.save_stage_checkpoint_progress(TX_JOURNAL_STAGE_ID, data) {
            warn!(
                target: "based_rollup::driver",
                %err,
                "failed to save tx journal"
            );
            return;
        }
        if let Err(err) = rw.commit() {
            warn!(
                target: "based_rollup::driver",
                %err,
                "failed to commit tx journal"
            );
        }
    }

    /// Load the transaction journal from the L2 database (crash recovery).
    ///
    /// Entries for blocks above the canonical head represent transactions from
    /// blocks that were being reverted when a crash occurred. These are decoded
    /// and placed in `pending_reinjection` for deferred re-injection.
    fn load_tx_journal(&mut self) {
        let data = match self
            .l2_provider
            .get_stage_checkpoint_progress(TX_JOURNAL_STAGE_ID)
        {
            Ok(Some(data)) => data,
            Ok(None) => return,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    "failed to load tx journal"
                );
                return;
            }
        };

        let entries = TxJournalEntry::decode_all(&data);
        if entries.is_empty() {
            return;
        }

        // Entries for blocks > canonical head need re-injection (crash recovery).
        let mut recovered = 0usize;
        for entry in &entries {
            if entry.l2_block_number > self.l2_head_number {
                let txs: Vec<reth_ethereum_primitives::TransactionSigned> =
                    match alloy_rlp::Decodable::decode(&mut entry.block_txs.as_slice()) {
                        Ok(txs) => txs,
                        Err(_) => continue,
                    };
                for tx in txs {
                    match tx.recover_signer() {
                        Ok(sender) => {
                            // Skip builder's protocol transactions.
                            if sender == self.config.builder_address {
                                continue;
                            }
                            self.pending_reinjection.push((sender, tx));
                            recovered += 1;
                        }
                        Err(_) => continue,
                    }
                }
            }
        }

        // Keep only entries for blocks <= canonical head.
        self.tx_journal = entries
            .into_iter()
            .filter(|e| e.l2_block_number <= self.l2_head_number)
            .collect();

        if recovered > 0 {
            info!(
                target: "based_rollup::driver",
                recovered,
                journal_size = self.tx_journal.len(),
                "recovered transactions from journal for re-injection (crash recovery)"
            );
            // Persist the cleaned journal (without the crash-recovery entries).
            self.save_tx_journal();
        }
    }

    /// Prune journal entries for L1-confirmed blocks.
    fn prune_tx_journal(&mut self, confirmed_l2_block: u64) {
        let before = self.tx_journal.len();
        self.tx_journal
            .retain(|e| e.l2_block_number > confirmed_l2_block);
        let pruned = before - self.tx_journal.len();
        if pruned > 0 {
            self.save_tx_journal();
            debug!(
                target: "based_rollup::driver",
                pruned,
                remaining = self.tx_journal.len(),
                confirmed_l2_block,
                "pruned confirmed entries from tx journal"
            );
        }
    }

    /// Set the pending rewind target to the EARLIEST (minimum) mismatch point.
    ///
    /// When multiple blocks in the same derivation batch have L1 context mismatches
    /// (e.g. a run of gap-fill blocks followed by a submitted block), we must rewind
    /// to the earliest one so all are re-derived with the correct context.
    fn set_rewind_target(&mut self, target: u64) {
        self.pending_rewind_target =
            Some(self.pending_rewind_target.map_or(target, |t| t.min(target)));
    }

    fn verify_local_block_matches_l1(
        &mut self,
        derived: &crate::derivation::DerivedBlock,
    ) -> Result<()> {
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
            return Ok(());
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
            return Ok(());
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
            // Set a rewind target so the block will be re-built with the correct L1
            // context on the next step. Stay in Builder mode to minimize disruption.
            info!(
                target: "based_rollup::driver",
                l2_block = derived.l2_block_number,
                local_l1_context = local_l1_number,
                derived_l1_context = derived_l1_number,
                is_gap_fill,
                "L1 context mismatch — will re-derive block with correct context"
            );
            // Roll back derivation so re-derive starts from the correct L1 block.
            // Only roll back if we haven't already rolled back to an earlier point
            // (rollback_to is idempotent but we avoid unnecessary work).
            if self.pending_rewind_target.is_none() {
                self.derivation.rollback_to(derived_l1_number);
            }
            // Clear all pending state — submissions contain state roots from the
            // wrong L1 context, and preconfirmed/deposit data may be stale.
            self.clear_internal_state();
            self.set_rewind_target(derived.l2_block_number.saturating_sub(1));
            return Ok(());
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
            return Ok(());
        }

        // With protocol tx filtering (§4f), derivation produces the correct root
        // for any consumption level. The derived root should match the header root
        // directly. If it doesn't, the builder's speculative block diverged from
        // the L1-derived block (e.g., entries were not consumed). Rewind is
        // productive — re-derivation with filtered txs produces the correct root.
        let header_root = local_header.state_root();
        if header_root != derived.state_root {
            // Entry-bearing block with pending verification: the consumption event
            // (ExecutionConsumed) may land 1-2 L1 blocks AFTER the postBatch due to
            // hold-then-forward timing. We defer verification a few times to give the
            // consumption event time to land on L1.
            //
            // After MAX_ENTRY_VERIFY_DEFERRALS, the entry's bridge tx likely reverted
            // permanently. `EntryVerificationHold::defer` returns
            // `DeferralResult::MustRewind` once the counter exhausts, with the target
            // pre-computed as `entry_block - 1` (invariant #10).
            if self.hold.is_armed_for(derived.l2_block_number) {
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
                        // Return Err to trigger retry via main loop backoff.
                        // The exponential backoff (2+4+8=14s for 3 deferrals) gives
                        // L1 time to mine the user's tx and emit ExecutionConsumed.
                        return Err(eyre::eyre!(
                            "entry verification deferred for block {} (attempt {}/{})",
                            derived.l2_block_number,
                            deferrals,
                            MAX_ENTRY_VERIFY_DEFERRALS
                        ));
                    }
                    DeferralResult::MustRewind { target: rewind_target } => {
                        // Exhausted deferrals — entry likely not consumed (user's L1 tx
                        // reverted or partial consumption). Rewind to re-derive the block
                        // with §4f filtering, which produces the correct nonces for
                        // subsequent blocks. Without rewind, fullnodes diverge permanently.
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
                        self.clear_internal_state();
                        self.derivation.set_last_derived_l2_block(rewind_target);
                        self.derivation.rollback_to(rollback_l1_block);
                        self.mode = DriverMode::Sync;
                        self.synced
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        self.consecutive_rewind_cycles =
                            self.consecutive_rewind_cycles.saturating_add(1);
                        self.set_rewind_target(rewind_target);
                        return Ok(());
                    }
                    DeferralResult::NotArmed => {
                        // Unreachable: we checked `is_armed_for(...)` above.
                        // Fall through to the generic mismatch handling below.
                    }
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
            self.set_rewind_target(derived.l2_block_number.saturating_sub(1));
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
    fn apply_deferred_filtering(
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
    fn apply_generic_filtering(
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
        let consumed_count = crate::cross_chain::compute_consumed_trigger_prefix(
            &receipts,
            self.config.cross_chain_manager_address,
            &mut l1_remaining,
            &trigger_indices,
        );

        let total_triggers = trigger_indices.len();
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
    fn apply_generic_filtering_via_rebuild(
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
        let consumed_count = crate::cross_chain::compute_consumed_trigger_prefix(
            &receipts,
            self.config.cross_chain_manager_address,
            &mut l1_remaining,
            &trigger_indices,
        );

        let total_triggers = trigger_indices.len();
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
    fn compute_state_root_with_entries(
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
    fn trial_execute_for_receipts(
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
    fn compute_intermediate_roots(
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

    /// Construct builder-signed protocol transactions for a builder block.
    ///
    /// Returns RLP-encoded transactions (setContext, deploy, loadTable, executeIncoming).
    /// The caller should append user txs (mempool) and pass to `build_and_insert_block`.
    ///
    /// `max_trigger_count` limits the number of `executeIncomingCrossChainCall` trigger
    /// transactions generated. `loadExecutionTable` is always generated if table entries
    /// are present (regardless of this limit). Pass `usize::MAX` to generate all triggers.
    fn build_builder_protocol_txs(
        &mut self,
        l2_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        execution_entries: &[CrossChainExecutionEntry],
        max_trigger_count: usize,
    ) -> Result<Bytes> {
        use crate::cross_chain;

        let signer = self
            .proposer
            .as_ref()
            .ok_or_else(|| eyre::eyre!("proposer required for builder protocol txs"))?
            .create_signer()?;

        let chain_id = self.evm_config.chain_spec().chain().id();

        // Use next block's base fee (not parent's) for protocol tx gas_price.
        // This ensures protocol txs are correctly priced even when parent was >50% utilized.
        let parent_header = self
            .l2_provider
            .sealed_header(self.l2_head_number)
            .wrap_err("failed to get parent header for gas price")?
            .ok_or_eyre("parent header not found for gas price")?;
        let gas_price = parent_header
            .next_block_base_fee(
                self.evm_config
                    .chain_spec()
                    .base_fee_params_at_timestamp(timestamp),
            )
            .unwrap_or(1)
            .max(1) as u128;

        let mut block_txs: Vec<reth_ethereum_primitives::TransactionSigned> = Vec::new();

        // Block 1: deploy L2Context and CCM contracts
        if l2_block_number == 1 {
            block_txs.push(cross_chain::build_deploy_l2context_tx(
                self.config.builder_address,
                &signer,
                chain_id,
                gas_price,
            )?);
            // Only deploy CCM and Bridge if cross-chain is configured
            if !self.config.rollups_address.is_zero() {
                block_txs.push(cross_chain::build_deploy_ccm_tx(
                    self.config.rollup_id,
                    self.config.builder_address,
                    &signer,
                    chain_id,
                    gas_price,
                )?);
                // Deploy Bridge on L2 (nonce=2) and initialize (nonce=3)
                block_txs.push(cross_chain::build_deploy_bridge_tx(
                    &signer, chain_id, gas_price,
                )?);
                block_txs.push(cross_chain::build_initialize_bridge_tx(
                    self.config.cross_chain_manager_address,
                    self.config.rollup_id,
                    self.config.builder_address,
                    self.config.bridge_l2_address,
                    &signer,
                    chain_id,
                    gas_price,
                )?);
                self.builder_l2_nonce = 4;
            } else {
                self.builder_l2_nonce = 1;
            }
            // Bootstrap transfers
            for account in &self.config.bootstrap_accounts {
                block_txs.push(cross_chain::build_bootstrap_transfer_tx(
                    account.address,
                    account.amount_wei,
                    &signer,
                    self.builder_l2_nonce,
                    chain_id,
                    gas_price,
                )?);
                self.builder_l2_nonce += 1;
            }
        }

        // setCanonicalBridgeAddress: if bridge_l1_address is configured and this is
        // block 2, set the canonical bridge address on the L2 bridge contract.
        // This is a one-time protocol tx required for multi-call continuation entries.
        // Block 2 because the bridge is deployed in block 1 (nonce=2, initialized nonce=3).
        if l2_block_number == 2
            && !self.config.bridge_l1_address.is_zero()
            && !self.config.bridge_l2_address.is_zero()
        {
            info!(
                target: "based_rollup::driver",
                bridge_l2 = %self.config.bridge_l2_address,
                canonical = %self.config.bridge_l1_address,
                nonce = self.builder_l2_nonce,
                "setting canonical bridge address on L2 bridge (block 2 protocol tx)"
            );
            block_txs.push(cross_chain::build_set_canonical_bridge_tx(
                self.config.bridge_l2_address,
                self.config.bridge_l1_address,
                &signer,
                self.builder_l2_nonce,
                chain_id,
                gas_price,
            )?);
            self.builder_l2_nonce += 1;
        }

        // setContext (every block)
        if !self.config.l2_context_address.is_zero() {
            block_txs.push(cross_chain::build_set_context_tx(
                l1_block_number,
                l1_block_hash,
                self.config.l2_context_address,
                &signer,
                self.builder_l2_nonce,
                chain_id,
                gas_price,
            )?);
            self.builder_l2_nonce += 1;
        }

        // loadExecutionTable + executeIncomingCrossChainCall (if cross-chain entries)
        if !execution_entries.is_empty() && !self.config.cross_chain_manager_address.is_zero() {
            let our_rollup_id = cross_chain::RollupId::new(alloy_primitives::U256::from(
                self.config.rollup_id,
            ));
            let (table_entries, mut trigger_entries) =
                cross_chain::partition_entries(execution_entries, our_rollup_id);

            // Scope override for REVERT patterns: when table entries contain a
            // REVERT, the trigger's executeIncomingCrossChainCall must use a
            // scope one level deeper than the REVERT's scope. This ensures
            // newScope creates the nested scope for try/catch isolation.
            // E.g., REVERT has scope=[0] → trigger uses scope=[0,0].
            let has_revert = table_entries
                .iter()
                .any(|e| e.next_action.action_type == cross_chain::CrossChainActionType::Revert);
            if has_revert {
                // Find the REVERT entry's scope length to compute trigger scope depth.
                let revert_scope_len = table_entries
                    .iter()
                    .filter(|e| {
                        e.next_action.action_type == cross_chain::CrossChainActionType::Revert
                    })
                    .map(|e| e.next_action.scope.len())
                    .max()
                    .unwrap_or(0);
                let trigger_scope: Vec<alloy_primitives::U256> =
                    vec![alloy_primitives::U256::ZERO; revert_scope_len + 1];
                for trigger in &mut trigger_entries {
                    info!(
                        target: "based_rollup::driver",
                        old_scope_len = trigger.next_action.scope.len(),
                        new_scope_len = trigger_scope.len(),
                        "overriding trigger scope for REVERT pattern"
                    );
                    trigger.next_action.scope =
                        crate::cross_chain::ScopePath::from_parts(trigger_scope.clone());
                }
            }

            if !table_entries.is_empty() {
                block_txs.push(cross_chain::build_load_table_tx(
                    &table_entries,
                    self.config.cross_chain_manager_address,
                    &signer,
                    self.builder_l2_nonce,
                    chain_id,
                    gas_price,
                )?);
                self.builder_l2_nonce += 1;
            }
            let trigger_limit = trigger_entries.len().min(max_trigger_count);
            for trigger in &trigger_entries[..trigger_limit] {
                block_txs.push(cross_chain::build_execute_incoming_tx(
                    &trigger.next_action,
                    self.config.cross_chain_manager_address,
                    &signer,
                    self.builder_l2_nonce,
                    chain_id,
                    gas_price,
                )?);
                self.builder_l2_nonce += 1;
            }
        }

        // Drain user transactions from the mempool, respecting the gas budget.
        let block_gas_limit = calc_gas_limit(parent_header.gas_limit(), DESIRED_GAS_LIMIT);
        let builder_gas_used = cross_chain::estimate_builder_tx_gas(&block_txs);
        let mut cumulative_gas_used = builder_gas_used;

        let base_fee = parent_header
            .next_block_base_fee(
                self.evm_config
                    .chain_spec()
                    .base_fee_params_at_timestamp(timestamp),
            )
            .unwrap_or(1);

        let mut best_txs = self.pool.best_transactions_with_attributes(
            reth_transaction_pool::BestTransactionsAttributes::base_fee(base_fee),
        );

        // Validate pool tx nonces against canonical state. After a chain rewind
        // (e.g., phantom state detection), the pool's nonce tracking may be stale
        // — returning txs with nonces that don't match the actual chain state.
        // Without this check, the builder includes a stale-nonce tx, the EVM
        // rejects it, and the builder gets stuck in a Sync↔Builder loop.
        let state_for_nonce_check = self.l2_provider.state_by_block_hash(self.head_hash).ok();
        let mut expected_nonces: std::collections::HashMap<alloy_primitives::Address, u64> =
            std::collections::HashMap::new();

        while let Some(pool_tx) = best_txs.next() {
            // Skip transactions from the builder's own address — their nonces
            // conflict with protocol transactions (setContext, deploys, etc.)
            // that are already in block_txs with specific nonces.
            if pool_tx.sender() == self.config.builder_address {
                continue;
            }

            // Check nonce against canonical state to catch stale pool entries.
            if let Some(ref state) = state_for_nonce_check {
                use reth_provider::AccountReader;
                let sender = pool_tx.sender();
                let tx_nonce = pool_tx.nonce();
                let expected = expected_nonces.entry(sender).or_insert_with(|| {
                    state
                        .basic_account(&sender)
                        .ok()
                        .flatten()
                        .map_or(0, |acct| acct.nonce)
                });
                if tx_nonce != *expected {
                    warn!(
                        target: "based_rollup::driver",
                        %sender,
                        tx_nonce,
                        expected = *expected,
                        "skipping pool tx with stale nonce (pool may be stale after rewind)"
                    );
                    best_txs.mark_invalid(
                        &pool_tx,
                        &reth_transaction_pool::error::InvalidPoolTransactionError::ExceedsGasLimit(
                            0, 0,
                        ),
                    );
                    continue;
                }
                *expected = tx_nonce + 1;
            }

            let tx_gas = pool_tx.gas_limit();

            // Skip transactions that don't fit in the remaining gas budget.
            if cumulative_gas_used + tx_gas > block_gas_limit {
                best_txs.mark_invalid(
                    &pool_tx,
                    &reth_transaction_pool::error::InvalidPoolTransactionError::ExceedsGasLimit(
                        tx_gas,
                        block_gas_limit,
                    ),
                );
                continue;
            }

            // Convert pool tx to signed transaction for block inclusion.
            let recovered = pool_tx.to_consensus();
            block_txs.push(recovered.into_inner());
            cumulative_gas_used += tx_gas;
        }

        Ok(encode_block_transactions(&block_txs))
    }

    /// Build a block directly from L1-derived transactions using the EVM config's
    /// `builder_for_next_block` API.
    ///
    /// `parent_block_number` specifies which block to build on top of.
    /// `l1_block_number` is passed via `prev_randao` so the EVM config can read it.
    fn build_derived_block(
        &self,
        parent_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        derived_transactions: &Bytes,
    ) -> Result<(BuiltBlock, ExecutionData)> {
        use reth_evm::execute::BlockBuilder;

        // Get parent header
        let parent_header = self
            .l2_provider
            .sealed_header(parent_block_number)
            .wrap_err("failed to get parent header")?
            .ok_or_eyre("parent header not found")?;

        // Get state provider at parent
        let state_provider = self
            .l2_provider
            .state_by_block_hash(parent_header.hash())
            .wrap_err("failed to get state provider")?;

        let state_db = StateProviderDatabase::new(state_provider.as_ref());
        let mut db = State::builder()
            .with_database(state_db)
            .with_bundle_update()
            .build();

        // Encode L1 block number into prev_randao so the EVM config can read it
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

        let mut builder = self
            .evm_config
            .builder_for_next_block(&mut db, &parent_header, attributes)
            .wrap_err("failed to create block builder")?;

        // Apply pre-execution changes (beacon root contract)
        builder
            .apply_pre_execution_changes()
            .wrap_err("pre-execution changes failed")?;

        // Decode and execute L1-derived transactions
        if !derived_transactions.is_empty() {
            let txs: Vec<reth_ethereum_primitives::TransactionSigned> =
                alloy_rlp::Decodable::decode(&mut derived_transactions.as_ref())
                    .wrap_err("failed to RLP-decode derived transactions")?;

            for (tx_idx, tx) in txs.into_iter().enumerate() {
                let tx_hash = *tx.tx_hash();
                let recovered = SignedTransaction::try_into_recovered(tx)
                    .map_err(|_| eyre::eyre!("failed to recover signer for L1-derived tx"))?;

                let signer = recovered.signer();
                builder.execute_transaction(recovered).wrap_err_with(|| {
                    format!(
                        "failed to execute L1-derived tx #{tx_idx} (hash={tx_hash}, signer={signer})"
                    )
                })?;
            }
        }

        // Finish building the block (computes state root, assembles sealed block)
        let outcome = builder
            .finish(state_provider.as_ref())
            .wrap_err("block builder finish failed")?;

        let sealed_block = outcome.block.sealed_block().clone();
        let block_hash = sealed_block.sealed_header().hash();
        let state_root = sealed_block.sealed_header().state_root();
        let tx_count = sealed_block.body().transactions.len();
        let encoded_transactions = encode_block_transactions(&sealed_block.body().transactions);

        let execution_data = <EthEngineTypes as PayloadTypes>::block_to_payload(sealed_block);

        let built = BuiltBlock {
            hash: block_hash,
            pre_state_root: parent_header.state_root(),
            state_root,
            tx_count,
            encoded_transactions,
        };

        Ok((built, execution_data))
    }

    /// Build a block with the given parameters and insert it into the chain.
    ///
    /// Always uses `build_derived_block` with exact transactions. In builder mode,
    /// protocol transactions (setContext, loadTable, etc.) and mempool transactions
    /// are assembled by the caller and passed as `derived_transactions`.
    async fn build_and_insert_block(
        &mut self,
        l2_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        derived_transactions: &Bytes,
    ) -> Result<BuiltBlock> {
        // Sanity check: we should be building the next sequential block
        let expected = self.l2_head_number.saturating_add(1);
        if l2_block_number != expected {
            return Err(eyre::eyre!(
                "expected sequential block {expected}, got {l2_block_number}",
            ));
        }

        let (built, execution_data) = self.build_derived_block(
            self.l2_head_number,
            timestamp,
            l1_block_hash,
            l1_block_number,
            derived_transactions,
        )?;

        // Submit to the engine via newPayload — reth re-executes the block.
        let status = self.engine.new_payload(execution_data).await?;

        if !status.is_valid() {
            eyre::bail!("newPayload rejected: {:?}", status);
        }

        // Update fork choice to accept the new head
        self.update_fork_choice(built.hash).await?;

        Ok(built)
    }

    /// Send a fork choice update with exponential-backoff retry on SYNCING.
    ///
    /// SYNCING is transient — the engine needs time to reconcile its state tree
    /// after blocks are unwound or rebuilt. Without retry, SYNCING causes the
    /// driver to bail and enter exponential backoff in the main loop.
    async fn fork_choice_updated_with_retry(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<PayloadAttributes>,
    ) -> Result<ForkchoiceUpdated> {
        let mut backoff_ms = FCU_SYNCING_INITIAL_BACKOFF_MS;
        for attempt in 0..FCU_SYNCING_MAX_RETRIES {
            let fcu = self
                .engine
                .fork_choice_updated(
                    state,
                    payload_attrs.clone(),
                    EngineApiMessageVersion::default(),
                )
                .await
                .wrap_err("fork choice update failed")?;

            if fcu.is_valid() || fcu.is_invalid() {
                return Ok(fcu);
            }

            // SYNCING — retry with exponential backoff
            if attempt + 1 < FCU_SYNCING_MAX_RETRIES {
                warn!(
                    target: "based_rollup::driver",
                    attempt = attempt + 1,
                    max_retries = FCU_SYNCING_MAX_RETRIES,
                    backoff_ms,
                    "FCU returned SYNCING, retrying"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms *= 2;
            }
        }

        eyre::bail!(
            "engine stuck in SYNCING after {} retries",
            FCU_SYNCING_MAX_RETRIES
        );
    }

    /// Update fork choice state after inserting a new block.
    ///
    /// IMPORTANT: State mutations happen AFTER the engine confirms the fork choice
    /// update, not before. This prevents driver/engine desync if the engine rejects.
    async fn update_fork_choice(&mut self, block_hash: B256) -> Result<()> {
        // Temporarily compute the forkchoice state with the new block hash
        // without mutating self yet.
        let mut tentative_hashes = self.block_hashes.clone();
        tentative_hashes.push_back(block_hash);
        if tentative_hashes.len() > FORK_CHOICE_DEPTH {
            tentative_hashes.pop_front();
        }
        let fcs = compute_forkchoice_state(block_hash, &tentative_hashes);

        let fcu = self.fork_choice_updated_with_retry(fcs, None).await?;

        if fcu.is_invalid() {
            eyre::bail!(
                "fork choice finalization rejected: {:?}",
                fcu.payload_status
            );
        }

        // Only mutate driver state after engine confirms success
        self.block_hashes = tentative_hashes;
        self.head_hash = block_hash;
        self.l2_head_number = self.l2_head_number.saturating_add(1);

        Ok(())
    }

    /// Rewind the L2 chain to a target block number by sending a fork choice
    /// update pointing to an ancestor. Reth will internally unwind blocks
    /// above the target.
    async fn rewind_l2_chain(&mut self, target_l2_block: u64) -> Result<()> {
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

    /// Save the L1-confirmed anchor to the L2 database.
    fn save_l1_confirmed_anchor(&self) {
        let Some(anchor) = self.l1_confirmed_anchor else {
            return;
        };
        let rw = match self.l2_provider.database_provider_rw() {
            Ok(rw) => rw,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    "failed to open DB for L1-confirmed anchor save"
                );
                return;
            }
        };
        if let Err(err) = rw.save_stage_checkpoint(
            L1_CONFIRMED_L2_STAGE_ID,
            StageCheckpoint::new(anchor.l2_block_number),
        ) {
            warn!(target: "based_rollup::driver", %err, "failed to save L1-confirmed L2 anchor");
            return;
        }
        if let Err(err) = rw.save_stage_checkpoint(
            L1_CONFIRMED_L1_STAGE_ID,
            StageCheckpoint::new(anchor.l1_block_number),
        ) {
            warn!(target: "based_rollup::driver", %err, "failed to save L1-confirmed L1 anchor");
            return;
        }
        if let Err(err) = rw.commit() {
            warn!(target: "based_rollup::driver", %err, "failed to commit L1-confirmed anchor");
            return;
        }
        info!(
            target: "based_rollup::driver",
            l2_block = anchor.l2_block_number,
            l1_block = anchor.l1_block_number,
            "recorded L1-confirmed anchor"
        );
    }

    /// Load the L1-confirmed anchor from the L2 database.
    fn load_l1_confirmed_anchor(&mut self) {
        let l2_cp = match self
            .l2_provider
            .get_stage_checkpoint(L1_CONFIRMED_L2_STAGE_ID)
        {
            Ok(Some(cp)) => cp.block_number,
            _ => return,
        };
        let l1_cp = match self
            .l2_provider
            .get_stage_checkpoint(L1_CONFIRMED_L1_STAGE_ID)
        {
            Ok(Some(cp)) => cp.block_number,
            _ => return,
        };
        self.l1_confirmed_anchor = Some(L1ConfirmedAnchor {
            l2_block_number: l2_cp,
            l1_block_number: l1_cp,
        });
        info!(
            target: "based_rollup::driver",
            l2_block = l2_cp,
            l1_block = l1_cp,
            "loaded L1-confirmed anchor from DB"
        );
    }

    pub fn derivation(&self) -> &DerivationPipeline {
        &self.derivation
    }

    pub fn derivation_mut(&mut self) -> &mut DerivationPipeline {
        &mut self.derivation
    }
}

/// Compute the fork choice state from a head hash and a deque of recent block hashes.
///
/// - `head`: the latest block hash
/// - `safe`: 32 blocks behind head (or oldest tracked, or head if empty)
/// - `finalized`: the oldest tracked hash (or head if empty)
fn compute_forkchoice_state(head_hash: B256, block_hashes: &VecDeque<B256>) -> ForkchoiceState {
    let safe = block_hashes
        .get(block_hashes.len().saturating_sub(32))
        .copied()
        .unwrap_or(head_hash);
    let finalized = block_hashes.front().copied().unwrap_or(head_hash);

    ForkchoiceState {
        head_block_hash: head_hash,
        safe_block_hash: safe,
        finalized_block_hash: finalized,
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
