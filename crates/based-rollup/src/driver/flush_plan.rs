//! Typestate flush plan (refactor PLAN step 1.7).
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1d. See [`FlushPlan`]
//! and the block comment below for the full rationale. The three phantom
//! marker types (`NoEntries`, `Collected`, `HoldArmed`) and the sealed
//! [`Sendable`] trait live in this module together because they are
//! inseparable: the typestate guarantee relies on the fact that no code
//! outside this module can create new `Sendable` implementors.

use super::pending_queue::PendingL1SubmissionQueue;
use crate::cross_chain::CrossChainExecutionEntry;
use crate::driver::hold::EntryVerificationHold;
use crate::proposer::{GasPriceHint, PendingBlock, Proposer};
use alloy_primitives::B256;
use std::marker::PhantomData;

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
