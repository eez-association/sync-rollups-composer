//! Unit tests for table_builder.rs — validates continuation entry generation
//! against the IntegrationTestFlashLoan.t.sol Solidity test.

use super::*;
use alloy_primitives::{Address, address};

/// Helper: create a simple L1→L2 CALL action (deposit-like).
fn make_l1_to_l2_call(
    destination: Address,
    data: Vec<u8>,
    source_address: Address,
    l2_rollup_id: RollupId,
) -> CrossChainAction {
    CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: l2_rollup_id,
        destination,
        value: U256::ZERO,
        data,
        failed: false,
        source_address,
        source_rollup: RollupId::MAINNET, // MAINNET
        scope: vec![],
    }
}

/// Helper: create a simple L2→L1 CALL action (withdrawal-like).
fn make_l2_to_l1_call(
    destination: Address,
    data: Vec<u8>,
    source_address: Address,
    l2_rollup_id: RollupId,
) -> CrossChainAction {
    CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: RollupId::MAINNET, // targeting MAINNET
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
    let result = build_continuation_entries(&[], RollupId::new(U256::from(1)));
    assert!(result.l2_entries.is_empty());
    assert!(result.l1_entries.is_empty());
}

#[test]
fn test_single_l1_to_l2_call_produces_simple_entries() {
    // Single deposit-like call: CALL_A (L1→L2), no continuation, no children.
    let l2_id = RollupId::new(U256::from(1));
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
        in_reverted_frame: false,
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
    let l2_id = RollupId::new(U256::from(1));
    let mainnet_id = RollupId::MAINNET;

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
        in_reverted_frame: false,
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
        in_reverted_frame: false,
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
        in_reverted_frame: false,
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
        rollup_id: RollupId::new(U256::from(1)),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
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
    let l2_id = RollupId::new(U256::from(1));
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
        in_reverted_frame: false,
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
        in_reverted_frame: false,
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
    l2_rollup_id: RollupId,
    parent_call_index: Option<usize>,
    depth: usize,
) -> DetectedCall {
    DetectedCall {
        direction: CallDirection::L2ToL1,
        call_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: RollupId::MAINNET, // targeting L1 (MAINNET)
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
        in_reverted_frame: false,
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
    let l2_id = RollupId::new(U256::from(1));
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
    let result = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0], false);

    // ── L2 entries: 5 total ──
    assert_eq!(
        result.l2_entries.len(),
        5,
        "expected 5 L2 entries for depth-2 tree"
    );

    let l1_result_void = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: RollupId::MAINNET,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
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
        source_rollup: RollupId::MAINNET,
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
        RollupId::MAINNET,
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
        RollupId::MAINNET,
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
        RollupId::MAINNET,
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
        source_rollup: RollupId::MAINNET,
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
        source_rollup: RollupId::MAINNET,
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
        source_rollup: RollupId::MAINNET,
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
    assert_eq!(entries_d[0].next_action.rollup_id, RollupId::MAINNET);

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
    let l2_id = RollupId::new(U256::from(1));
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

    let depth1_result = build_l2_to_l1_continuation_entries(
        &[d1_root.clone(), d1_child.clone()],
        l2_id,
        &[0xc0],
        false,
    );

    // depth-2 scenario (root + child + grandchild).
    let d2_root = make_l2_to_l1_detected(dest_root, vec![0x11], src_root, l2_id, None, 0);
    let d2_child = make_l2_to_l1_detected(dest_child, vec![0x22], src_child, l2_id, Some(0), 1);
    let d2_grand = make_l2_to_l1_detected(dest_grand, vec![0x33], src_grand, l2_id, Some(1), 2);

    let depth2_result = build_l2_to_l1_continuation_entries(
        &[d2_root, d2_child, d2_grand.clone()],
        l2_id,
        &[0xc0],
        false,
    );

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
        source_rollup: RollupId::MAINNET,
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
/// This is the standard 2-call L2→L1 multi-call continuation pattern:
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
    let l2_id = RollupId::new(U256::from(1));
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
    let result = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0], false);

    // ── L2 entries: exactly 3 ──
    assert_eq!(
        result.l2_entries.len(),
        3,
        "depth-1 must produce exactly 3 L2 entries"
    );

    let l1_result_void = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: RollupId::MAINNET,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
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
        source_rollup: RollupId::MAINNET,
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
        RollupId::MAINNET,
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
        RollupId::MAINNET,
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
        source_rollup: RollupId::MAINNET,
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
        source_rollup: RollupId::MAINNET,
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
    let l2_id = RollupId::new(U256::from(1));
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
        in_reverted_frame: false,
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
    let l2_id = RollupId::new(U256::from(1));
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
                rollup_id: RollupId::MAINNET,
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
            in_reverted_frame: false,
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
                source_rollup: RollupId::MAINNET,
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
            in_reverted_frame: false,
        },
    ];

    let cont = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0], false);

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
        source_rollup: RollupId::MAINNET,
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
    let l2_id = RollupId::new(U256::from(1));
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
                rollup_id: RollupId::MAINNET,
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
            in_reverted_frame: false,
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
                source_rollup: RollupId::MAINNET,
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
            in_reverted_frame: false,
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
                source_rollup: RollupId::MAINNET,
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
            in_reverted_frame: false,
        },
    ];

    let cont = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0], false);

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
        source_rollup: RollupId::MAINNET,
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
    let l2_id = RollupId::new(U256::from(1));
    let parent = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let child = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let _builder = address!("dddddddddddddddddddddddddddddddddddddddd");

    let delivery_data = vec![0xDE, 0xAD, 0xBE, 0xEF]; // non-void delivery return

    let detected = vec![
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: RollupId::MAINNET,
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
            in_reverted_frame: false,
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
                source_rollup: RollupId::MAINNET,
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
            in_reverted_frame: false,
        },
    ];

    let cont = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0], false);

    // L1 entries should include the delivery RESULT with non-void data
    let void_l1 = result_void(RollupId::MAINNET);
    let void_l1_hash = compute_action_hash(&void_l1);

    // The delivery RESULT entry (Entry 0b) should NOT use void hash
    // because delivery_return_data is non-empty.
    // rollupId: _processCallAtScope uses action.rollupId (child's target).
    // For L1→L2 return calls (child targets L2), rollupId = our_rollup_id (L2).
    let delivery_result_entry = cont.l1_entries.iter().find(|e| {
        e.action_hash != void_l1_hash
            && e.next_action.action_type == CrossChainActionType::Result
            && e.next_action.rollup_id == RollupId::new(alloy_primitives::U256::from(1u64))
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
            && e.next_action.rollup_id == RollupId::new(alloy_primitives::U256::from(1u64))
    });
    assert!(
        root_delivery_entry.is_some(),
        "L1 delivery RESULT nextAction must be terminal void with rollupId=L2 per §C.6"
    );
}

