//! Neutral DSL for L1↔L2 mirror tests.
//!
//! This module is the output of refactor PLAN step 0.5 and closes
//! invariant #18 of `docs/refactor/INVARIANT_MAP.md`:
//!
//! > L1 and L2 entry structures must MIRROR each other.
//!
//! ## Why this lives in `src/test_support/`
//!
//! Mirror tests must be importable from sibling `*_tests.rs` files under
//! `crates/based-rollup/src/`, which are compiled as child modules of
//! their production sibling under `#[cfg(test)]`. Those files cannot
//! `use crate::tests::fixtures::*` because the integration test root in
//! `crates/based-rollup/tests/` is a separate crate. Living under
//! `src/test_support/` makes the DSL reachable via the absolute path
//! `crate::test_support::mirror_case::*` from any unit test.
//!
//! ## How to extend
//!
//! 1. Add a new `case_*` constructor that builds a `MirrorCase` for
//!    your scenario by calling the real builders in `cross_chain` /
//!    `table_builder`.
//! 2. Append it to the [`canonical_cases`] vector.
//! 3. Run `cargo nextest run -p based-rollup` — every existing mirror
//!    test will pick up the new case automatically.
//!
//! ## Scope of the DSL
//!
//! `MirrorCase` is a CONTAINER, not a factory. Each canonical case
//! constructs the actual `(l1_entries, l2_entries)` produced by the
//! real builder functions for a specific input. Mirror tests then
//! loop over `canonical_cases()` and assert structural invariants
//! that must hold across both directions.
//!
//! Deeper mirror property tests (action-hash equivalence between
//! direction-paired entries, scope navigation symmetry, etc.) are
//! introduced in Phase 3 of the refactor when the `Direction` trait
//! lands. Step 0.5 only ships the scaffolding so subsequent steps
//! can build on it without retrofitting.

use crate::cross_chain::{
    CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, RollupId, ScopePath,
    build_cross_chain_call_entries, build_l2_to_l1_call_entries,
};
use crate::table_builder::{
    CallDirection, DetectedCall, build_continuation_entries,
    build_l2_to_l1_continuation_entries,
};
use alloy_primitives::{Address, B256, U256};

/// Direction-agnostic enum identifying the canonical cross-chain pattern
/// under test. Used by mirror tests to gate assertions that only apply
/// to a specific pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorPattern {
    /// Single CALL+RESULT pair, no continuation. Closes the simplest
    /// case for both directions.
    Simple,
    /// Multi-call continuation chain (e.g., flash loan, PingPong).
    /// Exercises the continuation builders and scope navigation.
    Continuation,
}

/// A canonical cross-chain test scenario, used to verify that L1→L2 and
/// L2→L1 entry construction produce mirrored shapes.
///
/// Each instance carries:
/// - the scenario `name` (used in test diagnostics);
/// - the `pattern` kind (Simple vs Continuation);
/// - the `direction` (L1→L2 vs L2→L1);
/// - the actual `l1_entries` and `l2_entries` produced by the real
///   builders for this scenario;
/// - all `action_hashes` collected across both entry lists for quick
///   uniqueness / non-zero assertions.
///
/// Construct cases via the `case_*` functions in this module; iterate
/// via [`canonical_cases`].
#[derive(Debug, Clone)]
pub struct MirrorCase {
    pub name: &'static str,
    pub pattern: MirrorPattern,
    pub direction: CallDirection,
    pub l1_entries: Vec<CrossChainExecutionEntry>,
    pub l2_entries: Vec<CrossChainExecutionEntry>,
    pub all_action_hashes: Vec<B256>,
}

impl MirrorCase {
    /// Sum of L1 + L2 entries.
    pub fn total_entries(&self) -> usize {
        self.l1_entries.len() + self.l2_entries.len()
    }

    /// Convenience: collect every `action_hash` from both entry lists.
    fn collect_hashes(
        l1: &[CrossChainExecutionEntry],
        l2: &[CrossChainExecutionEntry],
    ) -> Vec<B256> {
        let mut hashes = Vec::with_capacity(l1.len() + l2.len());
        for e in l1 {
            hashes.push(e.action_hash);
        }
        for e in l2 {
            hashes.push(e.action_hash);
        }
        hashes
    }
}

