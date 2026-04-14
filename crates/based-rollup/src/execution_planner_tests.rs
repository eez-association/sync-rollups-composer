use super::*;

#[test]
fn test_compute_l2tx_action_hash_varies_with_rollup_id() {
    let hash1 = compute_l2tx_action_hash(1, &[0x01]);
    let hash2 = compute_l2tx_action_hash(2, &[0x01]);
    assert_ne!(hash1, hash2);
}

#[test]
fn test_compute_l2tx_action_hash_varies_with_data() {
    let hash1 = compute_l2tx_action_hash(1, &[0x01]);
    let hash2 = compute_l2tx_action_hash(1, &[0x02]);
    assert_ne!(hash1, hash2);
}

#[test]
fn test_build_entries_for_block_empty_transactions() {
    let entries = build_entries_for_block(1, B256::ZERO, B256::with_last_byte(1), &[]);
    assert!(entries.is_empty());
}

#[test]
fn test_build_entries_empty_tx_list() {
    let pre_root = B256::with_last_byte(0xAA);
    let post_root = B256::with_last_byte(0xBB);
    let entries = build_entries_for_block(42, pre_root, post_root, &[]);
    assert!(
        entries.is_empty(),
        "empty tx list should produce no entries"
    );
}

#[test]
fn test_build_entries_for_block_nonzero_rollup_id() {
    // Build a minimal signed transaction to pass to build_entries_for_block
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 0,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: TxKind::Call(Address::with_last_byte(0x01)),
        value: U256::ZERO,
        input: Default::default(),
    };
    let signed_legacy = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed_legacy);

    let rollup_id = 42u64;
    let pre_root = B256::with_last_byte(0xAA);
    let post_root = B256::with_last_byte(0xBB);
    let entries = build_entries_for_block(rollup_id, pre_root, post_root, &[signed_tx]);

    assert_eq!(entries.len(), 1, "one tx should produce one entry");
    let entry = &entries[0];

    // rollup_id should propagate into state_deltas and next_action
    assert_eq!(entry.state_deltas.len(), 1);
    assert_eq!(
        entry.state_deltas[0].rollup_id,
        RollupId::new(U256::from(42)),
        "rollup_id should propagate to state delta"
    );
    assert_eq!(
        entry.next_action.rollup_id,
        RollupId::new(U256::from(42)),
        "rollup_id should propagate to next_action"
    );
}

#[test]
fn test_build_entries_for_block_state_roots_set_correctly() {
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 0,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: TxKind::Call(Address::with_last_byte(0x01)),
        value: U256::ZERO,
        input: Default::default(),
    };
    let signed_legacy = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed_legacy);

    let pre_root = B256::with_last_byte(0xAA);
    let post_root = B256::with_last_byte(0xBB);
    let entries = build_entries_for_block(1, pre_root, post_root, &[signed_tx]);

    assert_eq!(entries.len(), 1);
    let delta = &entries[0].state_deltas[0];
    assert_eq!(
        delta.current_state, pre_root,
        "current_state should match pre_state_root"
    );
    assert_eq!(
        delta.new_state, post_root,
        "new_state should match post_state_root"
    );
    assert_eq!(
        delta.ether_delta,
        I256::ZERO,
        "ether_delta should be zero for standard block entries"
    );
}

#[test]
fn test_build_entries_for_block_next_action_is_result() {
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 0,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: TxKind::Call(Address::with_last_byte(0x01)),
        value: U256::ZERO,
        input: Default::default(),
    };
    let signed_legacy = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed_legacy);

    let entries = build_entries_for_block(1, B256::ZERO, B256::with_last_byte(0x01), &[signed_tx]);

    assert_eq!(entries.len(), 1);
    let action = &entries[0].next_action;
    assert_eq!(
        action.action_type,
        CrossChainActionType::Result,
        "next_action should be Result type"
    );
    assert!(!action.failed, "next_action should not be failed");
    assert!(action.data.is_empty(), "next_action data should be empty");
    assert!(action.scope.is_empty(), "next_action scope should be empty");
    assert_eq!(action.destination, Address::ZERO);
    assert_eq!(action.value, U256::ZERO);
}

#[test]
fn test_build_entries_action_hash_varies_with_rollup_id() {
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 0,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: TxKind::Call(Address::with_last_byte(0x01)),
        value: U256::ZERO,
        input: Default::default(),
    };
    let signed_legacy = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed_legacy);

    let entries_r1 = build_entries_for_block(
        1,
        B256::ZERO,
        B256::with_last_byte(0x01),
        std::slice::from_ref(&signed_tx),
    );
    let entries_r2 = build_entries_for_block(
        2,
        B256::ZERO,
        B256::with_last_byte(0x01),
        std::slice::from_ref(&signed_tx),
    );

    assert_ne!(
        entries_r1[0].action_hash, entries_r2[0].action_hash,
        "different rollup_ids should produce different action hashes"
    );
}

#[test]
fn test_build_entries_empty_transactions_returns_empty_vec() {
    // Verify the exact return: an empty Vec, not a vec with a zero-delta entry
    let entries = build_entries_for_block(
        99,
        B256::with_last_byte(0x01),
        B256::with_last_byte(0x02),
        &[],
    );
    assert!(entries.is_empty());
    assert_eq!(entries.len(), 0);
}

#[test]
fn test_build_entries_preserves_transaction_order() {
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    // Create two distinct transactions (different nonces)
    let make_tx = |nonce: u64| -> reth_ethereum_primitives::TransactionSigned {
        let tx = TxLegacy {
            chain_id: Some(42069),
            nonce,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::with_last_byte(0x01)),
            value: U256::ZERO,
            input: Default::default(),
        };
        let signed = alloy_consensus::Signed::new_unhashed(
            tx,
            alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
        );
        reth_ethereum_primitives::TransactionSigned::Legacy(signed)
    };

    let tx_a = make_tx(0);
    let tx_b = make_tx(1);
    let tx_c = make_tx(2);

    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);

    // build_entries_for_block produces one entry per block covering all txs.
    // The action hash must be deterministic for a given tx ordering.
    let entries_abc =
        build_entries_for_block(1, pre, post, &[tx_a.clone(), tx_b.clone(), tx_c.clone()]);
    let entries_abc_again =
        build_entries_for_block(1, pre, post, &[tx_a.clone(), tx_b.clone(), tx_c.clone()]);
    assert_eq!(
        entries_abc[0].action_hash, entries_abc_again[0].action_hash,
        "same tx order should produce identical action hash"
    );

    // Different ordering should produce a different action hash
    let entries_cba = build_entries_for_block(1, pre, post, &[tx_c, tx_b, tx_a]);
    assert_ne!(
        entries_abc[0].action_hash, entries_cba[0].action_hash,
        "reversed tx order should produce a different action hash"
    );
}

#[test]
fn test_build_entries_same_pre_post_root() {
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 0,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: TxKind::Call(Address::with_last_byte(0x01)),
        value: U256::ZERO,
        input: Default::default(),
    };
    let signed_legacy = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed_legacy);

    // Same root for pre and post (no-op transaction scenario)
    let root = B256::with_last_byte(0xAA);
    let entries = build_entries_for_block(1, root, root, &[signed_tx]);

    assert_eq!(entries.len(), 1);
    let delta = &entries[0].state_deltas[0];
    assert_eq!(delta.current_state, root);
    assert_eq!(delta.new_state, root);
    assert_eq!(
        delta.current_state, delta.new_state,
        "pre and post state roots should both equal the same value"
    );
    // action_hash should still be non-zero (it hashes the tx data, not roots)
    assert_ne!(entries[0].action_hash, crate::cross_chain::ActionHash::ZERO);
}

// ── build_entries_from_encoded tests ──