/// Test that void children still produce result_void hash (no regression).
#[test]
fn test_void_children_still_use_result_void() {
    let l2_id = RollupId::new(U256::from(1));
    let parent = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let child = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let _builder = address!("dddddddddddddddddddddddddddddddddddddddd");

    let detected = vec![
        DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: RollupId::MAINNET,
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
            in_reverted_frame: false,
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
                source_rollup: RollupId::MAINNET,
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
            in_reverted_frame: false,
        },
    ];

    let cont = build_l2_to_l1_continuation_entries(&detected, l2_id, &[0xc0], false);

    // All RESULT entries should use result_void hashes
    let void_l2_hash = compute_action_hash(&result_void(l2_id));
    let void_l1_hash = compute_action_hash(&result_void(RollupId::MAINNET));

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

// ──────────────────────────────────────────────
//  Step 0.4 (refactor) — reorder_for_swap_and_pop invariants
//
//  Targets table_builder::reorder_for_swap_and_pop (table_builder.rs:124),
//  which is private and only reachable from this sibling test file.
//
//  Properties verified:
//    1. Multiset preservation (no entry lost or duplicated).
//    2. No-op when every action_hash group has size ≤ 2 (matches the
//       function's docstring claim).
//    3. After reorder, every same-hash group is contiguous in the output.
//    4. The first entry of each multi-hash group (by order of first
//       appearance in the input) is preserved at the start of its
//       contiguous block — this is what makes Solidity's swap-and-pop
//       FIFO consumption correct (the spec docstring's "Proof (N=3)").
//    5. End-to-end: simulating Solidity's swap-and-pop on the reordered
//       array produces FIFO consumption order, for groups of any size.
// ──────────────────────────────────────────────