/// Returns the 5 canonical mirror cases used by mirror tests.
///
/// These cover the key cross-chain patterns the rollup must handle:
///
/// 1. **`deposit_simple`** — L1→L2 single CALL+RESULT pair (no continuation).
/// 2. **`withdrawal_simple`** — L2→L1 single CALL+RESULT pair (no continuation).
/// 3. **`flash_loan_3_call`** — L1→L2 multi-call continuation (canonical
///    flash loan: bridgeTokens → claimAndBridgeBack → bridgeTokens return).
/// 4. **`ping_pong_depth_2`** — L2→L1 with depth-2 child tree
///    (root + child + grandchild), the existing
///    `test_l2_to_l1_depth2_entry_generation` pattern.
/// 5. **`ping_pong_depth_3`** — L2→L1 linear chain of 4 calls
///    (root → child → grandchild → great-grandchild).
pub fn canonical_cases() -> Vec<MirrorCase> {
    vec![
        case_deposit_simple(),
        case_withdrawal_simple(),
        case_flash_loan_3_call(),
        case_ping_pong_depth_2(),
        case_ping_pong_depth_3(),
    ]
}

// ──────────────────────────────────────────────
//  Common test addresses (deterministic, non-zero)
//
//  Each constructed via `Address::with_last_byte` so the only
//  non-zero byte is the trailing one — keeps cases readable and
//  avoids hex literal length mistakes.
// ──────────────────────────────────────────────

const L2_ROLLUP_ID: u64 = 1;

fn addr_user() -> Address {
    Address::with_last_byte(0xA0)
}
fn addr_dest_a() -> Address {
    Address::with_last_byte(0xA1)
}
fn addr_dest_b() -> Address {
    Address::with_last_byte(0xB1)
}
fn addr_dest_c() -> Address {
    Address::with_last_byte(0xC1)
}
fn addr_dest_d() -> Address {
    Address::with_last_byte(0xD1)
}
fn addr_src_b() -> Address {
    Address::with_last_byte(0xB2)
}
fn addr_src_c() -> Address {
    Address::with_last_byte(0xC2)
}
fn addr_src_d() -> Address {
    Address::with_last_byte(0xD2)
}

/// Build a `DetectedCall` for an L1→L2 hop with the given fields.
fn detected_l1_to_l2(
    destination: Address,
    data: Vec<u8>,
    source_address: Address,
    is_continuation: bool,
) -> DetectedCall {
    DetectedCall {
        direction: CallDirection::L1ToL2,
        call_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: RollupId::new(U256::from(L2_ROLLUP_ID)),
            destination,
            value: U256::ZERO,
            data,
            failed: false,
            source_address,
            source_rollup: RollupId::MAINNET, // MAINNET
            scope: ScopePath::root(),
        },
        parent_call_index: None,
        is_continuation,
        depth: 0,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: ScopePath::root(),
        discovery_iteration: 0,
        in_reverted_frame: false,
    }
}

/// Build a `DetectedCall` for an L2→L1 hop, optionally as a child of an
/// earlier call (parent_call_index, depth).
fn detected_l2_to_l1(
    destination: Address,
    data: Vec<u8>,
    source_address: Address,
    parent_call_index: Option<usize>,
    depth: usize,
) -> DetectedCall {
    DetectedCall {
        direction: CallDirection::L2ToL1,
        call_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: RollupId::MAINNET, // L1 (MAINNET)
            destination,
            value: U256::ZERO,
            data,
            failed: false,
            source_address,
            source_rollup: RollupId::new(U256::from(L2_ROLLUP_ID)),
            scope: ScopePath::root(),
        },
        parent_call_index,
        is_continuation: false,
        depth,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: ScopePath::root(),
        discovery_iteration: 0,
        in_reverted_frame: false,
    }
}

// ──────────────────────────────────────────────
//  Case 1: deposit_simple — L1→L2 single CALL+RESULT pair
// ──────────────────────────────────────────────