#[test]
fn test_build_entries_from_encoded_produces_valid_entry() {
    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);
    let rlp_data = vec![0xc0, 0x01, 0x02]; // arbitrary non-empty RLP

    let entries = build_entries_from_encoded(42, pre, post, &rlp_data);
    assert_eq!(entries.len(), 1);

    let entry = &entries[0];
    assert_eq!(entry.state_deltas.len(), 1);
    assert_eq!(
        entry.state_deltas[0].rollup_id,
        RollupId::new(U256::from(42))
    );
    assert_eq!(entry.state_deltas[0].current_state, pre);
    assert_eq!(entry.state_deltas[0].new_state, post);
    assert_ne!(entry.action_hash, crate::cross_chain::ActionHash::ZERO);
    assert_eq!(entry.next_action.action_type, CrossChainActionType::Result);
}

#[test]
fn test_build_entries_from_encoded_matches_build_entries_for_block() {
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    // Create a transaction
    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 0,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: TxKind::Call(Address::with_last_byte(0x01)),
        value: U256::ZERO,
        input: Default::default(),
    };
    let signed_legacy = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed_legacy);

    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);

    // Build entries via both paths
    let entries_from_tx = build_entries_for_block(1, pre, post, std::slice::from_ref(&signed_tx));

    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(&[signed_tx], &mut rlp_buf);
    let entries_from_encoded = build_entries_from_encoded(1, pre, post, &rlp_buf);

    // Both should produce the same action hash
    assert_eq!(entries_from_tx.len(), 1);
    assert_eq!(entries_from_encoded.len(), 1);
    assert_eq!(
        entries_from_tx[0].action_hash, entries_from_encoded[0].action_hash,
        "both paths should produce the same action hash"
    );
    assert_eq!(
        entries_from_tx[0].state_deltas[0],
        entries_from_encoded[0].state_deltas[0]
    );
}

#[test]
fn test_build_entries_from_encoded_large_rlp_100_plus_txs() {
    // Simulate a block with 100+ transactions encoded as RLP.
    // The RLP blob is hashed (keccak256), so even very large inputs
    // produce a fixed-size action_hash without allocation issues.
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    let txs: Vec<reth_ethereum_primitives::TransactionSigned> = (0..150)
        .map(|nonce| {
            let tx = TxLegacy {
                chain_id: Some(42069),
                nonce,
                gas_price: 1_000_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: U256::ZERO,
                input: Default::default(),
            };
            let signed = alloy_consensus::Signed::new_unhashed(
                tx,
                alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
            );
            reth_ethereum_primitives::TransactionSigned::Legacy(signed)
        })
        .collect();

    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(&txs, &mut rlp_buf);

    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);

    // build_entries_from_encoded should handle large RLP without issue
    let entries = build_entries_from_encoded(1, pre, post, &rlp_buf);
    assert_eq!(
        entries.len(),
        1,
        "should produce exactly one entry per block"
    );
    assert_ne!(entries[0].action_hash, crate::cross_chain::ActionHash::ZERO);

    // Verify consistency with build_entries_for_block
    let entries_from_txs = build_entries_for_block(1, pre, post, &txs);
    assert_eq!(
        entries[0].action_hash, entries_from_txs[0].action_hash,
        "both paths must produce identical action hashes for 150 txs"
    );
}

/// Full pipeline roundtrip test: validates that entries built by the builder
/// survive the entire journey through encoding, L1 event parsing, and L2
/// protocol transaction encoding — producing identical entries at every step.
///
/// Pipeline: build_entries_from_encoded → encode_post_batch_calldata →
///           (simulate BatchPosted event) → parse_batch_posted_logs →
///           encode_load_execution_table_calldata → decode
#[test]
fn test_e2e_pipeline_roundtrip_build_to_load_execution_table() {
    use crate::cross_chain::{
        ICrossChainManagerL2, encode_load_execution_table_calldata, encode_post_batch_calldata,
        parse_batch_posted_logs,
    };
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Bytes, LogData, TxKind};
    use alloy_rpc_types::Log;
    use alloy_sol_types::{SolCall, SolEvent};

    // Step 1: Create realistic transaction data
    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 42,
        gas_price: 1_000_000_000,
        gas_limit: 100_000,
        to: TxKind::Call(Address::with_last_byte(0xBB)),
        value: U256::from(1_000_000),
        input: alloy_primitives::Bytes::from(vec![0xDE, 0xAD, 0xBE, 0xEF]),
    };
    let signed = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed);

    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(&[signed_tx], &mut rlp_buf);

    let pre_root = B256::with_last_byte(0xAA);
    let post_root = B256::with_last_byte(0xBB);
    let rollup_id: u64 = 1;

    // Step 2: Builder builds entries from encoded transactions
    let builder_entries = build_entries_from_encoded(rollup_id, pre_root, post_root, &rlp_buf);
    assert_eq!(builder_entries.len(), 1);
    let original_entry = &builder_entries[0];
    assert_ne!(
        original_entry.action_hash,
        crate::cross_chain::ActionHash::ZERO
    );
    assert_eq!(original_entry.state_deltas[0].current_state, pre_root);
    assert_eq!(original_entry.state_deltas[0].new_state, post_root);

    // Step 3: Builder encodes postBatch calldata for L1 submission
    let post_batch_calldata =
        encode_post_batch_calldata(&builder_entries, Bytes::default(), Bytes::default());
    assert!(!post_batch_calldata.is_empty());

    // Decode the postBatch calldata to extract the entries (simulating
    // what the L1 contract sees)
    let decoded_post_batch = ICrossChainManagerL2::postBatchCall::abi_decode(&post_batch_calldata)
        .expect("postBatch calldata should decode");
    assert_eq!(decoded_post_batch.entries.len(), 1);

    // Step 4: Simulate L1 emitting a BatchPosted event with these entries
    let batch_posted_event = ICrossChainManagerL2::BatchPosted {
        entries: decoded_post_batch.entries.clone(),
        publicInputsHash: B256::ZERO,
    };
    let log_data = batch_posted_event.encode_log_data();
    let mock_log = Log {
        inner: alloy_primitives::Log {
            address: Address::with_last_byte(0xFF),
            data: LogData::new(log_data.topics().to_vec(), log_data.data.clone())
                .expect("valid log data"),
        },
        block_hash: Some(B256::with_last_byte(0x01)),
        block_number: Some(100),
        block_timestamp: Some(1000),
        transaction_hash: Some(B256::with_last_byte(0x02)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    };

    // Step 5: Fullnode parses the BatchPosted event
    let derived = parse_batch_posted_logs(&[mock_log], U256::from(rollup_id));
    assert_eq!(derived.len(), 1, "fullnode should derive exactly one entry");
    assert_eq!(derived[0].l1_block_number, 100);

    let derived_entry = &derived[0].entry;

    // Step 6: Verify the derived entry matches the original builder entry
    assert_eq!(
        derived_entry.action_hash, original_entry.action_hash,
        "action_hash must survive L1 roundtrip"
    );
    assert_eq!(
        derived_entry.state_deltas.len(),
        original_entry.state_deltas.len()
    );
    assert_eq!(
        derived_entry.state_deltas[0].rollup_id,
        original_entry.state_deltas[0].rollup_id
    );
    assert_eq!(
        derived_entry.state_deltas[0].current_state,
        original_entry.state_deltas[0].current_state
    );
    assert_eq!(
        derived_entry.state_deltas[0].new_state,
        original_entry.state_deltas[0].new_state
    );
    assert_eq!(
        derived_entry.next_action, original_entry.next_action,
        "next_action must survive L1 roundtrip"
    );

    // Step 7: Fullnode encodes loadExecutionTable calldata for builder protocol transaction
    let load_calldata = encode_load_execution_table_calldata(std::slice::from_ref(derived_entry));
    assert!(!load_calldata.is_empty());

    // Step 8: Decode the loadExecutionTable calldata (simulating what the
    // L2 contract receives)
    let decoded_load = ICrossChainManagerL2::loadExecutionTableCall::abi_decode(&load_calldata)
        .expect("loadExecutionTable calldata should decode");
    assert_eq!(decoded_load.entries.len(), 1);

    // Final verification: the entry that arrives at the L2 contract is
    // identical to what the builder originally created.
    // Compare at the Solidity ABI level since to_sol/from_sol are private.
    let final_sol = &decoded_load.entries[0];
    assert_eq!(
        B256::from(final_sol.actionHash),
        original_entry.action_hash.as_b256(),
        "action_hash must survive full pipeline"
    );
    assert_eq!(final_sol.stateDeltas.len(), 1);
    assert_eq!(
        B256::from(final_sol.stateDeltas[0].currentState),
        pre_root,
        "pre_state_root must survive full pipeline"
    );
    assert_eq!(
        B256::from(final_sol.stateDeltas[0].newState),
        post_root,
        "post_state_root must survive full pipeline"
    );
    assert_eq!(
        final_sol.stateDeltas[0].rollupId,
        U256::from(rollup_id),
        "rollup_id must survive full pipeline"
    );
    // Verify the loadExecutionTable calldata round-trips identically
    // by re-encoding the derived entry and comparing bytes
    let re_encoded = encode_load_execution_table_calldata(std::slice::from_ref(derived_entry));
    assert_eq!(
        load_calldata, re_encoded,
        "re-encoding derived entry must produce identical calldata"
    );
}