/// Test helper: build a CrossChainExecutionEntry whose `action_hash` is
/// `B256::with_last_byte(hash_byte)` and whose `next_action.value` is
/// `seq` so distinct entries within the same hash group are
/// distinguishable for ordering assertions.
fn mk_reorder_entry(hash_byte: u8, seq: u64) -> CrossChainExecutionEntry {
    CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: B256::with_last_byte(hash_byte),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: RollupId::new(U256::from(1)),
            destination: Address::ZERO,
            value: U256::from(seq),
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: vec![],
        },
    }
}

#[test]
fn test_reorder_for_swap_and_pop_empty() {
    let mut entries: Vec<CrossChainExecutionEntry> = vec![];
    reorder_for_swap_and_pop(&mut entries);
    assert!(entries.is_empty());
}

#[test]
fn test_reorder_for_swap_and_pop_all_singletons_is_noop() {
    let mut entries = vec![
        mk_reorder_entry(1, 100),
        mk_reorder_entry(2, 200),
        mk_reorder_entry(3, 300),
    ];
    let original = entries.clone();
    reorder_for_swap_and_pop(&mut entries);
    assert_eq!(entries, original, "no group ≥ 3 means the function is a no-op");
}

#[test]
fn test_reorder_for_swap_and_pop_all_pairs_is_noop() {
    // Two pairs: hash 1 (twice) and hash 2 (twice). No group ≥ 3 → no-op.
    let mut entries = vec![
        mk_reorder_entry(1, 10),
        mk_reorder_entry(2, 20),
        mk_reorder_entry(1, 11),
        mk_reorder_entry(2, 21),
    ];
    let original = entries.clone();
    reorder_for_swap_and_pop(&mut entries);
    assert_eq!(
        entries, original,
        "pair-only groups must not trigger any reorder"
    );
}

#[test]
fn test_reorder_for_swap_and_pop_n3_group_exact_layout() {
    // Single group of 3 entries: input [E0, E1, E2] must become [E0, E2, E1]
    // per the function's docstring "Proof (N=3)".
    let mut entries = vec![
        mk_reorder_entry(1, 0),
        mk_reorder_entry(1, 1),
        mk_reorder_entry(1, 2),
    ];
    reorder_for_swap_and_pop(&mut entries);
    assert_eq!(entries[0].next_action.value, U256::from(0));
    assert_eq!(entries[1].next_action.value, U256::from(2));
    assert_eq!(entries[2].next_action.value, U256::from(1));
}

#[test]
fn test_reorder_for_swap_and_pop_n4_group_exact_layout() {
    // [E0, E1, E2, E3] must become [E0, E3, E2, E1]
    // (E0 stays first, [E1..] reversed = [E3, E2, E1]).
    let mut entries = vec![
        mk_reorder_entry(1, 0),
        mk_reorder_entry(1, 1),
        mk_reorder_entry(1, 2),
        mk_reorder_entry(1, 3),
    ];
    reorder_for_swap_and_pop(&mut entries);
    assert_eq!(
        entries
            .iter()
            .map(|e| e.next_action.value.to::<u64>())
            .collect::<Vec<_>>(),
        vec![0u64, 3, 2, 1]
    );
}

#[test]
fn test_reorder_for_swap_and_pop_groups_first_then_singletons() {
    // Mixed input: a 3-group + a singleton. After reorder the multi-group
    // must come first (contiguous), then the singleton.
    let mut entries = vec![
        mk_reorder_entry(2, 99), // singleton
        mk_reorder_entry(1, 0),
        mk_reorder_entry(1, 1),
        mk_reorder_entry(1, 2),
    ];
    reorder_for_swap_and_pop(&mut entries);
    // Multi-group [hash=1] sits first.
    assert_eq!(entries[0].action_hash, B256::with_last_byte(1));
    assert_eq!(entries[1].action_hash, B256::with_last_byte(1));
    assert_eq!(entries[2].action_hash, B256::with_last_byte(1));
    // Singleton last.
    assert_eq!(entries[3].action_hash, B256::with_last_byte(2));
    assert_eq!(entries[3].next_action.value, U256::from(99));
}

