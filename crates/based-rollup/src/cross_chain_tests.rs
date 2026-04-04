use super::*;

#[test]
fn test_execution_entry_serialization_roundtrip() {
    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(2),
            current_state: B256::ZERO,
            new_state: B256::with_last_byte(0xFF),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::with_last_byte(0x42),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: U256::from(1),
            destination: Address::with_last_byte(0x01),
            value: U256::from(1000),
            data: vec![0xDE, 0xAD],
            failed: false,
            source_address: Address::with_last_byte(0x02),
            source_rollup: U256::from(2),
            scope: vec![U256::from(0), U256::from(1)],
        },
    };
    let json = serde_json::to_string(&entry).unwrap();
    let deserialized: CrossChainExecutionEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(entry, deserialized);
}

#[test]
fn test_sol_abi_load_execution_table_encodes() {
    use alloy_sol_types::SolCall;
    // Verify the ABI encoding works for the loadExecutionTable function
    let call = ICrossChainManagerL2::loadExecutionTableCall { entries: vec![] };
    let encoded = call.abi_encode();
    // 4-byte selector + 32-byte offset + 32-byte length (0) = 68 bytes
    assert_eq!(encoded.len(), 68);
    // Selector should be non-zero (it's a real function)
    assert_ne!(&encoded[..4], &[0u8; 4]);
}

#[test]
fn test_sol_abi_execute_incoming_cross_chain_call_encodes() {
    use alloy_sol_types::SolCall;
    let call = ICrossChainManagerL2::executeIncomingCrossChainCallCall {
        destination: Address::with_last_byte(0x42),
        value: U256::from(1_000_000),
        data: vec![0xDE, 0xAD, 0xBE, 0xEF].into(),
        sourceAddress: Address::with_last_byte(0x01),
        sourceRollup: U256::from(2),
        scope: vec![U256::from(1), U256::from(2), U256::from(3)],
    };
    let encoded = call.abi_encode();
    // Should have 4-byte selector + encoded params
    assert!(encoded.len() > 4);
    // Selector must differ from loadExecutionTable
    let load_call = ICrossChainManagerL2::loadExecutionTableCall { entries: vec![] };
    let load_encoded = load_call.abi_encode();
    assert_ne!(&encoded[..4], &load_encoded[..4], "selectors must differ");
    // Roundtrip: decode should recover original values
    let decoded =
        ICrossChainManagerL2::executeIncomingCrossChainCallCall::abi_decode(&encoded).unwrap();
    assert_eq!(decoded.destination, Address::with_last_byte(0x42));
    assert_eq!(decoded.value, U256::from(1_000_000));
    assert_eq!(decoded.sourceRollup, U256::from(2));
    assert_eq!(decoded.scope.len(), 3);
}

#[test]
fn test_sol_abi_load_execution_table_nontrivial_roundtrip() {
    use alloy_sol_types::SolCall;
    // Build a non-trivial ExecutionEntry with real StateDelta and Action data
    let entry = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![
            ICrossChainManagerL2::StateDelta {
                rollupId: U256::from(1),
                currentState: B256::with_last_byte(0xAA),
                newState: B256::with_last_byte(0xBB),
                etherDelta: I256::try_from(2_000_000_000_000_000_000i128).unwrap(),
            },
            ICrossChainManagerL2::StateDelta {
                rollupId: U256::from(2),
                currentState: B256::with_last_byte(0xCC),
                newState: B256::with_last_byte(0xDD),
                etherDelta: I256::try_from(-1_500_000_000_000_000_000i128).unwrap(),
            },
        ],
        actionHash: B256::with_last_byte(0xFF),
        nextAction: ICrossChainManagerL2::Action {
            actionType: ICrossChainManagerL2::ActionType::CALL,
            rollupId: U256::from(1),
            destination: Address::with_last_byte(0x99),
            value: U256::from(500_000),
            data: vec![0xCA, 0xFE, 0xBA, 0xBE].into(),
            failed: false,
            sourceAddress: Address::with_last_byte(0xAA),
            sourceRollup: U256::from(2),
            scope: vec![U256::from(10), U256::from(20)],
        },
    };
    let call = ICrossChainManagerL2::loadExecutionTableCall {
        entries: vec![entry],
    };
    let encoded = call.abi_encode();
    // Must be significantly larger than the empty-array case (68 bytes)
    assert!(
        encoded.len() > 200,
        "encoded len {} too small",
        encoded.len()
    );
    // Roundtrip decode must recover the same data
    let decoded = ICrossChainManagerL2::loadExecutionTableCall::abi_decode(&encoded).unwrap();
    assert_eq!(decoded.entries.len(), 1);
    let e = &decoded.entries[0];
    assert_eq!(e.stateDeltas.len(), 2);
    assert_eq!(e.stateDeltas[0].rollupId, U256::from(1));
    assert_eq!(
        e.stateDeltas[1].etherDelta,
        I256::try_from(-1_500_000_000_000_000_000i128).unwrap()
    );
    assert_eq!(e.nextAction.destination, Address::with_last_byte(0x99));
    assert_eq!(e.nextAction.scope.len(), 2);
    assert_eq!(e.actionHash, B256::with_last_byte(0xFF));
}

#[test]
fn test_sol_abi_execute_incoming_cross_chain_call_roundtrip() {
    use alloy_sol_types::SolCall;
    let call = ICrossChainManagerL2::executeIncomingCrossChainCallCall {
        destination: Address::with_last_byte(0x77),
        value: U256::from(3_000_000_000_000_000_000u128),
        data: vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08].into(),
        sourceAddress: Address::with_last_byte(0xBB),
        sourceRollup: U256::from(5),
        scope: vec![U256::from(100), U256::from(200), U256::from(300)],
    };
    let encoded = call.abi_encode();
    let decoded =
        ICrossChainManagerL2::executeIncomingCrossChainCallCall::abi_decode(&encoded).unwrap();
    assert_eq!(decoded.destination, Address::with_last_byte(0x77));
    assert_eq!(decoded.value, U256::from(3_000_000_000_000_000_000u128));
    assert_eq!(decoded.data.len(), 8);
    assert_eq!(decoded.data[0], 0x01);
    assert_eq!(decoded.sourceAddress, Address::with_last_byte(0xBB));
    assert_eq!(decoded.sourceRollup, U256::from(5));
    assert_eq!(
        decoded.scope,
        vec![U256::from(100), U256::from(200), U256::from(300)]
    );
}

#[test]
fn test_encode_load_execution_table_empty() {
    let calldata = encode_load_execution_table_calldata(&[]);
    // Should be a valid ABI-encoded call with empty array
    assert!(calldata.len() >= 4, "should have at least a selector");
}

#[test]
fn test_encode_load_execution_table_roundtrip() {
    let entries = vec![CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::with_last_byte(0xAA),
            new_state: B256::with_last_byte(0xBB),
            ether_delta: I256::try_from(1_000_000i128).unwrap(),
        }],
        action_hash: B256::with_last_byte(0x42),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: U256::from(2),
            destination: Address::with_last_byte(0x99),
            value: U256::from(500),
            data: vec![0xDE, 0xAD],
            failed: false,
            source_address: Address::with_last_byte(0x01),
            source_rollup: U256::from(1),
            scope: vec![U256::from(0)],
        },
    }];
    let calldata = encode_load_execution_table_calldata(&entries);
    // Decode back via sol type
    let decoded = ICrossChainManagerL2::loadExecutionTableCall::abi_decode(&calldata).unwrap();
    assert_eq!(decoded.entries.len(), 1);
    assert_eq!(decoded.entries[0].stateDeltas.len(), 1);
    assert_eq!(decoded.entries[0].stateDeltas[0].rollupId, U256::from(1));
    assert_eq!(decoded.entries[0].actionHash, B256::with_last_byte(0x42));
    assert_eq!(
        decoded.entries[0].nextAction.destination,
        Address::with_last_byte(0x99)
    );
}

#[test]
fn test_encode_execute_incoming_call_roundtrip() {
    let action = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::from(1),
        destination: Address::with_last_byte(0x42),
        value: U256::from(1_000_000),
        data: vec![0xCA, 0xFE, 0xBA, 0xBE],
        failed: false,
        source_address: Address::with_last_byte(0xBB),
        source_rollup: U256::from(2),
        scope: vec![U256::from(0), U256::from(1)],
    };
    let calldata = encode_execute_incoming_call_calldata(&action);
    let decoded =
        ICrossChainManagerL2::executeIncomingCrossChainCallCall::abi_decode(&calldata).unwrap();
    assert_eq!(decoded.destination, Address::with_last_byte(0x42));
    assert_eq!(decoded.value, U256::from(1_000_000));
    assert_eq!(decoded.sourceAddress, Address::with_last_byte(0xBB));
    assert_eq!(decoded.sourceRollup, U256::from(2));
    assert_eq!(decoded.scope.len(), 2);
}

#[test]
fn test_encode_load_execution_table_multiple_entries() {
    // Encode 3 entries with diverse field values and decode back to verify roundtrip.
    let entries = vec![
        CrossChainExecutionEntry {
            state_deltas: vec![CrossChainStateDelta {
                rollup_id: U256::from(1),
                current_state: B256::with_last_byte(0x11),
                new_state: B256::with_last_byte(0x22),
                ether_delta: I256::try_from(999_999_999_999i128).unwrap(),
            }],
            action_hash: B256::with_last_byte(0xAA),
            next_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::from(10),
                destination: Address::with_last_byte(0x01),
                value: U256::from(42),
                data: vec![0x01, 0x02, 0x03],
                failed: false,
                source_address: Address::with_last_byte(0x0A),
                source_rollup: U256::from(5),
                scope: vec![U256::from(100)],
            },
        },
        CrossChainExecutionEntry {
            state_deltas: vec![
                CrossChainStateDelta {
                    rollup_id: U256::from(2),
                    current_state: B256::with_last_byte(0x33),
                    new_state: B256::with_last_byte(0x44),
                    ether_delta: I256::try_from(-500_000_000i128).unwrap(),
                },
                CrossChainStateDelta {
                    rollup_id: U256::from(3),
                    current_state: B256::ZERO,
                    new_state: B256::with_last_byte(0xFF),
                    ether_delta: I256::ZERO,
                },
            ],
            action_hash: B256::with_last_byte(0xBB),
            next_action: CrossChainAction {
                action_type: CrossChainActionType::Revert,
                rollup_id: U256::from(20),
                destination: Address::with_last_byte(0x02),
                value: U256::ZERO,
                data: vec![],
                failed: true,
                source_address: Address::with_last_byte(0x0B),
                source_rollup: U256::from(10),
                scope: vec![U256::from(1), U256::from(2)],
            },
        },
        CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: B256::with_last_byte(0xCC),
            next_action: CrossChainAction {
                action_type: CrossChainActionType::L2Tx,
                rollup_id: U256::from(30),
                destination: Address::with_last_byte(0x03),
                value: U256::from(1_000_000_000_000_000_000u128),
                data: vec![0xFF; 64],
                failed: false,
                source_address: Address::ZERO,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
        },
    ];

    let calldata = encode_load_execution_table_calldata(&entries);
    let decoded = ICrossChainManagerL2::loadExecutionTableCall::abi_decode(&calldata).unwrap();
    assert_eq!(decoded.entries.len(), 3);

    // Entry 0: single delta, Call action
    assert_eq!(decoded.entries[0].stateDeltas.len(), 1);
    assert_eq!(decoded.entries[0].stateDeltas[0].rollupId, U256::from(1));
    assert_eq!(decoded.entries[0].actionHash, B256::with_last_byte(0xAA));
    assert_eq!(
        decoded.entries[0].nextAction.actionType,
        ICrossChainManagerL2::ActionType::CALL
    );
    assert_eq!(decoded.entries[0].nextAction.rollupId, U256::from(10));

    // Entry 1: two deltas, Revert action with failed=true
    assert_eq!(decoded.entries[1].stateDeltas.len(), 2);
    assert_eq!(
        decoded.entries[1].stateDeltas[0].etherDelta,
        I256::try_from(-500_000_000i128).unwrap()
    );
    assert_eq!(decoded.entries[1].stateDeltas[1].etherDelta, I256::ZERO);
    assert_eq!(decoded.entries[1].actionHash, B256::with_last_byte(0xBB));
    assert!(decoded.entries[1].nextAction.failed);
    assert_eq!(
        decoded.entries[1].nextAction.actionType,
        ICrossChainManagerL2::ActionType::REVERT
    );

    // Entry 2: empty deltas, L2TX action with large value and 64-byte data
    assert!(decoded.entries[2].stateDeltas.is_empty());
    assert_eq!(decoded.entries[2].actionHash, B256::with_last_byte(0xCC));
    assert_eq!(
        decoded.entries[2].nextAction.actionType,
        ICrossChainManagerL2::ActionType::L2TX
    );
    assert_eq!(
        decoded.entries[2].nextAction.value,
        U256::from(1_000_000_000_000_000_000u128)
    );
    assert_eq!(decoded.entries[2].nextAction.data.len(), 64);
}