/// Verify that the pipeline roundtrip works with multiple transactions
/// per block, ensuring the action hash correctly aggregates all txs.
#[test]
fn test_e2e_pipeline_multi_tx_block_action_hash_consistency() {
    use crate::cross_chain::{
        ICrossChainManagerL2, encode_load_execution_table_calldata, encode_post_batch_calldata,
        parse_batch_posted_logs,
    };
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Bytes, LogData, TxKind};
    use alloy_rpc_types::Log;
    use alloy_sol_types::{SolCall, SolEvent};

    // Create 5 transactions with different nonces
    let txs: Vec<reth_ethereum_primitives::TransactionSigned> = (0..5)
        .map(|nonce| {
            let tx = TxLegacy {
                chain_id: Some(42069),
                nonce,
                gas_price: 1_000_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(Address::with_last_byte((nonce + 1) as u8)),
                value: U256::from(nonce * 1000),
                input: Default::default(),
            };
            let signed = alloy_consensus::Signed::new_unhashed(
                tx,
                alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
            );
            reth_ethereum_primitives::TransactionSigned::Legacy(signed)
        })
        .collect();

    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(&txs, &mut rlp_buf);

    let pre = B256::with_last_byte(0x11);
    let post = B256::with_last_byte(0x22);
    let rollup_id: u64 = 2;

    // Build entries via both paths
    let entries_from_encoded = build_entries_from_encoded(rollup_id, pre, post, &rlp_buf);
    let entries_from_block = build_entries_for_block(rollup_id, pre, post, &txs);
    assert_eq!(
        entries_from_encoded[0].action_hash, entries_from_block[0].action_hash,
        "both builder paths must produce identical action hashes"
    );

    // Now push through L1 roundtrip
    let post_batch_calldata =
        encode_post_batch_calldata(&entries_from_encoded, Bytes::default(), Bytes::default());
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&post_batch_calldata).unwrap();
    let event = ICrossChainManagerL2::BatchPosted {
        entries: decoded.entries,
        publicInputsHash: B256::ZERO,
    };
    let log_data = event.encode_log_data();
    let mock_log = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(log_data.topics().to_vec(), log_data.data.clone())
                .expect("valid log data"),
        },
        block_hash: Some(B256::ZERO),
        block_number: Some(50),
        block_timestamp: Some(500),
        transaction_hash: Some(B256::ZERO),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    };

    let derived = parse_batch_posted_logs(&[mock_log], U256::from(rollup_id));
    assert_eq!(derived.len(), 1);

    // Encode for L2 and verify final consistency
    let load_calldata = encode_load_execution_table_calldata(&[derived[0].entry.clone()]);
    let final_decoded =
        ICrossChainManagerL2::loadExecutionTableCall::abi_decode(&load_calldata).unwrap();

    assert_eq!(
        B256::from(final_decoded.entries[0].actionHash),
        entries_from_encoded[0].action_hash.as_b256(),
        "action hash for 5-tx block must survive full pipeline"
    );
}

/// Property test: `compute_l2tx_action_hash` (paths 1 & 2) and
/// `compute_action_hash_from_params` (path 3) must produce identical
/// hashes when given the same inputs.
///
/// Path (1)/(2) hardcode actionType=L2TX with zeroed fields and pass
/// raw data bytes. Path (3) takes all fields explicitly via ActionParams.
/// This test verifies they agree for a variety of inputs.
#[test]
fn test_cross_path_action_hash_consistency_l2tx() {
    use crate::rpc::{ActionParams, compute_action_hash_from_params};

    // Test a variety of rollup_ids and data payloads
    let test_cases: Vec<(u64, Vec<u8>)> = vec![
        (1, vec![]),
        (1, vec![0x01]),
        (1, vec![0x01, 0x02, 0x03]),
        (0, vec![0xDE, 0xAD, 0xBE, 0xEF]),
        (42, vec![0xAB; 100]),
        (u64::MAX, vec![0xFF; 32]),
        (1, vec![0x00; 1024]),
        (999, (0u8..=255).collect()),
    ];

    for (rollup_id, data) in &test_cases {
        // Path (1)/(2): compute_l2tx_action_hash
        let hash_l2tx = compute_l2tx_action_hash(*rollup_id, data);

        // Path (3): compute_action_hash_from_params with matching fields
        let params = ActionParams {
            action_type: "L2TX".to_string(),
            rollup_id: U256::from(*rollup_id),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: alloy_primitives::Bytes::from(data.clone()),
            failed: false,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: vec![],
        };
        let hash_rpc =
            compute_action_hash_from_params(&params).expect("L2TX is a valid action type");

        assert_eq!(
            hash_l2tx.as_b256(),
            hash_rpc,
            "cross-path hash mismatch for rollup_id={}, data_len={}: \
             compute_l2tx_action_hash={:?} vs compute_action_hash_from_params={:?}",
            rollup_id,
            data.len(),
            hash_l2tx,
            hash_rpc,
        );
    }
}

/// Verify that paths (1) and (2) produce the same hash as path (3)
/// when using RLP-encoded transaction data from a real transaction.
#[test]
fn test_cross_path_action_hash_with_real_transaction() {
    use crate::rpc::{ActionParams, compute_action_hash_from_params};
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 7,
        gas_price: 2_000_000_000,
        gas_limit: 50_000,
        to: TxKind::Call(Address::with_last_byte(0x42)),
        value: U256::from(1_000_000_000_000_000_000u128),
        input: alloy_primitives::Bytes::from(vec![0xCA, 0xFE, 0xBA, 0xBE]),
    };
    let signed_legacy = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed_legacy);

    let rollup_id = 1u64;
    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);

    // Path (1): build_entries_for_block (encodes internally)
    let entries_path1 =
        build_entries_for_block(rollup_id, pre, post, std::slice::from_ref(&signed_tx));
    let hash_path1 = entries_path1[0].action_hash;

    // Path (2): build_entries_from_encoded (pre-encoded RLP)
    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(&[signed_tx], &mut rlp_buf);
    let entries_path2 = build_entries_from_encoded(rollup_id, pre, post, &rlp_buf);
    let hash_path2 = entries_path2[0].action_hash;

    // Path (3): compute_action_hash_from_params with RLP data
    let params = ActionParams {
        action_type: "L2TX".to_string(),
        rollup_id: U256::from(rollup_id),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: alloy_primitives::Bytes::from(rlp_buf.clone()),
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let hash_path3 = compute_action_hash_from_params(&params).expect("valid");

    assert_eq!(
        hash_path1, hash_path2,
        "path (1) build_entries_for_block != path (2) build_entries_from_encoded"
    );
    assert_eq!(
        hash_path1.as_b256(),
        hash_path3,
        "path (1) build_entries_for_block != path (3) compute_action_hash_from_params"
    );
    assert_eq!(
        hash_path2.as_b256(),
        hash_path3,
        "path (2) build_entries_from_encoded != path (3) compute_action_hash_from_params"
    );
}