#[test]
fn test_reorder_for_swap_and_pop_solidity_swap_and_pop_yields_fifo_n3() {
    // The whole point of this function: after reordering, simulating
    // Solidity's _consumeExecution (forward scan + swap-and-pop) on the
    // reordered array must consume entries in input order (FIFO).
    let original = vec![
        mk_reorder_entry(1, 0), // E0
        mk_reorder_entry(1, 1), // E1
        mk_reorder_entry(1, 2), // E2
    ];
    let mut storage = original.clone();
    reorder_for_swap_and_pop(&mut storage);

    // Simulate consumption: pop "the first entry matching action_hash A"
    // 3 times in a row.
    let target_hash = B256::with_last_byte(1);
    let mut consumed_order = Vec::new();
    while let Some(idx) = storage.iter().position(|e| e.action_hash == target_hash) {
        consumed_order.push(storage[idx].next_action.value.to::<u64>());
        // swap-and-pop
        let last = storage.len() - 1;
        storage.swap(idx, last);
        storage.pop();
    }

    assert_eq!(
        consumed_order,
        vec![0u64, 1, 2],
        "Solidity swap-and-pop on reordered array must yield FIFO"
    );
}

#[test]
fn test_reorder_for_swap_and_pop_solidity_swap_and_pop_yields_fifo_n5() {
    let original: Vec<CrossChainExecutionEntry> =
        (0..5u64).map(|i| mk_reorder_entry(7, i)).collect();
    let mut storage = original.clone();
    reorder_for_swap_and_pop(&mut storage);

    let target_hash = B256::with_last_byte(7);
    let mut consumed_order = Vec::new();
    while let Some(idx) = storage.iter().position(|e| e.action_hash == target_hash) {
        consumed_order.push(storage[idx].next_action.value.to::<u64>());
        let last = storage.len() - 1;
        storage.swap(idx, last);
        storage.pop();
    }

    assert_eq!(
        consumed_order,
        vec![0u64, 1, 2, 3, 4],
        "Solidity swap-and-pop on N=5 reordered must yield FIFO"
    );
}

#[test]
fn test_reorder_for_swap_and_pop_with_interleaved_other_groups() {
    // The function moves multi-groups to the front so that consuming a
    // singleton from the back (swap-and-pop) does NOT disrupt the multi-
    // group's ordering. Verify by simulating mixed consumption.
    //
    // Input: [E0(hash=1), S0(hash=2), E1(hash=1), S1(hash=3), E2(hash=1)]
    // After reorder, hash=1 group is at the front; consumption of S0/S1
    // never touches it.
    let mut storage = vec![
        mk_reorder_entry(1, 0),
        mk_reorder_entry(2, 50),
        mk_reorder_entry(1, 1),
        mk_reorder_entry(3, 60),
        mk_reorder_entry(1, 2),
    ];
    reorder_for_swap_and_pop(&mut storage);

    // First: consume singletons hash=2 and hash=3 (in any order). They
    // should NOT disturb the hash=1 ordering.
    let s2_idx = storage
        .iter()
        .position(|e| e.action_hash == B256::with_last_byte(2))
        .unwrap();
    let last = storage.len() - 1;
    storage.swap(s2_idx, last);
    storage.pop();

    let s3_idx = storage
        .iter()
        .position(|e| e.action_hash == B256::with_last_byte(3))
        .unwrap();
    let last = storage.len() - 1;
    storage.swap(s3_idx, last);
    storage.pop();

    // Now consume the hash=1 group; expected FIFO order.
    let mut consumed = Vec::new();
    while let Some(idx) = storage
        .iter()
        .position(|e| e.action_hash == B256::with_last_byte(1))
    {
        consumed.push(storage[idx].next_action.value.to::<u64>());
        let last = storage.len() - 1;
        storage.swap(idx, last);
        storage.pop();
    }
    assert_eq!(consumed, vec![0u64, 1, 2]);
}

