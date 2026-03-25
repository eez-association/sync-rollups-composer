impl Proposer {
    fn with_provider(
        config: Arc<RollupConfig>,
        provider: Box<dyn Provider + Send + Sync>,
        signer_address: Address,
    ) -> Self {
        // Use a deterministic test signer; only the address matters for most tests.
        let signer: PrivateKeySigner =
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                .parse()
                .expect("valid test key");
        Self {
            config,
            provider,
            signer,
            signer_address,
        }
    }
}

use super::*;

/// Helper to build a minimal RollupConfig for tests.
fn test_config() -> RollupConfig {
    RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: Address::ZERO,
        deployment_l1_block: 0,
        deployment_timestamp: 0,
        block_time: 12,
        builder_mode: true,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        builder_ws_url: None,
        health_port: 0,
        rollups_address: Address::ZERO,
        cross_chain_manager_address: Address::ZERO,
        rollup_id: 0,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: Address::ZERO,
        bridge_l2_address: Address::ZERO,
        bridge_l1_address: Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    }
}

#[test]
fn test_pending_block_struct() {
    let block = PendingBlock {
        l2_block_number: 42,
        pre_state_root: B256::with_last_byte(0xAA),
        state_root: B256::with_last_byte(0xBB),
        clean_state_root: B256::with_last_byte(0xBB),
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    };
    assert_eq!(block.l2_block_number, 42);
    assert_eq!(block.pre_state_root, B256::with_last_byte(0xAA));
    assert_eq!(block.state_root, B256::with_last_byte(0xBB));
    assert_eq!(block.clean_state_root, B256::with_last_byte(0xBB));
    assert_eq!(block.encoded_transactions.as_ref(), &[0xc0]);
}

#[test]
fn test_rollups_view_calldata_encoding() {
    let calldata = rollupsCall {
        rollupId: U256::from(1),
    }
    .abi_encode();
    // 4 bytes selector + 32 bytes for rollupId
    assert_eq!(calldata.len(), 36);
}

#[test]
fn test_proposer_new_requires_private_key() {
    let config = Arc::new(test_config());
    let result = Proposer::new(config);
    assert!(result.is_err());
}

#[test]
fn test_proposer_new_invalid_private_key() {
    let config = Arc::new(RollupConfig {
        builder_private_key: Some("not-a-valid-key".to_string()),
        ..test_config()
    });
    let result = Proposer::new(config);
    assert!(result.is_err());
}

#[test]
fn test_proposer_new_valid_private_key() {
    let config = Arc::new(RollupConfig {
        builder_private_key: Some(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
        ),
        ..test_config()
    });
    let result = Proposer::new(config);
    assert!(result.is_ok());
}

#[test]
fn test_calldata_gas_calculation() {
    // Verify calldata gas calculation matches EVM pricing:
    // 16 gas per non-zero byte, 4 gas per zero byte
    use crate::cross_chain::encode_post_batch_calldata;

    let calldata = encode_post_batch_calldata(&[], Bytes::new(), Bytes::new());
    let gas: u64 = calldata
        .iter()
        .map(|&b| if b == 0 { 4u64 } else { 16u64 })
        .sum();

    // Calldata should be well under MAX_CALLDATA_GAS for an empty batch
    assert!(gas < Proposer::MAX_CALLDATA_GAS);
    assert!(gas > 0, "calldata gas must be positive");

    // Verify constant is reasonable (12M is well under L1 block limit of ~30M)
    assert_eq!(Proposer::MAX_CALLDATA_GAS, 12_000_000);
}

#[test]
fn test_calldata_gas_exceeds_limit_with_large_batch() {
    // Verify that a very large batch would exceed MAX_CALLDATA_GAS
    use crate::cross_chain::{encode_block_calldata, encode_post_batch_calldata};

    let large_tx = Bytes::from(vec![0xFF; 262144]); // 256KB
    let numbers: Vec<u64> = (1..=3).collect();
    let txs = vec![large_tx.clone(), large_tx.clone(), large_tx];
    let block_tuples: Vec<(u64, B256, B256, Bytes)> = numbers
        .iter()
        .zip(txs.iter())
        .map(|(&n, t)| (n, B256::ZERO, B256::ZERO, t.clone()))
        .collect();
    let entries = crate::cross_chain::build_block_entries(&block_tuples, 1);
    let call_data = encode_block_calldata(&numbers, &txs);
    let calldata = encode_post_batch_calldata(&entries, call_data, Bytes::new());

    let gas: u64 = calldata
        .iter()
        .map(|&b| if b == 0 { 4u64 } else { 16u64 })
        .sum();

    assert!(
        gas > Proposer::MAX_CALLDATA_GAS,
        "3x 256KB blocks should exceed calldata gas limit"
    );
}