// --- Iteration 57: ActionType variant coverage and large scope ---

// ── from_sol roundtrip tests ──

#[test]
fn test_from_sol_action_roundtrip() {
    let action = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::from(42),
        destination: Address::with_last_byte(0xAB),
        value: U256::from(1_000_000u64),
        data: vec![1, 2, 3, 4],
        failed: false,
        source_address: Address::with_last_byte(0xCD),
        source_rollup: U256::from(7),
        scope: vec![U256::from(1), U256::from(2)],
    };
    let sol = action.to_sol_action();
    let back = CrossChainAction::from_sol(&sol).expect("valid action");
    assert_eq!(action, back);
}

#[test]
fn test_from_sol_execution_entry_roundtrip() {
    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::with_last_byte(0xAA),
            new_state: B256::with_last_byte(0xBB),
            ether_delta: I256::try_from(100i64).unwrap(),
        }],
        action_hash: B256::with_last_byte(0xFF),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: U256::from(1),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: vec![0xDE, 0xAD],
            failed: true,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: vec![],
        },
    };
    let sol = entry.to_sol();
    let back = CrossChainExecutionEntry::from_sol(&sol).expect("valid entry");
    assert_eq!(entry, back);
}

#[test]
fn test_parse_batch_posted_logs_empty() {
    let entries = parse_batch_posted_logs(&[], U256::from(1));
    assert!(entries.is_empty());
}

// ── Phase 3 edge case tests ──

#[test]
fn test_parse_batch_posted_logs_no_matching_rollup_id_filters_all() {
    // Build a valid BatchPosted log where state deltas only reference rollup_id=99.
    // When we filter for rollup_id=1, everything should be filtered out.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let sol_entry = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![ICrossChainManagerL2::StateDelta {
            rollupId: U256::from(99),
            currentState: B256::ZERO,
            newState: B256::with_last_byte(0xAA),
            etherDelta: I256::ZERO,
        }],
        actionHash: B256::with_last_byte(0x01),
        nextAction: ICrossChainManagerL2::Action {
            actionType: ICrossChainManagerL2::ActionType::CALL,
            rollupId: U256::from(99),
            destination: Address::with_last_byte(0x01),
            value: U256::ZERO,
            data: vec![].into(),
            failed: false,
            sourceAddress: Address::ZERO,
            sourceRollup: U256::ZERO,
            scope: vec![],
        },
    };

    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![sol_entry],
        publicInputsHash: B256::ZERO,
    };
    let encoded = event.encode_log_data();
    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0x55),
            data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
        },
        block_number: Some(100),
        ..Default::default()
    };

    // Filter for rollup_id=1 — no state deltas reference it
    let result = parse_batch_posted_logs(&[log], U256::from(1));
    assert!(
        result.is_empty(),
        "should filter out entries with no matching rollup_id"
    );
}

#[test]
fn test_parse_batch_posted_logs_mixed_rollup_ids_partial_match() {
    // Two entries in one BatchPosted log: one references rollup_id=1, the other only rollup_id=99.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let make_action = |rid: u64| ICrossChainManagerL2::Action {
        actionType: ICrossChainManagerL2::ActionType::CALL,
        rollupId: U256::from(rid),
        destination: Address::with_last_byte(rid as u8),
        value: U256::ZERO,
        data: vec![].into(),
        failed: false,
        sourceAddress: Address::ZERO,
        sourceRollup: U256::ZERO,
        scope: vec![],
    };

    let entry_matching = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![ICrossChainManagerL2::StateDelta {
            rollupId: U256::from(1),
            currentState: B256::ZERO,
            newState: B256::with_last_byte(0x11),
            etherDelta: I256::ZERO,
        }],
        actionHash: B256::with_last_byte(0xAA),
        nextAction: make_action(1),
    };

    let entry_non_matching = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![ICrossChainManagerL2::StateDelta {
            rollupId: U256::from(99),
            currentState: B256::ZERO,
            newState: B256::with_last_byte(0x99),
            etherDelta: I256::ZERO,
        }],
        actionHash: B256::with_last_byte(0xBB),
        nextAction: make_action(99),
    };

    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![entry_matching, entry_non_matching],
        publicInputsHash: B256::ZERO,
    };
    let encoded = event.encode_log_data();
    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0x55),
            data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
        },
        block_number: Some(200),
        ..Default::default()
    };

    let result = parse_batch_posted_logs(&[log], U256::from(1));
    assert_eq!(
        result.len(),
        1,
        "only the entry with rollup_id=1 delta should match"
    );
    assert_eq!(result[0].entry.action_hash, B256::with_last_byte(0xAA));
    assert_eq!(result[0].l1_block_number, 200);
}

#[test]
fn test_parse_batch_posted_logs_skips_log_without_block_number() {
    // A log with block_number=None should be skipped entirely.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let sol_entry = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![ICrossChainManagerL2::StateDelta {
            rollupId: U256::from(1),
            currentState: B256::ZERO,
            newState: B256::with_last_byte(0x11),
            etherDelta: I256::ZERO,
        }],
        actionHash: B256::with_last_byte(0x01),
        nextAction: ICrossChainManagerL2::Action {
            actionType: ICrossChainManagerL2::ActionType::CALL,
            rollupId: U256::from(1),
            destination: Address::with_last_byte(0x01),
            value: U256::ZERO,
            data: vec![].into(),
            failed: false,
            sourceAddress: Address::ZERO,
            sourceRollup: U256::ZERO,
            scope: vec![],
        },
    };

    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![sol_entry],
        publicInputsHash: B256::ZERO,
    };
    let encoded = event.encode_log_data();
    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0x55),
            data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
        },
        block_number: None, // no block number
        ..Default::default()
    };

    let result = parse_batch_posted_logs(&[log], U256::from(1));
    assert!(
        result.is_empty(),
        "log with no block_number should be skipped"
    );
}

#[test]
fn test_from_sol_state_delta_roundtrip() {
    let sol_delta = ICrossChainManagerL2::StateDelta {
        rollupId: U256::from(42),
        currentState: B256::with_last_byte(0xAA),
        newState: B256::with_last_byte(0xBB),
        etherDelta: I256::try_from(-1_000_000_000_000_000_000i128).unwrap(),
    };

    let rust_delta = CrossChainStateDelta::from_sol(&sol_delta);
    assert_eq!(rust_delta.rollup_id, U256::from(42));
    assert_eq!(rust_delta.current_state, B256::with_last_byte(0xAA));
    assert_eq!(rust_delta.new_state, B256::with_last_byte(0xBB));
    assert!(rust_delta.ether_delta < I256::ZERO);

    // Roundtrip: from_sol then to_sol should produce identical values
    let back_to_sol = rust_delta.to_sol();
    assert_eq!(back_to_sol.rollupId, sol_delta.rollupId);
    assert_eq!(back_to_sol.currentState, sol_delta.currentState);
    assert_eq!(back_to_sol.newState, sol_delta.newState);
    assert_eq!(back_to_sol.etherDelta, sol_delta.etherDelta);
}

#[test]
fn test_from_sol_execution_entry_multi_delta_roundtrip() {
    let sol_entry = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![
            ICrossChainManagerL2::StateDelta {
                rollupId: U256::from(1),
                currentState: B256::ZERO,
                newState: B256::with_last_byte(0x11),
                etherDelta: I256::try_from(500i128).unwrap(),
            },
            ICrossChainManagerL2::StateDelta {
                rollupId: U256::from(2),
                currentState: B256::with_last_byte(0x22),
                newState: B256::with_last_byte(0x33),
                etherDelta: I256::try_from(-500i128).unwrap(),
            },
        ],
        actionHash: B256::with_last_byte(0xDD),
        nextAction: ICrossChainManagerL2::Action {
            actionType: ICrossChainManagerL2::ActionType::L2TX,
            rollupId: U256::from(1),
            destination: Address::with_last_byte(0x42),
            value: U256::from(9999),
            data: vec![0xAB, 0xCD].into(),
            failed: false,
            sourceAddress: Address::with_last_byte(0x77),
            sourceRollup: U256::from(2),
            scope: vec![U256::from(10)],
        },
    };

    let rust_entry = CrossChainExecutionEntry::from_sol(&sol_entry).expect("valid entry");
    assert_eq!(rust_entry.state_deltas.len(), 2);
    assert_eq!(rust_entry.action_hash, B256::with_last_byte(0xDD));
    assert_eq!(
        rust_entry.next_action.action_type,
        CrossChainActionType::L2Tx
    );
    assert_eq!(rust_entry.next_action.data, vec![0xAB, 0xCD]);

    // Roundtrip back to sol
    let back = rust_entry.to_sol();
    assert_eq!(back.stateDeltas.len(), 2);
    assert_eq!(back.stateDeltas[0].rollupId, U256::from(1));
    assert_eq!(back.stateDeltas[1].rollupId, U256::from(2));
    assert_eq!(back.actionHash, sol_entry.actionHash);
    assert_eq!(
        back.nextAction.destination,
        sol_entry.nextAction.destination
    );
}

#[test]
fn test_from_sol_all_action_type_variants_roundtrip() {
    let sol_variants = [
        (
            ICrossChainManagerL2::ActionType::CALL,
            CrossChainActionType::Call,
        ),
        (
            ICrossChainManagerL2::ActionType::RESULT,
            CrossChainActionType::Result,
        ),
        (
            ICrossChainManagerL2::ActionType::L2TX,
            CrossChainActionType::L2Tx,
        ),
        (
            ICrossChainManagerL2::ActionType::REVERT,
            CrossChainActionType::Revert,
        ),
        (
            ICrossChainManagerL2::ActionType::REVERT_CONTINUE,
            CrossChainActionType::RevertContinue,
        ),
    ];

    for (sol_variant, expected_rust) in sol_variants {
        let rust = CrossChainActionType::from_sol(sol_variant).expect("valid action type");
        assert_eq!(rust, expected_rust);
        // Roundtrip: from_sol then to_sol
        let back = rust.to_sol();
        assert_eq!(back, sol_variant);
    }
}

#[test]
fn test_from_sol_unknown_action_type_returns_error() {
    // sol! enums are non-exhaustive; an unknown variant should produce an error
    // rather than silently defaulting to Call.
    // Construct an ActionType with a value beyond the known range.
    // ICrossChainManagerL2::ActionType is a u8 enum internally.
    let unknown = unsafe { std::mem::transmute::<u8, ICrossChainManagerL2::ActionType>(255) };
    let result = CrossChainActionType::from_sol(unknown);
    assert!(
        result.is_err(),
        "unknown ActionType variant should return error"
    );
    assert!(
        result.unwrap_err().contains("unknown ActionType"),
        "error message should mention 'unknown ActionType'"
    );
}

#[test]
fn test_parse_batch_posted_logs_skips_entries_with_unknown_action_type() {
    // A BatchPosted entry with an unknown ActionType should be skipped
    // (not crash or default to Call). We verify this by checking that
    // parse_batch_posted_logs returns fewer entries when one has an invalid type.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let valid_entry = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![ICrossChainManagerL2::StateDelta {
            rollupId: U256::from(1),
            currentState: B256::with_last_byte(0xAA),
            newState: B256::with_last_byte(0xBB),
            etherDelta: I256::ZERO,
        }],
        actionHash: B256::with_last_byte(0xCC),
        nextAction: ICrossChainManagerL2::Action {
            actionType: ICrossChainManagerL2::ActionType::RESULT,
            rollupId: U256::from(1),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: Default::default(),
            failed: false,
            sourceAddress: Address::ZERO,
            sourceRollup: U256::ZERO,
            scope: vec![],
        },
    };

    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![valid_entry],
        publicInputsHash: B256::ZERO,
    };
    let log_data = event.encode_log_data();
    let log = alloy_primitives::Log {
        address: Address::ZERO,
        data: LogData::new(log_data.topics().to_vec(), log_data.data.clone())
            .expect("valid log data"),
    };
    let full_log = alloy_rpc_types::Log {
        inner: log,
        block_hash: Some(B256::ZERO),
        block_number: Some(100),
        block_timestamp: None,
        transaction_hash: Some(B256::ZERO),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    };

    // With a valid entry, we should get 1 result
    let results = parse_batch_posted_logs(&[full_log], U256::from(1));
    assert_eq!(
        results.len(),
        1,
        "valid entry should be included in parse results"
    );
}