mod proptests_reorder {
    use super::*;
    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;
    use std::collections::HashMap;

    /// Strategy: a single entry whose action_hash is drawn from a small
    /// palette so that group sizes >1 are likely. `seq` is unique within
    /// the test for verifiable ordering.
    fn arb_entry(hash_palette: u8, seq: u64) -> CrossChainExecutionEntry {
        mk_reorder_entry(hash_palette, seq)
    }

    fn arb_entry_list() -> impl Strategy<Value = Vec<CrossChainExecutionEntry>> {
        prop_vec(0u8..4u8, 0..16usize).prop_map(|tags| {
            tags.into_iter()
                .enumerate()
                .map(|(i, tag)| arb_entry(tag, i as u64))
                .collect()
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        /// `reorder_for_swap_and_pop` preserves the multiset of entries.
        #[test]
        fn reorder_preserves_multiset(input in arb_entry_list()) {
            let mut copy = input.clone();
            reorder_for_swap_and_pop(&mut copy);

            prop_assert_eq!(copy.len(), input.len());

            let mut a = input.clone();
            let mut b = copy.clone();
            // Sort by (hash, value) so duplicates are ordered identically.
            a.sort_by(|x, y| {
                x.action_hash
                    .cmp(&y.action_hash)
                    .then(x.next_action.value.cmp(&y.next_action.value))
            });
            b.sort_by(|x, y| {
                x.action_hash
                    .cmp(&y.action_hash)
                    .then(x.next_action.value.cmp(&y.next_action.value))
            });
            prop_assert_eq!(a, b);
        }

        /// `reorder_for_swap_and_pop` is a no-op when no group has 3+ entries.
        #[test]
        fn reorder_noop_when_all_groups_smaller_than_three(input in arb_entry_list()) {
            let mut counts: HashMap<B256, usize> = HashMap::new();
            for e in &input {
                *counts.entry(e.action_hash).or_insert(0) += 1;
            }
            let any_large = counts.values().any(|&n| n >= 3);

            let mut copy = input.clone();
            reorder_for_swap_and_pop(&mut copy);

            if !any_large {
                prop_assert_eq!(
                    copy, input,
                    "no group ≥ 3 must produce a byte-identical no-op"
                );
            }
        }

        /// When the function actually reorders (input has at least one
        /// group of size ≥ 3), every same-hash group is contiguous in the
        /// output: each `action_hash` appears in exactly one contiguous
        /// run. When the function is a no-op (all groups ≤ 2),
        /// contiguity is NOT a property of the function — the input may
        /// have interleaved 2-groups and that is intentionally preserved.
        #[test]
        fn reorder_groups_are_contiguous_when_reordered(input in arb_entry_list()) {
            let mut counts: HashMap<B256, usize> = HashMap::new();
            for e in &input {
                *counts.entry(e.action_hash).or_insert(0) += 1;
            }
            let any_large = counts.values().any(|&n| n >= 3);
            if !any_large {
                // No precondition met → no contiguity guarantee.
                return Ok(());
            }

            let mut copy = input;
            reorder_for_swap_and_pop(&mut copy);

            let mut closed: std::collections::BTreeSet<B256> = std::collections::BTreeSet::new();
            let mut last_hash: Option<B256> = None;
            for e in &copy {
                if Some(e.action_hash) != last_hash {
                    if let Some(prev) = last_hash {
                        closed.insert(prev);
                    }
                    prop_assert!(
                        !closed.contains(&e.action_hash),
                        "after reorder, hash reappears after being interrupted"
                    );
                    last_hash = Some(e.action_hash);
                }
            }
        }

        /// When the function reorders, the first entry of each group
        /// (by input order) is preserved at the start of its contiguous
        /// block in the output. This is the FIFO-correctness property
        /// of the docstring's "Proof (N=3)".
        #[test]
        fn reorder_preserves_first_entry_when_reordered(input in arb_entry_list()) {
            let mut counts: HashMap<B256, usize> = HashMap::new();
            for e in &input {
                *counts.entry(e.action_hash).or_insert(0) += 1;
            }
            let any_large = counts.values().any(|&n| n >= 3);
            if !any_large {
                return Ok(());
            }

            let mut first_input_per_hash: HashMap<B256, &CrossChainExecutionEntry> = HashMap::new();
            for e in &input {
                first_input_per_hash.entry(e.action_hash).or_insert(e);
            }

            let mut copy = input.clone();
            reorder_for_swap_and_pop(&mut copy);

            let mut last_hash: Option<B256> = None;
            for e in &copy {
                if Some(e.action_hash) != last_hash {
                    let expected = first_input_per_hash[&e.action_hash];
                    prop_assert_eq!(e, expected);
                    last_hash = Some(e.action_hash);
                }
            }
        }

        /// When the function reorders, the multi-group contiguous block
        /// (entries from groups of size ≥ 2) precedes the singleton
        /// block (entries from groups of size 1). This is the layout
        /// invariant that lets singletons be consumed without disrupting
        /// the multi-group's swap-and-pop order.
        #[test]
        fn reorder_multigroups_precede_singletons_when_reordered(input in arb_entry_list()) {
            let mut counts: HashMap<B256, usize> = HashMap::new();
            for e in &input {
                *counts.entry(e.action_hash).or_insert(0) += 1;
            }
            let any_large = counts.values().any(|&n| n >= 3);
            if !any_large {
                return Ok(());
            }

            let multi_hashes: std::collections::BTreeSet<B256> = counts
                .iter()
                .filter(|&(_, n)| *n >= 2)
                .map(|(&h, _)| h)
                .collect();

            let mut copy = input;
            reorder_for_swap_and_pop(&mut copy);

            // Walk forward; once we see the first singleton entry, no
            // further multi-group entry may appear.
            let mut singleton_block_started = false;
            for e in &copy {
                let is_multi = multi_hashes.contains(&e.action_hash);
                if singleton_block_started {
                    prop_assert!(
                        !is_multi,
                        "multi-group entry appears after singleton block started"
                    );
                } else if !is_multi {
                    singleton_block_started = true;
                }
            }
        }

        // NOTE: end-to-end FIFO under arbitrary consumption interleavings
        // is NOT a property of `reorder_for_swap_and_pop`. The function
        // only guarantees FIFO when:
        //   (a) the array contains a single multi-group + singletons, AND
        //   (b) singletons are consumed BEFORE the multi-group (so the
        //       multi-group sits alone at the front when its turn comes).
        //
        // This narrow consumption pattern is what real callers in
        // table_builder.rs use (multi-call continuation chains are
        // consumed in one user-tx burst, with no other consumptions
        // interleaved). The unit tests
        // `test_reorder_for_swap_and_pop_solidity_swap_and_pop_yields_fifo_*`
        // exercise this pattern explicitly. We deliberately do NOT
        // generate a proptest that drains arbitrary consumption orders,
        // because such orders fall outside the function's contract.
    }
}

// ──────────────────────────────────────────────
//  Step 0.5 (refactor) — MirrorCase loop tests
//
//  Closes invariant #18 (L1 and L2 entry structures must MIRROR each
//  other) at the test/gate level for the 5 canonical cases.
//
//  These are intentionally LOW-LEVEL guard-rails: they assert that the
//  DSL is wired correctly and that every canonical case produces
//  internally consistent entries. Deeper mirror property tests
//  (action-hash equivalence, scope navigation symmetry, etc.) land
//  in Phase 3 of the refactor when the `Direction` trait introduces
//  symmetry by construction.
// ──────────────────────────────────────────────

mod mirror_loop_tests {
    use super::*;
    use crate::test_support::mirror_case::{
        canonical_cases, MirrorCase, MirrorPattern,
    };

    /// Each canonical case constructs successfully and produces at least
    /// one entry across L1 + L2.
    #[test]
    fn mirror_each_case_has_entries() {
        let cases = canonical_cases();
        assert_eq!(cases.len(), 5, "expected 5 canonical cases");
        for case in &cases {
            assert!(
                case.total_entries() > 0,
                "case {} produced 0 total entries",
                case.name
            );
        }
    }

    /// Every action_hash in every canonical case is non-zero. A zero
    /// hash would indicate a builder produced an entry with an
    /// uninitialized action.
    #[test]
    fn mirror_action_hashes_are_non_zero() {
        for case in &canonical_cases() {
            for h in &case.all_action_hashes {
                assert_ne!(
                    *h,
                    B256::ZERO,
                    "case {} contains a zero action_hash",
                    case.name
                );
            }
        }
    }

    /// `MirrorCase::all_action_hashes` is the concatenation of L1
    /// hashes followed by L2 hashes. Verify length consistency.
    #[test]
    fn mirror_collected_hashes_match_entry_counts() {
        for case in &canonical_cases() {
            let expected = case.l1_entries.len() + case.l2_entries.len();
            assert_eq!(
                case.all_action_hashes.len(),
                expected,
                "case {} hash count mismatch",
                case.name
            );
        }
    }

    /// Continuation cases must produce ≥ 2 L2 entries (a 1-entry L2
    /// table is by definition non-continuation). Simple cases must
    /// produce exactly 2 L2 entries (the CALL+RESULT pair).
    #[test]
    fn mirror_pattern_kinds_have_expected_minimum_shapes() {
        for case in &canonical_cases() {
            match case.pattern {
                MirrorPattern::Simple => {
                    assert_eq!(
                        case.l2_entries.len(),
                        2,
                        "Simple case {} should have exactly 2 L2 entries (CALL+RESULT)",
                        case.name
                    );
                }
                MirrorPattern::Continuation => {
                    assert!(
                        case.l2_entries.len() >= 2,
                        "Continuation case {} should have ≥ 2 L2 entries (got {})",
                        case.name,
                        case.l2_entries.len()
                    );
                }
            }
        }
    }

    /// Every L2 entry's `next_action.rollup_id` must be one of the two
    /// rollup ids participating in the test scenario (MAINNET = 0 or
    /// our test rollup = 1). A foreign rollup id would mean the
    /// builder leaked some unrelated chain into the entries.
    ///
    /// Scope navigation `callReturn` entries (used in L2→L1
    /// continuation patterns) target our rollup directly because
    /// they re-enter the same L2 to drive the next call in the
    /// scope chain — that is why we accept rollup_id ∈ {0, 1}
    /// uniformly across both directions.
    #[test]
    fn mirror_l2_entries_carry_known_rollup_ids() {
        for case in &canonical_cases() {
            for entry in &case.l2_entries {
                let target = entry.next_action.rollup_id;
                let source = entry.next_action.source_rollup;
                assert!(
                    target == RollupId::MAINNET || target == RollupId::new(U256::from(1)),
                    "case {}: L2-entry next_action.rollup_id={} not in {{0, 1}}",
                    case.name,
                    target
                );
                assert!(
                    source == RollupId::MAINNET || source == RollupId::new(U256::from(1)),
                    "case {}: L2-entry next_action.source_rollup={} not in {{0, 1}}",
                    case.name,
                    source
                );
            }
        }
    }

    /// Smoke check the DSL is the only place where canonical cases live:
    /// every case has the expected name in the documented order. If a
    /// case is renamed or reordered, this test catches it before
    /// downstream mirror tests start observing different fixtures.
    #[test]
    fn mirror_canonical_cases_have_documented_names_in_order() {
        let cases = canonical_cases();
        let names: Vec<&'static str> = cases.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "deposit_simple",
                "withdrawal_simple",
                "flash_loan_3_call",
                "ping_pong_depth_2",
                "ping_pong_depth_3",
            ]
        );
    }

    // Trivial use of MirrorCase to keep the import alive in case all
    // assertions above are removed during future refactoring. The
    // compiler will warn otherwise.
    #[allow(dead_code)]
    fn _force_mirror_case_in_scope() -> &'static str {
        let _: Option<MirrorCase> = None;
        "in scope"
    }
}