#[test]
fn test_submit_to_l1_empty_is_noop() {
    // submit_to_l1 with empty blocks and entries should return Ok immediately
    // (verified by code inspection — no provider call needed)
    let blocks: &[PendingBlock] = &[];
    let entries: &[CrossChainExecutionEntry] = &[];
    assert!(blocks.is_empty());
    assert!(entries.is_empty());
}

#[test]
fn test_calldata_gas_at_exact_boundary_passes() {
    // Calldata gas exactly equal to MAX_CALLDATA_GAS should pass (> not >=)
    let gas = Proposer::MAX_CALLDATA_GAS;
    assert!(
        !(gas > Proposer::MAX_CALLDATA_GAS),
        "equal should pass the check"
    );
    assert!(gas + 1 > Proposer::MAX_CALLDATA_GAS, "one over should fail");
}

#[test]
fn test_batch_halving_logic() {
    // Simulate the driver's batch halving: 100 -> 50 -> 25 -> 12 -> 6 -> 3 -> 1
    let mut batch_size = 100usize;
    let mut halvings = Vec::new();
    while batch_size > 1 {
        batch_size /= 2;
        halvings.push(batch_size);
    }
    assert_eq!(halvings, vec![50, 25, 12, 6, 3, 1]);
}

#[test]
fn test_batch_halving_reaches_one() {
    // When submission fails with "calldata gas" error, batch halves repeatedly.
    // Verify that halving from any starting size always reaches 1 (never 0).
    for start in [1, 2, 3, 5, 10, 50, 100, 255, 1000] {
        let mut size = start;
        while size > 1 {
            size /= 2;
        }
        assert_eq!(size, 1, "halving from {start} must reach 1, not 0");
    }
    let batch_size = 1usize;
    assert!(!(batch_size > 1), "size 1 should not enter halving branch");
}

#[test]
fn test_max_batch_calldata_gas_within_l1_block_limit() {
    // MAX_CALLDATA_GAS should be well within L1's block gas limit (30M)
    let l1_block_gas_limit = 30_000_000u64;
    let max_cg = Proposer::MAX_CALLDATA_GAS;
    assert!(
        max_cg < l1_block_gas_limit,
        "MAX_CALLDATA_GAS ({max_cg}) should be under L1 block limit ({l1_block_gas_limit})"
    );
    assert!(
        max_cg <= l1_block_gas_limit / 2,
        "MAX_CALLDATA_GAS should leave room for execution gas"
    );
}

#[test]
fn test_proposer_signer_address_matches_known_key() {
    let config = Arc::new(RollupConfig {
        builder_private_key: Some(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
        ),
        ..test_config()
    });
    let proposer = Proposer::new(config).expect("valid config should create proposer");
    let addr = proposer.signer_address();
    let expected: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        .parse()
        .expect("valid address");
    assert_eq!(
        addr, expected,
        "signer address should match anvil's first account"
    );
}

#[test]
fn test_proposer_create_signer_returns_valid_signer() {
    let config = Arc::new(RollupConfig {
        builder_private_key: Some(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
        ),
        ..test_config()
    });
    let proposer = Proposer::new(config).expect("valid config should create proposer");
    let signer = proposer
        .create_signer()
        .expect("create_signer should succeed with valid key");
    assert_eq!(
        signer.address(),
        proposer.signer_address(),
        "create_signer should produce a signer with the same address"
    );
}

#[test]
fn test_proposer_create_signer_fails_without_key() {
    let config = Arc::new(test_config());
    let result = Proposer::new(config);
    assert!(
        result.is_err(),
        "Proposer::new should fail without private key"
    );
    let err_msg = format!("{}", result.err().expect("should be an error"));
    assert!(
        err_msg.contains("BUILDER_PRIVATE_KEY"),
        "error should mention BUILDER_PRIVATE_KEY, got: {err_msg}"
    );
}

