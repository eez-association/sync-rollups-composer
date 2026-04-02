//! Unit tests for table_builder.rs — validates continuation entry generation
//! against the IntegrationTestFlashLoan.t.sol Solidity test.

use super::*;
use alloy_primitives::{Address, address};

/// Helper: create a simple L1→L2 CALL action (deposit-like).
fn make_l1_to_l2_call(
    destination: Address,
    data: Vec<u8>,
    source_address: Address,
    l2_rollup_id: U256,
) -> CrossChainAction {
    CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: l2_rollup_id,
        destination,
        value: U256::ZERO,
        data,
        failed: false,
        source_address,
        source_rollup: U256::ZERO, // MAINNET
        scope: vec![],
    }
}

/// Helper: create a simple L2→L1 CALL action (withdrawal-like).
fn make_l2_to_l1_call(
    destination: Address,
    data: Vec<u8>,
    source_address: Address,
    l2_rollup_id: U256,
) -> CrossChainAction {
    CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::ZERO, // targeting MAINNET
        destination,
        value: U256::ZERO,
        data,
        failed: false,
        source_address,
        source_rollup: l2_rollup_id,
        scope: vec![],
    }
}

#[test]
fn test_empty_calls_produces_empty_entries() {
    let result = build_continuation_entries(&[], U256::from(1));
    assert!(result.l2_entries.is_empty());
    assert!(result.l1_entries.is_empty());
}

#[test]
fn test_single_l1_to_l2_call_produces_simple_entries() {
    // Single deposit-like call: CALL_A (L1→L2), no continuation, no children.
    let l2_id = U256::from(1);
    let bridge_l1 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let bridge_l2 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    let call_a = DetectedCall {
        direction: CallDirection::L1ToL2,
        call_action: make_l1_to_l2_call(bridge_l2, vec![0x01, 0x02], bridge_l1, l2_id),
        parent_call_index: None,
        is_continuation: false,
        depth: 0,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: vec![],
        discovery_iteration: 0,
    };

    let result = build_continuation_entries(std::slice::from_ref(&call_a), l2_id);

    // L2: 1 terminal entry (RESULT(L2,void) hash → RESULT(L2,void))
    assert_eq!(
        result.l2_entries.len(),
        1,
        "L2 should have 1 terminal entry"
    );
    let l2_0 = &result.l2_entries[0];
    assert_eq!(l2_0.next_action.action_type, CrossChainActionType::Result);
    assert_eq!(l2_0.next_action.rollup_id, l2_id);
    assert!(l2_0.next_action.data.is_empty());

    // L1: 1 entry (CALL_A hash → RESULT(L2,void))
    assert_eq!(result.l1_entries.len(), 1, "L1 should have 1 entry");
    let l1_0 = &result.l1_entries[0];
    assert_eq!(l1_0.action_hash, compute_action_hash(&call_a.call_action));
    assert_eq!(l1_0.next_action.action_type, CrossChainActionType::Result);
    assert_eq!(l1_0.next_action.rollup_id, l2_id);
}