/// Property test: for multiple transactions with varying nonces, all three
/// paths must agree on the action hash for the full transaction list.
#[test]
fn test_cross_path_action_hash_multi_tx_property() {
    use crate::rpc::{ActionParams, compute_action_hash_from_params};
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    for tx_count in [1, 2, 5, 10, 50] {
        let txs: Vec<reth_ethereum_primitives::TransactionSigned> = (0..tx_count)
            .map(|nonce| {
                let tx = TxLegacy {
                    chain_id: Some(42069),
                    nonce,
                    gas_price: 1_000_000_000,
                    gas_limit: 21_000,
                    to: TxKind::Call(Address::with_last_byte((nonce % 256) as u8)),
                    value: U256::from(nonce * 1000),
                    input: Default::default(),
                };
                let signed = alloy_consensus::Signed::new_unhashed(
                    tx,
                    alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
                );
                reth_ethereum_primitives::TransactionSigned::Legacy(signed)
            })
            .collect();

        let pre = B256::with_last_byte(0x11);
        let post = B256::with_last_byte(0x22);
        let rollup_id = 1u64;

        // Path (1)
        let entries1 = build_entries_for_block(rollup_id, pre, post, &txs);
        let hash1 = entries1[0].action_hash;

        // Path (2)
        let mut rlp_buf = Vec::new();
        alloy_rlp::encode_list(&txs, &mut rlp_buf);
        let entries2 = build_entries_from_encoded(rollup_id, pre, post, &rlp_buf);
        let hash2 = entries2[0].action_hash;

        // Path (3)
        let params = ActionParams {
            action_type: "L2TX".to_string(),
            rollup_id: U256::from(rollup_id),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: alloy_primitives::Bytes::from(rlp_buf),
            failed: false,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: vec![],
        };
        let hash3 = compute_action_hash_from_params(&params).expect("valid");

        assert_eq!(
            hash1, hash2,
            "paths (1) and (2) disagree for tx_count={}",
            tx_count
        );
        assert_eq!(
            hash1.as_b256(),
            hash3,
            "paths (1) and (3) disagree for tx_count={}",
            tx_count
        );
    }
}

// Verify that `build_entries_from_encoded` always sets `ether_delta` to zero
// for L2TX actions (no cross-chain ETH transfer in standard block entries).
// Verify that `build_entries_from_encoded` propagates rollup_id into both
// the state_delta and next_action fields.
// Verify that even with many transactions, both builder paths produce
// exactly one entry with exactly one state delta (not one per transaction).
// Document that `build_entries_for_block` always sets `failed=false` on the
// next_action. Individual transaction reverts are not tracked at the entry
// level — the entry covers the entire block's state transition. A block with
// reverted txs still produces a valid state transition (gas consumed, nonces
// incremented), so `failed` must be false.
// Verify that the pre/post state roots passed to `build_entries_from_encoded`
// are faithfully recorded in the state delta without transformation.
// This documents the contract between the driver (which sets pre=parent.state_root,
// post=sealed_block.state_root) and the execution planner.
//
// ── Iteration 80: Synchronous composability invariant tests ──
//
// Core guarantee: "any L2 transaction executed by the builder produces
// an execution entry that, when submitted to L1 and derived by a
// fullnode, results in the same state root."
//
// Since we can't run a full EVM here, we test the pipeline-level
// invariants that underpin this guarantee:
//   1. Determinism: same inputs → same entry (hash, state delta, action)
//   2. Round-trip fidelity: entry → ABI encode → L1 event → parse → identical entry
//   3. Path equivalence: build_entries_for_block ≡ build_entries_from_encoded
//   4. All 5 tx types produce structurally valid entries
//   5. Mixed blocks maintain invariants

/// Helper: create a signed legacy transaction for tests.
fn make_signed_tx(
    nonce: u64,
    to: alloy_primitives::TxKind,
    value: U256,
    input: Vec<u8>,
) -> reth_ethereum_primitives::TransactionSigned {
    use alloy_consensus::TxLegacy;

    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce,
        gas_price: 1_000_000_000,
        gas_limit: 100_000,
        to,
        value,
        input: alloy_primitives::Bytes::from(input),
    };
    let signed = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    reth_ethereum_primitives::TransactionSigned::Legacy(signed)
}

/// Helper: run the full pipeline roundtrip for a set of transactions and
/// verify the entry is identical at every stage.
///
/// Returns the original entry for further assertions.
fn assert_pipeline_invariant(
    rollup_id: u64,
    pre_root: B256,
    post_root: B256,
    txs: &[reth_ethereum_primitives::TransactionSigned],
) -> CrossChainExecutionEntry {
    use crate::cross_chain::{
        encode_load_execution_table_calldata, encode_post_batch_calldata, parse_batch_posted_logs,
    };
    use alloy_primitives::LogData;
    use alloy_rpc_types::Log;
    use alloy_sol_types::{SolCall, SolEvent};

    // Path 1: build from typed transactions
    let entries_typed = build_entries_for_block(rollup_id, pre_root, post_root, txs);
    assert_eq!(entries_typed.len(), 1, "should produce exactly one entry");

    // Path 2: build from pre-encoded RLP
    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(txs, &mut rlp_buf);
    let entries_encoded = build_entries_from_encoded(rollup_id, pre_root, post_root, &rlp_buf);
    assert_eq!(entries_encoded.len(), 1);

    // INVARIANT: both builder paths produce identical entries
    let original = &entries_typed[0];
    assert_eq!(
        original.action_hash, entries_encoded[0].action_hash,
        "path equivalence: action_hash must match"
    );
    assert_eq!(
        original.state_deltas, entries_encoded[0].state_deltas,
        "path equivalence: state_deltas must match"
    );
    assert_eq!(
        original.next_action, entries_encoded[0].next_action,
        "path equivalence: next_action must match"
    );

    // INVARIANT: determinism (build twice, get same result)
    let entries_again = build_entries_for_block(rollup_id, pre_root, post_root, txs);
    assert_eq!(
        original, &entries_again[0],
        "determinism: rebuilding must produce identical entry"
    );

    // INVARIANT: round-trip through L1 (postBatch → BatchPosted → parse)
    let post_batch_calldata = encode_post_batch_calldata(
        &entries_typed,
        alloy_primitives::Bytes::default(),
        alloy_primitives::Bytes::default(),
    );
    let decoded_post = ICrossChainManagerL2::postBatchCall::abi_decode(&post_batch_calldata)
        .expect("postBatch calldata should decode");
    let event = ICrossChainManagerL2::BatchPosted {
        entries: decoded_post.entries,
        publicInputsHash: B256::ZERO,
    };
    let log_data = event.encode_log_data();
    let mock_log = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(log_data.topics().to_vec(), log_data.data.clone())
                .expect("valid log data"),
        },
        block_hash: Some(B256::ZERO),
        block_number: Some(100),
        block_timestamp: Some(1000),
        transaction_hash: Some(B256::ZERO),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    };
    let derived = parse_batch_posted_logs(&[mock_log], U256::from(rollup_id));
    assert_eq!(derived.len(), 1, "fullnode must derive exactly one entry");

    let derived_entry = &derived[0].entry;
    assert_eq!(
        derived_entry.action_hash, original.action_hash,
        "L1 roundtrip: action_hash must survive"
    );
    assert_eq!(
        derived_entry.state_deltas, original.state_deltas,
        "L1 roundtrip: state_deltas must survive"
    );
    assert_eq!(
        derived_entry.next_action, original.next_action,
        "L1 roundtrip: next_action must survive"
    );

    // INVARIANT: loadExecutionTable encoding is deterministic
    let load_calldata = encode_load_execution_table_calldata(std::slice::from_ref(derived_entry));
    let re_encoded = encode_load_execution_table_calldata(std::slice::from_ref(original));
    assert_eq!(
        load_calldata, re_encoded,
        "loadExecutionTable encoding must be identical for builder vs derived entries"
    );

    original.clone()
}