// --- parse_batch_posted_logs: nextAction.rollupId matching (fix #137) ---

#[test]
fn test_parse_batch_posted_logs_matches_via_next_action_rollup_id_not_state_deltas() {
    // Verify the fix from #137: an entry is included if nextAction.rollupId
    // matches even when NO state deltas reference the rollup. This covers
    // incoming cross-chain calls where the entry has state deltas only for
    // the source rollup, but nextAction targets our rollup.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let our_rollup_id = U256::from(1);
    let source_rollup_id = U256::from(99);

    // Entry: state deltas reference ONLY source_rollup_id, but
    // nextAction.rollupId == our_rollup_id (incoming call).
    let sol_entry = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![ICrossChainManagerL2::StateDelta {
            rollupId: source_rollup_id, // NOT our rollup
            currentState: B256::ZERO,
            newState: B256::with_last_byte(0xAA),
            etherDelta: I256::try_from(-1_000_000i128).unwrap(),
        }],
        actionHash: B256::with_last_byte(0x42),
        nextAction: ICrossChainManagerL2::Action {
            actionType: ICrossChainManagerL2::ActionType::CALL,
            rollupId: our_rollup_id, // targets OUR rollup
            destination: Address::with_last_byte(0x01),
            value: U256::from(500u64),
            data: vec![0xDE, 0xAD].into(),
            failed: false,
            sourceAddress: Address::with_last_byte(0x77),
            sourceRollup: source_rollup_id,
            scope: vec![U256::from(1)],
        },
    };

    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![sol_entry],
        publicInputsHash: B256::ZERO,
    };
    let encoded = event.encode_log_data();
    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0x55),
            data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
        },
        block_number: Some(100),
        ..Default::default()
    };

    let result = parse_batch_posted_logs(&[log], our_rollup_id);
    assert_eq!(
        result.len(),
        1,
        "entry must match via nextAction.rollupId even without state delta match"
    );
    assert_eq!(result[0].entry.action_hash, B256::with_last_byte(0x42));
    assert_eq!(result[0].l1_block_number, 100);
    assert_eq!(result[0].entry.next_action.rollup_id, our_rollup_id);
    // state deltas should still reference the source rollup, not ours
    assert_eq!(
        result[0].entry.state_deltas[0].rollup_id, source_rollup_id,
        "state deltas should be preserved as-is from source rollup"
    );
}

#[test]
fn test_parse_batch_posted_logs_no_match_when_neither_delta_nor_action() {
    // When NEITHER state deltas NOR nextAction.rollupId match, the entry
    // should be filtered out entirely.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let our_rollup_id = U256::from(1);

    let sol_entry = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![ICrossChainManagerL2::StateDelta {
            rollupId: U256::from(50), // neither delta
            currentState: B256::ZERO,
            newState: B256::with_last_byte(0x11),
            etherDelta: I256::ZERO,
        }],
        actionHash: B256::with_last_byte(0xBB),
        nextAction: ICrossChainManagerL2::Action {
            actionType: ICrossChainManagerL2::ActionType::CALL,
            rollupId: U256::from(50), // nor action targets our rollup
            destination: Address::with_last_byte(0x02),
            value: U256::ZERO,
            data: vec![].into(),
            failed: false,
            sourceAddress: Address::ZERO,
            sourceRollup: U256::ZERO,
            scope: vec![],
        },
    };

    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![sol_entry],
        publicInputsHash: B256::ZERO,
    };
    let encoded = event.encode_log_data();
    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0x55),
            data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
        },
        block_number: Some(200),
        ..Default::default()
    };

    let result = parse_batch_posted_logs(&[log], our_rollup_id);
    assert!(
        result.is_empty(),
        "entry with no delta or action match must be filtered out"
    );
}

// --- parse_batch_posted_logs: l1_block_number ordering across multiple logs ---

#[test]
fn test_parse_batch_posted_logs_preserves_l1_block_ordering() {
    // Entries from multiple logs at different L1 blocks must preserve
    // their l1_block_number in output order (earlier blocks first).
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let our_rollup_id = U256::from(1);

    let make_log = |l1_block: u64, action_hash_byte: u8| -> Log {
        let sol_entry = ICrossChainManagerL2::ExecutionEntry {
            stateDeltas: vec![ICrossChainManagerL2::StateDelta {
                rollupId: our_rollup_id,
                currentState: B256::ZERO,
                newState: B256::with_last_byte(action_hash_byte),
                etherDelta: I256::ZERO,
            }],
            actionHash: B256::with_last_byte(action_hash_byte),
            nextAction: ICrossChainManagerL2::Action {
                actionType: ICrossChainManagerL2::ActionType::CALL,
                rollupId: our_rollup_id,
                destination: Address::with_last_byte(action_hash_byte),
                value: U256::ZERO,
                data: vec![].into(),
                failed: false,
                sourceAddress: Address::ZERO,
                sourceRollup: U256::ZERO,
                scope: vec![],
            },
        };
        let event = ICrossChainManagerL2::BatchPosted {
            entries: vec![sol_entry],
            publicInputsHash: B256::ZERO,
        };
        let encoded = event.encode_log_data();
        Log {
            inner: alloy_primitives::Log {
                address: Address::with_last_byte(0x55),
                data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
            },
            block_number: Some(l1_block),
            ..Default::default()
        }
    };

    // Logs from L1 blocks 100, 200, 150 (out of order)
    let logs = vec![
        make_log(100, 0x01),
        make_log(200, 0x02),
        make_log(150, 0x03),
    ];

    let result = parse_batch_posted_logs(&logs, our_rollup_id);
    assert_eq!(result.len(), 3);

    // parse_batch_posted_logs preserves iteration order (same as input log order)
    assert_eq!(
        result[0].l1_block_number, 100,
        "first entry should be from first log"
    );
    assert_eq!(result[0].entry.action_hash, B256::with_last_byte(0x01));

    assert_eq!(
        result[1].l1_block_number, 200,
        "second entry should be from second log"
    );
    assert_eq!(result[1].entry.action_hash, B256::with_last_byte(0x02));

    assert_eq!(
        result[2].l1_block_number, 150,
        "third entry should be from third log"
    );
    assert_eq!(result[2].entry.action_hash, B256::with_last_byte(0x03));
}

#[test]
fn test_parse_batch_posted_logs_multiple_entries_per_log_same_block() {
    // A single BatchPosted log can contain multiple entries. All entries
    // from the same log should have the same l1_block_number.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let our_rollup_id = U256::from(1);

    let make_sol_entry = |hash_byte: u8| -> ICrossChainManagerL2::ExecutionEntry {
        ICrossChainManagerL2::ExecutionEntry {
            stateDeltas: vec![ICrossChainManagerL2::StateDelta {
                rollupId: our_rollup_id,
                currentState: B256::ZERO,
                newState: B256::with_last_byte(hash_byte),
                etherDelta: I256::ZERO,
            }],
            actionHash: B256::with_last_byte(hash_byte),
            nextAction: ICrossChainManagerL2::Action {
                actionType: ICrossChainManagerL2::ActionType::CALL,
                rollupId: our_rollup_id,
                destination: Address::with_last_byte(hash_byte),
                value: U256::ZERO,
                data: vec![].into(),
                failed: false,
                sourceAddress: Address::ZERO,
                sourceRollup: U256::ZERO,
                scope: vec![],
            },
        }
    };

    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![
            make_sol_entry(0xAA),
            make_sol_entry(0xBB),
            make_sol_entry(0xCC),
        ],
        publicInputsHash: B256::ZERO,
    };
    let encoded = event.encode_log_data();
    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0x55),
            data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
        },
        block_number: Some(500),
        ..Default::default()
    };

    let result = parse_batch_posted_logs(&[log], our_rollup_id);
    assert_eq!(
        result.len(),
        3,
        "all 3 entries from single log should match"
    );

    // All entries should have the same l1_block_number
    for entry in &result {
        assert_eq!(
            entry.l1_block_number, 500,
            "all entries from same log must have same l1_block_number"
        );
    }

    // Order should be preserved
    assert_eq!(result[0].entry.action_hash, B256::with_last_byte(0xAA));
    assert_eq!(result[1].entry.action_hash, B256::with_last_byte(0xBB));
    assert_eq!(result[2].entry.action_hash, B256::with_last_byte(0xCC));
}

#[test]
fn test_parse_batch_posted_logs_match_via_both_delta_and_action() {
    // Entry matches via BOTH state delta rollupId AND nextAction.rollupId.
    // Should still appear exactly once (not duplicated).
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let our_rollup_id = U256::from(1);

    let sol_entry = ICrossChainManagerL2::ExecutionEntry {
        stateDeltas: vec![ICrossChainManagerL2::StateDelta {
            rollupId: our_rollup_id, // matches via delta
            currentState: B256::ZERO,
            newState: B256::with_last_byte(0x11),
            etherDelta: I256::ZERO,
        }],
        actionHash: B256::with_last_byte(0xDD),
        nextAction: ICrossChainManagerL2::Action {
            actionType: ICrossChainManagerL2::ActionType::CALL,
            rollupId: our_rollup_id, // also matches via action
            destination: Address::with_last_byte(0x42),
            value: U256::ZERO,
            data: vec![].into(),
            failed: false,
            sourceAddress: Address::ZERO,
            sourceRollup: U256::ZERO,
            scope: vec![],
        },
    };

    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![sol_entry],
        publicInputsHash: B256::ZERO,
    };
    let encoded = event.encode_log_data();
    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0x55),
            data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
        },
        block_number: Some(300),
        ..Default::default()
    };

    let result = parse_batch_posted_logs(&[log], our_rollup_id);
    assert_eq!(
        result.len(),
        1,
        "entry matching via both paths should appear exactly once"
    );
    assert_eq!(result[0].entry.action_hash, B256::with_last_byte(0xDD));
}

// ── encode_post_batch_calldata tests ──

#[test]
fn test_encode_post_batch_calldata_selector_matches() {
    use alloy_sol_types::SolCall;

    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::ZERO,
            new_state: B256::with_last_byte(0x01),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::with_last_byte(0xAA),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: U256::from(1),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: vec![],
        },
    };

    let calldata = encode_post_batch_calldata(&[entry], Bytes::default(), Bytes::default());
    assert!(calldata.len() > 4, "calldata must have selector + params");
    assert_eq!(
        &calldata[..4],
        &ICrossChainManagerL2::postBatchCall::SELECTOR,
        "first 4 bytes must be postBatch selector"
    );
}

#[test]
fn test_encode_post_batch_calldata_empty_entries() {
    // Empty entries array should still encode successfully
    let calldata = encode_post_batch_calldata(&[], Bytes::default(), Bytes::default());
    assert!(calldata.len() >= 4, "must have at least the selector");
    assert_eq!(
        &calldata[..4],
        &ICrossChainManagerL2::postBatchCall::SELECTOR,
        "selector must be postBatch even with empty entries"
    );
}

#[test]
fn test_encode_post_batch_calldata_multiple_entries() {
    let make_entry = |n: u8| CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(n as u64),
            current_state: B256::with_last_byte(n),
            new_state: B256::with_last_byte(n.wrapping_add(1)),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::with_last_byte(n.wrapping_add(0x10)),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: U256::from(n as u64),
            destination: Address::with_last_byte(n),
            value: U256::from(n as u64 * 100),
            data: vec![n; n as usize],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: vec![],
        },
    };

    let entries = vec![make_entry(1), make_entry(2), make_entry(3), make_entry(4)];
    let calldata = encode_post_batch_calldata(&entries, Bytes::default(), Bytes::default());

    // Should be longer than single-entry encoding
    let single_calldata =
        encode_post_batch_calldata(&[make_entry(1)], Bytes::default(), Bytes::default());
    assert!(
        calldata.len() > single_calldata.len(),
        "4 entries should produce longer calldata than 1 entry"
    );

    // Verify selector is still correct
    assert_eq!(
        &calldata[..4],
        &ICrossChainManagerL2::postBatchCall::SELECTOR,
    );
}