fn case_deposit_simple() -> MirrorCase {
    let l2_id = RollupId::new(U256::from(L2_ROLLUP_ID));
    let (call_entry, result_entry) = build_cross_chain_call_entries(
        l2_id,
        addr_dest_a(),
        vec![0xDE, 0xAD, 0xBE, 0xEF],
        U256::ZERO,
        addr_user(),
        RollupId::MAINNET, // source_rollup = MAINNET
        true,              // call_success
        vec![],            // return_data
    );
    // For a simple deposit there is no L1 entry produced by this builder
    // — the L1 side is the user's tx itself. The L2 table entries
    // (CALL + RESULT) are what the builder pushes via loadExecutionTable.
    let l2_entries = vec![call_entry, result_entry];
    let l1_entries: Vec<CrossChainExecutionEntry> = vec![];
    let all_action_hashes = MirrorCase::collect_hashes(&l1_entries, &l2_entries);
    MirrorCase {
        name: "deposit_simple",
        pattern: MirrorPattern::Simple,
        direction: CallDirection::L1ToL2,
        l1_entries,
        l2_entries,
        all_action_hashes,
    }
}

// ──────────────────────────────────────────────
//  Case 2: withdrawal_simple — L2→L1 single CALL+RESULT pair
// ──────────────────────────────────────────────

fn case_withdrawal_simple() -> MirrorCase {
    let entries = build_l2_to_l1_call_entries(
        addr_dest_a(), // L1 destination
        vec![],        // ETH withdrawal: no calldata
        U256::from(1_000_000u64),
        addr_user(),
        L2_ROLLUP_ID,
        vec![0xc0],       // RLP-encoded L2TX trigger payload (sentinel)
        vec![],           // delivery_return_data
        false,            // delivery_failed
        vec![U256::ZERO], // l1_delivery_scope
        false,            // tx_reverts
    );
    let l2_entries = entries.l2_table_entries;
    let l1_entries = entries.l1_deferred_entries;
    let all_action_hashes = MirrorCase::collect_hashes(&l1_entries, &l2_entries);
    MirrorCase {
        name: "withdrawal_simple",
        pattern: MirrorPattern::Simple,
        direction: CallDirection::L2ToL1,
        l1_entries,
        l2_entries,
        all_action_hashes,
    }
}

// ──────────────────────────────────────────────
//  Case 3: flash_loan_3_call — L1→L2 multi-call continuation
// ──────────────────────────────────────────────
//
// Canonical pattern (mirrors `test_three_call_continuation_with_l2_to_l1_child`):
//   CALL_A (L1→L2)  Bridge_L1 → Bridge_L2.receiveTokens
//   CALL_B (L1→L2)  executor → executorL2.claimAndBridgeBack (continuation)
//     └─ CALL_C (L2→L1)  Bridge_L2 → Bridge_L1.receiveTokens (child of B)

fn case_flash_loan_3_call() -> MirrorCase {
    let l2_id = RollupId::new(U256::from(L2_ROLLUP_ID));
    let call_a = detected_l1_to_l2(addr_dest_a(), vec![0xAA; 4], addr_user(), false);
    let call_b = detected_l1_to_l2(addr_dest_b(), vec![0xBB; 4], addr_src_b(), true);
    let mut call_c = detected_l2_to_l1(addr_dest_c(), vec![0xCC; 4], addr_src_c(), Some(1), 1);
    // Child of CALL_B; ensure direction is L2ToL1.
    call_c.direction = CallDirection::L2ToL1;

    let calls = vec![call_a, call_b, call_c];
    let result = build_continuation_entries(&calls, l2_id);
    let all_action_hashes = MirrorCase::collect_hashes(&result.l1_entries, &result.l2_entries);
    MirrorCase {
        name: "flash_loan_3_call",
        pattern: MirrorPattern::Continuation,
        direction: CallDirection::L1ToL2,
        l1_entries: result.l1_entries,
        l2_entries: result.l2_entries,
        all_action_hashes,
    }
}