/// Flash loan continuation entry test — matches IntegrationTestFlashLoan.t.sol exactly.
///
/// Detected calls:
///   CALL_A (L1→L2): Bridge_L1 → Bridge_L2.receiveTokens
///   CALL_B (L1→L2): executor → executorL2.claimAndBridgeBack (continuation of A)
///   CALL_C (L2→L1): Bridge_L2 → Bridge_L1.receiveTokens (child of B)
///
/// Expected L2 entries (3):
///   1. hash(RESULT(L2,void)) → CALL_B (continuation)
///   2. hash(CALL_C_unscoped) → RESULT(MAINNET,void)
///   3. hash(RESULT(L2,void)) → RESULT(L2,void) terminal
///
/// Expected L1 entries (3):
///   1. hash(CALL_A) → RESULT(L2,void)
///   2. hash(CALL_B) → CALL_C with scope=[0]
///   3. hash(RESULT(MAINNET,void)) → RESULT(L2,void)
#[test]
fn test_flash_loan_continuation_entries() {
    let l2_id = U256::from(1);
    let mainnet_id = U256::ZERO;

    // Addresses (arbitrary for test)
    let bridge_l1 = address!("1111111111111111111111111111111111111111");
    let bridge_l2 = address!("2222222222222222222222222222222222222222");
    let executor = address!("3333333333333333333333333333333333333333");
    let executor_l2 = address!("4444444444444444444444444444444444444444");

    // Calldata (simplified for test)
    let fwd_receive_tokens = vec![0xAA; 32]; // receiveTokens calldata
    let claim_and_bridge_back = vec![0xBB; 32]; // claimAndBridgeBack calldata
    let ret_receive_tokens = vec![0xCC; 32]; // return receiveTokens calldata

    // CALL_A: Bridge_L1 → Bridge_L2.receiveTokens
    let call_a_action = make_l1_to_l2_call(bridge_l2, fwd_receive_tokens.clone(), bridge_l1, l2_id);
    let call_a = DetectedCall {
        direction: CallDirection::L1ToL2,
        call_action: call_a_action.clone(),
        parent_call_index: None,
        is_continuation: false,
        depth: 0,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: vec![],
        discovery_iteration: 0,
    };

    // CALL_B: executor → executorL2.claimAndBridgeBack (continuation of A)
    let call_b_action =
        make_l1_to_l2_call(executor_l2, claim_and_bridge_back.clone(), executor, l2_id);
    let call_b = DetectedCall {
        direction: CallDirection::L1ToL2,
        call_action: call_b_action.clone(),
        parent_call_index: None,
        is_continuation: true,
        depth: 0,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: vec![],
        discovery_iteration: 0,
    };

    // CALL_C: Bridge_L2 → Bridge_L1.receiveTokens (child of B)
    let call_c_action = make_l2_to_l1_call(bridge_l1, ret_receive_tokens.clone(), bridge_l2, l2_id);
    let call_c = DetectedCall {
        direction: CallDirection::L2ToL1,
        call_action: call_c_action.clone(),
        parent_call_index: Some(1), // child of CALL_B (index 1)
        is_continuation: false,
        depth: 1,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: vec![],
        discovery_iteration: 0,
    };

    let calls = vec![call_a, call_b, call_c];
    let result = build_continuation_entries(&calls, l2_id);

    // ── Verify L2 entries (3) ──
    assert_eq!(result.l2_entries.len(), 3, "L2 should have 3 entries");

    let l2_result_void = result_void(l2_id);
    let l2_result_void_hash = compute_action_hash(&l2_result_void);
    let l1_result_void = result_void(mainnet_id);

    // L2 Entry 1: hash(RESULT(L2,void)) → CALL_B (continuation)
    let l2_e1 = &result.l2_entries[0];
    assert_eq!(
        l2_e1.action_hash, l2_result_void_hash,
        "L2[0] actionHash should be hash(RESULT(L2,void))"
    );
    assert_eq!(
        l2_e1.next_action.action_type,
        CrossChainActionType::Call,
        "L2[0] nextAction should be CALL"
    );
    assert_eq!(
        l2_e1.next_action.destination, executor_l2,
        "L2[0] nextAction destination should be executorL2"
    );
    assert_eq!(
        l2_e1.next_action.data, claim_and_bridge_back,
        "L2[0] nextAction data should be claimAndBridgeBack"
    );
    assert_eq!(
        l2_e1.next_action.source_address, executor,
        "L2[0] nextAction sourceAddress should be executor"
    );
    assert!(
        l2_e1.state_deltas.is_empty(),
        "L2 entries have no state deltas"
    );

    // L2 Entry 2: hash(CALL_C_unscoped) → RESULT(MAINNET,void)
    let l2_e2 = &result.l2_entries[1];
    let call_c_hash = compute_action_hash(&call_c_action);
    assert_eq!(
        l2_e2.action_hash, call_c_hash,
        "L2[1] actionHash should be hash(CALL_C)"
    );
    assert_eq!(
        l2_e2.next_action.action_type,
        CrossChainActionType::Result,
        "L2[1] nextAction should be RESULT"
    );
    assert_eq!(
        l2_e2.next_action.rollup_id, mainnet_id,
        "L2[1] nextAction rollupId should be MAINNET"
    );
    assert!(
        l2_e2.next_action.data.is_empty(),
        "L2[1] nextAction data should be empty (void)"
    );

    // L2 Entry 3: hash(RESULT(L2,void)) → RESULT(L2,void) terminal
    let l2_e3 = &result.l2_entries[2];
    assert_eq!(
        l2_e3.action_hash, l2_result_void_hash,
        "L2[2] actionHash should be hash(RESULT(L2,void))"
    );
    assert_eq!(
        l2_e3.next_action.action_type,
        CrossChainActionType::Result,
        "L2[2] nextAction should be RESULT"
    );
    assert_eq!(
        l2_e3.next_action.rollup_id, l2_id,
        "L2[2] nextAction rollupId should be L2"
    );
    assert!(
        l2_e3.next_action.data.is_empty(),
        "L2[2] nextAction data should be empty (void terminal)"
    );

    // ── Verify L1 entries (3) ──
    assert_eq!(result.l1_entries.len(), 3, "L1 should have 3 entries");

    // L1 Entry 1: hash(CALL_A) → RESULT(L2,void) terminal
    let l1_e1 = &result.l1_entries[0];
    assert_eq!(
        l1_e1.action_hash,
        compute_action_hash(&call_a_action),
        "L1[0] actionHash should be hash(CALL_A)"
    );
    assert_eq!(
        l1_e1.next_action.action_type,
        CrossChainActionType::Result,
        "L1[0] nextAction should be RESULT"
    );
    assert_eq!(
        l1_e1.next_action.rollup_id, l2_id,
        "L1[0] nextAction rollupId should be L2"
    );

    // L1 Entry 2: hash(CALL_B) → CALL_C with scope=[0]
    let l1_e2 = &result.l1_entries[1];
    assert_eq!(
        l1_e2.action_hash,
        compute_action_hash(&call_b_action),
        "L1[1] actionHash should be hash(CALL_B)"
    );
    assert_eq!(
        l1_e2.next_action.action_type,
        CrossChainActionType::Call,
        "L1[1] nextAction should be CALL"
    );
    assert_eq!(
        l1_e2.next_action.destination, bridge_l1,
        "L1[1] nextAction destination should be Bridge_L1"
    );
    assert_eq!(
        l1_e2.next_action.data, ret_receive_tokens,
        "L1[1] nextAction data should be receiveTokens return"
    );
    assert_eq!(
        l1_e2.next_action.source_address, bridge_l2,
        "L1[1] nextAction sourceAddress should be Bridge_L2"
    );
    assert_eq!(
        l1_e2.next_action.source_rollup, l2_id,
        "L1[1] nextAction sourceRollup should be L2"
    );
    assert_eq!(
        l1_e2.next_action.scope,
        vec![U256::ZERO],
        "L1[1] nextAction scope should be [0]"
    );

    // L1 Entry 3: hash(RESULT(MAINNET,void)) → RESULT(L2,void)
    let l1_e3 = &result.l1_entries[2];
    let l1_result_void_hash = compute_action_hash(&l1_result_void);
    assert_eq!(
        l1_e3.action_hash, l1_result_void_hash,
        "L1[2] actionHash should be hash(RESULT(MAINNET,void))"
    );
    assert_eq!(
        l1_e3.next_action.action_type,
        CrossChainActionType::Result,
        "L1[2] nextAction should be RESULT"
    );
    assert_eq!(
        l1_e3.next_action.rollup_id, l2_id,
        "L1[2] nextAction rollupId should be L2"
    );
}

/// Test that action hashes match between Rust and Solidity encoding.
///
/// The hash is `keccak256(abi.encode(Action))` — this test verifies that
/// our `compute_action_hash` produces deterministic, correct results.
#[test]
fn test_action_hash_determinism() {
    let action = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: U256::from(1),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };

    let hash1 = compute_action_hash(&action);
    let hash2 = compute_action_hash(&action);
    assert_eq!(hash1, hash2, "Action hash must be deterministic");
    assert_ne!(hash1, B256::ZERO, "Action hash must not be zero");
}