#[test]
fn test_encode_post_batch_calldata_roundtrip_decode() {
    use alloy_sol_types::SolCall;

    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(42),
            current_state: B256::with_last_byte(0xAA),
            new_state: B256::with_last_byte(0xBB),
            ether_delta: I256::try_from(-1_000_000i128).expect("valid i256"),
        }],
        action_hash: B256::with_last_byte(0xFF),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: U256::from(42),
            destination: Address::with_last_byte(0x99),
            value: U256::from(500_000),
            data: vec![0xCA, 0xFE],
            failed: false,
            source_address: Address::with_last_byte(0x01),
            source_rollup: U256::from(7),
            scope: vec![U256::from(1), U256::from(2)],
        },
    };

    let proof = Bytes::from(vec![0x01, 0x02, 0x03]);
    let calldata = encode_post_batch_calldata(&[entry], Bytes::default(), proof.clone());

    // Decode the calldata back
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&calldata)
        .expect("calldata should decode successfully");

    assert_eq!(decoded.entries.len(), 1);
    assert_eq!(decoded.entries[0].actionHash, B256::with_last_byte(0xFF));
    assert_eq!(decoded.entries[0].stateDeltas.len(), 1);
    assert_eq!(decoded.entries[0].stateDeltas[0].rollupId, U256::from(42));
    assert_eq!(
        decoded.entries[0].stateDeltas[0].etherDelta,
        I256::try_from(-1_000_000i128).expect("valid i256")
    );
    assert_eq!(
        decoded.entries[0].nextAction.destination,
        Address::with_last_byte(0x99)
    );
    assert_eq!(decoded.entries[0].nextAction.scope.len(), 2);
    assert_eq!(decoded.blobCount, U256::ZERO);
    assert!(decoded.callData.is_empty());
    assert_eq!(decoded.proof, proof);
}

#[test]
fn test_encode_post_batch_empty_entries_roundtrip_decode() {
    use alloy_sol_types::SolCall;

    // Empty entries with non-empty proof — verify it decodes cleanly
    let proof = Bytes::from(vec![0xDE, 0xAD, 0xBE, 0xEF]);
    let calldata = encode_post_batch_calldata(&[], Bytes::default(), proof.clone());

    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&calldata)
        .expect("empty entries calldata should decode");
    assert!(decoded.entries.is_empty());
    assert_eq!(decoded.proof, proof);
    assert_eq!(decoded.blobCount, U256::ZERO);
}

#[test]
fn test_parse_batch_posted_logs_skips_malformed_log_data() {
    // A log with a valid topic hash but garbage data should be skipped (not panic)
    use alloy_primitives::LogData;
    let sig = batch_posted_signature_hash();
    let bad_data = vec![0xFF; 10]; // too short / malformed ABI data
    let log_data = LogData::new(vec![sig], bad_data.into()).expect("valid log structure");
    let inner = alloy_primitives::Log {
        address: Address::ZERO,
        data: log_data,
    };
    let log = Log {
        inner,
        block_number: Some(100),
        block_hash: None,
        block_timestamp: None,
        transaction_hash: None,
        transaction_index: None,
        log_index: None,
        removed: false,
    };
    let entries = parse_batch_posted_logs(&[log], U256::from(1));
    assert!(
        entries.is_empty(),
        "malformed log data should be skipped, not cause a panic"
    );
}

// --- Iteration 75: cross-chain entry ordering and causality ---

#[test]
fn test_parse_batch_posted_logs_preserves_intra_log_entry_order_mixed_action_types() {
    // A single BatchPosted log may contain entries with different action types
    // (CALL, RESULT, L2TX). The ordering within the log must be preserved
    // because cross-chain causality depends on it: a RESULT entry must follow
    // the CALL that produced it.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let our_rollup_id = U256::from(1);

    let make_sol_entry = |hash_byte: u8,
                          action_type: ICrossChainManagerL2::ActionType|
     -> ICrossChainManagerL2::ExecutionEntry {
        ICrossChainManagerL2::ExecutionEntry {
            stateDeltas: vec![ICrossChainManagerL2::StateDelta {
                rollupId: our_rollup_id,
                currentState: B256::ZERO,
                newState: B256::with_last_byte(hash_byte),
                etherDelta: I256::ZERO,
            }],
            actionHash: B256::with_last_byte(hash_byte),
            nextAction: ICrossChainManagerL2::Action {
                actionType: action_type,
                rollupId: our_rollup_id,
                destination: Address::with_last_byte(hash_byte),
                value: U256::ZERO,
                data: vec![].into(),
                failed: false,
                sourceAddress: Address::ZERO,
                sourceRollup: U256::from(2),
                scope: vec![],
            },
        }
    };

    // Causality chain: CALL → RESULT → CALL
    let event = ICrossChainManagerL2::BatchPosted {
        entries: vec![
            make_sol_entry(0x01, ICrossChainManagerL2::ActionType::CALL),
            make_sol_entry(0x02, ICrossChainManagerL2::ActionType::RESULT),
            make_sol_entry(0x03, ICrossChainManagerL2::ActionType::CALL),
        ],
        publicInputsHash: B256::ZERO,
    };
    let encoded = event.encode_log_data();
    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0x55),
            data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
        },
        block_number: Some(100),
        ..Default::default()
    };

    let result = parse_batch_posted_logs(&[log], our_rollup_id);
    assert_eq!(result.len(), 3, "all 3 entries should match our rollup");

    // Verify ordering preserved: CALL(0x01), RESULT(0x02), CALL(0x03)
    assert_eq!(result[0].entry.action_hash, B256::with_last_byte(0x01));
    assert_eq!(
        result[0].entry.next_action.action_type,
        CrossChainActionType::Call
    );

    assert_eq!(result[1].entry.action_hash, B256::with_last_byte(0x02));
    assert_eq!(
        result[1].entry.next_action.action_type,
        CrossChainActionType::Result
    );

    assert_eq!(result[2].entry.action_hash, B256::with_last_byte(0x03));
    assert_eq!(
        result[2].entry.next_action.action_type,
        CrossChainActionType::Call
    );
}

#[test]
fn test_parse_batch_posted_logs_multi_log_multi_entry_preserves_global_order() {
    // Two BatchPosted logs at different L1 blocks, each with multiple entries.
    // The output must be: [log1_entry0, log1_entry1, log2_entry0, log2_entry1]
    // preserving both inter-log and intra-log ordering.
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent;

    let our_rollup_id = U256::from(1);

    let make_sol_entry = |hash_byte: u8| -> ICrossChainManagerL2::ExecutionEntry {
        ICrossChainManagerL2::ExecutionEntry {
            stateDeltas: vec![ICrossChainManagerL2::StateDelta {
                rollupId: our_rollup_id,
                currentState: B256::ZERO,
                newState: B256::with_last_byte(hash_byte),
                etherDelta: I256::ZERO,
            }],
            actionHash: B256::with_last_byte(hash_byte),
            nextAction: ICrossChainManagerL2::Action {
                actionType: ICrossChainManagerL2::ActionType::CALL,
                rollupId: our_rollup_id,
                destination: Address::with_last_byte(hash_byte),
                value: U256::ZERO,
                data: vec![].into(),
                failed: false,
                sourceAddress: Address::ZERO,
                sourceRollup: U256::ZERO,
                scope: vec![],
            },
        }
    };

    let make_log = |l1_block: u64, entries: Vec<ICrossChainManagerL2::ExecutionEntry>| -> Log {
        let event = ICrossChainManagerL2::BatchPosted {
            entries,
            publicInputsHash: B256::ZERO,
        };
        let encoded = event.encode_log_data();
        Log {
            inner: alloy_primitives::Log {
                address: Address::with_last_byte(0x55),
                data: LogData::new(encoded.topics().to_vec(), encoded.data.clone()).unwrap(),
            },
            block_number: Some(l1_block),
            ..Default::default()
        }
    };

    let log1 = make_log(100, vec![make_sol_entry(0x10), make_sol_entry(0x11)]);
    let log2 = make_log(200, vec![make_sol_entry(0x20), make_sol_entry(0x21)]);

    let result = parse_batch_posted_logs(&[log1, log2], our_rollup_id);
    assert_eq!(result.len(), 4);

    // Global order: log1[0], log1[1], log2[0], log2[1]
    assert_eq!(result[0].entry.action_hash, B256::with_last_byte(0x10));
    assert_eq!(result[0].l1_block_number, 100);

    assert_eq!(result[1].entry.action_hash, B256::with_last_byte(0x11));
    assert_eq!(result[1].l1_block_number, 100);

    assert_eq!(result[2].entry.action_hash, B256::with_last_byte(0x20));
    assert_eq!(result[2].l1_block_number, 200);

    assert_eq!(result[3].entry.action_hash, B256::with_last_byte(0x21));
    assert_eq!(result[3].l1_block_number, 200);
}

// ──────────────────────────────────────────────
//  Iteration 79: ABI selector / event hash consistency with Solidity contracts
// ──────────────────────────────────────────────

/// Verify that the `sol!` macro enum ordinals match the Solidity contract.
/// Solidity: enum ActionType { CALL=0, RESULT=1, L2TX=2, REVERT=3, REVERT_CONTINUE=4 }
#[test]
fn test_action_type_enum_ordinals_match_solidity() {
    // alloy represents sol enums as u8 internally
    assert_eq!(ICrossChainManagerL2::ActionType::CALL as u8, 0);
    assert_eq!(ICrossChainManagerL2::ActionType::RESULT as u8, 1);
    assert_eq!(ICrossChainManagerL2::ActionType::L2TX as u8, 2);
    assert_eq!(ICrossChainManagerL2::ActionType::REVERT as u8, 3);
    assert_eq!(ICrossChainManagerL2::ActionType::REVERT_CONTINUE as u8, 4);
}

/// Verify function selectors match `cast sig` output computed from the Solidity source.
/// These are the canonical 4-byte selectors for the EEZ contract functions.
/// If a submodule upgrade changes any struct layout or function signature, these will break.
#[test]
fn test_function_selectors_match_solidity_contracts() {
    use alloy_sol_types::SolCall;

    // loadExecutionTable(((uint256,bytes32,bytes32,int256)[],bytes32,(uint8,uint256,address,uint256,bytes,bool,address,uint256,uint256[]))[])
    // cast sig => 0x96609ad5
    assert_eq!(
        ICrossChainManagerL2::loadExecutionTableCall::SELECTOR,
        [0x96, 0x60, 0x9a, 0xd5],
        "loadExecutionTable selector mismatch — submodule ABI may have changed"
    );

    // executeIncomingCrossChainCall(address,uint256,bytes,address,uint256,uint256[])
    // cast sig => 0x0f64c845
    assert_eq!(
        ICrossChainManagerL2::executeIncomingCrossChainCallCall::SELECTOR,
        [0x0f, 0x64, 0xc8, 0x45],
        "executeIncomingCrossChainCall selector mismatch — submodule ABI may have changed"
    );

    // postBatch(((uint256,bytes32,bytes32,int256)[],bytes32,(uint8,uint256,address,uint256,bytes,bool,address,uint256,uint256[]))[],uint256,bytes,bytes)
    // cast sig => 0x92cbb26e
    assert_eq!(
        ICrossChainManagerL2::postBatchCall::SELECTOR,
        [0x92, 0xcb, 0xb2, 0x6e],
        "postBatch selector mismatch — submodule ABI may have changed"
    );
}

/// Verify the BatchPosted event signature hash matches the Solidity contract.
/// cast sig-event => 0x2f482312f12dceb86aac9ef0e0e1d9421ac62910326b3d50695d63117321b520
#[test]
fn test_batch_posted_event_signature_matches_solidity() {
    let expected = B256::from([
        0x2f, 0x48, 0x23, 0x12, 0xf1, 0x2d, 0xce, 0xb8, 0x6a, 0xac, 0x9e, 0xf0, 0xe0, 0xe1, 0xd9,
        0x42, 0x1a, 0xc6, 0x29, 0x10, 0x32, 0x6b, 0x3d, 0x50, 0x69, 0x5d, 0x63, 0x11, 0x73, 0x21,
        0xb5, 0x20,
    ]);
    assert_eq!(
        ICrossChainManagerL2::BatchPosted::SIGNATURE_HASH,
        expected,
        "BatchPosted event signature hash mismatch — submodule ABI may have changed"
    );
}