/// Invariant test case 1: Simple ETH transfer.
///
/// A plain value transfer is the simplest state transition. The entry
/// must carry the correct pre/post state roots and survive the full
/// pipeline roundtrip.
#[test]
fn test_composability_invariant_eth_transfer() {
    use alloy_primitives::TxKind;

    let tx = make_signed_tx(
        0,
        TxKind::Call(Address::with_last_byte(0x42)),
        U256::from(1_000_000_000_000_000_000u128), // 1 ETH
        vec![],                                    // no calldata
    );

    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);
    let entry = assert_pipeline_invariant(1, pre, post, &[tx]);

    // ETH transfer specific: no return data, action is Result, not failed
    assert_eq!(entry.next_action.action_type, CrossChainActionType::Result);
    assert!(!entry.next_action.failed);
    assert!(entry.next_action.data.is_empty());
    assert_eq!(entry.state_deltas[0].ether_delta, I256::ZERO);
}

/// Invariant test case 2: Contract deployment.
///
/// A contract creation tx uses `TxKind::Create` and has bytecode as input.
/// The pipeline must handle Create transactions identically to Call txs.
#[test]
fn test_composability_invariant_contract_deployment() {
    use alloy_primitives::TxKind;

    // Simulate a contract deploy: Create with bytecode
    let bytecode = vec![
        0x60, 0x80, 0x60, 0x40, 0x52, // PUSH1 0x80 PUSH1 0x40 MSTORE
        0x34, 0x80, 0x15, // CALLVALUE DUP1 ISZERO
        0x60, 0x0f, // PUSH1 0x0f
        0x57, // JUMPI
        0x60, 0x00, // PUSH1 0x00
        0x80, 0xfd, // DUP1 REVERT
        0x5b, // JUMPDEST
        0x50, // POP
        0xfe, // INVALID (runtime code placeholder)
    ];
    let tx = make_signed_tx(0, TxKind::Create, U256::ZERO, bytecode);

    let pre = B256::with_last_byte(0x11);
    let post = B256::with_last_byte(0x22);
    let entry = assert_pipeline_invariant(1, pre, post, &[tx]);

    // Contract deploy specific: action hash covers the bytecode
    assert_ne!(entry.action_hash, crate::cross_chain::ActionHash::ZERO);
    assert_eq!(entry.state_deltas[0].current_state, pre);
    assert_eq!(entry.state_deltas[0].new_state, post);
}

/// Invariant test case 3: Contract call with storage writes.
///
/// A call to an existing contract with calldata (e.g., storage write).
/// The state root delta reflects the storage change.
#[test]
fn test_composability_invariant_contract_call_with_storage() {
    use alloy_primitives::TxKind;

    // Simulate: call to a contract with function selector + storage slot data
    // function setStorage(uint256 slot, uint256 value)
    let mut calldata = vec![0xDE, 0xAD, 0xBE, 0xEF]; // selector
    calldata.extend_from_slice(&U256::from(1).to_be_bytes::<32>()); // slot
    calldata.extend_from_slice(&U256::from(42).to_be_bytes::<32>()); // value

    let tx = make_signed_tx(
        5,
        TxKind::Call(Address::with_last_byte(0x99)),
        U256::ZERO,
        calldata,
    );

    // State root changes because storage was written
    let pre = B256::with_last_byte(0xCC);
    let post = B256::with_last_byte(0xDD);
    let entry = assert_pipeline_invariant(1, pre, post, &[tx]);

    assert_ne!(
        entry.state_deltas[0].current_state, entry.state_deltas[0].new_state,
        "storage write must change state root"
    );
}

/// Invariant test case 4: Failing transaction (revert).
///
/// A reverting transaction still changes the state root (due to gas
/// accounting — the sender's nonce and balance change). The builder
/// marks `failed=false` on the *entry* because the entry itself
/// succeeded — the EVM reverted the inner call but the block was built.
#[test]
fn test_composability_invariant_failing_transaction() {
    use alloy_primitives::TxKind;

    // Simulate a tx that would revert (0xFE = INVALID opcode as input)
    let tx = make_signed_tx(
        0,
        TxKind::Call(Address::with_last_byte(0x01)),
        U256::ZERO,
        vec![0xFE], // INVALID opcode
    );

    // Even a reverting tx changes state (nonce increment, gas payment)
    let pre = B256::with_last_byte(0xEE);
    let post = B256::with_last_byte(0xFF);
    let entry = assert_pipeline_invariant(1, pre, post, &[tx]);

    // build_entries_for_block always sets failed=false on the entry
    // (the entry represents the block-level state transition, not the tx outcome)
    assert!(
        !entry.next_action.failed,
        "block-level entry should not be marked failed"
    );
    assert_eq!(entry.next_action.action_type, CrossChainActionType::Result);
}

/// Invariant test case 5: Mixed block with multiple transaction types.
///
/// A single block containing an ETH transfer, a contract deploy, a
/// contract call with storage, a reverting tx, and another ETH transfer.
/// The entry must correctly aggregate all txs into a single action hash.
#[test]
fn test_composability_invariant_mixed_block() {
    use alloy_primitives::TxKind;

    let tx_transfer = make_signed_tx(
        0,
        TxKind::Call(Address::with_last_byte(0x01)),
        U256::from(1_000_000_000_000_000_000u128),
        vec![],
    );
    let tx_deploy = make_signed_tx(1, TxKind::Create, U256::ZERO, vec![0x60, 0x80, 0xFE]);
    let tx_storage_call = make_signed_tx(
        2,
        TxKind::Call(Address::with_last_byte(0x99)),
        U256::ZERO,
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02],
    );
    let tx_revert = make_signed_tx(
        3,
        TxKind::Call(Address::with_last_byte(0x02)),
        U256::ZERO,
        vec![0xFE],
    );
    let tx_transfer2 = make_signed_tx(
        4,
        TxKind::Call(Address::with_last_byte(0x03)),
        U256::from(500_000_000_000_000_000u128),
        vec![],
    );

    let all_txs = vec![
        tx_transfer,
        tx_deploy,
        tx_storage_call,
        tx_revert,
        tx_transfer2,
    ];

    let pre = B256::with_last_byte(0x11);
    let post = B256::with_last_byte(0x55);
    let entry = assert_pipeline_invariant(1, pre, post, &all_txs);

    // The mixed block produces exactly one entry covering all 5 txs
    assert_eq!(entry.state_deltas.len(), 1);
    assert_eq!(entry.state_deltas[0].current_state, pre);
    assert_eq!(entry.state_deltas[0].new_state, post);
    assert_ne!(entry.action_hash, crate::cross_chain::ActionHash::ZERO);

    // The action hash must differ from any single-tx hash
    let single_entry = build_entries_for_block(1, pre, post, &all_txs[..1]);
    assert_ne!(
        entry.action_hash, single_entry[0].action_hash,
        "mixed block hash must differ from single-tx hash"
    );
}