/// Test two L1→L2 calls with a continuation but no children.
///
/// CALL_A (L1→L2) → CALL_B (L1→L2, continuation)
/// No L2→L1 children.
#[test]
fn test_two_continuations_no_children() {
    let l2_id = U256::from(1);
    let addr_a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let addr_b = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let src = address!("cccccccccccccccccccccccccccccccccccccccc");

    let call_a = DetectedCall {
        direction: CallDirection::L1ToL2,
        call_action: make_l1_to_l2_call(addr_a, vec![0x01], src, l2_id),
        parent_call_index: None,
        is_continuation: false,
        depth: 0,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: vec![],
        discovery_iteration: 0,
    };
    let call_b = DetectedCall {
        direction: CallDirection::L1ToL2,
        call_action: make_l1_to_l2_call(addr_b, vec![0x02], src, l2_id),
        parent_call_index: None,
        is_continuation: true,
        depth: 0,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: vec![],
        discovery_iteration: 0,
    };

    let result = build_continuation_entries(&[call_a, call_b], l2_id);

    // L2: 2 entries — continuation + terminal
    assert_eq!(result.l2_entries.len(), 2);
    // Entry 1: continuation → CALL_B
    assert_eq!(
        result.l2_entries[0].next_action.action_type,
        CrossChainActionType::Call
    );
    assert_eq!(result.l2_entries[0].next_action.destination, addr_b);
    // Entry 2: terminal RESULT
    assert_eq!(
        result.l2_entries[1].next_action.action_type,
        CrossChainActionType::Result
    );

    // L1: 2 entries — both simple (no children)
    assert_eq!(result.l1_entries.len(), 2);
    assert_eq!(
        result.l1_entries[0].next_action.action_type,
        CrossChainActionType::Result
    );
    assert_eq!(
        result.l1_entries[1].next_action.action_type,
        CrossChainActionType::Result
    );
}

/// Helper: create a simple L2→L1 CALL action for use in L2→L1 continuation tests.
fn make_l2_to_l1_detected(
    destination: Address,
    data: Vec<u8>,
    source_address: Address,
    l2_rollup_id: U256,
    parent_call_index: Option<usize>,
    depth: usize,
) -> DetectedCall {
    DetectedCall {
        direction: CallDirection::L2ToL1,
        call_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: U256::ZERO, // targeting L1 (MAINNET)
            destination,
            value: U256::ZERO,
            data,
            failed: false,
            source_address,
            source_rollup: l2_rollup_id,
            scope: vec![],
        },
        parent_call_index,
        is_continuation: false,
        depth,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: vec![],
        discovery_iteration: 0,
    }
}