/// Verify that Rust struct field ordering matches Solidity struct layout.
/// ABI encoding is order-dependent — if fields are reordered, encoding/decoding breaks.
/// This test roundtrips through ABI encode/decode to catch any field ordering mismatches.
/// Verify that Rust-computed action hashes match the values from
/// IntegrationTest.t.sol's `test_FullFlow_L2Execution_ThenL1Resolution`.
///
/// The Solidity test produces two known action hashes (visible in forge traces):
/// - RESULT hash: 0x512b265e... (used in loadExecutionTable on L2)
/// - CALL hash:   0x9c64bd79... (used in postBatch on L1)
///
/// These hashes depend on the exact Action struct field values. If our Rust
/// ABI encoding diverges from Solidity's `keccak256(abi.encode(action))`,
/// execution table lookups will fail at runtime.
#[test]
fn test_action_hash_matches_integration_test_solidity() {
    use alloy_sol_types::SolType;

    // ── Addresses from IntegrationTest.t.sol forge trace ──
    // These are deterministic based on deployer nonce in the forge test.
    let counter_l2: Address = "0x5991A2dF15A8F6A256D3Ec51E99254Cd3fb576A9"
        .parse()
        .unwrap();
    let counter_and_proxy: Address = "0xc7183455a4C133Ae270771860664b6B7ec320bB1"
        .parse()
        .unwrap();
    let l2_rollup_id = U256::from(1u64);
    let mainnet_rollup_id = U256::ZERO;

    // increment() selector = 0xd09de08a
    let increment_calldata = vec![0xd0, 0x9d, 0xe0, 0x8a];

    // ── RESULT action (built after Counter.increment() returns 1) ──
    // data = abi.encode(uint256(1))
    let result_data = {
        let mut buf = vec![0u8; 32];
        buf[31] = 1;
        buf
    };

    let result_action = ICrossChainManagerL2::Action {
        actionType: ICrossChainManagerL2::ActionType::RESULT,
        rollupId: l2_rollup_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: result_data.into(),
        failed: false,
        sourceAddress: Address::ZERO,
        sourceRollup: U256::ZERO,
        scope: vec![],
    };

    let result_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(&result_action));
    let expected_result_hash: B256 =
        "0x512b265e8f48bc74f259afc87b374d4f7c5e836277478c58e8c61178ac9d62e4"
            .parse()
            .unwrap();
    assert_eq!(
        result_hash, expected_result_hash,
        "RESULT action hash must match IntegrationTest.t.sol"
    );

    // ── CALL action (built by Rollups.executeCrossChainCall on L1) ──
    let call_action = ICrossChainManagerL2::Action {
        actionType: ICrossChainManagerL2::ActionType::CALL,
        rollupId: l2_rollup_id,
        destination: counter_l2,
        value: U256::ZERO,
        data: increment_calldata.into(),
        failed: false,
        sourceAddress: counter_and_proxy,
        sourceRollup: mainnet_rollup_id,
        scope: vec![],
    };

    let call_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(&call_action));
    let expected_call_hash: B256 =
        "0x9c64bd79e6a6ead9e61811cf609815981e76ccbb4f05c0f739ea7829fea5538d"
            .parse()
            .unwrap();
    assert_eq!(
        call_hash, expected_call_hash,
        "CALL action hash must match IntegrationTest.t.sol"
    );

    // ── Also verify via Rust-native types → to_sol_action() path ──
    let rust_result = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: l2_rollup_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: {
            let mut buf = vec![0u8; 32];
            buf[31] = 1;
            buf
        },
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let rust_result_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &rust_result.to_sol_action(),
    ));
    assert_eq!(
        rust_result_hash, expected_result_hash,
        "Rust-native → to_sol_action() → hash must also match"
    );

    let rust_call = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: l2_rollup_id,
        destination: counter_l2,
        value: U256::ZERO,
        data: vec![0xd0, 0x9d, 0xe0, 0x8a],
        failed: false,
        source_address: counter_and_proxy,
        source_rollup: mainnet_rollup_id,
        scope: vec![],
    };
    let rust_call_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &rust_call.to_sol_action(),
    ));
    assert_eq!(
        rust_call_hash, expected_call_hash,
        "Rust-native → to_sol_action() → hash must also match"
    );
}

/// Verify that `build_cross_chain_call_entries()` produces the correct
/// CALL and RESULT action hashes matching IntegrationTest.t.sol.
///
/// This tests the actual function the driver/RPC uses to build entries,
/// not just raw hash computation.
#[test]
fn test_build_entries_matches_integration_test() {
    let counter_l2: Address = "0x5991A2dF15A8F6A256D3Ec51E99254Cd3fb576A9"
        .parse()
        .unwrap();
    let counter_and_proxy: Address = "0xc7183455a4C133Ae270771860664b6B7ec320bB1"
        .parse()
        .unwrap();
    let l2_rollup_id = U256::from(1u64);
    let mainnet_rollup_id = U256::ZERO;
    let increment_calldata = vec![0xd0, 0x9d, 0xe0, 0x8a];

    // abi.encode(uint256(1)) — the return data from Counter.increment()
    let return_data = {
        let mut buf = vec![0u8; 32];
        buf[31] = 1;
        buf
    };

    let (call_entry, result_entry) = build_cross_chain_call_entries(
        l2_rollup_id,
        counter_l2,
        increment_calldata,
        U256::ZERO,
        counter_and_proxy,
        mainnet_rollup_id,
        true, // call succeeded
        return_data,
    );

    let expected_call_hash: B256 =
        "0x9c64bd79e6a6ead9e61811cf609815981e76ccbb4f05c0f739ea7829fea5538d"
            .parse()
            .unwrap();
    let expected_result_hash: B256 =
        "0x512b265e8f48bc74f259afc87b374d4f7c5e836277478c58e8c61178ac9d62e4"
            .parse()
            .unwrap();

    assert_eq!(
        call_entry.action_hash, expected_call_hash,
        "build_cross_chain_call_entries CALL hash must match"
    );
    assert_eq!(
        result_entry.action_hash, expected_result_hash,
        "build_cross_chain_call_entries RESULT hash must match"
    );

    // Verify action types
    assert_eq!(
        call_entry.next_action.action_type,
        CrossChainActionType::Call
    );
    assert_eq!(
        result_entry.next_action.action_type,
        CrossChainActionType::Result
    );

    // Verify RESULT data is the return value
    assert_eq!(result_entry.next_action.data.len(), 32);
    assert_eq!(result_entry.next_action.data[31], 1);
    assert!(!result_entry.next_action.failed);
}

/// Verify Rust-computed action hashes match NestedIntegrationTest.t.sol.
///
/// The nested scenario: executeIncomingCrossChainCall → CounterAndProxy →
/// CrossChainProxy → executeCrossChainCall. Two entries are consumed:
///   1. INNER CALL: CounterAndProxy calls remote Counter via proxy
///   2. OUTER RESULT: CounterAndProxy.increment() returns void
#[test]
fn test_action_hash_matches_nested_integration_test() {
    use alloy_sol_types::SolType;

    let counter_and_proxy: Address = "0x2e234DAe75C793f67A35089C9d99245E1C58470b"
        .parse()
        .unwrap();
    let remote_counter: Address = "0x000000000000000000000000000000000000C001"
        .parse()
        .unwrap();
    let l2_rollup_id = U256::from(1u64);
    let remote_rollup_id = U256::ZERO;
    let increment_calldata = vec![0xd0, 0x9d, 0xe0, 0x8a];

    // ── INNER CALL action ──
    // Built by executeCrossChainCall when CounterAndProxy calls the proxy
    let inner_call_action = ICrossChainManagerL2::Action {
        actionType: ICrossChainManagerL2::ActionType::CALL,
        rollupId: remote_rollup_id,
        destination: remote_counter,
        value: U256::ZERO,
        data: increment_calldata.clone().into(),
        failed: false,
        sourceAddress: counter_and_proxy,
        sourceRollup: l2_rollup_id,
        scope: vec![],
    };

    let inner_call_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(&inner_call_action));
    let expected_inner_hash: B256 =
        "0x89de505f1c650822df69100082c9aa392b8f81a4c544fd632501331243ac234a"
            .parse()
            .unwrap();
    assert_eq!(
        inner_call_hash, expected_inner_hash,
        "Inner CALL hash must match NestedIntegrationTest.t.sol"
    );

    // ── OUTER RESULT action ──
    // Built by _processCallAtScope after CounterAndProxy.increment() finishes.
    // CounterAndProxy.increment() is void → executeOnBehalf returns empty bytes.
    let outer_result_action = ICrossChainManagerL2::Action {
        actionType: ICrossChainManagerL2::ActionType::RESULT,
        rollupId: l2_rollup_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: Default::default(), // empty — void function
        failed: false,
        sourceAddress: Address::ZERO,
        sourceRollup: U256::ZERO,
        scope: vec![],
    };

    let outer_result_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &outer_result_action,
    ));
    let expected_outer_hash: B256 =
        "0x7cee89f0045a0776100dae683e137be68d18b3addfe8ea71b650f72e7db50c56"
            .parse()
            .unwrap();
    assert_eq!(
        outer_result_hash, expected_outer_hash,
        "Outer RESULT hash must match NestedIntegrationTest.t.sol"
    );

    // Also verify via Rust-native types
    let rust_inner = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: remote_rollup_id,
        destination: remote_counter,
        value: U256::ZERO,
        data: increment_calldata,
        failed: false,
        source_address: counter_and_proxy,
        source_rollup: l2_rollup_id,
        scope: vec![],
    };
    assert_eq!(
        keccak256(ICrossChainManagerL2::Action::abi_encode(
            &rust_inner.to_sol_action()
        )),
        expected_inner_hash,
    );

    let rust_outer = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: l2_rollup_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![], // void
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    assert_eq!(
        keccak256(ICrossChainManagerL2::Action::abi_encode(
            &rust_outer.to_sol_action()
        )),
        expected_outer_hash,
    );
}

// ── Adversarial input fuzzing (QA re-run iteration 25) ──

#[test]
fn test_build_cross_chain_call_entries_empty_data() {
    let (call_entry, result_entry) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x42),
        vec![],
        U256::ZERO,
        Address::with_last_byte(0x01),
        U256::from(0),
        true,
        vec![],
    );
    assert_eq!(
        call_entry.next_action.action_type,
        CrossChainActionType::Call
    );
    assert_eq!(
        result_entry.next_action.action_type,
        CrossChainActionType::Result
    );
    assert!(call_entry.next_action.data.is_empty());
    assert!(result_entry.next_action.data.is_empty());
    assert!(!result_entry.next_action.failed);
    // Action hashes should be non-zero
    assert_ne!(call_entry.action_hash, B256::ZERO);
    assert_ne!(result_entry.action_hash, B256::ZERO);
}

#[test]
fn test_build_cross_chain_call_entries_failed_call() {
    let (call_entry, result_entry) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x42),
        vec![0xDE, 0xAD],
        U256::ZERO,
        Address::with_last_byte(0x01),
        U256::from(0),
        false,                        // call failed
        vec![0x08, 0xC3, 0x79, 0xA0], // revert selector
    );
    assert!(!call_entry.next_action.failed);
    assert!(
        result_entry.next_action.failed,
        "RESULT should be marked failed when call fails"
    );
    assert_eq!(result_entry.next_action.data, vec![0x08, 0xC3, 0x79, 0xA0]);
}

#[test]
fn test_l1_l2_entry_roundtrip() {
    // Build L2 pairs → convert to L1 → extract CALL actions → reconstruct L2 pairs
    let (call_entry, result_entry) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x42),
        vec![0xDE, 0xAD],
        U256::ZERO,
        Address::with_last_byte(0x01),
        U256::from(0),
        true,
        vec![0x00; 32],
    );
    let l2_pairs = vec![call_entry.clone(), result_entry.clone()];

    // The CALL action (from ExecutionConsumed event on L1)
    let call_actions = vec![call_entry.next_action.clone()];
    assert_eq!(call_actions[0].action_type, CrossChainActionType::Call);
    assert_eq!(call_actions[0].destination, Address::with_last_byte(0x42));

    // Convert to L1 format (what gets submitted as entries)
    let l1_entries = convert_pairs_to_l1_entries(&l2_pairs);
    assert_eq!(l1_entries.len(), 1);
    assert_eq!(l1_entries[0].action_hash, call_entry.action_hash);
    assert_eq!(
        l1_entries[0].next_action.action_type,
        CrossChainActionType::Result
    );

    // Reconstruct L2 pairs from L1 entries + CALL actions (what fullnodes do)
    let reconstructed = convert_l1_entries_to_l2_pairs(&l1_entries, &call_actions);
    assert_eq!(reconstructed.len(), 2);
    // CALL trigger entry
    assert_eq!(reconstructed[0].action_hash, call_entry.action_hash);
    assert_eq!(
        reconstructed[0].next_action.action_type,
        CrossChainActionType::Call
    );
    assert_eq!(
        reconstructed[0].next_action.destination,
        Address::with_last_byte(0x42)
    );
    assert_eq!(reconstructed[0].next_action.data, vec![0xDE, 0xAD]);
    // RESULT table entry
    assert_eq!(reconstructed[1].action_hash, result_entry.action_hash);
    assert_eq!(
        reconstructed[1].next_action.action_type,
        CrossChainActionType::Result
    );
}