/// Invariant: different state roots produce different entries but the same
/// action hash (action hash depends only on tx data, not state roots).
#[test]
fn test_composability_invariant_action_hash_independent_of_state_roots() {
    use alloy_primitives::TxKind;

    let tx = make_signed_tx(
        0,
        TxKind::Call(Address::with_last_byte(0x42)),
        U256::from(1000),
        vec![],
    );

    let entry_a = build_entries_for_block(
        1,
        B256::with_last_byte(0xAA),
        B256::with_last_byte(0xBB),
        std::slice::from_ref(&tx),
    );
    let entry_b = build_entries_for_block(
        1,
        B256::with_last_byte(0xCC),
        B256::with_last_byte(0xDD),
        &[tx],
    );

    assert_eq!(
        entry_a[0].action_hash, entry_b[0].action_hash,
        "action_hash must be independent of state roots"
    );
    assert_ne!(
        entry_a[0].state_deltas[0].current_state, entry_b[0].state_deltas[0].current_state,
        "state deltas should differ"
    );
}

/// Invariant: the pipeline produces identical loadExecutionTable calldata
/// whether the entry was built from typed txs or from pre-encoded RLP,
/// ensuring builder and fullnode will always agree.
#[test]
fn test_composability_invariant_load_calldata_builder_vs_fullnode() {
    use crate::cross_chain::encode_load_execution_table_calldata;
    use alloy_primitives::TxKind;

    let txs: Vec<reth_ethereum_primitives::TransactionSigned> = (0..3)
        .map(|nonce| {
            make_signed_tx(
                nonce,
                TxKind::Call(Address::with_last_byte((nonce + 1) as u8)),
                U256::from(nonce * 1000),
                vec![0xAB; (nonce as usize + 1) * 10],
            )
        })
        .collect();

    let pre = B256::with_last_byte(0x11);
    let post = B256::with_last_byte(0x22);

    // Builder path: typed transactions
    let builder_entries = build_entries_for_block(1, pre, post, &txs);
    let builder_calldata = encode_load_execution_table_calldata(&builder_entries);

    // Fullnode path: pre-encoded RLP (simulating what derivation produces)
    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(&txs, &mut rlp_buf);
    let fullnode_entries = build_entries_from_encoded(1, pre, post, &rlp_buf);
    let fullnode_calldata = encode_load_execution_table_calldata(&fullnode_entries);

    assert_eq!(
        builder_calldata, fullnode_calldata,
        "builder and fullnode must produce byte-identical loadExecutionTable calldata"
    );
}

/// Invariant: multiple rollup IDs produce distinct entries and action hashes
/// for the same transaction data, ensuring cross-rollup isolation.
#[test]
fn test_composability_invariant_rollup_isolation() {
    use alloy_primitives::TxKind;

    let tx = make_signed_tx(
        0,
        TxKind::Call(Address::with_last_byte(0x42)),
        U256::from(1_000_000),
        vec![0xCA, 0xFE],
    );

    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);

    let entries: Vec<_> = (1..=5)
        .map(|rid| build_entries_for_block(rid, pre, post, std::slice::from_ref(&tx)))
        .collect();

    // Every pair of rollup IDs must produce different action hashes
    for i in 0..entries.len() {
        for j in (i + 1)..entries.len() {
            assert_ne!(
                entries[i][0].action_hash,
                entries[j][0].action_hash,
                "rollup {} and {} must produce different action hashes",
                i + 1,
                j + 1
            );
        }
    }
}

/// Invariant: the full pipeline roundtrip for all 5 tx types simultaneously
/// in a single block preserves the entry through postBatch → BatchPosted →
/// parse → loadExecutionTable, proving that the "same state root" guarantee
/// holds at the pipeline level for arbitrarily complex blocks.
#[test]
fn test_composability_invariant_full_pipeline_all_tx_types() {
    use crate::cross_chain::{
        encode_load_execution_table_calldata, encode_post_batch_calldata, parse_batch_posted_logs,
    };
    use alloy_primitives::{LogData, TxKind};
    use alloy_rpc_types::Log;
    use alloy_sol_types::{SolCall, SolEvent};

    // Build a block with all 5 transaction types
    let txs = vec![
        // 1. ETH transfer
        make_signed_tx(
            0,
            TxKind::Call(Address::with_last_byte(0x01)),
            U256::from(1_000_000_000_000_000_000u128),
            vec![],
        ),
        // 2. Contract deployment
        make_signed_tx(
            1,
            TxKind::Create,
            U256::ZERO,
            vec![0x60, 0x80, 0x60, 0x40, 0x52, 0xFE],
        ),
        // 3. Contract call with storage writes
        make_signed_tx(
            2,
            TxKind::Call(Address::with_last_byte(0x99)),
            U256::ZERO,
            {
                let mut d = vec![0x55, 0x55, 0x55, 0x55]; // SSTORE selector
                d.extend_from_slice(&[0xAB; 64]); // slot + value
                d
            },
        ),
        // 4. Failing transaction (revert payload)
        make_signed_tx(
            3,
            TxKind::Call(Address::with_last_byte(0x02)),
            U256::ZERO,
            vec![0xFE, 0xFE, 0xFE],
        ),
        // 5. Another ETH transfer (different value)
        make_signed_tx(
            4,
            TxKind::Call(Address::with_last_byte(0x03)),
            U256::from(500_000_000_000_000_000u128),
            vec![],
        ),
    ];

    let pre = B256::with_last_byte(0x11);
    let post = B256::with_last_byte(0x55);
    let rollup_id = 1u64;

    // Builder builds entry
    let builder_entries = build_entries_for_block(rollup_id, pre, post, &txs);
    assert_eq!(builder_entries.len(), 1);

    // Builder encodes for L1
    let l1_calldata = encode_post_batch_calldata(
        &builder_entries,
        alloy_primitives::Bytes::default(),
        alloy_primitives::Bytes::default(),
    );

    // L1 decodes and emits event
    let decoded =
        ICrossChainManagerL2::postBatchCall::abi_decode(&l1_calldata).expect("postBatch decode");
    let event = ICrossChainManagerL2::BatchPosted {
        entries: decoded.entries,
        publicInputsHash: B256::ZERO,
    };
    let log_data = event.encode_log_data();
    let mock_log = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(log_data.topics().to_vec(), log_data.data.clone())
                .expect("valid log data"),
        },
        block_hash: Some(B256::with_last_byte(0x01)),
        block_number: Some(200),
        block_timestamp: Some(2000),
        transaction_hash: Some(B256::with_last_byte(0x02)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    };

    // Fullnode derives from L1 event
    let derived = parse_batch_posted_logs(&[mock_log], U256::from(rollup_id));
    assert_eq!(derived.len(), 1);

    // Fullnode encodes for L2 builder protocol transaction
    let fullnode_load_calldata = encode_load_execution_table_calldata(&[derived[0].entry.clone()]);

    // Builder also encodes for comparison
    let builder_load_calldata = encode_load_execution_table_calldata(&builder_entries);

    // THE CORE INVARIANT: builder and fullnode produce byte-identical
    // loadExecutionTable calldata, meaning the L2 contract will receive
    // the same execution table entries and thus compute the same state root.
    assert_eq!(
        builder_load_calldata, fullnode_load_calldata,
        "CORE INVARIANT VIOLATION: builder and fullnode loadExecutionTable \
         calldata differ for a mixed block with all 5 tx types"
    );

    // Verify the final decoded entry matches the original
    let final_decoded =
        ICrossChainManagerL2::loadExecutionTableCall::abi_decode(&fullnode_load_calldata)
            .expect("loadExecutionTable decode");
    assert_eq!(final_decoded.entries.len(), 1);
    assert_eq!(
        B256::from(final_decoded.entries[0].actionHash),
        builder_entries[0].action_hash.as_b256(),
        "action hash must survive the full builder → L1 → fullnode → L2 pipeline"
    );
    assert_eq!(
        B256::from(final_decoded.entries[0].stateDeltas[0].currentState),
        pre,
        "pre_state_root must survive the full pipeline"
    );
    assert_eq!(
        B256::from(final_decoded.entries[0].stateDeltas[0].newState),
        post,
        "post_state_root must survive the full pipeline"
    );
}