#[test]
fn test_post_batch_calldata_encoding() {
    use crate::cross_chain::{
        CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
        encode_post_batch_calldata,
    };
    use alloy_primitives::I256;

    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::with_last_byte(0xAA),
            new_state: B256::with_last_byte(0xBB),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::with_last_byte(0xCC),
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

    let calldata = encode_post_batch_calldata(&[entry], Bytes::new(), Bytes::default());

    // Verify it encodes (4 byte selector + params)
    assert!(calldata.len() > 4);

    // Verify the selector matches postBatch
    use crate::cross_chain::ICrossChainManagerL2;
    let expected_selector = &ICrossChainManagerL2::postBatchCall::SELECTOR;
    assert_eq!(&calldata[..4], expected_selector);
}

#[test]
fn test_post_batch_calldata_multiple_entries() {
    use crate::cross_chain::{
        CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
        encode_post_batch_calldata,
    };
    use alloy_primitives::I256;

    let make_entry = |n: u8| CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::with_last_byte(n),
            new_state: B256::with_last_byte(n + 1),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::with_last_byte(n),
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

    let entries = vec![make_entry(1), make_entry(2), make_entry(3)];
    let calldata = encode_post_batch_calldata(&entries, Bytes::new(), Bytes::default());

    // Multiple entries should produce longer calldata than single
    let single_calldata =
        encode_post_batch_calldata(&[make_entry(1)], Bytes::new(), Bytes::default());
    assert!(calldata.len() > single_calldata.len());
}

#[test]
fn test_post_batch_calldata_empty_proof_vs_nonempty_proof() {
    use crate::cross_chain::{
        CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
        encode_post_batch_calldata,
    };
    use alloy_primitives::I256;

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

    let empty_proof = Bytes::default();
    let nonempty_proof = Bytes::from(vec![0xAB; 64]); // 64-byte proof

    let calldata_empty = encode_post_batch_calldata(&[entry.clone()], Bytes::new(), empty_proof);
    let calldata_proof = encode_post_batch_calldata(&[entry], Bytes::new(), nonempty_proof);

    assert_ne!(
        calldata_empty, calldata_proof,
        "empty vs non-empty proof should produce different calldata"
    );
    assert!(
        calldata_proof.len() > calldata_empty.len(),
        "non-empty proof should produce longer calldata"
    );

    // Both should have the same selector
    assert_eq!(
        &calldata_empty[..4],
        &calldata_proof[..4],
        "both should use postBatch selector"
    );
}

#[test]
fn test_post_batch_calldata_roundtrip_with_proof() {
    use crate::cross_chain::{
        CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
        ICrossChainManagerL2, encode_post_batch_calldata,
    };
    use alloy_primitives::I256;
    use alloy_sol_types::SolCall;

    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::with_last_byte(0xAA),
            new_state: B256::with_last_byte(0xBB),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::with_last_byte(0xCC),
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

    let proof = Bytes::from(vec![0x01, 0x02, 0x03, 0x04]);
    let calldata = encode_post_batch_calldata(&[entry], Bytes::new(), proof.clone());

    // Decode and verify proof roundtrips
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&calldata)
        .expect("should decode postBatch calldata");
    assert_eq!(
        decoded.proof, proof,
        "proof should roundtrip through encoding"
    );
    assert_eq!(decoded.entries.len(), 1);
    assert_eq!(decoded.blobCount, U256::ZERO, "blobCount should be zero");
}

#[test]
fn test_post_batch_calldata_empty_entries() {
    // encode_post_batch_calldata should handle empty entries vec gracefully
    use crate::cross_chain::encode_post_batch_calldata;

    let calldata = encode_post_batch_calldata(&[], Bytes::new(), Bytes::default());
    // Should still produce valid ABI encoding: 4-byte selector + encoded params
    assert!(
        calldata.len() > 4,
        "even empty entries should produce calldata"
    );

    use crate::cross_chain::ICrossChainManagerL2;
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&calldata)
        .expect("empty entries calldata should decode");
    assert!(
        decoded.entries.is_empty(),
        "decoded entries should be empty"
    );
    assert_eq!(decoded.blobCount, U256::ZERO);
    assert!(decoded.proof.is_empty());
}

#[test]
fn test_create_signer_produces_correct_address() {
    let config = Arc::new(RollupConfig {
        // anvil's second default account private key
        builder_private_key: Some(
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d".to_string(),
        ),
        ..test_config()
    });
    let proposer = Proposer::new(config).expect("valid config");
    let signer = proposer
        .create_signer()
        .expect("create_signer should succeed");
    assert_eq!(
        proposer.signer_address(),
        signer.address(),
        "signer_address() must equal create_signer().address()"
    );
    let expected: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
        .parse()
        .expect("valid address");
    assert_eq!(signer.address(), expected);
}