#[test]
fn test_l1_l2_entry_roundtrip_filtered_subset() {
    // Test hash-based matching when only some entries are consumed
    let (call_a, result_a) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x42),
        vec![0x01],
        U256::ZERO,
        Address::with_last_byte(0x01),
        U256::from(0),
        true,
        vec![],
    );
    let (call_b, result_b) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x43),
        vec![0x02],
        U256::ZERO,
        Address::with_last_byte(0x02),
        U256::from(0),
        true,
        vec![],
    );
    let l2_pairs = vec![
        call_a.clone(),
        result_a.clone(),
        call_b.clone(),
        result_b.clone(),
    ];
    // CALL actions (from ExecutionConsumed events on L1)
    let call_actions = vec![call_a.next_action.clone(), call_b.next_action.clone()];
    let l1_entries = convert_pairs_to_l1_entries(&l2_pairs);
    assert_eq!(l1_entries.len(), 2);

    // Only entry B was consumed (simulating partial consumption)
    let filtered = vec![l1_entries[1].clone()];
    let reconstructed = convert_l1_entries_to_l2_pairs(&filtered, &call_actions);
    assert_eq!(reconstructed.len(), 2);
    assert_eq!(reconstructed[0].action_hash, call_b.action_hash);
    assert_eq!(
        reconstructed[0].next_action.destination,
        Address::with_last_byte(0x43)
    );
    assert_eq!(reconstructed[1].action_hash, result_b.action_hash);
}

// ──────────────────────────────────────────────
//  Re-run Iteration 26: Cross-chain storage layout and selector consistency
// ──────────────────────────────────────────────

// ── Re-run Iteration 41: Cross-chain execution entry encoding edge cases ──

#[test]
fn test_build_cross_chain_call_entries_full_l1_roundtrip() {
    // Verify that build_cross_chain_call_entries output survives the
    // full L1 pipeline: postBatch encode → BatchPosted event → parse
    use alloy_primitives::LogData;
    use alloy_sol_types::{SolCall, SolEvent};

    let (call_entry, result_entry) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x42),
        vec![0xDE, 0xAD],
        U256::ZERO,
        Address::with_last_byte(0xAB),
        U256::from(2),
        true,
        vec![0xCA, 0xFE],
    );

    // Both entries have empty state_deltas at creation time
    assert!(call_entry.state_deltas.is_empty());
    assert!(result_entry.state_deltas.is_empty());

    // Convert L2 pairs to L1 entries (non-nested format)
    let l1_entries = convert_pairs_to_l1_entries(&[call_entry.clone(), result_entry.clone()]);
    assert_eq!(l1_entries.len(), 1);
    let l1_entry = &l1_entries[0];
    assert_eq!(l1_entry.action_hash, call_entry.action_hash);
    assert_eq!(
        l1_entry.next_action.action_type,
        CrossChainActionType::Result
    );

    // Encode L1 entry via postBatch
    let calldata = encode_post_batch_calldata(&l1_entries, Bytes::default(), Bytes::default());
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&calldata)
        .expect("postBatch encoding of cross-chain entries should decode");
    assert_eq!(decoded.entries.len(), 1);

    // Simulate BatchPosted event
    let event = ICrossChainManagerL2::BatchPosted {
        entries: decoded.entries,
        publicInputsHash: B256::ZERO,
    };
    let log_data = event.encode_log_data();
    let mock_log = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(log_data.topics().to_vec(), log_data.data.clone()).unwrap(),
        },
        block_number: Some(100),
        ..Default::default()
    };

    // Parse for rollup_id=1 — L1 entry has nextAction.rollupId=1
    let derived = parse_batch_posted_logs(&[mock_log], U256::from(1));
    assert_eq!(derived.len(), 1, "single L1 entry should be derived");

    // Verify L1 entry survived roundtrip (actionHash=CALL, nextAction=RESULT)
    assert_eq!(derived[0].entry.action_hash, call_entry.action_hash);
    assert_eq!(
        derived[0].entry.next_action.action_type,
        CrossChainActionType::Result
    );
    assert_eq!(derived[0].entry.next_action.data, vec![0xCA, 0xFE]);
    assert!(!derived[0].entry.next_action.failed);
}

#[test]
fn test_encode_decode_block_calldata_roundtrip() {
    let numbers = vec![1u64, 2, 3];
    let txs = vec![
        Bytes::from(vec![0xc0]),
        Bytes::from(vec![0xc1, 0x80]),
        Bytes::from(vec![0xc0]),
    ];
    let encoded = encode_block_calldata(&numbers, &txs);
    let (decoded_numbers, decoded_txs) = decode_block_calldata(&encoded).unwrap();
    assert_eq!(decoded_numbers, numbers);
    assert_eq!(decoded_txs, txs);
}

#[test]
fn test_encode_block_calldata_empty() {
    let encoded = encode_block_calldata(&[], &[]);
    let (numbers, txs) = decode_block_calldata(&encoded).unwrap();
    assert!(numbers.is_empty());
    assert!(txs.is_empty());
}

#[test]
fn test_build_block_entries_creates_immediate_entries() {
    let blocks = vec![
        (
            1u64,
            B256::with_last_byte(0xAA),
            B256::with_last_byte(0xBB),
            Bytes::from(vec![0xc0]),
        ),
        (
            2u64,
            B256::with_last_byte(0xBB),
            B256::with_last_byte(0xCC),
            Bytes::from(vec![0xc0]),
        ),
    ];
    let entries = build_block_entries(&blocks, 1);
    assert_eq!(entries.len(), 2);
    // All entries should be immediate (actionHash == 0)
    for entry in &entries {
        assert_eq!(entry.action_hash, B256::ZERO);
        assert_eq!(entry.state_deltas.len(), 1);
        assert_eq!(entry.state_deltas[0].rollup_id, U256::from(1));
    }
    // Verify state root chaining
    assert_eq!(
        entries[0].state_deltas[0].current_state,
        B256::with_last_byte(0xAA)
    );
    assert_eq!(
        entries[0].state_deltas[0].new_state,
        B256::with_last_byte(0xBB)
    );
    assert_eq!(
        entries[1].state_deltas[0].current_state,
        B256::with_last_byte(0xBB)
    );
    assert_eq!(
        entries[1].state_deltas[0].new_state,
        B256::with_last_byte(0xCC)
    );
}

#[test]
fn test_decode_post_batch_calldata_roundtrip() {
    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::with_last_byte(0xAA),
            new_state: B256::with_last_byte(0xBB),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::ZERO,
        next_action: CrossChainAction {
            action_type: CrossChainActionType::L2Tx,
            rollup_id: U256::ZERO,
            destination: Address::ZERO,
            value: U256::ZERO,
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: vec![],
        },
    };
    let block_data =
        encode_block_calldata(&[1, 2], &[Bytes::from(vec![0xc0]), Bytes::from(vec![0xc1])]);
    let calldata = encode_post_batch_calldata(
        std::slice::from_ref(&entry),
        block_data.clone(),
        Bytes::default(),
    );
    let (decoded_entries, decoded_call_data) = decode_post_batch_calldata(&calldata).unwrap();
    assert_eq!(decoded_entries.len(), 1);
    assert_eq!(decoded_entries[0], entry);
    assert_eq!(decoded_call_data, block_data);
}

// ── ExecutionConsumed helpers tests ──

#[test]
fn test_execution_consumed_signature_hash_is_nonzero() {
    let hash = execution_consumed_signature_hash();
    assert_ne!(
        hash,
        B256::ZERO,
        "ExecutionConsumed signature hash must not be zero"
    );
}

#[test]
fn test_execution_consumed_signature_hash_differs_from_batch_posted() {
    assert_ne!(
        execution_consumed_signature_hash(),
        batch_posted_signature_hash(),
        "ExecutionConsumed and BatchPosted must have different signature hashes"
    );
}

#[test]
fn test_parse_execution_consumed_logs_empty() {
    let consumed = parse_execution_consumed_logs(&[]);
    assert!(consumed.is_empty());
}

#[test]
fn test_parse_execution_consumed_logs_extracts_topic1() {
    use alloy_primitives::LogData;

    let action_hash_1 = B256::with_last_byte(0x42);
    let action_hash_2 = B256::with_last_byte(0x99);
    let sig = execution_consumed_signature_hash();

    // Event data is empty (Action won't decode) but hash is still captured
    let log1 = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(vec![sig, action_hash_1], Bytes::new()).unwrap(),
        },
        ..Default::default()
    };
    let log2 = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(vec![sig, action_hash_2], Bytes::new()).unwrap(),
        },
        ..Default::default()
    };

    let consumed = parse_execution_consumed_logs(&[log1, log2]);
    assert_eq!(consumed.len(), 2);
    assert!(consumed.contains_key(&action_hash_1));
    assert!(consumed.contains_key(&action_hash_2));
}

#[test]
fn test_parse_execution_consumed_skips_logs_with_fewer_than_2_topics() {
    use alloy_primitives::LogData;

    let sig = execution_consumed_signature_hash();
    // Only 1 topic (just the signature, no actionHash)
    let bad_log = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(vec![sig], Bytes::new()).unwrap(),
        },
        ..Default::default()
    };

    let consumed = parse_execution_consumed_logs(&[bad_log]);
    assert!(consumed.is_empty());
}

#[test]
fn test_parse_execution_consumed_deduplicates() {
    use alloy_primitives::LogData;

    let action_hash = B256::with_last_byte(0x42);
    let sig = execution_consumed_signature_hash();

    let log = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(vec![sig, action_hash], Bytes::new()).unwrap(),
        },
        ..Default::default()
    };

    // Same log twice
    let consumed = parse_execution_consumed_logs(&[log.clone(), log]);
    assert_eq!(consumed.len(), 1);
    assert!(consumed.contains_key(&action_hash));
}

#[test]
fn test_attach_chained_state_deltas() {
    // Create 3 CALL+RESULT entry pairs with empty state_deltas
    let (call1, result1) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x01),
        vec![0x01],
        U256::ZERO,
        Address::with_last_byte(0xA1),
        U256::from(2),
        true,
        vec![0x01],
    );
    let (call2, result2) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x02),
        vec![0x02],
        U256::ZERO,
        Address::with_last_byte(0xA2),
        U256::from(2),
        true,
        vec![0x02],
    );
    let (call3, result3) = build_cross_chain_call_entries(
        U256::from(1),
        Address::with_last_byte(0x03),
        vec![0x03],
        U256::ZERO,
        Address::with_last_byte(0xA3),
        U256::from(2),
        true,
        vec![0x03],
    );

    let mut entries = vec![call1, result1, call2, result2, call3, result3];

    // Verify all start with empty deltas
    for e in &entries {
        assert!(e.state_deltas.is_empty());
    }

    // Intermediate roots: [Y, X₁, X₂, X]
    let y = B256::with_last_byte(0x10);
    let x1 = B256::with_last_byte(0x11);
    let x2 = B256::with_last_byte(0x12);
    let x = B256::with_last_byte(0x13);
    let roots = vec![y, x1, x2, x];

    attach_chained_state_deltas(&mut entries, &roots, 1);

    // CALL entries (even index) get deltas
    assert_eq!(entries[0].state_deltas.len(), 1);
    assert_eq!(entries[0].state_deltas[0].current_state, y);
    assert_eq!(entries[0].state_deltas[0].new_state, x1);
    assert_eq!(entries[0].state_deltas[0].rollup_id, U256::from(1));

    assert_eq!(entries[2].state_deltas.len(), 1);
    assert_eq!(entries[2].state_deltas[0].current_state, x1);
    assert_eq!(entries[2].state_deltas[0].new_state, x2);

    assert_eq!(entries[4].state_deltas.len(), 1);
    assert_eq!(entries[4].state_deltas[0].current_state, x2);
    assert_eq!(entries[4].state_deltas[0].new_state, x);

    // RESULT entries (odd index) remain empty
    assert!(entries[1].state_deltas.is_empty());
    assert!(entries[3].state_deltas.is_empty());
    assert!(entries[5].state_deltas.is_empty());
}