/// Depth-2 L2→L1 continuation entry generation test.
///
/// Call tree:
///   [0] CALL_A (root, depth=0, no children)
///   [1] CALL_B (root, depth=0, child=CALL_C)
///   [2] CALL_C (child of B, depth=1, child=CALL_D)
///   [3] CALL_D (grandchild of B, child of C, depth=2, leaf)
///
/// Expected L2 entries (5):
///   1. hash(CALL_A) → RESULT(L1, void)                   — simple terminal
///   2. hash(CALL_B) → callReturn_C{scope=[0]}             — scope nav for first child
///   3. hash(RESULT{L2,void}) → RESULT(L1, void)           — B's scope resolution
///   4. hash(CALL_C) → callReturn_D{scope=[0]}             — scope nav for grandchild
///   5. hash(RESULT{L2,void}) → RESULT(L1, void)           — C's scope resolution
///
/// Expected L1 entries (7):
///   1. hash(trigger_A)       → delivery_A{scope=[0]}      — first call delivery
///   2. hash(RESULT(L1,void)) → RESULT(L1, void)           — A's delivery result
///   3. hash(trigger_B)       → execution_B{scope=[0]}     — subsequent call with children
///   4. hash(child_trigger_C) → execution_C{scope=[0]}     — reentrant internal child C
///   5. hash(child_trigger_D) → RESULT(L1, void)           — reentrant leaf grandchild D
///   6. hash(RESULT(L1,void)) → RESULT(L1, void)           — C's scope resolution
///   7. hash(RESULT(L1,void)) → RESULT(L1, void)           — B's scope resolution
#[test]
fn test_l2_to_l1_depth2_entry_generation() {
    let l2_id = U256::from(1);
    let _builder = address!("0000000000000000000000000000000000000001");

    // Distinct addresses per call to prevent hash collisions masking bugs.
    let dest_a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let src_a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabb");
    let dest_b = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let src_b = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbcc");
    let dest_c = address!("cccccccccccccccccccccccccccccccccccccccc");
    let src_c = address!("ccccccccccccccccccccccccccccccccccccccdd");
    let dest_d = address!("dddddddddddddddddddddddddddddddddddddddd");
    let src_d = address!("ddddddddddddddddddddddddddddddddddddddee");

    let call_a = make_l2_to_l1_detected(dest_a, vec![0xA1], src_a, l2_id, None, 0);
    let call_b = make_l2_to_l1_detected(dest_b, vec![0xB1], src_b, l2_id, None, 0);
    let call_c = make_l2_to_l1_detected(dest_c, vec![0xC1], src_c, l2_id, Some(1), 1);
    let call_d = make_l2_to_l1_detected(dest_d, vec![0xD1], src_d, l2_id, Some(2), 2);

    let detected = vec![
        call_a.clone(),
        call_b.clone(),
        call_c.clone(),
        call_d.clone(),
    ];
    let result = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0]);

    // ── L2 entries: 5 total ──
    assert_eq!(
        result.l2_entries.len(),
        5,
        "expected 5 L2 entries for depth-2 tree"
    );

    let l1_result_void = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: U256::ZERO,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let l2_result_void = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: l2_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let l1_result_hash = compute_action_hash(&l1_result_void);
    let l2_result_hash = compute_action_hash(&l2_result_void);
    let call_a_hash = compute_action_hash(&call_a.call_action);
    let call_b_hash = compute_action_hash(&call_b.call_action);
    let call_c_hash = compute_action_hash(&call_c.call_action);

    // L2 Entry 0: CALL_A simple terminal
    let l2_e0 = &result.l2_entries[0];
    assert_eq!(
        l2_e0.action_hash, call_a_hash,
        "L2[0] actionHash must be hash(CALL_A)"
    );
    assert_eq!(
        l2_e0.next_action.action_type,
        CrossChainActionType::Result,
        "L2[0] should be terminal RESULT"
    );
    assert_eq!(
        l2_e0.next_action.rollup_id,
        U256::ZERO,
        "L2[0] RESULT rollupId must be L1 (0)"
    );

    // L2 Entry 1: CALL_B scope navigation — callReturn for CALL_C
    let l2_e1 = &result.l2_entries[1];
    assert_eq!(
        l2_e1.action_hash, call_b_hash,
        "L2[1] actionHash must be hash(CALL_B)"
    );
    assert_eq!(
        l2_e1.next_action.action_type,
        CrossChainActionType::Call,
        "L2[1] should be callReturn CALL"
    );
    // callReturn.destination = child.source_address (L2 contract, e.g. src_c)
    assert_eq!(
        l2_e1.next_action.destination, src_c,
        "L2[1] callReturn.destination must be CALL_C.source_address"
    );
    // callReturn.source_address = child.destination (proxy originalAddress, e.g. dest_c)
    assert_eq!(
        l2_e1.next_action.source_address, dest_c,
        "L2[1] callReturn.source_address must be CALL_C.destination"
    );
    assert_eq!(
        l2_e1.next_action.scope,
        vec![U256::ZERO],
        "L2[1] callReturn scope must be [0]"
    );

    // L2 Entry 2: B's scope resolution — hash(RESULT{L2,void}) → RESULT(L1,void)
    let l2_e2 = &result.l2_entries[2];
    assert_eq!(
        l2_e2.action_hash, l2_result_hash,
        "L2[2] actionHash must be hash(RESULT{{L2,void}})"
    );
    assert_eq!(
        l2_e2.next_action.action_type,
        CrossChainActionType::Result,
        "L2[2] should be RESULT(L1,void)"
    );
    assert_eq!(
        l2_e2.next_action.rollup_id,
        U256::ZERO,
        "L2[2] scope resolution must target L1"
    );

    // L2 Entry 3: CALL_C scope navigation — callReturn for CALL_D
    let l2_e3 = &result.l2_entries[3];
    assert_eq!(
        l2_e3.action_hash, call_c_hash,
        "L2[3] actionHash must be hash(CALL_C)"
    );
    assert_eq!(
        l2_e3.next_action.action_type,
        CrossChainActionType::Call,
        "L2[3] should be callReturn CALL"
    );
    // callReturn.destination = CALL_D.source_address
    assert_eq!(
        l2_e3.next_action.destination, src_d,
        "L2[3] callReturn.destination must be CALL_D.source_address"
    );
    assert_eq!(
        l2_e3.next_action.source_address, dest_d,
        "L2[3] callReturn.source_address must be CALL_D.destination"
    );
    assert_eq!(
        l2_e3.next_action.scope,
        vec![U256::ZERO],
        "L2[3] scope must be [0] — each reentrant call starts fresh"
    );

    // L2 Entry 4: C's scope resolution — hash(RESULT{L2,void}) → RESULT(L1,void)
    let l2_e4 = &result.l2_entries[4];
    assert_eq!(
        l2_e4.action_hash, l2_result_hash,
        "L2[4] actionHash must be hash(RESULT{{L2,void}})"
    );
    assert_eq!(
        l2_e4.next_action.action_type,
        CrossChainActionType::Result,
        "L2[4] should be RESULT(L1,void)"
    );
    assert_eq!(
        l2_e4.next_action.rollup_id,
        U256::ZERO,
        "L2[4] scope resolution must target L1"
    );

    // ── L1 entries: 6 total (chained model) ──
    // Chained: L2TX→A, RESULT(A)→B, child_C→D, leaf_D, scope_D, RESULT(B)→terminal
    // NOTE: After reorder_for_swap_and_pop, entries may be physically reordered.
    assert_eq!(
        result.l1_entries.len(),
        6,
        "expected 6 L1 entries for depth-2 tree (chained model)"
    );

    // Build trigger actions to compute expected hashes.
    // L2TX trigger: all root calls share the same L2TX hash (same rlp_encoded_tx).
    let l2tx_trigger = CrossChainAction {
        action_type: CrossChainActionType::L2Tx,
        rollup_id: l2_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![0xc0], // placeholder rlp_encoded_tx
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let child_trigger_c = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: l2_id,
        destination: src_c,
        value: U256::ZERO,
        data: vec![0xC1],
        failed: false,
        source_address: dest_c,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let child_trigger_d = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: l2_id,
        destination: src_d,
        value: U256::ZERO,
        data: vec![0xD1],
        failed: false,
        source_address: dest_d,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let l2tx_trigger_hash = compute_action_hash(&l2tx_trigger);
    let child_trigger_c_hash = compute_action_hash(&child_trigger_c);
    let child_trigger_d_hash = compute_action_hash(&child_trigger_d);

    // Helper: find entry by action_hash
    let find_l1 = |hash: B256| -> Vec<&CrossChainExecutionEntry> {
        result
            .l1_entries
            .iter()
            .filter(|e| e.action_hash == hash)
            .collect()
    };

    // L2TX trigger: only 1 entry (chained model: only first call has L2TX trigger).
    let entries_l2tx = find_l1(l2tx_trigger_hash);
    assert_eq!(
        entries_l2tx.len(),
        1,
        "chained model: only 1 L2TX trigger entry (first call only)"
    );
    // L2TX entry → CALL(A, scope=[])  — nested pattern (B has children), no sibling scope
    assert_eq!(
        entries_l2tx[0].next_action.action_type,
        CrossChainActionType::Call
    );
    assert_eq!(
        entries_l2tx[0].next_action.scope,
        vec![] as Vec<U256>,
        "nested pattern: delivery scope must be [] (no sibling routing)"
    );
    assert_eq!(entries_l2tx[0].next_action.destination, dest_a);

    // In the chained model, RESULT(A) → CALL(B, scope=[1]) is a RESULT-triggered entry.
    // It uses the same RESULT hash as other void results, verified via find_l1 below.

    // child_trigger_C → execution_C{scope=[0]}
    let entries_c = find_l1(child_trigger_c_hash);
    assert_eq!(
        entries_c.len(),
        1,
        "must have exactly 1 child_trigger_C entry"
    );
    assert_eq!(
        entries_c[0].next_action.action_type,
        CrossChainActionType::Call
    );
    assert_eq!(entries_c[0].next_action.scope, vec![U256::ZERO]);

    // child_trigger_D → RESULT(L1, void) — leaf grandchild must NOT be orphaned
    let entries_d = find_l1(child_trigger_d_hash);
    assert_eq!(
        entries_d.len(),
        1,
        "must have exactly 1 child_trigger_D entry — D must not be orphaned"
    );
    assert_eq!(
        entries_d[0].next_action.action_type,
        CrossChainActionType::Result
    );
    assert_eq!(entries_d[0].next_action.rollup_id, U256::ZERO);

    // 3 RESULT(L1,void)-triggered entries:
    //   1. RESULT(A) → CALL(B, scope=[1])  (chained sibling)
    //   2. scope_D resolution → RESULT
    //   3. RESULT(B) → RESULT(terminal)
    let entries_result = find_l1(l1_result_hash);
    assert_eq!(
        entries_result.len(),
        3,
        "must have 3 RESULT(L1,void)-triggered entries"
    );
    // At least one must chain to CALL (the sibling chain), rest are RESULT
    let call_count = entries_result
        .iter()
        .filter(|e| e.next_action.action_type == CrossChainActionType::Call)
        .count();
    let result_count = entries_result
        .iter()
        .filter(|e| e.next_action.action_type == CrossChainActionType::Result)
        .count();
    assert_eq!(call_count, 1, "one RESULT entry chains to CALL(B)");
    assert_eq!(
        result_count, 2,
        "two RESULT entries are terminal/scope resolution"
    );

    // All entries must have empty state deltas (driver fills later).
    for (i, e) in result.l2_entries.iter().enumerate() {
        assert!(
            e.state_deltas.is_empty(),
            "L2[{i}] state_deltas must be empty"
        );
    }
    for (i, e) in result.l1_entries.iter().enumerate() {
        // L1 entries may have placeholder state deltas with ether_delta
        // (e.g., withdrawal trigger entries with negative ether_delta).
        // The currentState/newState are placeholders (B256::ZERO) filled by the driver.
        for delta in &e.state_deltas {
            assert_eq!(
                delta.current_state,
                alloy_primitives::B256::ZERO,
                "L1[{i}] state_deltas.currentState must be placeholder ZERO"
            );
            assert_eq!(
                delta.new_state,
                alloy_primitives::B256::ZERO,
                "L1[{i}] state_deltas.newState must be placeholder ZERO"
            );
        }
    }
}

