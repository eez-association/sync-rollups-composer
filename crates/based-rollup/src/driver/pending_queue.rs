//! Pending L1 submission queue (refactor PLAN step 1.5).
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1c. See
//! [`PendingL1SubmissionQueue`] for the full rationale — the module
//! comment lives in that type's doc block so rust-doc surfaces it.

use crate::cross_chain::CrossChainExecutionEntry;
use alloy_primitives::{Address, U256};

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
    /// Structured trace metadata for composer-originated L1→L2 user txs.
    pub trace: Option<crate::arb_trace::ArbTraceMeta>,
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
        trace: Option<crate::arb_trace::ArbTraceMeta>,
    ) {
        let start = self.entries.len();
        self.entries.extend(group_entries);
        self.groups.push(PendingL1Group {
            start,
            mode,
            trigger,
            trace,
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