// ── build_state_only_entry tests ──

#[test]
fn test_build_state_only_entry_different_roots_produces_entry() {
    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);
    let entries = build_state_only_entry(42, pre, post);
    assert_eq!(entries.len(), 1, "different roots should produce one entry");
    let entry = &entries[0];

    // action_hash must be zero for immediate application
    assert_eq!(
        entry.action_hash,
        crate::cross_chain::ActionHash::ZERO,
        "state-only entry must have zero action hash"
    );

    // State delta correctness
    assert_eq!(entry.state_deltas.len(), 1);
    assert_eq!(entry.state_deltas[0].current_state, pre);
    assert_eq!(entry.state_deltas[0].new_state, post);
    assert_eq!(
        entry.state_deltas[0].rollup_id,
        RollupId::new(U256::from(42))
    );
    assert_eq!(entry.state_deltas[0].ether_delta, I256::ZERO);
}

// ── Cross-component invariant tests (QA re-run iteration 24) ──

/// Invariant: state-only entries (actionHash=0) survive the full L1
/// pipeline roundtrip: build_state_only_entry → encode_post_batch_calldata
/// → BatchPosted event → parse_batch_posted_logs → loadExecutionTable.
#[test]
fn test_state_only_entry_l1_pipeline_roundtrip() {
    use crate::cross_chain::{
        ICrossChainManagerL2, encode_load_execution_table_calldata, encode_post_batch_calldata,
        parse_batch_posted_logs,
    };
    use alloy_primitives::{Bytes, LogData};
    use alloy_rpc_types::Log;
    use alloy_sol_types::{SolCall, SolEvent};

    let pre = B256::with_last_byte(0xAA);
    let post = B256::with_last_byte(0xBB);
    let rollup_id: u64 = 1;

    // Step 1: Build state-only entry (empty block)
    let entries = build_state_only_entry(rollup_id, pre, post);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].action_hash,
        crate::cross_chain::ActionHash::ZERO,
        "state-only entry must have actionHash=0"
    );

    // Step 2: Encode as postBatch calldata
    let calldata = encode_post_batch_calldata(&entries, Bytes::default(), Bytes::default());
    let decoded =
        ICrossChainManagerL2::postBatchCall::abi_decode(&calldata).expect("postBatch decode");
    assert_eq!(decoded.entries.len(), 1);
    assert_eq!(
        B256::from(decoded.entries[0].actionHash),
        B256::ZERO,
        "actionHash=0 must survive ABI encoding"
    );

    // Step 3: Simulate BatchPosted event
    let event = ICrossChainManagerL2::BatchPosted {
        entries: decoded.entries,
        publicInputsHash: B256::ZERO,
    };
    let log_data = event.encode_log_data();
    let mock_log = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(log_data.topics().to_vec(), log_data.data.clone())
                .expect("valid log"),
        },
        block_hash: Some(B256::ZERO),
        block_number: Some(50),
        block_timestamp: Some(500),
        transaction_hash: Some(B256::ZERO),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    };

    // Step 4: Fullnode parses event
    let derived = parse_batch_posted_logs(&[mock_log], U256::from(rollup_id));
    assert_eq!(derived.len(), 1);
    let derived_entry = &derived[0].entry;
    assert_eq!(
        derived_entry.action_hash,
        crate::cross_chain::ActionHash::ZERO,
        "actionHash=0 must survive L1 event roundtrip"
    );
    assert_eq!(derived_entry.state_deltas[0].current_state, pre);
    assert_eq!(derived_entry.state_deltas[0].new_state, post);

    // Step 5: Encode for L2 builder protocol transaction
    let load_calldata = encode_load_execution_table_calldata(std::slice::from_ref(derived_entry));
    let final_decoded = ICrossChainManagerL2::loadExecutionTableCall::abi_decode(&load_calldata)
        .expect("loadExecutionTable decode");
    assert_eq!(
        B256::from(final_decoded.entries[0].actionHash),
        B256::ZERO,
        "actionHash=0 must survive full pipeline to L2 contract"
    );
}

/// Invariant: cross-chain CALL+RESULT entry pairs survive the full L1
/// pipeline roundtrip, preserving both action hashes and action fields.
#[test]
fn test_cross_chain_call_result_pair_l1_pipeline_roundtrip() {
    use crate::cross_chain::{
        ICrossChainManagerL2, build_cross_chain_call_entries, convert_pairs_to_l1_entries,
        encode_load_execution_table_calldata, encode_post_batch_calldata, parse_batch_posted_logs,
    };
    use alloy_primitives::{Bytes, LogData};
    use alloy_rpc_types::Log;
    use alloy_sol_types::{SolCall, SolEvent};

    let rollup_id = RollupId::new(U256::from(1u64));
    let destination = Address::with_last_byte(0x42);
    let call_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let source_address = Address::with_last_byte(0x01);
    let source_rollup = RollupId::new(U256::from(2u64));
    let return_data = vec![0x00; 32]; // simulated return

    // Step 1: Build CALL+RESULT pair (L2 format)
    let (call_entry, result_entry) = build_cross_chain_call_entries(
        rollup_id,
        destination,
        call_data.clone(),
        U256::ZERO,
        source_address,
        source_rollup,
        true,
        return_data.clone(),
    );
    assert_ne!(call_entry.action_hash, crate::cross_chain::ActionHash::ZERO);
    assert_ne!(
        result_entry.action_hash,
        crate::cross_chain::ActionHash::ZERO
    );
    assert_ne!(call_entry.action_hash, result_entry.action_hash);

    // Step 2: Convert to L1 format and roundtrip through postBatch → BatchPosted → parse
    let l1_entries = convert_pairs_to_l1_entries(&[call_entry.clone(), result_entry.clone()]);
    assert_eq!(l1_entries.len(), 1);
    let calldata = encode_post_batch_calldata(&l1_entries, Bytes::default(), Bytes::default());
    let decoded =
        ICrossChainManagerL2::postBatchCall::abi_decode(&calldata).expect("postBatch decode");
    assert_eq!(decoded.entries.len(), 1);

    let event = ICrossChainManagerL2::BatchPosted {
        entries: decoded.entries,
        publicInputsHash: B256::ZERO,
    };
    let log_data = event.encode_log_data();
    let mock_log = Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: LogData::new(log_data.topics().to_vec(), log_data.data.clone())
                .expect("valid log"),
        },
        block_hash: Some(B256::ZERO),
        block_number: Some(100),
        block_timestamp: Some(1000),
        transaction_hash: Some(B256::ZERO),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    };

    // L1 entry (actionHash=CALL, nextAction=RESULT) targets rollup_id=1
    let derived = parse_batch_posted_logs(&[mock_log], rollup_id.as_u256());
    assert_eq!(
        derived.len(),
        1,
        "single L1 entry must be parsed for our rollup"
    );

    // Step 3: Verify L1 entry survived (actionHash=CALL hash, nextAction=RESULT)
    assert_eq!(derived[0].entry.action_hash, call_entry.action_hash);
    assert_eq!(
        derived[0].entry.next_action.action_type,
        CrossChainActionType::Result
    );
    assert_eq!(derived[0].entry.next_action.data, return_data);
    assert!(!derived[0].entry.next_action.failed);

    // Step 4: loadExecutionTable roundtrip
    let derived_entries: Vec<_> = derived.iter().map(|d| d.entry.clone()).collect();
    let load_calldata = encode_load_execution_table_calldata(&derived_entries);
    let final_decoded = ICrossChainManagerL2::loadExecutionTableCall::abi_decode(&load_calldata)
        .expect("loadExecutionTable decode");
    assert_eq!(final_decoded.entries.len(), 1);
    assert_eq!(
        B256::from(final_decoded.entries[0].actionHash),
        call_entry.action_hash.as_b256()
    );
}