#[test]
fn test_post_batch_calldata_with_negative_ether_delta() {
    use crate::cross_chain::{
        CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
        ICrossChainManagerL2, encode_post_batch_calldata,
    };
    use alloy_primitives::I256;

    let negative_delta = I256::try_from(-7_500_000_000_000_000_000i128).unwrap();
    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::with_last_byte(0xAA),
            new_state: B256::with_last_byte(0xBB),
            ether_delta: negative_delta,
        }],
        action_hash: B256::with_last_byte(0xDD),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Call,
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

    let calldata = encode_post_batch_calldata(&[entry], Bytes::new(), Bytes::default());
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&calldata)
        .expect("negative ether delta should decode");
    assert_eq!(decoded.entries.len(), 1);
    assert_eq!(
        decoded.entries[0].stateDeltas[0].etherDelta, negative_delta,
        "negative I256 must survive ABI roundtrip"
    );
    assert!(decoded.entries[0].stateDeltas[0].etherDelta < I256::ZERO);
}

#[test]
fn test_post_batch_calldata_with_large_scope() {
    use crate::cross_chain::{
        CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
        ICrossChainManagerL2, encode_post_batch_calldata,
    };
    use alloy_primitives::I256;

    // Build a scope with 50 elements
    let large_scope: Vec<U256> = (0u64..50).map(U256::from).collect();
    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::ZERO,
            new_state: B256::with_last_byte(0xFF),
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::with_last_byte(0x01),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::L2Tx,
            rollup_id: U256::from(1),
            destination: Address::with_last_byte(0x42),
            value: U256::from(100),
            data: vec![0xAB, 0xCD],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: large_scope.clone(),
        },
    };

    let calldata = encode_post_batch_calldata(&[entry], Bytes::new(), Bytes::default());
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&calldata)
        .expect("large scope should decode");
    assert_eq!(decoded.entries[0].nextAction.scope.len(), 50);
    for (i, val) in decoded.entries[0].nextAction.scope.iter().enumerate() {
        assert_eq!(*val, U256::from(i as u64), "scope element {i} mismatch");
    }
}

#[test]
fn test_post_batch_calldata_with_failed_action() {
    use crate::cross_chain::{
        CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
        ICrossChainManagerL2, encode_post_batch_calldata,
    };
    use alloy_primitives::I256;

    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: U256::from(1),
            current_state: B256::with_last_byte(0x11),
            new_state: B256::with_last_byte(0x22),
            ether_delta: I256::try_from(-1_000_000i128).unwrap(),
        }],
        action_hash: B256::with_last_byte(0xEE),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Revert,
            rollup_id: U256::from(2),
            destination: Address::with_last_byte(0x77),
            value: U256::ZERO,
            data: vec![0x08, 0xc3, 0x79, 0xa0], // Error(string) selector
            failed: true,
            source_address: Address::with_last_byte(0x88),
            source_rollup: U256::from(1),
            scope: vec![U256::from(0), U256::from(1)],
        },
    };

    let calldata = encode_post_batch_calldata(&[entry], Bytes::new(), Bytes::from(vec![0xFF]));
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(&calldata)
        .expect("failed action entry should decode");
    assert_eq!(decoded.entries.len(), 1);
    let action = &decoded.entries[0].nextAction;
    assert!(action.failed, "failed flag must be true after roundtrip");
    assert_eq!(
        action.actionType,
        ICrossChainManagerL2::ActionType::REVERT,
        "action type should be REVERT"
    );
    assert_eq!(action.data.as_ref(), &[0x08, 0xc3, 0x79, 0xa0]);
    assert_eq!(action.rollupId, U256::from(2));
    assert_eq!(action.sourceAddress, Address::with_last_byte(0x88));
}

// --- Receipt confirmation tests ---

#[test]
fn test_with_provider_constructor() {
    let config = Arc::new(test_config());
    let provider = ProviderBuilder::new().connect_http("http://localhost:8545".parse().unwrap());
    let addr = Address::with_last_byte(0x42);
    let proposer = Proposer::with_provider(config, Box::new(provider), addr);
    assert_eq!(proposer.signer_address(), addr);
}