/// Specifically verifies that CALL_D (depth-2 grandchild, parent=Some(2)) is not orphaned.
///
/// Orphan = a detected call that never appears in any L1 entry. Before the depth-2 fix,
/// `push_reentrant_child_entries` only processed direct children; grandchildren whose
/// parent_call_index pointed to another child (not a root) were silently dropped.
///
/// This test confirms CALL_D appears in the L1 entries and that the depth-2 configuration
/// produces more entries than a depth-1 equivalent would.
#[test]
fn test_l2_to_l1_depth2_child_not_orphaned() {
    let l2_id = U256::from(1);
    let _builder = address!("0000000000000000000000000000000000000001");

    // Minimal tree: one root with one child (depth=1) that has one grandchild (depth=2).
    let dest_root = address!("1111111111111111111111111111111111111111");
    let src_root = address!("1111111111111111111111111111111111111122");
    let dest_child = address!("2222222222222222222222222222222222222222");
    let src_child = address!("2222222222222222222222222222222222222233");
    let dest_grand = address!("3333333333333333333333333333333333333333");
    let src_grand = address!("3333333333333333333333333333333333333344");

    // depth-1 scenario (root + one leaf child) for baseline comparison.
    let d1_root = make_l2_to_l1_detected(dest_root, vec![0x11], src_root, l2_id, None, 0);
    let d1_child = make_l2_to_l1_detected(dest_child, vec![0x22], src_child, l2_id, Some(0), 1);

    let depth1_result =
        build_l2_to_l1_continuation_entries(&[d1_root.clone(), d1_child.clone()], l2_id, &[0xc0]);

    // depth-2 scenario (root + child + grandchild).
    let d2_root = make_l2_to_l1_detected(dest_root, vec![0x11], src_root, l2_id, None, 0);
    let d2_child = make_l2_to_l1_detected(dest_child, vec![0x22], src_child, l2_id, Some(0), 1);
    let d2_grand = make_l2_to_l1_detected(dest_grand, vec![0x33], src_grand, l2_id, Some(1), 2);

    let depth2_result =
        build_l2_to_l1_continuation_entries(&[d2_root, d2_child, d2_grand.clone()], l2_id, &[0xc0]);

    // depth-2 must produce strictly more entries than depth-1.
    assert!(
        depth2_result.l1_entries.len() > depth1_result.l1_entries.len(),
        "depth-2 must produce more L1 entries than depth-1; got d1={} d2={}",
        depth1_result.l1_entries.len(),
        depth2_result.l1_entries.len(),
    );
    assert!(
        depth2_result.l2_entries.len() > depth1_result.l2_entries.len(),
        "depth-2 must produce more L2 entries than depth-1; got d1={} d2={}",
        depth1_result.l2_entries.len(),
        depth2_result.l2_entries.len(),
    );

    // Verify CALL_D (grandchild) appears in the depth-2 L1 entries.
    // Its trigger hash: destination=src_grand, source_address=dest_grand.
    let grandchild_trigger = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: l2_id,
        destination: src_grand,
        value: U256::ZERO,
        data: vec![0x33],
        failed: false,
        source_address: dest_grand,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let grandchild_trigger_hash = compute_action_hash(&grandchild_trigger);

    let found = depth2_result
        .l1_entries
        .iter()
        .any(|e| e.action_hash == grandchild_trigger_hash);

    assert!(
        found,
        "grandchild (depth-2) trigger hash must appear in L1 entries — it was previously orphaned"
    );

    // Confirm the grandchild entry is a leaf (next_action = RESULT).
    let grandchild_entry = depth2_result
        .l1_entries
        .iter()
        .find(|e| e.action_hash == grandchild_trigger_hash)
        .expect("grandchild entry must exist");

    assert_eq!(
        grandchild_entry.next_action.action_type,
        CrossChainActionType::Result,
        "grandchild leaf next_action must be terminal RESULT"
    );
}