/// Invariant: for cross-chain CALL/RESULT entries, the action_hash must
/// equal keccak256(abi.encode(next_action)). This self-consistency
/// property is critical — the L2 contract looks up entries by computing
/// the hash of the action it wants to consume.
#[test]
fn test_cross_chain_entry_action_hash_is_keccak_of_next_action() {
    use crate::cross_chain::{ICrossChainManagerL2, build_cross_chain_call_entries};
    use alloy_primitives::keccak256;
    use alloy_sol_types::SolType;

    let test_cases = vec![
        // (destination, data, source_address, source_rollup, success, return_data)
        (
            Address::with_last_byte(0x42),
            vec![0xDE, 0xAD],
            Address::with_last_byte(0x01),
            RollupId::new(U256::from(2u64)),
            true,
            vec![0x00; 32],
        ),
        (
            Address::with_last_byte(0xFF),
            vec![],
            Address::ZERO,
            RollupId::MAINNET,
            false,
            vec![0x08, 0xc3, 0x79, 0xa0], // revert selector
        ),
        (
            Address::with_last_byte(0xAA),
            vec![0xCA, 0xFE, 0xBA, 0xBE],
            Address::with_last_byte(0xBB),
            RollupId::new(U256::from(99u64)),
            true,
            vec![],
        ),
    ];

    for (i, (dest, data, src_addr, src_rollup, success, ret_data)) in
        test_cases.into_iter().enumerate()
    {
        let rollup_id = RollupId::new(U256::from(1u64));
        let (call_entry, result_entry) = build_cross_chain_call_entries(
            rollup_id,
            dest,
            data,
            U256::ZERO,
            src_addr,
            src_rollup,
            success,
            ret_data,
        );

        // Verify CALL entry: action_hash == keccak256(abi.encode(call_action))
        let call_sol = call_entry.next_action.to_sol_action();
        let expected_call_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(&call_sol));
        assert_eq!(
            call_entry.action_hash.as_b256(),
            expected_call_hash,
            "case {i}: CALL action_hash must be keccak256(abi.encode(next_action))"
        );

        // Verify RESULT entry: action_hash == keccak256(abi.encode(result_action))
        let result_sol = result_entry.next_action.to_sol_action();
        let expected_result_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(&result_sol));
        assert_eq!(
            result_entry.action_hash.as_b256(),
            expected_result_hash,
            "case {i}: RESULT action_hash must be keccak256(abi.encode(next_action))"
        );
    }
}

/// Invariant: PendingBlock state_root must match the execution entry's
/// state delta new_state. This ensures the builder's L1 submitBatch
/// (which uses PendingBlock.state_root) is consistent with the postBatch
/// entries (which use state delta current_state/new_state).
#[test]
fn test_pending_block_state_root_matches_execution_entry_delta() {
    use crate::proposer::PendingBlock;
    use alloy_consensus::TxLegacy;
    use alloy_primitives::TxKind;

    let tx = TxLegacy {
        chain_id: Some(42069),
        nonce: 0,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: TxKind::Call(Address::with_last_byte(0x01)),
        value: U256::ZERO,
        input: Default::default(),
    };
    let signed = alloy_consensus::Signed::new_unhashed(
        tx,
        alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
    );
    let signed_tx = reth_ethereum_primitives::TransactionSigned::Legacy(signed);

    let pre_state_root = B256::with_last_byte(0xAA);
    let post_state_root = B256::with_last_byte(0xBB);

    // Simulate what the driver does: build a PendingBlock and entries
    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(&[signed_tx], &mut rlp_buf);

    let pending_block = PendingBlock {
        l2_block_number: 42,
        pre_state_root,
        state_root: post_state_root,
        clean_state_root: crate::cross_chain::CleanStateRoot::new(post_state_root),
        encoded_transactions: alloy_primitives::Bytes::from(rlp_buf.clone()),
        intermediate_roots: vec![],
    };

    let entries = build_entries_from_encoded(1, pre_state_root, post_state_root, &rlp_buf);

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].state_deltas[0].new_state, pending_block.state_root,
        "execution entry new_state must match PendingBlock state_root"
    );
    assert_eq!(
        entries[0].state_deltas[0].current_state, pre_state_root,
        "execution entry current_state must be the parent block's state_root"
    );
}

/// Invariant: consecutive blocks produce chained state deltas where
/// block N's new_state == block N+1's current_state. This is required
/// by the Rollups contract which verifies the state root chain.
#[test]
fn test_consecutive_blocks_chain_state_deltas() {
    let roots = [
        B256::with_last_byte(0x01),
        B256::with_last_byte(0x02),
        B256::with_last_byte(0x03),
        B256::with_last_byte(0x04),
    ];
    let rlp = vec![0xc0, 0x01];

    let mut all_entries = Vec::new();
    for i in 0..3 {
        let entries = build_entries_from_encoded(1, roots[i], roots[i + 1], &rlp);
        all_entries.extend(entries);
    }

    assert_eq!(all_entries.len(), 3);
    for i in 0..2 {
        assert_eq!(
            all_entries[i].state_deltas[0].new_state,
            all_entries[i + 1].state_deltas[0].current_state,
            "block {}'s new_state must equal block {}'s current_state",
            i,
            i + 1
        );
    }
}

/// Invariant: empty blocks via build_state_only_entry and non-empty blocks
/// via build_entries_from_encoded can be interleaved and still maintain
/// the state root chain.
#[test]
fn test_mixed_empty_and_nonempty_blocks_chain_state_deltas() {
    let roots = [
        B256::with_last_byte(0x10),
        B256::with_last_byte(0x20),
        B256::with_last_byte(0x30),
        B256::with_last_byte(0x40),
    ];
    let rlp = vec![0xc0, 0x01];

    // Block 0: non-empty
    let e0 = build_entries_from_encoded(1, roots[0], roots[1], &rlp);
    // Block 1: empty (state-only)
    let e1 = build_state_only_entry(1, roots[1], roots[2]);
    // Block 2: non-empty
    let e2 = build_entries_from_encoded(1, roots[2], roots[3], &rlp);

    assert_eq!(e0.len(), 1);
    assert_eq!(e1.len(), 1);
    assert_eq!(e2.len(), 1);

    // Chain must be unbroken
    assert_eq!(
        e0[0].state_deltas[0].new_state, e1[0].state_deltas[0].current_state,
        "non-empty → empty transition must chain"
    );
    assert_eq!(
        e1[0].state_deltas[0].new_state, e2[0].state_deltas[0].current_state,
        "empty → non-empty transition must chain"
    );

    // action_hash semantics differ
    assert_ne!(
        e0[0].action_hash,
        crate::cross_chain::ActionHash::ZERO,
        "non-empty block has non-zero hash"
    );
    assert_eq!(
        e1[0].action_hash,
        crate::cross_chain::ActionHash::ZERO,
        "empty block has zero hash"
    );
    assert_ne!(
        e2[0].action_hash,
        crate::cross_chain::ActionHash::ZERO,
        "non-empty block has non-zero hash"
    );
}