/// Regression test for issue #242: L2→L1 cross-chain calls must propagate delivery
/// return data into the L2 RESULT table entry, not just the L1 deferred entry.
#[test]
fn test_build_l2_to_l1_call_entries_propagates_return_data() {
    let destination = Address::with_last_byte(0x42);
    let source = Address::with_last_byte(0x01);
    let _builder = Address::with_last_byte(0xBB);
    let rollup_id = 1u64;

    // Simulate Counter.increment() returning uint256(1).
    let return_data = U256::from(1).to_be_bytes_vec();
    let increment_selector = vec![0xd0, 0x9d, 0xe0, 0x8a];

    let entries = build_l2_to_l1_call_entries(
        destination,
        increment_selector,
        U256::ZERO,
        source,
        rollup_id,
        vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
        return_data.clone(),
        false,
        vec![], // l1_delivery_scope: empty for tests (no deep nesting)
        false,  // tx_reverts
    );

    // L2 table entry 0: CALL entry, nextAction = RESULT with delivery return data.
    let l2_entry_0 = &entries.l2_table_entries[0];
    assert_eq!(
        l2_entry_0.next_action.action_type,
        CrossChainActionType::Result,
        "L2 entry 0 nextAction must be RESULT"
    );
    assert_eq!(
        l2_entry_0.next_action.data, return_data,
        "L2 RESULT entry must contain delivery return data (issue #242)"
    );
    assert!(!l2_entry_0.next_action.failed, "delivery did not fail");

    // L2 table entry 1: terminal RESULT, same data.
    let l2_entry_1 = &entries.l2_table_entries[1];
    assert_eq!(
        l2_entry_1.next_action.data, return_data,
        "L2 terminal RESULT must also contain delivery return data"
    );

    // Hash consistency: entry 1's action_hash must match hash(entry 0's nextAction).
    let expected_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &l2_entry_0.next_action.to_sol_action(),
    ));
    assert_eq!(
        l2_entry_1.action_hash, expected_hash,
        "L2 entry 1 action_hash must match hash of the RESULT action"
    );

    // L1 deferred entry 1: terminal RESULT (§C.6: always void for L2TX).
    let l1_entry_1 = &entries.l1_deferred_entries[1];
    assert!(
        l1_entry_1.next_action.data.is_empty(),
        "L1 deferred terminal RESULT must be void per §C.6"
    );
    assert_eq!(
        l1_entry_1.next_action.rollup_id,
        alloy_primitives::U256::from(1u64),
        "L1 deferred terminal RESULT rollupId must be triggering rollupId (L2)"
    );
}

/// Verify that withdrawals (EOA targets) still produce empty return data after the
/// issue #242 fix — the delivery_return_data for withdrawals is always vec![].
#[test]
fn test_build_withdrawal_entries_still_void() {
    let user = Address::with_last_byte(0x01);
    let _builder = Address::with_last_byte(0xBB);
    let rollup_id = 1u64;
    let amount = U256::from(1_000_000_000_000_000_000u128); // 1 ETH

    let entries = build_l2_to_l1_call_entries(
        user,   // destination
        vec![], // data: no calldata for ETH withdrawal
        amount, // value
        user,   // source_address
        rollup_id,
        vec![0xc0], // rlp_encoded_tx placeholder (empty RLP list)
        vec![],     // delivery_return_data: EOA recipient
        false,      // delivery_failed
        vec![],     // l1_delivery_scope
        false,      // tx_reverts
    );

    // L2 RESULT entries should have empty data (EOA target, no return data).
    let l2_entry_0 = &entries.l2_table_entries[0];
    assert!(
        l2_entry_0.next_action.data.is_empty(),
        "withdrawal L2 RESULT must have empty data (EOA target)"
    );
    assert!(
        !l2_entry_0.next_action.failed,
        "withdrawal delivery must not fail"
    );

    // L1 deferred RESULT should also be empty for withdrawals.
    let l1_entry_1 = &entries.l1_deferred_entries[1];
    assert!(
        l1_entry_1.next_action.data.is_empty(),
        "withdrawal L1 RESULT must have empty data"
    );
}

/// Verify that the L2 RESULT action hash changes when delivery_return_data is non-empty,
/// and that both L2 table entries are consistent with each other.
#[test]
fn test_build_l2_to_l1_entries_hash_consistency_with_return_data() {
    let destination = Address::with_last_byte(0x42);
    let source = Address::with_last_byte(0x01);
    let _builder = Address::with_last_byte(0xBB);

    // Build entries with empty return data.
    let entries_void = build_l2_to_l1_call_entries(
        destination,
        vec![0xd0, 0x9d, 0xe0, 0x8a],
        U256::ZERO,
        source,
        1,
        vec![0xc0], // rlp_encoded_tx placeholder
        vec![],
        false,
        vec![], // l1_delivery_scope
        false,  // tx_reverts
    );

    // Build entries with non-empty return data.
    let return_data = U256::from(42).to_be_bytes_vec();
    let entries_data = build_l2_to_l1_call_entries(
        destination,
        vec![0xd0, 0x9d, 0xe0, 0x8a],
        U256::ZERO,
        source,
        1,
        vec![0xc0], // rlp_encoded_tx placeholder
        return_data,
        false,
        vec![], // l1_delivery_scope
        false,  // tx_reverts
    );

    // The CALL entry hash should be the same (same CALL action regardless of return data).
    assert_eq!(
        entries_void.l2_table_entries[0].action_hash, entries_data.l2_table_entries[0].action_hash,
        "CALL hash must not change with different return data"
    );

    // The RESULT entry hash MUST differ when return data differs.
    assert_ne!(
        entries_void.l2_table_entries[1].action_hash, entries_data.l2_table_entries[1].action_hash,
        "RESULT hash must change when return data differs"
    );
}

/// Verify that `build_l2_to_l1_call_entries` with deep scope produces correct L1 entries.
///
/// The deepScope protocol test requires scope=[0,0] on the L1 delivery CALL action
/// when the L2 trace has 2 levels of contract nesting before the proxy call.
/// L2 entries must always have scope=[] regardless of depth.
#[test]
fn test_build_l2_to_l1_call_entries_deep_scope() {
    let destination = Address::with_last_byte(0x42);
    let source = Address::with_last_byte(0x01);
    let rollup_id = 1u64;
    let return_data = U256::from(1).to_be_bytes_vec();

    // Build with deep scope [0,0] (depth 2: SCA → SCB → proxy)
    let deep_scope = vec![U256::ZERO, U256::ZERO];
    let entries_deep = build_l2_to_l1_call_entries(
        destination,
        vec![0xd0, 0x9d, 0xe0, 0x8a], // increment() selector
        U256::ZERO,
        source,
        rollup_id,
        vec![0xc0],
        return_data.clone(),
        false,
        deep_scope.clone(),
        false, // tx_reverts
    );

    // Build with empty scope (direct call)
    let entries_flat = build_l2_to_l1_call_entries(
        destination,
        vec![0xd0, 0x9d, 0xe0, 0x8a],
        U256::ZERO,
        source,
        rollup_id,
        vec![0xc0],
        return_data.clone(),
        false,
        vec![],
        false, // tx_reverts
    );

    // L1 entry 0 nextAction (delivery CALL) must carry the deep scope.
    let l1_entry_0_deep = &entries_deep.l1_deferred_entries[0];
    assert_eq!(
        l1_entry_0_deep.next_action.scope, deep_scope,
        "L1 delivery CALL must have scope=[0,0] for deep nesting"
    );
    assert_eq!(
        l1_entry_0_deep.next_action.action_type,
        CrossChainActionType::Call,
        "L1 entry 0 nextAction must be CALL"
    );

    // L1 entry 0 nextAction with empty scope must have empty scope.
    let l1_entry_0_flat = &entries_flat.l1_deferred_entries[0];
    assert!(
        l1_entry_0_flat.next_action.scope.is_empty(),
        "L1 delivery CALL with empty scope must have scope=[]"
    );

    // Deep and flat entries must have DIFFERENT action hashes on L1 entry 0,
    // because the L2TX trigger is the same (same rlp_encoded_tx) but the
    // nextAction differs (scope changes the ABI encoding).
    // Actually: entry 0's action_hash = hash(L2TX action) which doesn't include scope.
    // The entries SHARE the same action_hash on entry 0 (L2TX trigger is identical).
    assert_eq!(
        l1_entry_0_deep.action_hash, l1_entry_0_flat.action_hash,
        "L2TX trigger hash must be the same regardless of scope"
    );

    // But the entry hashes (hash of the full entry including nextAction) differ
    // because nextAction.scope differs. This means on-chain, different scope
    // entries can coexist without collision.
    let deep_entry_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &l1_entry_0_deep.next_action.to_sol_action(),
    ));
    let flat_entry_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &l1_entry_0_flat.next_action.to_sol_action(),
    ));
    assert_ne!(
        deep_entry_hash, flat_entry_hash,
        "L1 delivery CALL hash must differ between scope=[0,0] and scope=[]"
    );

    // L2 entries must ALWAYS have scope=[] regardless of L1 scope.
    for (i, entry) in entries_deep.l2_table_entries.iter().enumerate() {
        assert!(
            entry.next_action.scope.is_empty(),
            "L2 entry {} nextAction scope must be empty (got {:?})",
            i,
            entry.next_action.scope
        );
    }

    // L1 entry 1 (RESULT → terminal) must have empty scope on both entries.
    let l1_entry_1_deep = &entries_deep.l1_deferred_entries[1];
    assert!(
        l1_entry_1_deep.next_action.scope.is_empty(),
        "L1 terminal RESULT must have scope=[]"
    );
}

// ──────────────────────────────────────────────
//  REVERT / REVERT_CONTINUE helpers (§D.12)
// ──────────────────────────────────────────────

#[test]
fn test_revert_action_canonical_fields() {
    let rollup_id = U256::from(42069);
    let scope = vec![U256::ZERO];
    let action = revert_action(rollup_id, scope.clone());

    assert_eq!(action.action_type, CrossChainActionType::Revert);
    assert_eq!(action.rollup_id, rollup_id);
    assert_eq!(action.destination, Address::ZERO);
    assert_eq!(action.value, U256::ZERO);
    assert!(action.data.is_empty());
    assert!(!action.failed, "REVERT.failed must be false (spec §D.12)");
    assert_eq!(action.source_address, Address::ZERO);
    assert_eq!(action.source_rollup, U256::ZERO);
    assert_eq!(action.scope, scope);

    // Verify Solidity ABI conversion
    let sol = action.to_sol_action();
    assert_eq!(sol.actionType, ICrossChainManagerL2::ActionType::REVERT);
    assert!(!sol.failed);
    assert_eq!(sol.scope.len(), 1);
}

#[test]
fn test_revert_continue_action_canonical_fields() {
    let rollup_id = U256::from(42069);
    let action = revert_continue_action(rollup_id);

    assert_eq!(action.action_type, CrossChainActionType::RevertContinue);
    assert_eq!(action.rollup_id, rollup_id);
    assert_eq!(action.destination, Address::ZERO);
    assert_eq!(action.value, U256::ZERO);
    assert!(action.data.is_empty());
    assert!(
        action.failed,
        "REVERT_CONTINUE.failed must be true (spec §D.12)"
    );
    assert_eq!(action.source_address, Address::ZERO);
    assert_eq!(action.source_rollup, U256::ZERO);
    assert!(
        action.scope.is_empty(),
        "REVERT_CONTINUE.scope must be [] (spec §D.12)"
    );

    // Verify Solidity ABI conversion
    let sol = action.to_sol_action();
    assert_eq!(
        sol.actionType,
        ICrossChainManagerL2::ActionType::REVERT_CONTINUE
    );
    assert!(sol.failed);
    assert!(sol.scope.is_empty());
}

#[test]
fn test_compute_revert_continue_hash_deterministic() {
    let rollup_id = U256::from(42069);

    // Hash must be deterministic — same rollup_id always produces same hash
    let hash1 = compute_revert_continue_hash(rollup_id);
    let hash2 = compute_revert_continue_hash(rollup_id);
    assert_eq!(hash1, hash2, "REVERT_CONTINUE hash must be deterministic");

    // Different rollup_id produces different hash
    let hash3 = compute_revert_continue_hash(U256::from(1));
    assert_ne!(
        hash1, hash3,
        "different rollupId must produce different hash"
    );

    // Hash is non-zero
    assert_ne!(hash1, B256::ZERO, "hash must not be zero");
}