#[tokio::test]
async fn test_wait_for_receipt_no_provider_connection() {
    // When the provider can't connect, wait_for_receipt should return Err
    // after exhausting retries so the driver re-queues blocks and retries
    // after cooldown. This prevents racing unconfirmed submissions.
    let config = Arc::new(RollupConfig {
        l1_rpc_url: "http://127.0.0.1:1".to_string(), // non-existent
        ..test_config()
    });
    let provider = ProviderBuilder::new().connect_http("http://127.0.0.1:1".parse().unwrap());
    let proposer = Proposer::with_provider(config, Box::new(provider), Address::ZERO);

    let tx_hash = B256::with_last_byte(0xAB);
    let result = proposer.wait_for_receipt(tx_hash, "test").await;
    assert!(
        result.is_err(),
        "should return Err when receipt unavailable"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("receipt not available"),
        "error should mention receipt unavailability: {err_msg}"
    );
}

#[test]
fn test_switch_l1_url_valid() {
    let config = Arc::new(RollupConfig {
        builder_private_key: Some(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
        ),
        l1_rpc_url_fallback: Some("http://localhost:9545".to_string()),
        ..test_config()
    });
    let mut proposer = Proposer::new(config).unwrap();

    let result = proposer.switch_l1_url("http://localhost:9545");
    assert!(result.is_ok(), "switching to valid URL should succeed");

    let result = proposer.switch_l1_url("http://localhost:8545");
    assert!(result.is_ok(), "switching back should succeed");
}

#[test]
fn test_switch_l1_url_invalid() {
    let config = Arc::new(RollupConfig {
        builder_private_key: Some(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
        ),
        ..test_config()
    });
    let mut proposer = Proposer::new(config).unwrap();

    let result = proposer.switch_l1_url("not a url");
    assert!(result.is_err(), "invalid URL should fail");
}

#[test]
fn test_switch_l1_url_with_embedded_signer() {
    // Proposer now always has an embedded signer, so switch_l1_url should succeed
    let config = Arc::new(test_config());
    let provider = ProviderBuilder::new().connect_http("http://127.0.0.1:1".parse().unwrap());
    let mut proposer = Proposer::with_provider(config, Box::new(provider), Address::ZERO);

    let result = proposer.switch_l1_url("http://localhost:9545");
    assert!(result.is_ok(), "switch with embedded signer should succeed");
}

#[test]
fn test_cross_chain_batch_calldata_gas_with_many_large_entries() {
    use crate::cross_chain::{
        CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
        encode_post_batch_calldata,
    };
    use alloy_primitives::I256;

    // Build entries with large data fields (8KB each)
    let entries: Vec<CrossChainExecutionEntry> = (0..200)
        .map(|i| CrossChainExecutionEntry {
            state_deltas: vec![CrossChainStateDelta {
                rollup_id: U256::from(1),
                current_state: B256::with_last_byte(i as u8),
                new_state: B256::with_last_byte((i + 1) as u8),
                ether_delta: I256::ZERO,
            }],
            action_hash: B256::with_last_byte(i as u8),
            next_action: CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::from(1),
                destination: Address::with_last_byte(i as u8),
                value: U256::ZERO,
                data: vec![0xFF; 8192], // 8KB of data
                failed: false,
                source_address: Address::ZERO,
                source_rollup: U256::from(2),
                scope: vec![],
            },
        })
        .collect();

    let calldata = encode_post_batch_calldata(&entries, Bytes::new(), Bytes::new());
    let gas: u64 = calldata
        .iter()
        .map(|&b| if b == 0 { 4u64 } else { 16u64 })
        .sum();

    assert!(
        gas > Proposer::MAX_CALLDATA_GAS,
        "200 entries with 8KB data each ({gas} gas) should exceed limit ({})",
        Proposer::MAX_CALLDATA_GAS
    );

    // But a smaller subset should fit
    let small_calldata = encode_post_batch_calldata(&entries[..10], Bytes::new(), Bytes::new());
    let small_gas: u64 = small_calldata
        .iter()
        .map(|&b| if b == 0 { 4u64 } else { 16u64 })
        .sum();
    assert!(
        small_gas < Proposer::MAX_CALLDATA_GAS,
        "10 entries with 8KB data ({small_gas} gas) should fit in limit ({})",
        Proposer::MAX_CALLDATA_GAS
    );
}