/// Regression test: depth-1 L2→L1 continuation produces exactly the same structure as before.
///
/// This is the standard 2-call reverse flash loan pattern:
///   [0] CALL_A (root, no children)
///   [1] CALL_B (root, one child CALL_C)
///   [2] CALL_C (child of B, leaf)
///
/// Expected L2 entries (3):
///   1. hash(CALL_A) → RESULT(L1, void)               — simple terminal
///   2. hash(CALL_B) → callReturn_C{scope=[0]}         — scope navigation
///   3. hash(RESULT{L2,void}) → RESULT(L1, void)       — scope resolution
///
/// Expected L1 entries (5, matching docstring example):
///   0. hash(trigger_A)       → delivery_A{scope=[0]}
///   1. hash(RESULT(L1,void)) → RESULT(L1, void)       — delivery result
///   2. hash(trigger_B)       → execution_B{scope=[0]}
///   3. hash(child_trigger_C) → RESULT(L1, void)       — reentrant leaf child
///   4. hash(RESULT(L1,void)) → RESULT(L1, void)       — scope resolution
#[test]
fn test_l2_to_l1_depth1_regression() {
    let l2_id = U256::from(1);
    let _builder = address!("dead000000000000000000000000000000000000");

    let dest_a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let src_a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabb");
    let dest_b = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let src_b = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbcc");
    let dest_c = address!("cccccccccccccccccccccccccccccccccccccccc");
    let src_c = address!("ccccccccccccccccccccccccccccccccccccccdd");

    let call_a = make_l2_to_l1_detected(dest_a, vec![0xA1], src_a, l2_id, None, 0);
    let call_b = make_l2_to_l1_detected(dest_b, vec![0xB1], src_b, l2_id, None, 0);
    let call_c = make_l2_to_l1_detected(dest_c, vec![0xC1], src_c, l2_id, Some(1), 1);

    let detected = vec![call_a.clone(), call_b.clone(), call_c.clone()];
    let result = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0]);

    // ── L2 entries: exactly 3 ──
    assert_eq!(
        result.l2_entries.len(),
        3,
        "depth-1 must produce exactly 3 L2 entries"
    );

    let l1_result_void = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: U256::ZERO,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let l2_result_void = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: l2_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let l1_result_hash = compute_action_hash(&l1_result_void);
    let l2_result_hash = compute_action_hash(&l2_result_void);
    let call_a_hash = compute_action_hash(&call_a.call_action);
    let call_b_hash = compute_action_hash(&call_b.call_action);

    // L2 Entry 0: CALL_A simple terminal
    let l2_e0 = &result.l2_entries[0];
    assert_eq!(l2_e0.action_hash, call_a_hash, "L2[0] must be hash(CALL_A)");
    assert_eq!(
        l2_e0.next_action.action_type,
        CrossChainActionType::Result,
        "L2[0] must be terminal RESULT"
    );
    assert_eq!(
        l2_e0.next_action.rollup_id,
        U256::ZERO,
        "L2[0] RESULT must target L1"
    );

    // L2 Entry 1: CALL_B scope navigation callReturn for C
    let l2_e1 = &result.l2_entries[1];
    assert_eq!(l2_e1.action_hash, call_b_hash, "L2[1] must be hash(CALL_B)");
    assert_eq!(
        l2_e1.next_action.action_type,
        CrossChainActionType::Call,
        "L2[1] must be callReturn"
    );
    assert_eq!(
        l2_e1.next_action.destination, src_c,
        "L2[1] callReturn.destination must be CALL_C.source_address"
    );
    assert_eq!(
        l2_e1.next_action.scope,
        vec![U256::ZERO],
        "L2[1] scope must be [0]"
    );

    // L2 Entry 2: B's scope resolution
    let l2_e2 = &result.l2_entries[2];
    assert_eq!(
        l2_e2.action_hash, l2_result_hash,
        "L2[2] must be hash(RESULT{{L2,void}})"
    );
    assert_eq!(
        l2_e2.next_action.action_type,
        CrossChainActionType::Result,
        "L2[2] must be RESULT(L1,void)"
    );
    assert_eq!(
        l2_e2.next_action.rollup_id,
        U256::ZERO,
        "L2[2] RESULT must target L1"
    );

    // ── L1 entries: chained pattern ──
    // Nested pattern (B has child C): has_any_nested=true, so ALL L1 scopes are [].
    // Chained model: L2TX→CALL(A,scope=[]), RESULT(A)→CALL(B,scope=[]), child_C, RESULT(B)→terminal
    assert_eq!(
        result.l1_entries.len(),
        4,
        "chained multi-call must produce exactly 4 L1 entries (L2TX→A, RESULT→B, child_C, RESULT→terminal)"
    );

    let l2tx_trigger = CrossChainAction {
        action_type: CrossChainActionType::L2Tx,
        rollup_id: l2_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![0xc0],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let child_trigger_c = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: l2_id,
        destination: src_c,
        value: U256::ZERO,
        data: vec![0xC1],
        failed: false,
        source_address: dest_c,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let l2tx_trigger_hash = compute_action_hash(&l2tx_trigger);
    let child_trigger_c_hash = compute_action_hash(&child_trigger_c);

    // L1[0]: L2TX → CALL(A, scope=[])  — nested pattern, no sibling scope
    let l1_e0 = &result.l1_entries[0];
    assert_eq!(l1_e0.action_hash, l2tx_trigger_hash, "L1[0] trigger = L2TX");
    assert_eq!(
        l1_e0.next_action.action_type,
        CrossChainActionType::Call,
        "L1[0] next = CALL"
    );
    assert_eq!(
        l1_e0.next_action.scope,
        vec![] as Vec<U256>,
        "L1[0] scope=[]"
    );
    assert_eq!(l1_e0.next_action.destination, dest_a, "L1[0] dest = A");

    // L1[1]: RESULT(A,void) → CALL(B, scope=[])  (chained, nested pattern → no scope)
    let l1_e1 = &result.l1_entries[1];
    assert_eq!(
        l1_e1.action_hash, l1_result_hash,
        "L1[1] trigger = RESULT(void)"
    );
    assert_eq!(
        l1_e1.next_action.action_type,
        CrossChainActionType::Call,
        "L1[1] next = CALL (chained)"
    );
    assert_eq!(
        l1_e1.next_action.scope,
        vec![] as Vec<U256>,
        "L1[1] scope=[]"
    );
    assert_eq!(l1_e1.next_action.destination, dest_b, "L1[1] dest = B");

    // L1[2]: reentrant leaf child CALL_C
    let l1_e2 = &result.l1_entries[2];
    assert_eq!(
        l1_e2.action_hash, child_trigger_c_hash,
        "L1[2] trigger = child_C"
    );
    assert_eq!(
        l1_e2.next_action.action_type,
        CrossChainActionType::Result,
        "L1[2] next = RESULT (leaf)"
    );

    // L1[3]: RESULT(B,void) → RESULT(terminal)  (last call)
    let l1_e3 = &result.l1_entries[3];
    assert_eq!(
        l1_e3.action_hash, l1_result_hash,
        "L1[3] trigger = RESULT(void)"
    );
    assert_eq!(
        l1_e3.next_action.action_type,
        CrossChainActionType::Result,
        "L1[3] next = terminal"
    );
    assert_eq!(
        l1_e3.next_action.rollup_id, l2_id,
        "L1[3] terminal rollupId = L2"
    );
}

