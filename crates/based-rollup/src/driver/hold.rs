//! Entry-verification hold state machine (refactor PLAN step 1.6).
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1b. See this module's
//! doc comment for the full lifecycle description.

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