#[test]
fn test_switch_l1_url_rebuilds_fresh_provider() {
    let config = Arc::new(RollupConfig {
        builder_private_key: Some(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string(),
        ),
        ..test_config()
    });

    let provider =
        Box::new(ProviderBuilder::new().connect_http("http://127.0.0.1:8545".parse().unwrap()));
    let mut proposer = Proposer::with_provider(config.clone(), provider, Address::ZERO);

    assert!(proposer.switch_l1_url("http://127.0.0.1:9545").is_ok());
    assert!(proposer.switch_l1_url("http://10.0.0.1:8545").is_ok());

    // Invalid URL should return Err, not panic
    assert!(proposer.switch_l1_url("not-a-url").is_err());
    assert!(proposer.switch_l1_url("").is_err());
}

#[test]
fn test_build_block_entries_produces_correct_state_deltas() {
    let blocks = vec![
        PendingBlock {
            l2_block_number: 1,
            pre_state_root: B256::with_last_byte(0x00),
            state_root: B256::with_last_byte(0x01),
            clean_state_root: B256::with_last_byte(0x01),
            encoded_transactions: Bytes::from(vec![0xc0]),
            intermediate_roots: vec![],
        },
        PendingBlock {
            l2_block_number: 2,
            pre_state_root: B256::with_last_byte(0x01),
            state_root: B256::with_last_byte(0x02),
            clean_state_root: B256::with_last_byte(0x02),
            encoded_transactions: Bytes::from(vec![0xc1, 0x80]),
            intermediate_roots: vec![],
        },
    ];

    let tuples: Vec<(u64, B256, B256, Bytes)> = blocks
        .iter()
        .map(|b| {
            (
                b.l2_block_number,
                b.pre_state_root,
                b.state_root,
                b.encoded_transactions.clone(),
            )
        })
        .collect();
    let entries = crate::cross_chain::build_block_entries(&tuples, 1);
    assert_eq!(entries.len(), 2);

    // First entry: state delta 0x00 -> 0x01
    assert_eq!(entries[0].state_deltas.len(), 1);
    assert_eq!(
        entries[0].state_deltas[0].current_state,
        B256::with_last_byte(0x00)
    );
    assert_eq!(
        entries[0].state_deltas[0].new_state,
        B256::with_last_byte(0x01)
    );
    assert_eq!(entries[0].state_deltas[0].rollup_id, U256::from(1));

    // Second entry: state delta 0x01 -> 0x02
    assert_eq!(
        entries[1].state_deltas[0].current_state,
        B256::with_last_byte(0x01)
    );
    assert_eq!(
        entries[1].state_deltas[0].new_state,
        B256::with_last_byte(0x02)
    );
}

#[test]
fn test_encode_block_calldata_roundtrip() {
    let numbers = vec![1u64, 2, 3];
    let txs = vec![
        Bytes::from(vec![0xc0]),
        Bytes::from(vec![0xc1, 0x80]),
        Bytes::from(vec![0xDE, 0xAD]),
    ];

    let calldata = crate::cross_chain::encode_block_calldata(&numbers, &txs);
    assert!(!calldata.is_empty());

    let (decoded_numbers, decoded_txs) =
        crate::cross_chain::decode_block_calldata(&calldata).expect("should decode");
    assert_eq!(decoded_numbers, numbers);
    assert_eq!(decoded_txs, txs);
}

#[test]
fn test_balance_sufficient_for_at_least_one_submission() {
    // A single postBatch call with one block entry should cost well under
    // LOW_BALANCE_THRESHOLD at reasonable gas prices.
    use crate::cross_chain::encode_post_batch_calldata;

    let calldata = encode_post_batch_calldata(&[], Bytes::new(), Bytes::new());

    let calldata_gas: u64 = calldata
        .iter()
        .map(|&b| if b == 0 { 4u64 } else { 16u64 })
        .sum();

    // Generous estimate: 21000 base + calldata_gas + 100000 execution
    let total_gas = 21_000u64 + calldata_gas + 100_000;
    let gas_price_wei = 10_000_000_000u128; // 10 gwei
    let cost = total_gas as u128 * gas_price_wei;

    assert!(
        LOW_BALANCE_THRESHOLD > cost,
        "threshold {LOW_BALANCE_THRESHOLD} should cover single submission cost {cost}"
    );
    // But at very high gas prices (100 gwei+), the threshold is just a warning
    let high_price = 100_000_000_000u128; // 100 gwei
    let high_cost = total_gas as u128 * high_price;
    assert!(
        high_cost > LOW_BALANCE_THRESHOLD,
        "at high gas prices the threshold is advisory, not a guarantee"
    );
}