/// Test that L2 entries have empty state deltas (driver fills them later).
#[test]
fn test_all_entries_have_empty_state_deltas() {
    let l2_id = U256::from(1);
    let addr = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let src = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    let call = DetectedCall {
        direction: CallDirection::L1ToL2,
        call_action: make_l1_to_l2_call(addr, vec![0x01], src, l2_id),
        parent_call_index: None,
        is_continuation: false,
        depth: 0,
        delivery_return_data: vec![],
        l2_return_data: vec![],
        l2_delivery_failed: false,
        scope: vec![],
        discovery_iteration: 0,
    };

    let result = build_continuation_entries(&[call], l2_id);

    for entry in &result.l2_entries {
        assert!(
            entry.state_deltas.is_empty(),
            "L2 state deltas should be empty"
        );
    }
    // L1 trigger entries have placeholder state deltas (with ether_delta
    // from the call value, roots filled later by the driver).
    // For value=0 calls, ether_delta should be 0.
    for entry in &result.l1_entries {
        if !entry.state_deltas.is_empty() {
            assert_eq!(
                entry.state_deltas[0].ether_delta,
                alloy_primitives::I256::ZERO,
                "L1 ether_delta should be 0 for value=0 call"
            );
        }
    }
}

/// Regression test for issue #245: L2 scope resolution RESULT hash must include
/// the return call's l2_return_data when non-empty.
#[test]
fn test_l2_scope_resolution_uses_l2_return_data() {
    let l2_id = U256::from(1);
    let logger_l2 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let logger_l1 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let counter_l2 = address!("cccccccccccccccccccccccccccccccccccccccc");
    let _builder = address!("dddddddddddddddddddddddddddddddddddddddd");
    let increment_data = vec![0xd0, 0x9d, 0xe0, 0x8a];

    // Simulate Counter.increment() returning uint256(1) = 32 bytes
    let counter_return = U256::from(1).to_be_bytes_vec();
    // The L1 delivery of the root call also returns counter_return (#246: scope resolution
    // nextAction must carry this so _resolveScopes can return it to the L2 caller).
    let delivery_return = counter_return.clone();

    // Build detected calls: 1 L2→L1 call + 1 return call with l2_return_data
    let detected = vec![
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::ZERO,
                destination: logger_l1,
                value: U256::ZERO,
                data: increment_data.clone(),
                failed: false,
                source_address: logger_l2,
                source_rollup: l2_id,
                scope: vec![],
            },
            parent_call_index: None,
            is_continuation: false,
            depth: 0,
            delivery_return_data: counter_return.clone(), // L1 delivery also returns the counter value
            l2_return_data: vec![],
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: l2_id,
                destination: counter_l2,
                value: U256::ZERO,
                data: increment_data.clone(),
                failed: false,
                source_address: logger_l1,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
            parent_call_index: Some(0),
            is_continuation: false,
            depth: 1,
            delivery_return_data: vec![],
            l2_return_data: counter_return.clone(),
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
    ];

    let cont = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0]);

    // L2 entries: 2 (CALL + scope resolution)
    assert_eq!(cont.l2_entries.len(), 2, "should have 2 L2 entries");

    // The scope resolution entry (index 1) should NOT use result_void
    // because the child returns data.
    let scope_entry = &cont.l2_entries[1];
    let result_void_hash = compute_action_hash(&result_void(l2_id));
    assert_ne!(
        scope_entry.action_hash, result_void_hash,
        "L2 scope resolution hash must differ from result_void when child returns data"
    );

    // The scope resolution hash should match RESULT{rollupId=L2, data=counter_return}
    let expected_result = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: l2_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: counter_return,
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let expected_hash = compute_action_hash(&expected_result);
    assert_eq!(
        scope_entry.action_hash, expected_hash,
        "L2 scope resolution hash must include the child's l2_return_data"
    );

    // Issue #246: nextAction must also carry delivery return data so _resolveScopes
    // returns it to the L2 caller.
    assert_eq!(
        scope_entry.next_action.data, delivery_return,
        "L2 scope resolution nextAction.data must carry delivery return data (#246)"
    );
    assert_eq!(
        scope_entry.next_action.action_type,
        CrossChainActionType::Result,
        "L2 scope resolution nextAction must be RESULT"
    );
}