// ──────────────────────────────────────────────
//  Case 4: ping_pong_depth_2 — L2→L1 with depth-2 child tree
// ──────────────────────────────────────────────
//
// Mirrors `test_l2_to_l1_depth2_entry_generation`:
//   [0] CALL_A (root, depth=0, no children)
//   [1] CALL_B (root, depth=0, child=CALL_C)
//   [2] CALL_C (child of B, depth=1, child=CALL_D)
//   [3] CALL_D (grandchild of B, child of C, depth=2, leaf)

fn case_ping_pong_depth_2() -> MirrorCase {
    let l2_id = RollupId::new(U256::from(L2_ROLLUP_ID));
    let call_a = detected_l2_to_l1(addr_dest_a(), vec![0xA1], addr_user(), None, 0);
    let call_b = detected_l2_to_l1(addr_dest_b(), vec![0xB1], addr_src_b(), None, 0);
    let call_c = detected_l2_to_l1(addr_dest_c(), vec![0xC1], addr_src_c(), Some(1), 1);
    let call_d = detected_l2_to_l1(addr_dest_d(), vec![0xD1], addr_src_d(), Some(2), 2);

    let detected = vec![call_a, call_b, call_c, call_d];
    let result = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0], false);
    let all_action_hashes = MirrorCase::collect_hashes(&result.l1_entries, &result.l2_entries);
    MirrorCase {
        name: "ping_pong_depth_2",
        pattern: MirrorPattern::Continuation,
        direction: CallDirection::L2ToL1,
        l1_entries: result.l1_entries,
        l2_entries: result.l2_entries,
        all_action_hashes,
    }
}

// ──────────────────────────────────────────────
//  Case 5: ping_pong_depth_3 — L2→L1 linear 4-call chain
// ──────────────────────────────────────────────
//
// Pure linear chain (no siblings):
//   [0] CALL_A (root, depth=0, child=CALL_B)
//   [1] CALL_B (child of A, depth=1, child=CALL_C)
//   [2] CALL_C (child of B, depth=2, child=CALL_D)
//   [3] CALL_D (child of C, depth=3, leaf)
//
// This stresses the deeper-than-2 path in
// `build_l2_to_l1_continuation_entries` without introducing branching.

fn case_ping_pong_depth_3() -> MirrorCase {
    let l2_id = RollupId::new(U256::from(L2_ROLLUP_ID));
    let call_a = detected_l2_to_l1(addr_dest_a(), vec![0xA1], addr_user(), None, 0);
    let call_b = detected_l2_to_l1(addr_dest_b(), vec![0xB1], addr_src_b(), Some(0), 1);
    let call_c = detected_l2_to_l1(addr_dest_c(), vec![0xC1], addr_src_c(), Some(1), 2);
    let call_d = detected_l2_to_l1(addr_dest_d(), vec![0xD1], addr_src_d(), Some(2), 3);

    let detected = vec![call_a, call_b, call_c, call_d];
    let result = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0], false);
    let all_action_hashes = MirrorCase::collect_hashes(&result.l1_entries, &result.l2_entries);
    MirrorCase {
        name: "ping_pong_depth_3",
        pattern: MirrorPattern::Continuation,
        direction: CallDirection::L2ToL1,
        l1_entries: result.l1_entries,
        l2_entries: result.l2_entries,
        all_action_hashes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: every canonical case is constructible without panicking
    /// and produces at least one entry overall. This is the bare minimum
    /// guarantee of the DSL — deeper assertions live in mirror tests in
    /// `table_builder_tests.rs`.
    #[test]
    fn canonical_cases_construct_without_panicking() {
        let cases = canonical_cases();
        assert_eq!(cases.len(), 5, "expected exactly 5 canonical cases");
        for case in &cases {
            assert!(
                case.total_entries() > 0,
                "case {} produced zero entries",
                case.name
            );
        }
    }

    #[test]
    fn canonical_cases_have_distinct_names() {
        let cases = canonical_cases();
        let mut names: Vec<&'static str> = cases.iter().map(|c| c.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            5,
            "every canonical case must have a unique name"
        );
    }
}