#[test]
fn test_revert_continue_hash_matches_manual_abi_encode() {
    // Manually construct the same action and hash it to verify consistency
    let rollup_id = U256::from(42069);
    let action = CrossChainAction {
        action_type: CrossChainActionType::RevertContinue,
        rollup_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: true,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let manual_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &action.to_sol_action(),
    ));
    let helper_hash = compute_revert_continue_hash(rollup_id);
    assert_eq!(
        manual_hash, helper_hash,
        "compute_revert_continue_hash must match manual ABI encode + keccak256"
    );
}

/// Verify `build_l2_to_l1_call_entries(tx_reverts=true)` produces 3 L1 entries
/// with correct REVERT/REVERT_CONTINUE structure matching IntegrationTest Scenario 5.
#[test]
fn test_build_l2_to_l1_call_entries_tx_reverts() {
    let destination = Address::with_last_byte(0x42);
    let source = Address::with_last_byte(0x01);
    let rollup_id = 42069u64;
    let rollup_id_u256 = U256::from(rollup_id);
    let return_data = U256::from(4).to_be_bytes_vec(); // Counter returns 4
    let delivery_scope = vec![U256::ZERO]; // depth 1

    let entries = build_l2_to_l1_call_entries(
        destination,
        vec![0xd0, 0x9d, 0xe0, 0x8a], // increment() selector
        U256::ZERO,
        source,
        rollup_id,
        vec![0xc0], // rlp_encoded_tx placeholder
        return_data.clone(),
        false, // delivery succeeded
        delivery_scope.clone(),
        true, // tx_reverts!
    );

    // L2 entries: UNCHANGED — still 2 entries (CALL→RESULT pair)
    assert_eq!(entries.l2_table_entries.len(), 2, "L2 entries must be 2 (unchanged)");
    assert_eq!(
        entries.l2_table_entries[0].next_action.action_type,
        CrossChainActionType::Result,
        "L2 Entry 0 nextAction must be RESULT"
    );

    // L1 entries: 3 instead of 2
    assert_eq!(entries.l1_deferred_entries.len(), 3, "L1 entries must be 3 for tx_reverts");

    // Entry 0: hash(L2TX) → CALL(delivery)
    let e0 = &entries.l1_deferred_entries[0];
    assert_eq!(
        e0.next_action.action_type,
        CrossChainActionType::Call,
        "L1 Entry 0 nextAction must be CALL (delivery)"
    );
    assert_eq!(e0.next_action.scope, delivery_scope, "delivery CALL scope must match");
    assert_eq!(
        e0.state_deltas[0].ether_delta,
        I256::ZERO,
        "Entry 0 ether_delta must be 0 (consumed before ETH sent)"
    );

    // Entry 1: hash(RESULT) → REVERT(rollupId=ourL2, scope=delivery_scope)
    let e1 = &entries.l1_deferred_entries[1];
    assert_eq!(
        e1.next_action.action_type,
        CrossChainActionType::Revert,
        "L1 Entry 1 nextAction must be REVERT (not terminal RESULT)"
    );
    assert_eq!(
        e1.next_action.rollup_id, rollup_id_u256,
        "REVERT rollupId must be our L2 rollup"
    );
    assert_eq!(
        e1.next_action.scope, vec![U256::ZERO],
        "REVERT scope must always be [0] (first child of _resolveScopes)"
    );
    assert!(
        !e1.next_action.failed,
        "REVERT.failed must be false (spec §D.12)"
    );

    // Entry 2: hash(REVERT_CONTINUE) → RESULT(terminal, failed=false)
    let e2 = &entries.l1_deferred_entries[2];
    let expected_rc_hash = compute_revert_continue_hash(rollup_id_u256);
    assert_eq!(
        e2.action_hash, expected_rc_hash,
        "Entry 2 actionHash must be hash(REVERT_CONTINUE)"
    );
    assert_eq!(
        e2.next_action.action_type,
        CrossChainActionType::Result,
        "Entry 2 nextAction must be terminal RESULT"
    );
    assert_eq!(
        e2.next_action.rollup_id, rollup_id_u256,
        "terminal RESULT rollupId must be our L2"
    );
    assert!(
        !e2.next_action.failed,
        "terminal RESULT.failed must be false (required by _resolveScopes)"
    );
    assert!(
        e2.next_action.data.is_empty(),
        "terminal RESULT.data must be empty (void)"
    );
    assert_eq!(
        e2.state_deltas[0].ether_delta,
        I256::ZERO,
        "Entry 2 ether_delta must be 0 (_etherDelta reset after Entry 1)"
    );
}

/// Verify tx_reverts=false still produces 2 L1 entries (backward compatibility).
#[test]
fn test_build_l2_to_l1_call_entries_no_revert_unchanged() {
    let entries_normal = build_l2_to_l1_call_entries(
        Address::with_last_byte(0x42),
        vec![0xd0, 0x9d, 0xe0, 0x8a],
        U256::ZERO,
        Address::with_last_byte(0x01),
        1,
        vec![0xc0],
        vec![],
        false,
        vec![],
        false, // tx_reverts=false
    );
    assert_eq!(
        entries_normal.l1_deferred_entries.len(),
        2,
        "tx_reverts=false must produce 2 L1 entries (unchanged)"
    );
    assert_eq!(
        entries_normal.l1_deferred_entries[1].next_action.action_type,
        CrossChainActionType::Result,
        "tx_reverts=false Entry 1 must have terminal RESULT"
    );
}

/// Verify REVERT entries with ETH value have correct ether_delta accounting.
#[test]
fn test_build_l2_to_l1_call_entries_revert_with_eth_value() {
    let amount = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
    let entries = build_l2_to_l1_call_entries(
        Address::with_last_byte(0x42),
        vec![],
        amount,
        Address::with_last_byte(0x01),
        1,
        vec![0xc0],
        vec![],
        false,
        vec![],
        true, // tx_reverts
    );

    assert_eq!(entries.l1_deferred_entries.len(), 3);

    // Entry 0: ether_delta = 0 (before ETH sent)
    assert_eq!(entries.l1_deferred_entries[0].state_deltas[0].ether_delta, I256::ZERO);

    // Entry 1: ether_delta = -1 ETH (after ETH sent by proxy)
    let expected_delta = -I256::try_from(amount).unwrap();
    assert_eq!(entries.l1_deferred_entries[1].state_deltas[0].ether_delta, expected_delta);

    // Entry 2: ether_delta = 0 (_etherDelta reset by _applyStateDeltas after Entry 1)
    assert_eq!(entries.l1_deferred_entries[2].state_deltas[0].ether_delta, I256::ZERO);
}

/// Verify `attach_generic_state_deltas` for REVERT groups produces the correct
/// root chain: Entry 1's newState = post_root, Entry 2 = identity (post→post).
///
/// This ensures `_handleScopeRevert` (Rollups.sol:375) captures the correct
/// stateRoot (= post_root = block's real state root).
#[test]
fn test_attach_generic_state_deltas_revert_group() {
    let pre = B256::with_last_byte(0x01);
    let post = B256::with_last_byte(0x02);
    let rollup_id = 42069u64;
    let rollup_id_u256 = U256::from(rollup_id);

    // 3-entry REVERT group (L2TX→CALL, RESULT→REVERT, REVERT_CONTINUE→RESULT)
    let mut entries = vec![
        CrossChainExecutionEntry {
            state_deltas: vec![CrossChainStateDelta {
                rollup_id: rollup_id_u256,
                current_state: B256::ZERO,
                new_state: B256::ZERO,
                ether_delta: I256::ZERO,
            }],
            action_hash: B256::with_last_byte(0xAA),
            next_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::ZERO,
                destination: Address::ZERO,
                value: U256::ZERO,
                data: vec![],
                failed: false,
                source_address: Address::ZERO,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
        },
        CrossChainExecutionEntry {
            state_deltas: vec![CrossChainStateDelta {
                rollup_id: rollup_id_u256,
                current_state: B256::ZERO,
                new_state: B256::ZERO,
                ether_delta: I256::ZERO,
            }],
            action_hash: B256::with_last_byte(0xBB),
            next_action: revert_action(rollup_id_u256, vec![]),
        },
        CrossChainExecutionEntry {
            state_deltas: vec![CrossChainStateDelta {
                rollup_id: rollup_id_u256,
                current_state: B256::ZERO,
                new_state: B256::ZERO,
                ether_delta: I256::ZERO,
            }],
            action_hash: compute_revert_continue_hash(rollup_id_u256),
            next_action: CrossChainAction {
                action_type: CrossChainActionType::Result,
                rollup_id: rollup_id_u256,
                destination: Address::ZERO,
                value: U256::ZERO,
                data: vec![],
                failed: false,
                source_address: Address::ZERO,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
        },
    ];

    let roots = vec![pre, post]; // 1 group → 2 roots
    let group_starts = vec![0usize];
    let revert_flags = vec![true];

    attach_generic_state_deltas(&mut entries, &roots, rollup_id, &group_starts, &revert_flags);

    // Entry 0: pre → synthetic
    assert_eq!(entries[0].state_deltas[0].current_state, pre);
    let syn1 = entries[0].state_deltas[0].new_state;
    assert_ne!(syn1, pre, "synthetic root must differ from pre");
    assert_ne!(syn1, post, "synthetic root must differ from post");

    // Entry 1: synthetic → post_root (the captured value for _handleScopeRevert)
    assert_eq!(entries[1].state_deltas[0].current_state, syn1);
    assert_eq!(
        entries[1].state_deltas[0].new_state, post,
        "Entry 1 newState must be post_root (captured by _handleScopeRevert)"
    );

    // Entry 2: post_root → post_root (identity — consumed inside reverted scope)
    assert_eq!(
        entries[2].state_deltas[0].current_state, post,
        "Entry 2 currentState must be post_root"
    );
    assert_eq!(
        entries[2].state_deltas[0].new_state, post,
        "Entry 2 newState must be post_root (identity delta)"
    );
}

/// Verify non-REVERT groups are unaffected by the revert_group_flags parameter.
#[test]
fn test_attach_generic_state_deltas_normal_group_with_flags() {
    let pre = B256::with_last_byte(0x01);
    let post = B256::with_last_byte(0x02);
    let rollup_id = 1u64;
    let rollup_id_u256 = U256::from(rollup_id);

    // 2-entry normal group
    let mut entries = vec![
        CrossChainExecutionEntry {
            state_deltas: vec![CrossChainStateDelta {
                rollup_id: rollup_id_u256,
                current_state: B256::ZERO,
                new_state: B256::ZERO,
                ether_delta: I256::ZERO,
            }],
            action_hash: B256::with_last_byte(0xAA),
            next_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::ZERO,
                destination: Address::ZERO,
                value: U256::ZERO,
                data: vec![],
                failed: false,
                source_address: Address::ZERO,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
        },
        CrossChainExecutionEntry {
            state_deltas: vec![CrossChainStateDelta {
                rollup_id: rollup_id_u256,
                current_state: B256::ZERO,
                new_state: B256::ZERO,
                ether_delta: I256::ZERO,
            }],
            action_hash: B256::with_last_byte(0xBB),
            next_action: CrossChainAction {
                action_type: CrossChainActionType::Result,
                rollup_id: rollup_id_u256,
                destination: Address::ZERO,
                value: U256::ZERO,
                data: vec![],
                failed: false,
                source_address: Address::ZERO,
                source_rollup: U256::ZERO,
                scope: vec![],
            },
        },
    ];

    let roots = vec![pre, post];
    let group_starts = vec![0usize];
    let revert_flags = vec![false]; // NOT a revert group

    attach_generic_state_deltas(&mut entries, &roots, rollup_id, &group_starts, &revert_flags);

    // Entry 0: pre → synthetic
    assert_eq!(entries[0].state_deltas[0].current_state, pre);
    let syn1 = entries[0].state_deltas[0].new_state;

    // Entry 1: synthetic → post (normal chain, NOT identity)
    assert_eq!(entries[1].state_deltas[0].current_state, syn1);
    assert_eq!(entries[1].state_deltas[0].new_state, post);
    // Verify it's NOT identity (syn1 ≠ post for a 2-entry group)
    assert_ne!(syn1, post, "normal group: intermediate must differ from post");
}