/// Test mixed void/non-void children: first child void, second child returns data.
/// The L2 scope resolution uses the LAST child's data, additional child transitions
/// use the PREVIOUS child's data.
#[test]
fn test_l2_mixed_void_nonvoid_children() {
    let l2_id = U256::from(1);
    let parent = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let child_a = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let child_b = address!("cccccccccccccccccccccccccccccccccccccccc");
    let _builder = address!("dddddddddddddddddddddddddddddddddddddddd");

    let child_b_return = vec![0x00, 0x01, 0x02]; // non-void return
    // The L1 delivery of the root call also returns data (#246: scope resolution
    // nextAction must carry this so _resolveScopes can return it to the L2 caller).
    let root_delivery_return = vec![0xAA, 0xBB, 0xCC];

    let detected = vec![
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::ZERO,
                destination: parent,
                value: U256::ZERO,
                data: vec![0x01],
                failed: false,
                source_address: parent,
                source_rollup: l2_id,
                scope: vec![],
            },
            parent_call_index: None,
            is_continuation: false,
            depth: 0,
            delivery_return_data: root_delivery_return.clone(),
            l2_return_data: vec![],
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
        // Child A: void return (return call targeting our rollup)
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: l2_id,
                destination: child_a,
                value: U256::ZERO,
                data: vec![0x02],
                failed: false,
                source_address: parent,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
            parent_call_index: Some(0),
            is_continuation: false,
            depth: 1,
            delivery_return_data: vec![],
            l2_return_data: vec![], // void
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
        // Child B: non-void return
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: l2_id,
                destination: child_b,
                value: U256::ZERO,
                data: vec![0x03],
                failed: false,
                source_address: parent,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
            parent_call_index: Some(0),
            is_continuation: false,
            depth: 1,
            delivery_return_data: vec![],
            l2_return_data: child_b_return.clone(),
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
    ];

    let cont = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0]);

    // Should have L2 entries: CALL(parent) → callReturn[0] for child_a,
    // then RESULT(void) → callReturn[1] for child_b (transition uses child_a's void data),
    // then scope resolution using child_b's non-void data.
    assert!(
        cont.l2_entries.len() >= 3,
        "mixed children need >= 3 L2 entries"
    );

    // The LAST L2 entry (scope resolution) should use child_b's return data
    let last = cont.l2_entries.last().unwrap();
    let void_hash = compute_action_hash(&result_void(l2_id));
    let nonvoid_result = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: l2_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: child_b_return,
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let nonvoid_hash = compute_action_hash(&nonvoid_result);
    assert_eq!(
        last.action_hash, nonvoid_hash,
        "scope resolution must use LAST child's non-void data"
    );

    // Issue #246: nextAction.data must carry delivery return data
    assert_eq!(
        last.next_action.data, root_delivery_return,
        "scope resolution nextAction.data must carry delivery return data (#246)"
    );

    // The intermediate transition (child_a → child_b) should use void hash
    // because child_a returns void
    let intermediate = &cont.l2_entries[1];
    assert_eq!(
        intermediate.action_hash, void_hash,
        "intermediate transition uses PREVIOUS child (void) data"
    );
}

/// Test L1 delivery RESULT hash with non-empty delivery_return_data.
/// Verifies push_reentrant_child_entries uses child.delivery_return_data (#246).
#[test]
fn test_l1_reentrant_child_delivery_return_data() {
    let l2_id = U256::from(1);
    let parent = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let child = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let _builder = address!("dddddddddddddddddddddddddddddddddddddddd");

    let delivery_data = vec![0xDE, 0xAD, 0xBE, 0xEF]; // non-void delivery return

    let detected = vec![
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::ZERO,
                destination: parent,
                value: U256::ZERO,
                data: vec![0x01],
                failed: false,
                source_address: parent,
                source_rollup: l2_id,
                scope: vec![],
            },
            parent_call_index: None,
            is_continuation: false,
            depth: 0,
            delivery_return_data: delivery_data.clone(), // L1 delivery returns data
            l2_return_data: vec![],
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: l2_id,
                destination: child,
                value: U256::ZERO,
                data: vec![0x02],
                failed: false,
                source_address: parent,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
            parent_call_index: Some(0),
            is_continuation: false,
            depth: 1,
            delivery_return_data: vec![0xCA, 0xFE], // child also has delivery data
            l2_return_data: vec![],
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
    ];

    let cont = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0]);

    // L1 entries should include the delivery RESULT with non-void data
    let void_l1 = result_void(U256::ZERO);
    let void_l1_hash = compute_action_hash(&void_l1);

    // The delivery RESULT entry (Entry 0b) should NOT use void hash
    // because delivery_return_data is non-empty.
    // rollupId: _processCallAtScope uses action.rollupId (child's target).
    // For L1→L2 return calls (child targets L2), rollupId = our_rollup_id (L2).
    let delivery_result_entry = cont.l1_entries.iter().find(|e| {
        e.action_hash != void_l1_hash
            && e.next_action.action_type == CrossChainActionType::Result
            && e.next_action.rollup_id == alloy_primitives::U256::from(1u64)
    });
    assert!(
        delivery_result_entry.is_some(),
        "L1 delivery RESULT entry must use non-void hash when delivery returns data (#246)"
    );

    // §C.6: L2TX terminal RESULT is always void with rollupId = triggering rollupId.
    // The root delivery RESULT entry's nextAction is the terminal, not the delivery data.
    let root_delivery_entry = cont.l1_entries.iter().find(|e| {
        e.action_hash != void_l1_hash
            && e.next_action.action_type == CrossChainActionType::Result
            && e.next_action.data.is_empty()
            && e.next_action.rollup_id == alloy_primitives::U256::from(1u64)
    });
    assert!(
        root_delivery_entry.is_some(),
        "L1 delivery RESULT nextAction must be terminal void with rollupId=L2 per §C.6"
    );
}

/// Test that void children still produce result_void hash (no regression).
#[test]
fn test_void_children_still_use_result_void() {
    let l2_id = U256::from(1);
    let parent = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let child = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let _builder = address!("dddddddddddddddddddddddddddddddddddddddd");

    let detected = vec![
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::ZERO,
                destination: parent,
                value: U256::ZERO,
                data: vec![0x01],
                failed: false,
                source_address: parent,
                source_rollup: l2_id,
                scope: vec![],
            },
            parent_call_index: None,
            is_continuation: false,
            depth: 0,
            delivery_return_data: vec![], // void
            l2_return_data: vec![],
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: l2_id,
                destination: child,
                value: U256::ZERO,
                data: vec![0x02],
                failed: false,
                source_address: parent,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
            parent_call_index: Some(0),
            is_continuation: false,
            depth: 1,
            delivery_return_data: vec![], // void
            l2_return_data: vec![],       // void
            l2_delivery_failed: false,
            scope: vec![],
            discovery_iteration: 0,
        },
    ];

    let cont = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0]);

    // All RESULT entries should use result_void hashes
    let void_l2_hash = compute_action_hash(&result_void(l2_id));
    let void_l1_hash = compute_action_hash(&result_void(U256::ZERO));

    // L2 scope resolution should be void
    let l2_scope = cont.l2_entries.last().unwrap();
    assert_eq!(
        l2_scope.action_hash, void_l2_hash,
        "void child → L2 scope uses result_void"
    );

    // L1 delivery result should be void
    let l1_delivery_results: Vec<_> = cont
        .l1_entries
        .iter()
        .filter(|e| e.action_hash == void_l1_hash)
        .collect();
    assert!(
        !l1_delivery_results.is_empty(),
        "void delivery → L1 uses result_void"
    );
}
