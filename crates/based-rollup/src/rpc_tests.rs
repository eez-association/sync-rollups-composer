use super::*;
use crate::cross_chain::{CrossChainStateDelta, RollupId, ScopePath};
use alloy_primitives::address;

#[test]
fn test_entry_to_serializable_roundtrip() {
    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: RollupId::new(U256::from(1)),
            current_state: B256::with_last_byte(0xAA),
            new_state: B256::with_last_byte(0xBB),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::with_last_byte(0xCC),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: RollupId::new(U256::from(1)),
            destination: address!("0x1111111111111111111111111111111111111111"),
            value: U256::ZERO,
            data: vec![0xDE, 0xAD],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: RollupId::new(U256::from(2)),
            scope: ScopePath::from_parts(vec![U256::from(1), U256::from(2)]),
        },
    };

    let serializable = entry_to_serializable(&entry);
    assert_eq!(serializable.state_deltas.len(), 1);
    assert_eq!(serializable.action_hash, entry.action_hash);
    assert_eq!(serializable.next_action.action_type, "RESULT");
    assert_eq!(serializable.next_action.scope.len(), 2);
}

#[test]
fn test_different_action_types_produce_different_hashes() {
    let base = ActionParams {
        action_type: "CALL".to_string(),
        rollup_id: U256::from(1),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: Bytes::default(),
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };

    let call_hash = compute_action_hash_from_params(&base).expect("valid");
    let l2tx_hash = compute_action_hash_from_params(&ActionParams {
        action_type: "L2TX".to_string(),
        ..base.clone()
    })
    .expect("valid");

    assert_ne!(call_hash, l2tx_hash);
}

#[test]
fn test_compute_action_hash_unknown_type_returns_error() {
    let unknown = ActionParams {
        action_type: "UNKNOWN".to_string(),
        rollup_id: U256::from(1),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: Bytes::default(),
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let result = compute_action_hash_from_params(&unknown);
    assert!(result.is_err(), "unknown action type should return error");
    assert!(
        result.unwrap_err().contains("unknown action type"),
        "error message should mention 'unknown action type'"
    );
}

// ──────────────────────────────────────────────────────────────────
//  Tests for L1 forward tx queue (atomic cross-chain submission)
// ──────────────────────────────────────────────────────────────────

#[test]
fn test_queued_cross_chain_call_push_drain_and_sort() {
    // Simulate the unified queue lifecycle: RPC pushes, driver sorts + drains.
    let queue: Arc<std::sync::Mutex<Vec<QueuedCrossChainCall>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let make_entry = |hash_byte: u8| CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: B256::with_last_byte(hash_byte),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: RollupId::new(U256::from(1)),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: ScopePath::root(),
        },
    };

    // RPC side: push 3 calls with different gas prices
    {
        let mut q = queue.lock().unwrap();
        q.push(QueuedCrossChainCall {
            call_entry: make_entry(0x01),
            result_entry: make_entry(0x02),
            effective_gas_price: 100, // lowest
            raw_l1_tx: Bytes::from(vec![0x01]),
            extra_l2_entries: vec![],
            l1_entries: vec![],
            tx_reverts: false,
            l1_independent_entries: false,
        });
        q.push(QueuedCrossChainCall {
            call_entry: make_entry(0x03),
            result_entry: make_entry(0x04),
            effective_gas_price: 1000, // highest
            raw_l1_tx: Bytes::from(vec![0x02]),
            extra_l2_entries: vec![],
            l1_entries: vec![],
            tx_reverts: false,
            l1_independent_entries: false,
        });
        q.push(QueuedCrossChainCall {
            call_entry: make_entry(0x05),
            result_entry: make_entry(0x06),
            effective_gas_price: 500, // middle
            raw_l1_tx: Bytes::from(vec![0x03]),
            extra_l2_entries: vec![],
            l1_entries: vec![],
            tx_reverts: false,
            l1_independent_entries: false,
        });
    }

    // Driver side: drain and sort by gas price descending
    let mut drained: Vec<QueuedCrossChainCall> = {
        let mut q = queue.lock().unwrap();
        q.drain(..).collect()
    };
    drained.sort_by(|a, b| b.effective_gas_price.cmp(&a.effective_gas_price));

    assert_eq!(drained.len(), 3);
    assert_eq!(drained[0].effective_gas_price, 1000); // highest first
    assert_eq!(drained[1].effective_gas_price, 500); // middle
    assert_eq!(drained[2].effective_gas_price, 100); // lowest last

    // Queue is now empty
    assert!(queue.lock().unwrap().is_empty());
}

// ──────────────────────────────────────────────────────────────────
//  QA iteration 45 — RPC namespace simulation safety
// ──────────────────────────────────────────────────────────────────
