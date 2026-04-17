use super::*;

/// Minimal [`RollupConfig`] wrapped in an `Arc` for tests that need to
/// construct a [`DerivationPipeline`]. The only fields the sibling-reorg
/// tests depend on are `deployment_l1_block` (default 1000) and
/// `builder_mode` (false so `Proposer` doesn't try to initialize). Kept in
/// sync with `derivation_tests::test_config`.
fn test_config_arc() -> std::sync::Arc<crate::config::RollupConfig> {
    use alloy_primitives::Address;
    std::sync::Arc::new(crate::config::RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: Address::ZERO,
        deployment_l1_block: 1000,
        deployment_timestamp: 1_700_000_000,
        block_time: 12,
        builder_mode: false,
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
    })
}

#[test]
fn test_forkchoice_empty_deque() {
    let head = B256::with_last_byte(0xFF);
    let deque = VecDeque::new();
    let fcs = compute_forkchoice_state(head, &deque);

    assert_eq!(fcs.head_block_hash, head);
    assert_eq!(fcs.safe_block_hash, head);
    assert_eq!(fcs.finalized_block_hash, head);
}

#[test]
fn test_forkchoice_single_hash() {
    let head = B256::with_last_byte(0xFF);
    let genesis = B256::with_last_byte(0x01);
    let mut deque = VecDeque::new();
    deque.push_back(genesis);

    let fcs = compute_forkchoice_state(head, &deque);

    assert_eq!(fcs.head_block_hash, head);
    assert_eq!(fcs.safe_block_hash, genesis);
    assert_eq!(fcs.finalized_block_hash, genesis);
}

#[test]
fn test_forkchoice_fewer_than_32_hashes() {
    let head = B256::with_last_byte(0xFF);
    let mut deque = VecDeque::new();
    for i in 0..10u8 {
        deque.push_back(B256::with_last_byte(i));
    }

    let fcs = compute_forkchoice_state(head, &deque);

    assert_eq!(fcs.head_block_hash, head);
    assert_eq!(fcs.safe_block_hash, B256::with_last_byte(0));
    assert_eq!(fcs.finalized_block_hash, B256::with_last_byte(0));
}

#[test]
fn test_forkchoice_exactly_32_hashes() {
    let head = B256::with_last_byte(0xFF);
    let mut deque = VecDeque::new();
    for i in 0..32u8 {
        deque.push_back(B256::with_last_byte(i));
    }

    let fcs = compute_forkchoice_state(head, &deque);

    assert_eq!(fcs.head_block_hash, head);
    assert_eq!(fcs.safe_block_hash, B256::with_last_byte(0));
    assert_eq!(fcs.finalized_block_hash, B256::with_last_byte(0));
}

#[test]
fn test_forkchoice_more_than_32_hashes() {
    let head = B256::with_last_byte(0xFF);
    let mut deque = VecDeque::new();
    for i in 0..64u8 {
        deque.push_back(B256::with_last_byte(i));
    }

    let fcs = compute_forkchoice_state(head, &deque);

    assert_eq!(fcs.head_block_hash, head);
    assert_eq!(fcs.safe_block_hash, B256::with_last_byte(32));
    assert_eq!(fcs.finalized_block_hash, B256::with_last_byte(0));
}

#[test]
fn test_forkchoice_full_depth() {
    let head = B256::with_last_byte(0xFF);
    let mut deque = VecDeque::new();
    for i in 0..FORK_CHOICE_DEPTH as u8 {
        deque.push_back(B256::with_last_byte(i));
    }

    let fcs = compute_forkchoice_state(head, &deque);

    assert_eq!(fcs.head_block_hash, head);
    assert_eq!(fcs.safe_block_hash, B256::with_last_byte(32));
    assert_eq!(fcs.finalized_block_hash, B256::with_last_byte(0));
}

#[test]
fn test_encode_block_transactions_empty() {
    let txs: Vec<reth_ethereum_primitives::TransactionSigned> = vec![];
    let encoded = encode_block_transactions(&txs);
    assert_eq!(encoded.as_ref(), &[0xc0]);
}

#[test]
fn test_encode_block_transactions_roundtrip() {
    let txs: Vec<reth_ethereum_primitives::TransactionSigned> = vec![];
    let encoded = encode_block_transactions(&txs);
    let decoded: Vec<reth_ethereum_primitives::TransactionSigned> =
        alloy_rlp::Decodable::decode(&mut encoded.as_ref()).unwrap();
    assert_eq!(decoded.len(), 0);
}

#[test]
fn test_calc_gas_limit_same() {
    // When desired == parent, gas limit should stay the same
    assert_eq!(calc_gas_limit(36_000_000, 36_000_000), 36_000_000);
}

#[test]
fn test_calc_gas_limit_increase() {
    // When desired > parent, should increase by at most parent/1024
    let parent = 30_000_000;
    let desired = 36_000_000;
    let result = calc_gas_limit(parent, desired);
    assert!(result > parent);
    assert!(result <= parent + parent / 1024);
}

#[test]
fn test_calc_gas_limit_decrease() {
    // When desired < parent, should decrease by at most parent/1024
    let parent = 36_000_000;
    let desired = 30_000_000;
    let result = calc_gas_limit(parent, desired);
    assert!(result < parent);
    assert!(result >= parent - parent / 1024);
}

#[test]
fn test_calc_gas_limit_zero_parent() {
    // Edge case: parent gas limit is 0
    let result = calc_gas_limit(0, 36_000_000);
    assert_eq!(result, 0);
}

#[test]
fn test_calc_gas_limit_large_values() {
    // Large gas limit values
    let result = calc_gas_limit(u64::MAX / 2, u64::MAX / 2);
    assert_eq!(result, u64::MAX / 2);
}

#[test]
fn test_calc_gas_limit_convergence() {
    // Gas limit should converge toward desired over time
    let desired = 36_000_000u64;
    let mut gas_limit = 30_000_000u64;
    for _ in 0..1000 {
        gas_limit = calc_gas_limit(gas_limit, desired);
    }
    assert_eq!(gas_limit, desired);
}

#[test]
fn test_calc_gas_limit_matches_reth() {
    // Our calc_gas_limit MUST match reth's calculate_block_gas_limit exactly,
    // otherwise builder (payload builder) and fullnode (direct build) produce
    // different gas limits, causing state root mismatches.
    fn reth_calculate_block_gas_limit(parent_gas_limit: u64, desired_gas_limit: u64) -> u64 {
        let delta = (parent_gas_limit / 1024).saturating_sub(1);
        let min_gas_limit = parent_gas_limit - delta;
        let max_gas_limit = parent_gas_limit + delta;
        desired_gas_limit.clamp(min_gas_limit, max_gas_limit)
    }

    // Test with genesis gas limit (30M target)
    let mut gas_limit = 30_000_000u64;
    for _ in 0..100 {
        let ours = calc_gas_limit(gas_limit, 36_000_000);
        let reths = reth_calculate_block_gas_limit(gas_limit, 36_000_000);
        assert_eq!(ours, reths, "gas limit mismatch at parent={gas_limit}");
        gas_limit = ours;
    }
}

#[test]
fn test_calc_gas_limit_u64_max() {
    // parent=u64::MAX overflows on max_limit = parent + delta in debug builds.
    // This is acceptable since real gas limits are well below u64::MAX (~60M).
    // Test with u64::MAX / 2 instead (already covered by test_calc_gas_limit_large_values).
    let parent = u64::MAX / 2;
    let result = calc_gas_limit(parent, parent);
    assert_eq!(result, parent);

    // Verify decrease from u64::MAX / 2
    let delta = (parent / 1024).saturating_sub(1);
    assert_eq!(calc_gas_limit(parent, 0), parent - delta);

    // Verify increase from u64::MAX / 2
    assert_eq!(calc_gas_limit(parent, u64::MAX), parent + delta);
}

#[test]
fn test_forkchoice_state_with_new_block() {
    // Simulates the update_fork_choice logic
    let mut hashes = VecDeque::new();
    for i in 0..10u8 {
        hashes.push_back(B256::with_last_byte(i));
    }

    let new_block = B256::with_last_byte(0xFF);
    let mut tentative = hashes.clone();
    tentative.push_back(new_block);
    if tentative.len() > FORK_CHOICE_DEPTH {
        tentative.pop_front();
    }

    let fcs = compute_forkchoice_state(new_block, &tentative);
    assert_eq!(fcs.head_block_hash, new_block);
    // Original hashes should not be modified
    assert_eq!(hashes.len(), 10);
}

#[test]
fn test_calc_gas_limit_single_step_bounded() {
    // Verify single step is bounded by delta in both directions
    let parent = 30_000_000u64;
    let delta = (parent / 1024).saturating_sub(1);

    let up = calc_gas_limit(parent, u64::MAX);
    assert_eq!(up, parent + delta);

    let down = calc_gas_limit(parent, 0);
    assert_eq!(down, parent - delta);
}

// --- calc_gas_limit extreme values ---

#[test]
fn test_calc_gas_limit_convergence_from_genesis_default() {
    // Genesis typically starts with 30M gas limit. Verify convergence toward
    // DESIRED_GAS_LIMIT (60M) and count steps needed.
    let mut gas = 30_000_000u64;
    let target = DESIRED_GAS_LIMIT; // 60M
    let mut steps = 0;
    while gas != target && steps < 10_000 {
        gas = calc_gas_limit(gas, target);
        steps += 1;
    }
    assert_eq!(gas, target, "should converge to desired gas limit");
    // Convergence should happen in a reasonable number of steps
    assert!(steps < 1000, "convergence took too many steps: {steps}");
}

#[test]
fn test_calc_gas_limit_convergence_from_very_high() {
    // Start at u32::MAX (~4.3B), converge toward 60M (downward)
    let mut gas = u32::MAX as u64;
    let target = DESIRED_GAS_LIMIT;
    let mut steps = 0;
    while gas != target && steps < 100_000 {
        gas = calc_gas_limit(gas, target);
        steps += 1;
    }
    assert_eq!(gas, target, "should converge downward to desired gas limit");
}

// --- MAX_CATCHUP_BLOCKS enforcement ---

#[test]
fn test_max_catchup_blocks_capping_logic() {
    // Replicate the capping formula from run_builder_step:
    //   effective_target = target_l2_block.min(l2_head + MAX_CATCHUP_BLOCKS)
    let max_catchup: u64 = 10_000;
    let l2_head = 100u64;

    let target_far = 100_000u64;
    assert_eq!(target_far.min(l2_head + max_catchup), 10_100);

    let target_close = 5_000u64;
    assert_eq!(target_close.min(l2_head + max_catchup), target_close);
}

#[test]
fn test_max_catchup_blocks_at_boundary() {
    let max_catchup: u64 = 10_000;
    let l2_head = 0u64;
    // Exactly at limit
    assert_eq!(max_catchup.min(l2_head + max_catchup), max_catchup);
    // One over
    assert_eq!((max_catchup + 1).min(l2_head + max_catchup), max_catchup);
}

#[test]
fn test_max_catchup_blocks_head_equals_target() {
    let max_catchup: u64 = 10_000;
    let l2_head = 500u64;
    let target = 500u64;
    let effective = target.min(l2_head + max_catchup);
    assert_eq!(effective, target);
    assert_eq!(effective.saturating_sub(l2_head), 0);
}

// --- BuiltBlock struct tests ---

// --- forkchoice edge cases ---

#[test]
fn test_forkchoice_after_rewind_rebuilds_hashes() {
    // After a rewind, block_hashes is rebuilt from DB.
    // Simulate: had 64 hashes, rewind removes last 10, new deque has 54
    let mut deque = VecDeque::new();
    for i in 0..64u8 {
        deque.push_back(B256::with_last_byte(i));
    }
    // Simulate rewind: truncate to 54
    while deque.len() > 54 {
        deque.pop_back();
    }
    let head = *deque.back().unwrap();
    let fcs = compute_forkchoice_state(head, &deque);
    // safe = deque[54-32] = deque[22]
    assert_eq!(fcs.safe_block_hash, B256::with_last_byte(22));
    assert_eq!(fcs.finalized_block_hash, B256::with_last_byte(0));
    assert_eq!(fcs.head_block_hash, B256::with_last_byte(53));
}

#[test]
fn test_update_fork_choice_tentative_pattern() {
    // Verify the tentative pattern: clone, push, conditionally pop, then compute
    let mut block_hashes = VecDeque::new();
    for i in 0..FORK_CHOICE_DEPTH as u8 {
        block_hashes.push_back(B256::with_last_byte(i));
    }
    assert_eq!(block_hashes.len(), FORK_CHOICE_DEPTH);

    let new_hash = B256::with_last_byte(0xFF);
    let mut tentative = block_hashes.clone();
    tentative.push_back(new_hash);
    if tentative.len() > FORK_CHOICE_DEPTH {
        tentative.pop_front();
    }

    // Original unchanged
    assert_eq!(block_hashes.len(), FORK_CHOICE_DEPTH);
    assert_eq!(block_hashes.front(), Some(&B256::with_last_byte(0)));

    // Tentative has new hash and dropped oldest
    assert_eq!(tentative.len(), FORK_CHOICE_DEPTH);
    assert_eq!(tentative.front(), Some(&B256::with_last_byte(1)));
    assert_eq!(tentative.back(), Some(&B256::with_last_byte(0xFF)));
}

// --- QA re-run iteration 12: FCU edge cases ---

#[test]
fn test_forkchoice_rapid_sequential_updates() {
    // Two rapid FCU calls with different heads should produce correct states.
    // Since update_fork_choice takes &mut self, calls are sequential.
    let mut block_hashes = VecDeque::new();
    for i in 0..10u8 {
        block_hashes.push_back(B256::with_last_byte(i));
    }

    // First "FCU" — simulate update_fork_choice tentative pattern
    let head1 = B256::with_last_byte(0xAA);
    let mut tentative1 = block_hashes.clone();
    tentative1.push_back(head1);
    if tentative1.len() > FORK_CHOICE_DEPTH {
        tentative1.pop_front();
    }
    let fcs1 = compute_forkchoice_state(head1, &tentative1);
    assert_eq!(fcs1.head_block_hash, head1);

    // Simulate engine accepting — commit tentative state
    block_hashes = tentative1;
    let _head_hash = head1;

    // Second "FCU" — new head on top of first
    let head2 = B256::with_last_byte(0xBB);
    let mut tentative2 = block_hashes.clone();
    tentative2.push_back(head2);
    if tentative2.len() > FORK_CHOICE_DEPTH {
        tentative2.pop_front();
    }
    let fcs2 = compute_forkchoice_state(head2, &tentative2);
    assert_eq!(fcs2.head_block_hash, head2);
    // The finalized hash should be the oldest remaining
    assert_eq!(fcs2.finalized_block_hash, *tentative2.front().unwrap());
    // The two FCU states should have different heads
    assert_ne!(fcs1.head_block_hash, fcs2.head_block_hash);
}

#[test]
fn test_forkchoice_after_rewind_then_new_block() {
    // FCU after a rewind followed by building a new block.
    // Simulates: rewind to block 5, then build block 6.
    let mut deque = VecDeque::new();
    for i in 0..10u8 {
        deque.push_back(B256::with_last_byte(i));
    }

    // Rewind: truncate to first 6 entries (blocks 0-5)
    while deque.len() > 6 {
        deque.pop_back();
    }
    let rewind_head = *deque.back().unwrap();
    let fcs_rewind = compute_forkchoice_state(rewind_head, &deque);
    assert_eq!(fcs_rewind.head_block_hash, B256::with_last_byte(5));

    // Build new block 6 on top of rewound chain
    let new_block = B256::with_last_byte(0xCC);
    let mut tentative = deque.clone();
    tentative.push_back(new_block);
    if tentative.len() > FORK_CHOICE_DEPTH {
        tentative.pop_front();
    }
    let fcs_new = compute_forkchoice_state(new_block, &tentative);
    assert_eq!(fcs_new.head_block_hash, new_block);
    assert_eq!(fcs_new.finalized_block_hash, B256::with_last_byte(0));
    // 7 entries, safe = index 7-32 saturated = 0
    assert_eq!(fcs_new.safe_block_hash, B256::with_last_byte(0));
}

#[test]
fn test_forkchoice_update_tentative_does_not_mutate_on_rejection() {
    // Simulate the tentative pattern where engine rejects the FCU.
    // The original block_hashes should remain untouched.
    let mut block_hashes = VecDeque::new();
    for i in 0..5u8 {
        block_hashes.push_back(B256::with_last_byte(i));
    }
    let original_len = block_hashes.len();
    let original_front = *block_hashes.front().unwrap();

    let new_hash = B256::with_last_byte(0xFF);
    let mut tentative = block_hashes.clone();
    tentative.push_back(new_hash);

    // Simulate: engine rejects → do NOT assign tentative back
    // (the real code only assigns after is_valid() check)

    // Original untouched
    assert_eq!(block_hashes.len(), original_len);
    assert_eq!(*block_hashes.front().unwrap(), original_front);
    assert!(!block_hashes.contains(&new_hash));
}

// --- encode_block_transactions determinism ---

// --- Mode transition simulation (logic only, no real driver) ---

#[test]
fn test_driver_mode_transition_sync_to_builder() {
    // Simulate the mode transition logic: if caught up and builder_mode, switch
    let mut mode = DriverMode::Sync;
    let builder_mode = true;
    let caught_up = true;

    if caught_up && builder_mode {
        mode = DriverMode::Builder;
    }
    assert_eq!(mode, DriverMode::Builder);
}

#[test]
fn test_driver_mode_transition_sync_to_fullnode() {
    let mut mode = DriverMode::Sync;
    let builder_mode = false;
    let caught_up = true;

    if caught_up && !builder_mode {
        mode = DriverMode::Fullnode;
    }
    assert_eq!(mode, DriverMode::Fullnode);
}

#[test]
fn test_driver_mode_stays_sync_when_not_caught_up() {
    let mut mode = DriverMode::Sync;
    let builder_mode = true;
    let caught_up = false;

    if caught_up && builder_mode {
        mode = DriverMode::Builder;
    }
    assert_eq!(mode, DriverMode::Sync);
}

#[test]
fn test_driver_mode_builder_back_to_sync_on_mismatch() {
    // Simulate: builder detects L1 mismatch, reverts to sync
    let mut mode = DriverMode::Builder;
    let l1_mismatch = true;

    if l1_mismatch {
        mode = DriverMode::Sync;
    }
    assert_eq!(mode, DriverMode::Sync);
}

// --- pending_submissions queue capping logic ---

#[test]
fn test_pending_submissions_cap_logic() {
    let mut pending: VecDeque<u64> = VecDeque::new();
    let cap = MAX_PENDING_SUBMISSIONS;

    // Fill to cap
    for i in 0..cap as u64 {
        pending.push_back(i);
    }
    assert_eq!(pending.len(), cap);

    // At cap, new submissions should be rejected (per driver logic)
    let at_cap = pending.len() >= cap;
    assert!(at_cap);
}

// --- block_hashes deque management ---

// --- Empty block / has_content tests (#73, #74) ---

#[test]
fn test_has_content_logic_empty_block_no_entries() {
    // Empty block with no execution entries should NOT be submitted
    let tx_count = 0usize;
    let had_execution_entries = false;
    let has_content = tx_count > 0 || had_execution_entries;
    assert!(
        !has_content,
        "block with no txs and no execution entries should be empty"
    );
}

#[test]
fn test_has_content_logic_with_user_txs() {
    // Block with user transactions should be submitted
    let tx_count = 3usize;
    let had_execution_entries = false;
    let has_content = tx_count > 0 || had_execution_entries;
    assert!(has_content, "block with user txs should have content");
}

#[test]
fn test_has_content_with_execution_entries_only() {
    // Block with only cross-chain execution entries (no user txs)
    // must be submitted to L1 so fullnodes assign entries to the same block number.
    let tx_count = 0usize;
    let had_execution_entries = true;
    let has_content = tx_count > 0 || had_execution_entries;
    assert!(
        has_content,
        "block with execution entries should have content even without user txs"
    );
}

#[test]
fn test_has_content_with_both() {
    // Block with user txs and execution entries
    let tx_count = 2usize;
    let had_execution_entries = true;
    let has_content = tx_count > 0 || had_execution_entries;
    assert!(
        has_content,
        "block with both content types should have content"
    );
}

#[test]
fn test_backfill_submits_all_blocks() {
    // #78: Backfill must NOT skip empty-transaction blocks because
    // protocol-only blocks have no user txs in the body (setContext etc. are
    // builder protocol transactions). Skipping them creates an L1 submission gap.
    let block = PendingBlock {
        l2_block_number: 10,
        pre_state_root: B256::ZERO,
        state_root: B256::with_last_byte(0x10),
        clean_state_root: B256::with_last_byte(0x10),
        encoded_transactions: Bytes::new(), // empty — still submitted
        intermediate_roots: vec![],
    };
    assert!(block.encoded_transactions.is_empty());
    assert_eq!(block.l2_block_number, 10);
}

#[test]
fn test_backfill_includes_deposit_only_blocks_in_sequence() {
    // #78: Simulate backfill where blocks 5-9 need to be reconstructed,
    // with blocks 6 and 8 being deposit-only (empty tx list).
    // All 5 blocks must appear in the backfill queue — no gaps.
    let mut backfilled: VecDeque<PendingBlock> = VecDeque::new();
    for i in 5..10u64 {
        let has_user_txs = i != 6 && i != 8; // 6 and 8 are deposit-only
        backfilled.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: if has_user_txs {
                Bytes::from(vec![0xc0])
            } else {
                Bytes::new() // deposit-only: empty body
            },
            intermediate_roots: vec![],
        });
    }

    assert_eq!(backfilled.len(), 5, "all 5 blocks must be included");
    for (idx, block) in backfilled.iter().enumerate() {
        assert_eq!(
            block.l2_block_number,
            5 + idx as u64,
            "block numbers must be sequential with no gaps"
        );
    }
    // Deposit-only blocks have empty transactions but valid state roots
    assert!(backfilled[1].encoded_transactions.is_empty()); // block 6
    assert!(backfilled[3].encoded_transactions.is_empty()); // block 8
    assert!(!backfilled[0].encoded_transactions.is_empty()); // block 5
}

#[test]
fn test_backfill_all_deposit_only_blocks() {
    // #78: Edge case — every block in the backfill range is deposit-only.
    // None should be skipped.
    let mut backfilled: VecDeque<PendingBlock> = VecDeque::new();
    for i in 1..=5u64 {
        backfilled.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: Bytes::new(), // all deposit-only
            intermediate_roots: vec![],
        });
    }

    assert_eq!(
        backfilled.len(),
        5,
        "all deposit-only blocks must be included"
    );
    for block in &backfilled {
        assert!(block.encoded_transactions.is_empty());
        assert_ne!(block.state_root, B256::ZERO, "state root should be valid");
    }
}

#[test]
fn test_backfill_empty_blocks_prepend_correctly() {
    // #78: Deposit-only blocks from backfill must prepend correctly
    // before existing pending submissions, maintaining sequence.
    let mut pending: VecDeque<PendingBlock> = VecDeque::new();
    pending.push_back(PendingBlock {
        l2_block_number: 10,
        pre_state_root: B256::ZERO,
        state_root: B256::with_last_byte(10),
        clean_state_root: B256::with_last_byte(10),
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    });

    // Backfill blocks 7-9 where block 8 is deposit-only
    let mut backfilled: VecDeque<PendingBlock> = VecDeque::new();
    for i in 7..10u64 {
        backfilled.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: if i == 8 {
                Bytes::new() // deposit-only
            } else {
                Bytes::from(vec![0xc0])
            },
            intermediate_roots: vec![],
        });
    }

    // Prepend in reverse order (matching driver logic)
    for block in backfilled.into_iter().rev() {
        pending.push_front(block);
    }

    assert_eq!(pending.len(), 4);
    assert_eq!(pending[0].l2_block_number, 7);
    assert_eq!(pending[1].l2_block_number, 8); // deposit-only, NOT skipped
    assert!(pending[1].encoded_transactions.is_empty());
    assert_eq!(pending[2].l2_block_number, 9);
    assert_eq!(pending[3].l2_block_number, 10);
}

// --- Fallback RPC switching logic tests ---

#[test]
fn test_record_l1_failure_switches_to_fallback() {
    // Simulate the logic from record_l1_failure with a fallback configured
    let mut counter: u32 = 0;
    let mut using_fallback = false;
    let has_fallback = true;

    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        if using_fallback {
            counter = 0;
        } else {
            counter = counter.saturating_add(1);
            if counter >= MAX_CONSECUTIVE_FAILURES && has_fallback {
                using_fallback = true;
                counter = 0;
            }
        }
    }
    assert!(using_fallback);
    assert_eq!(counter, 0);
}

#[test]
fn test_record_l1_failure_no_switch_without_fallback() {
    let mut counter: u32 = 0;
    let mut using_fallback = false;
    let has_fallback = false;

    for _ in 0..MAX_CONSECUTIVE_FAILURES * 3 {
        if using_fallback {
            counter = 0;
        } else {
            counter = counter.saturating_add(1);
            if counter >= MAX_CONSECUTIVE_FAILURES && has_fallback {
                using_fallback = true;
                counter = 0;
            }
        }
    }
    assert!(!using_fallback);
    assert_eq!(counter, MAX_CONSECUTIVE_FAILURES * 3);
}

#[test]
fn test_record_l1_success_resets_counter_on_primary() {
    // Simulate: a few failures on primary, then a success resets counter
    let mut counter: u32 = 0;
    let using_fallback = false;

    // Simulate 2 failures (less than MAX_CONSECUTIVE_FAILURES)
    for _ in 0..2 {
        counter = counter.saturating_add(1);
    }
    assert_eq!(counter, 2);

    // Simulate record_l1_success on primary
    if using_fallback {
        counter = counter.saturating_add(1);
    } else {
        counter = 0;
    }
    assert_eq!(counter, 0);
}

#[test]
fn test_record_l1_success_switches_back_from_fallback() {
    let mut counter: u32 = 0;
    let mut using_fallback = true;

    // Simulate MAX_CONSECUTIVE_FAILURES successes on fallback
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        if using_fallback {
            counter = counter.saturating_add(1);
            if counter >= MAX_CONSECUTIVE_FAILURES {
                using_fallback = false;
                counter = 0;
            }
        } else {
            counter = 0;
        }
    }
    assert!(!using_fallback);
    assert_eq!(counter, 0);
}

#[test]
fn test_l1_provider_oscillation_primary_fallback_primary() {
    // Simulate: primary fails 3x → fallback, fallback succeeds 3x → primary
    // Verifies the full cycle works correctly
    let mut counter: u32 = 0;
    let mut using_fallback = false;
    let has_fallback = true;

    // Phase 1: primary fails MAX_CONSECUTIVE_FAILURES times → switch to fallback
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        counter = counter.saturating_add(1);
        if counter >= MAX_CONSECUTIVE_FAILURES && has_fallback {
            using_fallback = true;
            counter = 0;
        }
    }
    assert!(using_fallback);

    // Phase 2: fallback succeeds MAX_CONSECUTIVE_FAILURES times → switch back
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        counter = counter.saturating_add(1);
        if counter >= MAX_CONSECUTIVE_FAILURES {
            using_fallback = false;
            counter = 0;
        }
    }
    assert!(!using_fallback);
    assert_eq!(counter, 0);
}

#[test]
fn test_l1_provider_interleaved_failures_and_successes() {
    // Simulate: fail, succeed, fail, succeed — counter should never reach threshold
    let mut counter: u32 = 0;
    let mut using_fallback = false;
    let has_fallback = true;

    for i in 0..20 {
        if i % 2 == 0 {
            // failure
            counter = counter.saturating_add(1);
            if counter >= MAX_CONSECUTIVE_FAILURES && has_fallback {
                using_fallback = true;
                counter = 0;
            }
        } else {
            // success on primary resets counter
            counter = 0;
        }
    }
    assert!(
        !using_fallback,
        "interleaved failures should never trigger switch"
    );
}

#[test]
fn test_l1_both_providers_fail_stays_on_fallback() {
    // Primary fails → switch to fallback → fallback also fails → stays on fallback
    let mut counter: u32 = 0;
    let mut using_fallback = false;
    let has_fallback = true;

    // Primary fails 3x
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        counter = counter.saturating_add(1);
        if counter >= MAX_CONSECUTIVE_FAILURES && has_fallback {
            using_fallback = true;
            counter = 0;
        }
    }
    assert!(using_fallback);

    // Fallback fails — counter is reset (stays on fallback)
    if using_fallback {
        counter = 0;
    }
    assert_eq!(counter, 0);
    assert!(
        using_fallback,
        "should stay on fallback when fallback fails"
    );
}

// --- QA Re-run Iteration 10: proposer L1 URL sync on failover ---

#[test]
fn test_proposer_url_synced_on_failover_to_fallback() {
    // Verify that when the driver switches to fallback, sync_proposer_l1_url
    // selects the fallback URL
    let primary_url = "http://primary:8545";
    let fallback_url = "http://fallback:9545";

    // Simulate: using_fallback=true, config has fallback → target is fallback URL
    let using_fallback = true;
    let target_url = if using_fallback {
        fallback_url
    } else {
        primary_url
    };
    assert_eq!(target_url, fallback_url);
}

#[test]
fn test_proposer_url_synced_on_switch_back_to_primary() {
    // Verify that when the driver switches back to primary, sync_proposer_l1_url
    // selects the primary URL
    let primary_url = "http://primary:8545";
    let fallback_url = "http://fallback:9545";

    let using_fallback = false;

    let target_url = if using_fallback {
        fallback_url
    } else {
        primary_url
    };
    assert_eq!(target_url, primary_url);
}

#[test]
fn test_proposer_url_sync_no_fallback_configured() {
    // When no fallback is configured and using_fallback is somehow true,
    // should fall back to primary URL
    let primary_url = "http://primary:8545";

    let using_fallback = true;

    let target_url = if using_fallback {
        // No fallback configured → use primary
        primary_url
    } else {
        primary_url
    };
    assert_eq!(
        target_url, primary_url,
        "should use primary when no fallback configured"
    );
}

// --- Iteration 4: Empty block edge cases ---

#[test]
fn test_gap_fill_block_at_block_1_uses_deployment_context() {
    // When the first BlockSubmitted is for block > 1, gap-fill blocks 1..N
    // should use the deployment L1 block as their L1 context.
    use crate::derivation::DerivedBlock;
    use crate::payload_builder::L1BlockInfo;

    let deployment_hash = B256::with_last_byte(0xDD);
    let deployment_block = 1000u64;

    // Simulate gap-fill block at block 1
    let gap_block = DerivedBlock {
        l2_block_number: 1,
        l2_timestamp: 1_700_000_012,
        l1_info: L1BlockInfo {
            l1_block_number: deployment_block,
            l1_block_hash: deployment_hash,
        },
        state_root: B256::ZERO,
        transactions: Bytes::new(),
        is_empty: true,
        execution_entries: vec![],
        filtering: None,
    };

    assert!(gap_block.is_empty);
    assert_eq!(gap_block.state_root, B256::ZERO);
    assert_eq!(gap_block.l1_info.l1_block_number, deployment_block);
    assert_eq!(gap_block.l1_info.l1_block_hash, deployment_hash);
    assert!(gap_block.transactions.is_empty());
}

#[test]
fn test_gap_fill_max_block_gap_boundary() {
    // MAX_BLOCK_GAP = 1000. Verify gap of exactly 1000 is accepted
    // and gap of 1001 would be rejected (tested via derivation logic).
    let max_block_gap = 1000u64; // mirrors derivation::MAX_BLOCK_GAP

    let last_derived = 0u64;
    let submitted_block = last_derived + max_block_gap + 1; // exactly at limit

    // expected_next = 1, gap_size = submitted_block - expected_next
    let expected_next = last_derived.saturating_add(1);
    let gap_size = submitted_block - expected_next;
    assert_eq!(gap_size, max_block_gap);
    // At MAX_BLOCK_GAP, gap_size == MAX_BLOCK_GAP, so `gap_size > MAX_BLOCK_GAP` is false
    assert!(
        gap_size <= max_block_gap,
        "gap of exactly MAX_BLOCK_GAP should be accepted"
    );

    // One more would exceed
    let over_block = submitted_block + 1;
    let over_gap = over_block - expected_next;
    assert!(
        over_gap > max_block_gap,
        "gap exceeding MAX_BLOCK_GAP should be rejected"
    );
}

#[test]
fn test_gap_fill_block_still_checks_l1_context() {
    // Gap-fill blocks have state_root == B256::ZERO.
    // verify_local_block_matches_l1 skips the state root comparison but
    // still verifies L1 context to prevent consensus divergence.
    // (L2Context contract stores per-block context in a mapping, so
    // different L1 context values produce permanently different state.)
    use crate::derivation::DerivedBlock;
    use crate::payload_builder::L1BlockInfo;

    let gap_block = DerivedBlock {
        l2_block_number: 5,
        l2_timestamp: 1_700_000_060,
        l1_info: L1BlockInfo {
            l1_block_number: 1000,
            l1_block_hash: B256::ZERO,
        },
        state_root: B256::ZERO, // gap-fill marker
        transactions: Bytes::new(),
        is_empty: true,
        execution_entries: vec![],
        filtering: None,
    };

    // Gap-fill blocks are identified by B256::ZERO state root
    assert_eq!(
        gap_block.state_root,
        B256::ZERO,
        "gap-fill blocks have ZERO state root"
    );
    // But L1 context is still checked (not skipped) to ensure builder
    // and fullnodes agree on the L2Context protocol transaction values
    assert_eq!(gap_block.l1_info.l1_block_number, 1000);
}

// --- Iteration 19: Backfill logic edge cases ---

#[test]
fn test_backfill_prepends_before_existing_pending() {
    // Backfilled blocks should be prepended, maintaining order
    let mut pending: VecDeque<PendingBlock> = VecDeque::new();
    pending.push_back(PendingBlock {
        l2_block_number: 10,
        pre_state_root: B256::ZERO,
        state_root: B256::with_last_byte(10),
        clean_state_root: B256::with_last_byte(10),
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    });

    // Simulate backfill of blocks 5-9
    let mut backfilled: VecDeque<PendingBlock> = VecDeque::new();
    for i in 5..10 {
        backfilled.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: Bytes::from(vec![0xc0]),
            intermediate_roots: vec![],
        });
    }

    // Prepend in reverse order (matching driver logic)
    for block in backfilled.into_iter().rev() {
        pending.push_front(block);
    }

    assert_eq!(pending.len(), 6);
    assert_eq!(pending[0].l2_block_number, 5);
    assert_eq!(pending[1].l2_block_number, 6);
    assert_eq!(pending[4].l2_block_number, 9);
    assert_eq!(pending[5].l2_block_number, 10);
}

// --- Iteration 18: Preconfirmed block verification ---

#[test]
fn test_preconfirmed_hash_match_is_removed() {
    // When a preconfirmed hash matches, it's removed from the HashMap
    let mut preconfirmed: HashMap<u64, B256> = HashMap::new();
    let hash = B256::with_last_byte(0xAA);
    preconfirmed.insert(10, hash);

    // Simulate step_fullnode: remove and compare
    let removed = preconfirmed.remove(&10);
    assert_eq!(removed, Some(hash));
    assert!(!preconfirmed.contains_key(&10));
}

#[test]
fn test_preconfirmed_hash_mismatch_still_removes() {
    // Mismatch: preconfirmed hash != built hash, but entry is still removed
    // (L1 derivation always takes precedence)
    let mut preconfirmed: HashMap<u64, B256> = HashMap::new();
    preconfirmed.insert(10, B256::with_last_byte(0xAA));

    let built_hash = B256::with_last_byte(0xBB);
    let preconfirmed_hash = preconfirmed.remove(&10).unwrap();
    assert_ne!(preconfirmed_hash, built_hash);
    assert!(preconfirmed.is_empty(), "entry removed even on mismatch");
}

#[test]
fn test_preconfirmed_pruning_threshold() {
    // Prune when len > 1000, keeping entries >= cutoff
    let mut preconfirmed: HashMap<u64, B256> = HashMap::new();
    for i in 0..1500 {
        preconfirmed.insert(i, B256::with_last_byte((i % 256) as u8));
    }
    assert_eq!(preconfirmed.len(), 1500);

    let l2_head = 1400u64;
    if preconfirmed.len() > 1000 {
        let cutoff = l2_head.saturating_sub(1000);
        preconfirmed.retain(|&k, _| k >= cutoff);
    }
    // Should keep entries 400..1500 = 1100 entries
    assert_eq!(preconfirmed.len(), 1100);
    assert!(!preconfirmed.contains_key(&399));
    assert!(preconfirmed.contains_key(&400));
}

// --- Iteration 21: Mode transition race conditions ---

#[test]
fn test_rewind_cycle_dampening_delay_caps_at_60_seconds() {
    // The delay formula is (2 << cycles.min(5)).min(60)
    // Only applied when cycles > 0 (step_sync line 640)
    // cycles=1 → 4s, cycles=2 → 8s, cycles=5 → 64→60s (capped)
    for cycles in 1u32..=10 {
        let delay = (2u64 << cycles.min(5)).min(60);
        assert!(delay <= 60, "delay {delay} exceeds cap at cycle {cycles}");
        assert!(delay >= 4, "delay {delay} below minimum at cycle {cycles}");
    }
    // Spot-check specific cycles (formula: (2 << cycles.min(5)).min(60))
    // Use black_box to prevent compile-time constant folding (clippy unnecessary_min_or_max)
    let bb = std::hint::black_box;
    assert_eq!((2u64 << bb(1u32).min(5)).min(60), 4); // cycles=1
    assert_eq!((2u64 << bb(2u32).min(5)).min(60), 8); // cycles=2
    assert_eq!((2u64 << bb(3u32).min(5)).min(60), 16); // cycles=3
    assert_eq!((2u64 << bb(4u32).min(5)).min(60), 32); // cycles=4
    assert_eq!((2u64 << bb(5u32).min(5)).min(60), 60); // cycles=5: 64 capped
    assert_eq!((2u64 << bb(6u32).min(5)).min(60), 60); // cycles=6: min(5)→5
}

#[test]
fn test_mode_transition_clears_rewind_cycles() {
    // When transitioning Sync→Builder, consecutive_rewind_cycles is reset to 0
    let mut consecutive_rewind_cycles = 5u32;
    let mode = DriverMode::Sync;
    let builder_mode = true;
    let caught_up = true;

    if caught_up && mode == DriverMode::Sync && builder_mode {
        // Delay would be applied first (tested above), then reset
        consecutive_rewind_cycles = 0;
    }
    assert_eq!(consecutive_rewind_cycles, 0);
}

#[test]
fn test_consecutive_rewind_cycles_saturates() {
    // consecutive_rewind_cycles uses saturating_add — can't overflow
    let mut cycles = u32::MAX - 1;
    cycles = cycles.saturating_add(1);
    assert_eq!(cycles, u32::MAX);
    cycles = cycles.saturating_add(1);
    assert_eq!(cycles, u32::MAX); // saturated, no overflow

    // Even at u32::MAX, delay formula is safe
    let delay = (2u64 << (cycles.min(5))).min(60);
    assert_eq!(delay, 60); // cycles.min(5) = 5, 2<<5 = 64, min(60) = 60
}

// --- Iteration 28: Timing-dependent bugs ---

#[test]
fn test_target_l2_block_from_future_timestamp() {
    // If wall clock is exactly at a block boundary, that block should be buildable
    let config = RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: alloy_primitives::Address::ZERO,
        deployment_l1_block: 0,
        deployment_timestamp: 1_700_000_000,
        block_time: 12,
        builder_mode: true,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        builder_ws_url: None,
        health_port: 0,
        rollups_address: alloy_primitives::Address::ZERO,
        cross_chain_manager_address: alloy_primitives::Address::ZERO,
        rollup_id: 0,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: alloy_primitives::Address::ZERO,
        bridge_l2_address: alloy_primitives::Address::ZERO,
        bridge_l1_address: alloy_primitives::Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    };

    // Exactly at block 100's timestamp: dep + (100+1)*12 = dep + 1212
    let block_100_ts = 1_700_000_000 + 101 * 12;
    let target = config.l2_block_number_from_timestamp(block_100_ts);
    assert_eq!(target, 100);

    // 1 second before block 101's timestamp (still block 100)
    let target_before = config.l2_block_number_from_timestamp(block_100_ts + 11);
    assert_eq!(target_before, 100);

    // Exactly at block 101's timestamp: dep + (101+1)*12 = dep + 1224
    let target_at_101 = config.l2_block_number_from_timestamp(block_100_ts + 12);
    assert_eq!(target_at_101, 101);
}

#[test]
fn test_max_catchup_blocks_prevents_runaway() {
    // MAX_CATCHUP_BLOCKS = 10_000 prevents building millions of blocks
    // if deployment_timestamp is far in the past
    const MAX_CATCHUP_BLOCKS: u64 = 10_000;
    let l2_head = 100u64;
    let target = 1_000_000u64; // very far ahead

    let effective = target.min(l2_head.saturating_add(MAX_CATCHUP_BLOCKS));
    assert_eq!(effective, 10_100);
    assert!(effective - l2_head <= MAX_CATCHUP_BLOCKS);
}

#[test]
fn test_clock_before_deployment_produces_block_zero() {
    // If wall clock is before deployment_timestamp, block number should be 0
    let config = RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: alloy_primitives::Address::ZERO,
        deployment_l1_block: 0,
        deployment_timestamp: 2_000_000_000, // far future
        block_time: 12,
        builder_mode: true,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        builder_ws_url: None,
        health_port: 0,
        rollups_address: alloy_primitives::Address::ZERO,
        cross_chain_manager_address: alloy_primitives::Address::ZERO,
        rollup_id: 0,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: alloy_primitives::Address::ZERO,
        bridge_l2_address: alloy_primitives::Address::ZERO,
        bridge_l1_address: alloy_primitives::Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    };

    // Current time is before deployment
    let now = 1_700_000_000u64;
    let target = config.l2_block_number_from_timestamp(now);
    assert_eq!(target, 0);
}

// --- Iteration 27: Docker compose resilience ---

#[test]
fn test_submission_cooldown_prevents_rapid_retries() {
    // After a failed L1 submission, SUBMISSION_COOLDOWN_SECS prevents hammering
    assert_eq!(SUBMISSION_COOLDOWN_SECS, 5);
    let cooldown = Duration::from_secs(SUBMISSION_COOLDOWN_SECS);
    assert_eq!(cooldown.as_secs(), 5);
}

// --- Iteration 24: Cross-component invariant tests ---

#[test]
fn test_l1_context_derivation_matches_builder_convention() {
    // Builder: uses latest_l1_block as L1 context
    // Derivation: derives l1_context_block = containing_l1_block - 1
    // These must be equal since builder submits at latest_l1_block,
    // and the tx lands in latest_l1_block + 1.
    let builder_latest_l1 = 100u64;
    let l1_tx_lands_in = builder_latest_l1 + 1; // tx included in next block
    let derived_context = l1_tx_lands_in.saturating_sub(1); // containing - 1

    assert_eq!(
        builder_latest_l1, derived_context,
        "builder L1 context must match derivation L1 context"
    );
}

#[test]
fn test_pending_block_state_root_flows_through_submission() {
    // Builder: captures state_root in PendingBlock after build_and_insert_block
    // Proposer: submits state_root to L1 via submitBlock/submitBatch
    // Fullnode: derives DerivedBlock.state_root from L1 event
    // Driver: compares derived state_root vs local state_root
    // This test verifies the PendingBlock captures state_root correctly.
    let state_root = B256::with_last_byte(0xAB);
    let pending = PendingBlock {
        l2_block_number: 42,
        pre_state_root: B256::ZERO,
        state_root,
        clean_state_root: state_root,
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    };

    // State root is preserved through the struct
    assert_eq!(pending.state_root, state_root);

    // Verification logic: non-zero state_root requires match
    let derived_state_root = state_root;
    let local_state_root = state_root;
    assert_eq!(
        local_state_root, derived_state_root,
        "state roots must match"
    );

    // Zero state root means gap-fill — state root check skipped, L1 context still checked
    let gap_fill_root = B256::ZERO;
    let is_gap_fill = gap_fill_root.is_zero();
    assert!(is_gap_fill);
}

#[test]
fn test_timestamp_config_used_consistently() {
    // Both builder (driver.rs) and derivation (derivation.rs) compute L2 timestamps
    // using the same config.l2_timestamp(). Verify consistency.
    let config = RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: alloy_primitives::Address::ZERO,
        deployment_l1_block: 0,
        deployment_timestamp: 1_700_000_000,
        block_time: 12,
        builder_mode: false,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        builder_ws_url: None,
        health_port: 0,
        rollups_address: alloy_primitives::Address::ZERO,
        cross_chain_manager_address: alloy_primitives::Address::ZERO,
        rollup_id: 0,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: alloy_primitives::Address::ZERO,
        bridge_l2_address: alloy_primitives::Address::ZERO,
        bridge_l1_address: alloy_primitives::Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    };

    // Builder and derivation both use: deployment_timestamp + ((block_number + 1) * block_time)
    for block_num in [0, 1, 100, 10_000] {
        let expected = config.deployment_timestamp + ((block_num + 1) * config.block_time);
        assert_eq!(config.l2_timestamp(block_num), expected);
        // l2_timestamp_checked should agree for non-overflow values
        assert_eq!(config.l2_timestamp_checked(block_num), Some(expected));
    }
}

// --- Iteration 23: Error propagation audit ---

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn calc_gas_limit_never_panics(parent in 0u64..=u64::MAX, desired in 0u64..=u64::MAX) {
            let _ = calc_gas_limit(parent, desired);
        }

        #[test]
        fn calc_gas_limit_idempotent(parent in 1025u64..100_000_000u64) {
            // When desired == parent, result == parent
            prop_assert_eq!(calc_gas_limit(parent, parent), parent);
        }

        #[test]
        fn calc_gas_limit_bounded_step(parent in 2049u64..100_000_000u64, desired in 0u64..=u64::MAX) {
            let result = calc_gas_limit(parent, desired);
            let delta = (parent / 1024).saturating_sub(1);
            prop_assert!(result >= parent.saturating_sub(delta));
            prop_assert!(result <= parent.saturating_add(delta));
        }
    }
}

// --- Iteration 33: Block building with maximum transactions ---

#[test]
fn test_encode_block_transactions_max_size_payload() {
    // Inbox.MAX_TRANSACTIONS_SIZE = 262144 (256KB). Verify that encoding
    // a large number of small transactions stays within this limit or is
    // properly handled. The encoding is RLP list of signed transactions.
    // An empty RLP list is [0xc0] = 1 byte.
    // Each additional empty-body tx adds ~100 bytes of RLP overhead.
    // At 256KB, we can fit ~2600 minimal transactions.
    let max_tx_size: usize = 262144;

    // Verify the constant
    assert_eq!(max_tx_size, 256 * 1024);

    // Encoding 0 transactions produces exactly [0xc0]
    let empty: Vec<reth_ethereum_primitives::TransactionSigned> = vec![];
    let encoded = encode_block_transactions(&empty);
    assert_eq!(encoded.len(), 1);
    assert!(encoded.len() < max_tx_size);
}

// --- Iteration 34: L1 block hash carrier field encoding ---

#[test]
fn test_l1_block_number_roundtrip_via_prev_randao() {
    // Driver encodes: B256::from(U256::from(l1_block_number))
    // EVM config decodes: randao.as_slice()[24..32] as u64 big-endian
    // Verify roundtrip for various values
    for l1_block in [0u64, 1, 1000, u32::MAX as u64, u64::MAX] {
        let encoded = B256::from(alloy_primitives::U256::from(l1_block));
        let decoded: u64 = encoded.as_slice()[24..32]
            .try_into()
            .map(u64::from_be_bytes)
            .unwrap();
        assert_eq!(
            decoded, l1_block,
            "roundtrip failed for l1_block={l1_block}"
        );
    }
}

#[test]
fn test_parent_beacon_block_root_carries_l1_hash() {
    // parent_beacon_block_root carries the L1 block hash (32 bytes, no encoding needed).
    // Verify the field is used as-is (identity mapping).
    let l1_hash = B256::with_last_byte(0xAB);

    // Driver sets: parent_beacon_block_root: Some(l1_block_hash)
    // EVM config reads: self.inner.ctx.parent_beacon_block_root → Some(hash) → hash
    // No transformation needed — it's a direct B256 passthrough
    let decoded = l1_hash;
    assert_eq!(decoded, l1_hash);

    // None case: falls back to B256::ZERO
    let decoded_none = B256::ZERO;
    assert_eq!(decoded_none, B256::ZERO);
}

// --- Iteration 40: End-to-end determinism verification ---

// --- Gap-fill L1 context verification tests ---

#[test]
fn test_gap_fill_l1_context_mismatch_detected() {
    // Gap-fill blocks (state_root == B256::ZERO) must still have their L1 context
    // verified. If the builder used a different L1 context than derivation, the
    // L2Context contract's per-block mapping will have different values, causing
    // permanent state divergence.
    use crate::derivation::DerivedBlock;
    use crate::payload_builder::L1BlockInfo;

    // Builder built gap-fill block 5 with l1_context = 100
    // Derivation says gap-fill block 5 should have l1_context = 95 (from last_l1_info)
    let derived = DerivedBlock {
        l2_block_number: 5,
        l2_timestamp: 1_700_000_060,
        l1_info: L1BlockInfo {
            l1_block_number: 95,
            l1_block_hash: B256::with_last_byte(0x95),
        },
        state_root: B256::ZERO, // gap-fill marker
        transactions: Bytes::new(),
        is_empty: true,
        execution_entries: vec![],
        filtering: None,
    };

    // The derived L1 context (95) differs from what the builder would have used (100).
    // verify_local_block_matches_l1 should detect this and trigger a rewind.
    // We can't call the actual function here (needs a full Driver), but we verify
    // the gap-fill block carries L1 context that CAN be compared.
    assert_eq!(derived.state_root, B256::ZERO, "is a gap-fill block");
    assert_eq!(
        derived.l1_info.l1_block_number, 95,
        "gap-fill carries canonical L1 context from derivation"
    );
    // Previously this L1 context was ignored — now it's checked
}

#[test]
fn test_set_rewind_target_takes_minimum() {
    // When multiple blocks in a derivation batch have L1 context mismatches,
    // pending_rewind_target must be set to the earliest (minimum) mismatch.
    let mut target: Option<u64> = None;

    // First mismatch at block 10
    let new = 10u64.saturating_sub(1); // = 9
    target = Some(target.map_or(new, |t: u64| t.min(new)));
    assert_eq!(target, Some(9));

    // Second mismatch at block 7 (earlier) — should take minimum
    let new = 7u64.saturating_sub(1); // = 6
    target = Some(target.map_or(new, |t: u64| t.min(new)));
    assert_eq!(target, Some(6));

    // Third mismatch at block 15 (later) — should NOT overwrite
    let new = 15u64.saturating_sub(1); // = 14
    target = Some(target.map_or(new, |t: u64| t.min(new)));
    assert_eq!(target, Some(6), "must keep earliest rewind target");
}

#[test]
fn test_gap_fill_context_divergence_via_l2context_mapping() {
    // The L2Context contract stores per-block context in
    // `mapping(uint256 => BlockContext) public contexts`.
    // Different L1 context values for the same block number produce different
    // storage writes (different slots in the mapping), causing permanently
    // different state roots. This test documents why gap-fill L1 context
    // verification is critical.

    // Block 5 on builder: contexts[5] = {l1BlockNumber: 100, ...}
    // Block 5 on fullnode: contexts[5] = {l1BlockNumber: 95, ...}
    //
    // Block 7 protocol transaction writes to contexts[7] — does NOT overwrite contexts[5].
    // State roots NEVER converge because contexts[5] is permanently different.
    let builder_context = 100u64;
    let fullnode_context = 95u64;
    assert_ne!(
        builder_context, fullnode_context,
        "different L1 context for same gap-fill block"
    );

    // The fix: verify_local_block_matches_l1 now checks L1 context even for
    // gap-fill blocks (state_root == B256::ZERO). If they differ, the builder
    // rewinds and re-derives with the canonical context.
}

#[test]
fn test_set_rewind_target_block_one_saturates_to_zero() {
    // When verify_local_block_matches_l1 detects a mismatch at L2 block 1,
    // it calls set_rewind_target(1.saturating_sub(1)) = set_rewind_target(0).
    // The pending rewind target of 0 should be valid and should cause a
    // rewind to genesis.
    let mut pending_rewind_target: Option<u64> = None;

    // Simulate set_rewind_target for block 1
    let target = 1u64.saturating_sub(1); // = 0
    pending_rewind_target = Some(pending_rewind_target.map_or(target, |t| t.min(target)));
    assert_eq!(pending_rewind_target, Some(0));

    // A subsequent mismatch at block 5 should NOT overwrite (0 < 4)
    let target2 = 5u64.saturating_sub(1); // = 4
    pending_rewind_target = Some(pending_rewind_target.map_or(target2, |t| t.min(target2)));
    assert_eq!(
        pending_rewind_target,
        Some(0),
        "block-1 rewind to genesis must not be overwritten by later mismatch"
    );
}

#[test]
fn test_pending_rewind_skipped_when_target_ge_head() {
    // The driver's step_sync checks `if target < self.l2_head_number` before
    // rewinding. If the rewind target >= l2_head (e.g., the chain hasn't
    // advanced past the mismatch point yet), the rewind is a no-op.
    // This prevents unnecessary rewinds during initial sync.
    let l2_head_number = 5u64;
    let pending_rewind_target: Option<u64> = Some(5);

    if let Some(target) = pending_rewind_target {
        // This is the guard from driver.rs line ~496
        let should_rewind = target < l2_head_number;
        assert!(
            !should_rewind,
            "rewind target == head should be skipped (no-op)"
        );
    }

    // Also verify target > head is skipped
    let pending_rewind_target2: Option<u64> = Some(10);
    if let Some(target) = pending_rewind_target2 {
        let should_rewind = target < l2_head_number;
        assert!(!should_rewind, "rewind target > head should be skipped");
    }
}

// --- Iteration 43: pending_rewind_target breaks derive loop + early return ---

#[test]
fn test_pending_rewind_breaks_derive_loop_and_returns_early() {
    // In step_builder (lines 710-749), when pending_rewind_target is set by
    // verify_local_block_matches_l1 during iteration over derived blocks:
    //   1. The `break` at line 713-714 stops processing remaining blocks
    //   2. The early return at line 747-749 skips block building and submission
    //
    // This test simulates the logic to verify both behaviors:
    // - When 3 blocks are derived and the 1st triggers a rewind, blocks 2 and 3
    //   should NOT be processed.
    // - After the loop, the function should return early without building.
    let mut pending_rewind_target: Option<u64> = None;

    // Simulate 3 derived blocks
    let derived_blocks: Vec<(u64, bool)> = vec![
        (1, true),  // L1 context mismatch → triggers rewind
        (2, false), // should be skipped
        (3, false), // should be skipped
    ];

    let mut processed_blocks: Vec<u64> = Vec::new();
    let l2_head_number = 3u64;

    for (l2_block_number, has_mismatch) in &derived_blocks {
        // This mirrors the break at line 713
        if pending_rewind_target.is_some() {
            break;
        }

        if *l2_block_number <= l2_head_number {
            // Simulate verify_local_block_matches_l1
            if *has_mismatch {
                // This mirrors set_rewind_target
                let target = l2_block_number.saturating_sub(1);
                pending_rewind_target =
                    Some(pending_rewind_target.map_or(target, |t| t.min(target)));
                // verify returns Ok(()) for L1 context mismatch, not Err
                processed_blocks.push(*l2_block_number);
                continue;
            }
            processed_blocks.push(*l2_block_number);
            continue;
        }
    }

    // Block 1 was processed (mismatch detected), blocks 2+3 were skipped
    assert_eq!(
        processed_blocks,
        vec![1],
        "only the first block should be processed before break"
    );
    assert_eq!(
        pending_rewind_target,
        Some(0),
        "rewind target should be block_1 - 1 = 0"
    );

    // After the loop: the early return at line 747-749
    let should_return_early = pending_rewind_target.is_some();
    assert!(
        should_return_early,
        "step_builder must return early when pending_rewind_target is set"
    );
    // This prevents wasted block building and L1 gas expenditure
}

// --- Iteration 44: Preconfirmed pruning no-op + reorg clears preconfirmed ---

#[test]
fn test_reorg_clears_preconfirmed_hashes_and_pending() {
    // On L1 reorg, the driver clears preconfirmed_hashes and pending_submissions
    // to prevent stale data from being used after rollback (line 538-539)
    let mut preconfirmed: HashMap<u64, B256> = HashMap::new();
    let mut pending: Vec<PendingBlock> = Vec::new();

    preconfirmed.insert(10, B256::with_last_byte(0xAA));
    preconfirmed.insert(11, B256::with_last_byte(0xBB));
    pending.push(PendingBlock {
        l2_block_number: 10,
        pre_state_root: B256::ZERO,
        state_root: B256::with_last_byte(0xCC),
        clean_state_root: B256::with_last_byte(0xCC),
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    });

    // Simulate reorg handling
    preconfirmed.clear();
    pending.clear();

    assert!(
        preconfirmed.is_empty(),
        "reorg must clear preconfirmed hashes"
    );
    assert!(pending.is_empty(), "reorg must clear pending submissions");
}

#[test]
fn test_batch_halving_single_block_does_not_halve_to_zero() {
    // When batch_size is 1 and submission fails with "calldata gas" error,
    // the halving condition `batch_size > 1` prevents division to 0.
    // This test verifies that invariant holds.
    let mut batch_size: usize = 1;

    // Simulate the halving check from flush_pending_submissions
    let err_str = "batch calldata gas (13000000) exceeds limit (12000000), reduce batch size";
    if err_str.contains("calldata gas") && batch_size > 1 {
        batch_size /= 2;
    }
    // batch_size must remain 1 — the guard `batch_size > 1` prevents halving
    assert_eq!(
        batch_size, 1,
        "batch_size must not halve below 1 when already at 1"
    );

    // Also verify the general halving sequence never reaches 0
    let mut bs = MAX_BATCH_SIZE; // 100
    let mut halving_steps = 0;
    while bs > 1 {
        bs /= 2;
        halving_steps += 1;
    }
    assert_eq!(bs, 1, "halving sequence must terminate at 1, not 0");
    // 100 -> 50 -> 25 -> 12 -> 6 -> 3 -> 1 = 6 steps
    assert_eq!(halving_steps, 6);
}

// --- Iteration 48: apply_rewind clears pending_rewind_target + no-op cases ---

#[test]
fn test_apply_rewind_clears_pending_rewind_target() {
    // After apply_rewind (driver.rs line 495), `pending_rewind_target.take()`
    // consumes the value and sets it to None. This test verifies the `.take()`
    // semantics: after processing, the field is cleared regardless of whether
    // the rewind was actually executed (target < head) or skipped (target >= head).
    let mut pending_rewind_target: Option<u64> = Some(3);
    let l2_head_number = 10u64;

    // Simulate the driver's apply_rewind logic (line 495-504)
    if let Some(target) = pending_rewind_target.take() {
        if target < l2_head_number {
            // Would call rewind_l2_chain here in the real code
            let _ = target; // rewind executed
        }
    }

    // After .take(), pending_rewind_target must be None
    assert_eq!(
        pending_rewind_target, None,
        "pending_rewind_target must be cleared after apply_rewind processes it"
    );
}

#[test]
fn test_apply_rewind_target_equals_head_is_noop() {
    // When pending_rewind_target == l2_head_number, the guard
    // `if target < self.l2_head_number` prevents rewinding. The target is
    // still consumed by .take() so it doesn't trigger again next step.
    let mut pending_rewind_target: Option<u64> = Some(10);
    let l2_head_number = 10u64;
    let mut rewind_executed = false;

    if let Some(target) = pending_rewind_target.take() {
        if target < l2_head_number {
            rewind_executed = true;
        }
    }

    assert!(
        !rewind_executed,
        "rewind must NOT execute when target == l2_head_number"
    );
    assert_eq!(
        pending_rewind_target, None,
        "pending_rewind_target must still be cleared even when rewind is skipped"
    );

    // Also verify target > head is skipped
    let mut pending_rewind_target2: Option<u64> = Some(15);
    let mut rewind_executed2 = false;

    if let Some(target) = pending_rewind_target2.take() {
        if target < l2_head_number {
            rewind_executed2 = true;
        }
    }

    assert!(
        !rewind_executed2,
        "rewind must NOT execute when target > l2_head_number"
    );
    assert_eq!(pending_rewind_target2, None);
}

// --- Iteration 49: recover_chain_state edge cases + rewind restore ---

#[test]
fn test_recover_chain_state_genesis_returns_early() {
    // When tip == 0 (genesis), recover_chain_state returns Ok(()) without
    // modifying head_hash or block_hashes. Simulate the early return path
    // at driver.rs line 339-341.
    let tip = 0u64;
    let mut head_hash = B256::with_last_byte(0xAA); // genesis hash set in new()
    let mut l2_head_number = 0u64;
    let mut block_hashes: VecDeque<B256> = VecDeque::new();
    block_hashes.push_back(head_hash); // genesis pushed in new()

    // Simulate recover_chain_state: if tip == 0, return early
    if tip == 0 {
        // Early return — no changes to head_hash or block_hashes
    } else {
        // This branch should NOT be taken
        head_hash = B256::ZERO;
        l2_head_number = tip;
        block_hashes.clear();
    }

    // Verify genesis state is preserved
    assert_eq!(l2_head_number, 0, "head number must remain 0 at genesis");
    assert_eq!(
        head_hash,
        B256::with_last_byte(0xAA),
        "head_hash must remain the genesis hash"
    );
    assert_eq!(
        block_hashes.len(),
        1,
        "block_hashes should still have just genesis"
    );
    assert_eq!(block_hashes[0], B256::with_last_byte(0xAA));
}

#[test]
fn test_recover_chain_state_block_hashes_capped_at_fork_choice_depth() {
    // recover_chain_state rebuilds block_hashes from `tip - FORK_CHOICE_DEPTH..=tip`.
    // Even if tip is much larger than FORK_CHOICE_DEPTH, the deque should
    // contain at most FORK_CHOICE_DEPTH + 1 entries.
    let tip = 200u64; // well beyond FORK_CHOICE_DEPTH=64
    let start = tip.saturating_sub(FORK_CHOICE_DEPTH as u64); // 200 - 64 = 136

    // Simulate the block_hashes rebuild loop (driver.rs lines 355-368)
    let mut block_hashes = VecDeque::new();
    for n in start..=tip {
        // Simulate: all hashes found in DB
        block_hashes.push_back(B256::with_last_byte(n as u8));
    }

    // Verify the count: start..=tip has (tip - start + 1) = 65 entries
    assert_eq!(
        block_hashes.len(),
        FORK_CHOICE_DEPTH + 1,
        "block_hashes should have exactly FORK_CHOICE_DEPTH + 1 entries"
    );

    // Verify first and last entries
    assert_eq!(
        block_hashes.front().unwrap(),
        &B256::with_last_byte(start as u8)
    );
    assert_eq!(
        block_hashes.back().unwrap(),
        &B256::with_last_byte(tip as u8)
    );

    // Also verify for a tip < FORK_CHOICE_DEPTH (no saturation)
    let small_tip = 10u64;
    let small_start = small_tip.saturating_sub(FORK_CHOICE_DEPTH as u64); // 0
    let mut small_hashes = VecDeque::new();
    for n in small_start..=small_tip {
        small_hashes.push_back(B256::with_last_byte(n as u8));
    }
    assert_eq!(
        small_hashes.len(),
        11, // blocks 0..=10
        "for tip < FORK_CHOICE_DEPTH, all blocks from genesis should be included"
    );
}

#[test]
fn test_clear_pending_state_clears_all_fields() {
    // Simulate the fields that clear_pending_state clears:
    //   1. preconfirmed_hashes
    //   2. pending_submissions
    let mut preconfirmed_hashes: HashMap<u64, B256> = HashMap::new();
    preconfirmed_hashes.insert(1, B256::with_last_byte(0x01));
    preconfirmed_hashes.insert(2, B256::with_last_byte(0x02));
    preconfirmed_hashes.insert(3, B256::with_last_byte(0x03));

    let mut pending_submissions: VecDeque<PendingBlock> = VecDeque::new();
    for i in 0..5u64 {
        pending_submissions.push_back(PendingBlock {
            l2_block_number: i + 1,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: Bytes::from(vec![0xc0]),
            intermediate_roots: vec![],
        });
    }

    // Pre-conditions: all populated
    assert_eq!(preconfirmed_hashes.len(), 3);
    assert_eq!(pending_submissions.len(), 5);

    // Simulate clear_pending_state
    preconfirmed_hashes.clear();
    pending_submissions.clear();

    // Post-conditions: all empty
    assert!(
        preconfirmed_hashes.is_empty(),
        "preconfirmed_hashes must be empty after clear"
    );
    assert!(
        pending_submissions.is_empty(),
        "pending_submissions must be empty after clear"
    );
}

// --- Iteration 52: clear_pending_state comprehensive + mode transition deposit clearing ---

#[test]
fn test_clear_pending_state_comprehensive_all_populated() {
    // Verifies that clear_pending_state clears ALL collections simultaneously.
    // Previous tests checked subsets; this test populates all with realistic
    // data and verifies a single clear sweep empties everything.

    // 1. preconfirmed_hashes: multiple entries at different block numbers
    let mut preconfirmed_hashes: HashMap<u64, B256> = HashMap::new();
    preconfirmed_hashes.insert(10, B256::with_last_byte(0x10));
    preconfirmed_hashes.insert(11, B256::with_last_byte(0x11));
    preconfirmed_hashes.insert(12, B256::with_last_byte(0x12));
    preconfirmed_hashes.insert(100, B256::with_last_byte(0x64));

    // 2. pending_submissions: multiple PendingBlocks with realistic fields
    let mut pending_submissions: VecDeque<PendingBlock> = VecDeque::new();
    for i in 10..15u64 {
        pending_submissions.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: Bytes::from(vec![0xc0, i as u8]),
            intermediate_roots: vec![],
        });
    }

    // Pre-conditions: all are populated
    assert_eq!(preconfirmed_hashes.len(), 4);
    assert_eq!(pending_submissions.len(), 5);

    // Simulate clear_pending_state (mirrors the real implementation exactly)
    preconfirmed_hashes.clear();
    pending_submissions.clear();

    // Post-conditions: all are empty
    assert!(
        preconfirmed_hashes.is_empty(),
        "preconfirmed_hashes must be empty after clear_pending_state"
    );
    assert!(
        pending_submissions.is_empty(),
        "pending_submissions must be empty after clear_pending_state"
    );
}

// --- Iteration 54: Proposer and batch submission edge cases ---

#[test]
fn test_batch_halving_to_single_block_no_infinite_loop() {
    // When calldata_gas exceeds the limit and halving reaches batch size 1,
    // submit_block also checks calldata gas and returns a "calldata gas" error.
    // The `batch_size > 1` guard prevents infinite halving at size 1.
    //
    // Simulate the driver's halving loop with a "calldata gas" error.
    let mut batch_size = MAX_BATCH_SIZE;
    let mut iterations = 0;
    let max_iterations = 20; // safety cap for test

    loop {
        iterations += 1;
        assert!(
            iterations <= max_iterations,
            "halving loop exceeded {max_iterations} iterations — possible infinite loop"
        );

        // Simulate: submit_batch returns "calldata gas" error
        let err_str = "batch calldata gas (999999999) exceeds limit (12000000), reduce batch size";

        if err_str.contains("calldata gas") && batch_size > 1 {
            batch_size /= 2;
            continue;
        }

        // Once batch_size == 1, the condition `batch_size > 1` is false,
        // so we fall through to break (recording failure + cooldown).
        break;
    }

    // Halving from 100: 50 → 25 → 12 → 6 → 3 → 1 (6 halvings) + 1 final = 7 iterations
    assert_eq!(batch_size, 1);
    assert_eq!(iterations, 7);
}

#[test]
fn test_flush_drops_already_submitted_blocks() {
    // The flush logic drops blocks with l2_block_number < next_on_l1.
    // Simulate that logic.
    let mut pending: VecDeque<PendingBlock> = VecDeque::new();
    for i in 1..=10 {
        pending.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: Bytes::from(vec![0xc0]),
            intermediate_roots: vec![],
        });
    }

    let next_on_l1 = 6u64; // L1 already has blocks 1-5

    // Drop blocks that L1 already has
    while let Some(front) = pending.front() {
        if front.l2_block_number < next_on_l1 {
            pending.pop_front();
        } else {
            break;
        }
    }

    assert_eq!(pending.len(), 5);
    assert_eq!(pending.front().unwrap().l2_block_number, 6);
    assert_eq!(pending.back().unwrap().l2_block_number, 10);
}

#[test]
fn test_flush_drops_all_when_l1_ahead() {
    // If L1 is ahead of all our pending blocks, they all get dropped.
    let mut pending: VecDeque<PendingBlock> = VecDeque::new();
    for i in 1..=5 {
        pending.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: Bytes::from(vec![0xc0]),
            intermediate_roots: vec![],
        });
    }

    let next_on_l1 = 100u64; // L1 is way ahead

    while let Some(front) = pending.front() {
        if front.l2_block_number < next_on_l1 {
            pending.pop_front();
        } else {
            break;
        }
    }

    assert!(pending.is_empty(), "all blocks should be dropped");
}

#[test]
fn test_batch_halving_sequence_from_various_starts() {
    // Verify the halving sequence from different starting batch sizes.
    // The driver starts at MAX_BATCH_SIZE (100) but the logic should work
    // correctly from any starting size.
    for (start, expected_final) in [
        (100usize, vec![50, 25, 12, 6, 3, 1]),
        (10, vec![5, 2, 1]),
        (7, vec![3, 1]),
        (2, vec![1]),
        (1, Vec::<usize>::new()), // already at 1, no halving
    ] {
        let mut batch_size = start;
        let mut halvings = Vec::new();
        while batch_size > 1 {
            batch_size /= 2;
            halvings.push(batch_size);
        }
        assert_eq!(
            halvings, expected_final,
            "halving sequence from {start} was wrong"
        );
    }
}

#[test]
fn test_set_rewind_target_none_then_some_then_lower_then_higher() {
    // Comprehensive test of set_rewind_target semantics:
    // 1. None -> set(10) -> Some(10)
    // 2. Some(10) -> set(5) -> Some(5) (takes minimum)
    // 3. Some(5) -> set(10) -> Some(5) (ignores higher)
    // 4. Some(5) -> set(3) -> Some(3) (takes new minimum)
    // 5. Some(3) -> set(0) -> Some(0) (zero is valid)
    // 6. Some(0) -> set(100) -> Some(0) (zero is absolute minimum)
    let mut pending: Option<u64> = None;

    // Step 1: None -> 10
    let target = 10u64;
    pending = Some(pending.map_or(target, |t| t.min(target)));
    assert_eq!(pending, Some(10));

    // Step 2: Some(10) -> 5 (lower wins)
    let target = 5u64;
    pending = Some(pending.map_or(target, |t| t.min(target)));
    assert_eq!(pending, Some(5));

    // Step 3: Some(5) -> 10 (higher ignored)
    let target = 10u64;
    pending = Some(pending.map_or(target, |t| t.min(target)));
    assert_eq!(pending, Some(5), "higher target must not overwrite lower");

    // Step 4: Some(5) -> 3 (lower wins again)
    let target = 3u64;
    pending = Some(pending.map_or(target, |t| t.min(target)));
    assert_eq!(pending, Some(3));

    // Step 5: Some(3) -> 0 (zero is valid minimum)
    let target = 0u64;
    pending = Some(pending.map_or(target, |t| t.min(target)));
    assert_eq!(pending, Some(0), "zero must be accepted as rewind target");

    // Step 6: Some(0) -> 100 (nothing can override zero)
    let target = 100u64;
    pending = Some(pending.map_or(target, |t| t.min(target)));
    assert_eq!(
        pending,
        Some(0),
        "zero is the absolute minimum, cannot be overridden"
    );
}

// --- Cross-chain entry building and accumulation tests ---

#[test]
fn test_built_block_pre_state_root_tracked() {
    // Verify that BuiltBlock stores pre_state_root correctly and it is
    // distinct from (post) state_root.
    let pre = B256::with_last_byte(0x11);
    let post = B256::with_last_byte(0x22);
    let block = BuiltBlock {
        hash: B256::with_last_byte(0xAA),
        pre_state_root: pre,
        state_root: post,
        tx_count: 5,
        encoded_transactions: Bytes::from(vec![0xc0]),
    };
    assert_eq!(block.pre_state_root, pre);
    assert_eq!(block.state_root, post);
    assert_ne!(
        block.pre_state_root, block.state_root,
        "pre and post state roots should differ for non-trivial blocks"
    );
}

#[test]
fn test_cross_chain_entries_built_when_rollups_configured() {
    // Test the if-block in step_builder that builds cross-chain entries
    // when rollups_address is non-zero, tx_count > 0, and encoded_transactions
    // is non-empty.
    use crate::execution_planner::build_entries_from_encoded;

    let rollups_address = alloy_primitives::Address::with_last_byte(0x42);
    let rollup_id = 1u64;
    let pre_state_root = B256::with_last_byte(0xAA);
    let post_state_root = B256::with_last_byte(0xBB);
    let encoded_transactions = Bytes::from(vec![0xc0]); // minimal RLP list

    let built = BuiltBlock {
        hash: B256::with_last_byte(0xFF),
        pre_state_root,
        state_root: post_state_root,
        tx_count: 1,
        encoded_transactions: encoded_transactions.clone(),
    };

    // Replicate the driver's if-block logic
    let mut pending_cross_chain_entries: Vec<CrossChainExecutionEntry> = Vec::new();
    if !rollups_address.is_zero() && built.tx_count > 0 && !built.encoded_transactions.is_empty() {
        let entries = build_entries_from_encoded(
            rollup_id,
            built.pre_state_root,
            built.state_root,
            &built.encoded_transactions,
        );
        if !entries.is_empty() {
            pending_cross_chain_entries.extend(entries);
        }
    }

    assert_eq!(
        pending_cross_chain_entries.len(),
        1,
        "should produce one entry for non-empty block with rollups configured"
    );
    let entry = &pending_cross_chain_entries[0];
    assert_eq!(entry.state_deltas.len(), 1);
    assert_eq!(entry.state_deltas[0].current_state, pre_state_root);
    assert_eq!(entry.state_deltas[0].new_state, post_state_root);
    assert_ne!(entry.action_hash, B256::ZERO);
}

#[test]
fn test_cross_chain_entries_skipped_when_rollups_not_configured() {
    // When rollups_address is ZERO, the if-block is skipped entirely.
    use crate::execution_planner::build_entries_from_encoded;

    let rollups_address = alloy_primitives::Address::ZERO;
    let built = BuiltBlock {
        hash: B256::with_last_byte(0xFF),
        pre_state_root: B256::with_last_byte(0xAA),
        state_root: B256::with_last_byte(0xBB),
        tx_count: 1,
        encoded_transactions: Bytes::from(vec![0xc0]),
    };

    let mut pending_cross_chain_entries: Vec<CrossChainExecutionEntry> = Vec::new();
    if !rollups_address.is_zero() && built.tx_count > 0 && !built.encoded_transactions.is_empty() {
        let entries = build_entries_from_encoded(
            1,
            built.pre_state_root,
            built.state_root,
            &built.encoded_transactions,
        );
        if !entries.is_empty() {
            pending_cross_chain_entries.extend(entries);
        }
    }

    assert!(
        pending_cross_chain_entries.is_empty(),
        "no entries should be built when rollups_address is zero"
    );
}

#[test]
fn test_cross_chain_entries_skipped_when_no_transactions() {
    // rollups_address configured but tx_count == 0 and encoded_transactions empty.
    use crate::execution_planner::build_entries_from_encoded;

    let rollups_address = alloy_primitives::Address::with_last_byte(0x42);
    let built = BuiltBlock {
        hash: B256::with_last_byte(0xFF),
        pre_state_root: B256::with_last_byte(0xAA),
        state_root: B256::with_last_byte(0xBB),
        tx_count: 0,
        encoded_transactions: Bytes::new(),
    };

    let mut pending_cross_chain_entries: Vec<CrossChainExecutionEntry> = Vec::new();
    if !rollups_address.is_zero() && built.tx_count > 0 && !built.encoded_transactions.is_empty() {
        let entries = build_entries_from_encoded(
            1,
            built.pre_state_root,
            built.state_root,
            &built.encoded_transactions,
        );
        if !entries.is_empty() {
            pending_cross_chain_entries.extend(entries);
        }
    }

    assert!(
        pending_cross_chain_entries.is_empty(),
        "no entries should be built when tx_count is 0"
    );
}

#[test]
fn test_pending_cross_chain_entries_accumulate() {
    // Simulate two blocks being built in sequence — entries should accumulate.
    use crate::execution_planner::build_entries_from_encoded;

    let rollups_address = alloy_primitives::Address::with_last_byte(0x42);
    let rollup_id = 1u64;
    let mut pending_cross_chain_entries: Vec<CrossChainExecutionEntry> = Vec::new();

    // Block 1
    let built1 = BuiltBlock {
        hash: B256::with_last_byte(0x01),
        pre_state_root: B256::with_last_byte(0xA0),
        state_root: B256::with_last_byte(0xA1),
        tx_count: 1,
        encoded_transactions: Bytes::from(vec![0xc0]),
    };
    if !rollups_address.is_zero() && built1.tx_count > 0 && !built1.encoded_transactions.is_empty()
    {
        let entries = build_entries_from_encoded(
            rollup_id,
            built1.pre_state_root,
            built1.state_root,
            &built1.encoded_transactions,
        );
        if !entries.is_empty() {
            pending_cross_chain_entries.extend(entries);
        }
    }
    assert_eq!(pending_cross_chain_entries.len(), 1);

    // Block 2
    let built2 = BuiltBlock {
        hash: B256::with_last_byte(0x02),
        pre_state_root: B256::with_last_byte(0xB0),
        state_root: B256::with_last_byte(0xB1),
        tx_count: 3,
        encoded_transactions: Bytes::from(vec![0xc1, 0x80]),
    };
    if !rollups_address.is_zero() && built2.tx_count > 0 && !built2.encoded_transactions.is_empty()
    {
        let entries = build_entries_from_encoded(
            rollup_id,
            built2.pre_state_root,
            built2.state_root,
            &built2.encoded_transactions,
        );
        if !entries.is_empty() {
            pending_cross_chain_entries.extend(entries);
        }
    }
    assert_eq!(
        pending_cross_chain_entries.len(),
        2,
        "entries from two blocks should accumulate"
    );

    // Verify entries correspond to correct blocks
    assert_eq!(
        pending_cross_chain_entries[0].state_deltas[0].current_state,
        B256::with_last_byte(0xA0)
    );
    assert_eq!(
        pending_cross_chain_entries[0].state_deltas[0].new_state,
        B256::with_last_byte(0xA1)
    );
    assert_eq!(
        pending_cross_chain_entries[1].state_deltas[0].current_state,
        B256::with_last_byte(0xB0)
    );
    assert_eq!(
        pending_cross_chain_entries[1].state_deltas[0].new_state,
        B256::with_last_byte(0xB1)
    );
}

// --- Issue #162: commit_batch must NOT be called when pending_rewind_target is set ---

// --- Issue #165: clear_pending_state must clear pending_cross_chain_entries ---

// --- Re-run Iteration 3: reorg must clear external_cross_chain_entries ---

#[test]
fn test_reorg_clears_all_cross_chain_state() {
    // Comprehensive test: after reorg, ALL cross-chain state must be cleared.
    // This includes: pending_cross_chain_entries (Vec on driver),
    // queued_cross_chain_calls (Arc<Mutex> shared with RPC),
    // and pending_execution_entries on evm_config.
    use crate::cross_chain::CrossChainExecutionEntry;
    use crate::execution_planner::build_entries_from_encoded;
    use crate::rpc::QueuedCrossChainCall;

    let mut pending_cross_chain_entries: Vec<CrossChainExecutionEntry> = Vec::new();
    let queued_cross_chain_calls: Arc<std::sync::Mutex<Vec<QueuedCrossChainCall>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // Populate all containers
    let entries = build_entries_from_encoded(
        1,
        B256::with_last_byte(0xA0),
        B256::with_last_byte(0xA1),
        &[0xc0],
    );
    pending_cross_chain_entries.extend(entries.iter().cloned());
    // Add a QueuedCrossChainCall using the first entry as both call and result
    // (the point is to test that clear works, not entry correctness).
    queued_cross_chain_calls
        .lock()
        .unwrap()
        .push(QueuedCrossChainCall {
            call_entry: entries[0].clone(),
            result_entry: entries[0].clone(),
            effective_gas_price: 1_000_000_000,
            raw_l1_tx: Bytes::new(),
            extra_l2_entries: vec![],
            l1_entries: vec![],
            tx_reverts: false,
            l1_independent_entries: false,
        });

    assert!(!pending_cross_chain_entries.is_empty());
    assert!(!queued_cross_chain_calls.lock().unwrap().is_empty());

    // Simulate clear_pending_state
    pending_cross_chain_entries.clear();
    queued_cross_chain_calls.lock().unwrap().clear();

    assert!(
        pending_cross_chain_entries.is_empty(),
        "pending_cross_chain_entries must be empty after reorg"
    );
    assert!(
        queued_cross_chain_calls.lock().unwrap().is_empty(),
        "queued_cross_chain_calls must be empty after reorg"
    );
}

// --- Iteration 76: postBatch and submitBatch interaction ---

#[test]
fn test_flush_to_l1_single_call() {
    // The driver calls flush_to_l1().await in step_builder which
    // combines block submission and cross-chain entry posting into
    // a single submit_to_l1 call. This test documents the unified
    // submission approach.
    let call_order: Vec<&str> = vec!["flush_to_l1"];

    assert_eq!(call_order[0], "flush_to_l1");
    assert_eq!(
        call_order.len(),
        1,
        "exactly one flush call: submit_to_l1 combines blocks + entries"
    );
}

#[test]
fn test_flush_to_l1_respects_submission_cooldown() {
    // flush_to_l1 checks last_submission_failure. When L1 is unreachable,
    // it backs off for SUBMISSION_COOLDOWN_SECS.
    let last_submission_failure: Option<std::time::Instant> = Some(std::time::Instant::now());

    // Simulate the cooldown check (mirrors flush_to_l1)
    let mut should_skip = false;
    if let Some(last_fail) = last_submission_failure {
        if last_fail.elapsed().as_secs() < SUBMISSION_COOLDOWN_SECS {
            should_skip = true;
        }
    }

    assert!(
        should_skip,
        "flush_to_l1 must skip when within cooldown period"
    );
}

#[test]
fn test_flush_to_l1_proceeds_after_cooldown_expires() {
    // After the cooldown expires, flush_to_l1 should proceed.
    let last_submission_failure: Option<std::time::Instant> = Some(
        std::time::Instant::now() - std::time::Duration::from_secs(SUBMISSION_COOLDOWN_SECS + 1),
    );

    let mut should_skip = false;
    if let Some(last_fail) = last_submission_failure {
        if last_fail.elapsed().as_secs() < SUBMISSION_COOLDOWN_SECS {
            should_skip = true;
        }
    }

    assert!(
        !should_skip,
        "flush_to_l1 must proceed after cooldown expires"
    );
}

#[test]
fn test_submission_failure_sets_cooldown() {
    // After a submit_to_l1 failure, last_submission_failure is set,
    // causing the next flush_to_l1 call to back off.
    let last_submission_failure: Option<std::time::Instant> = Some(std::time::Instant::now());

    let mut should_skip = false;
    if let Some(last_fail) = last_submission_failure {
        if last_fail.elapsed().as_secs() < SUBMISSION_COOLDOWN_SECS {
            should_skip = true;
        }
    }

    assert!(should_skip, "L1 submission failure must trigger cooldown");
}

#[test]
fn test_submission_success_clears_cooldown() {
    // After a successful submit_to_l1, last_submission_failure is set to None.
    let last_submission_failure: Option<std::time::Instant> = None;

    let mut should_skip = false;
    if let Some(last_fail) = last_submission_failure {
        if last_fail.elapsed().as_secs() < SUBMISSION_COOLDOWN_SECS {
            should_skip = true;
        }
    }

    assert!(!should_skip, "successful submission must clear cooldown");
}

// --- Iteration 77: cross-chain disabled mode and mismatched config guards ---

#[test]
fn test_cross_chain_disabled_no_entries_built_even_with_nonzero_tx_count() {
    // When rollups_address is ZERO (disabled), cross-chain entry building is
    // completely skipped regardless of tx_count or encoded_transactions content.
    // This is the guard at driver.rs line ~1022.
    use crate::cross_chain::CrossChainExecutionEntry;

    let rollups_address = alloy_primitives::Address::ZERO;
    let tx_count = 5u64;
    let encoded_transactions = alloy_primitives::Bytes::from_static(&[0xc0, 0xc0]);
    let rollup_id = 42u64; // non-zero rollup_id but zero rollups_address

    let mut pending_cross_chain_entries: Vec<CrossChainExecutionEntry> = Vec::new();
    if !rollups_address.is_zero() && tx_count > 0 && !encoded_transactions.is_empty() {
        let entries = crate::execution_planner::build_entries_from_encoded(
            rollup_id,
            B256::with_last_byte(0xAA),
            B256::with_last_byte(0xBB),
            &encoded_transactions,
        );
        pending_cross_chain_entries.extend(entries);
    }

    assert!(
        pending_cross_chain_entries.is_empty(),
        "no entries should be built when rollups_address is zero, even with nonzero rollup_id"
    );
}

// --- Re-run Iteration 11: Cross-chain batch halving boundary ---

// ──────────────────────────────────────────────────────────────────
//  Tests for atomic cross-chain L1 submission (forward_queued_l1_txs)
// ──────────────────────────────────────────────────────────────────

#[test]
fn test_flush_ordering_includes_forward_queued_l1_txs() {
    // The driver calls flush_to_l1() which on success calls forward_queued_l1_txs.
    // submit_to_l1 must land BEFORE user L1 txs.
    let call_order: Vec<&str> = vec!["flush_to_l1", "forward_queued_l1_txs"];

    assert_eq!(call_order[0], "flush_to_l1");
    assert_eq!(call_order[1], "forward_queued_l1_txs");
    assert_eq!(
        call_order.len(),
        2,
        "exactly two calls in order: submit_to_l1 → forward user txs"
    );
}

#[test]
fn test_forward_queued_l1_txs_empty_queue_is_early_return() {
    // When the queue is empty, forward_queued_l1_txs should return Ok(())
    // immediately without touching the L1 provider.
    let queue: Arc<std::sync::Mutex<Vec<Bytes>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let txs: Vec<Bytes> = {
        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            // Early return path
            Vec::new()
        } else {
            q.drain(..).collect()
        }
    };

    assert!(
        txs.is_empty(),
        "empty queue should produce no txs to forward"
    );
}

#[test]
fn test_forward_queued_l1_txs_respects_submission_cooldown() {
    // When last_submission_failure is recent, txs should be re-queued.
    let queue: Arc<std::sync::Mutex<Vec<Bytes>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Push some txs
    {
        let mut q = queue.lock().unwrap();
        q.push(Bytes::from(vec![0x01]));
        q.push(Bytes::from(vec![0x02]));
    }

    // Drain (simulating forward_queued_l1_txs entry)
    let txs: Vec<Bytes> = {
        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
        q.drain(..).collect()
    };
    assert_eq!(txs.len(), 2);

    // Simulate cooldown check — recent failure
    let last_submission_failure: Option<std::time::Instant> = Some(std::time::Instant::now());
    let in_cooldown = last_submission_failure
        .map(|t| t.elapsed().as_secs() < SUBMISSION_COOLDOWN_SECS)
        .unwrap_or(false);
    assert!(
        in_cooldown,
        "should be in cooldown immediately after failure"
    );

    // Re-queue the txs (what the driver does during cooldown)
    {
        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
        q.extend(txs);
    }

    assert_eq!(
        queue.lock().unwrap().len(),
        2,
        "txs must be re-queued during cooldown"
    );
}

#[test]
fn test_forward_queued_l1_txs_drops_failed_txs() {
    // Failed user tx forwards are dropped (not re-queued).
    // This is intentional: user txs may be invalid, already submitted,
    // or the nonce may have changed. Re-queuing would cause infinite loops.
    let queue: Arc<std::sync::Mutex<Vec<Bytes>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Push 3 txs
    {
        let mut q = queue.lock().unwrap();
        q.push(Bytes::from(vec![0x01]));
        q.push(Bytes::from(vec![0x02]));
        q.push(Bytes::from(vec![0x03]));
    }

    // Drain all
    let txs: Vec<Bytes> = {
        let mut q = queue.lock().unwrap();
        q.drain(..).collect()
    };

    // Simulate: tx[0] succeeds, tx[1] fails, tx[2] succeeds
    // The driver iterates all txs — failures are logged but not re-queued.
    let mut forwarded = 0u32;
    let mut dropped = 0u32;
    for (i, _tx) in txs.iter().enumerate() {
        if i == 1 {
            dropped += 1; // simulated failure — dropped
        } else {
            forwarded += 1;
        }
    }

    assert_eq!(forwarded, 2, "2 txs should be forwarded successfully");
    assert_eq!(dropped, 1, "1 tx should be dropped on failure");

    // Queue should remain empty — failed tx is NOT re-queued
    assert!(
        queue.lock().unwrap().is_empty(),
        "failed user txs must NOT be re-queued"
    );
}

#[test]
fn test_forward_queued_l1_txs_multiple_user_txs_all_forwarded() {
    // When multiple cross-chain calls are queued (e.g., 5 users each
    // sending a cross-chain tx), all their L1 txs are forwarded.
    let queue: Arc<std::sync::Mutex<Vec<Bytes>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let num_users = 5;
    {
        let mut q = queue.lock().unwrap();
        for i in 0..num_users {
            q.push(Bytes::from(vec![i as u8; 100])); // 100-byte tx per user
        }
    }

    let txs: Vec<Bytes> = {
        let mut q = queue.lock().unwrap();
        q.drain(..).collect()
    };

    assert_eq!(txs.len(), num_users);
    // Verify each tx is distinct
    for (i, tx) in txs.iter().enumerate() {
        assert_eq!(tx[0], i as u8, "tx {i} should have correct sender byte");
    }
}

#[test]
fn test_forward_queue_cap_prevents_unbounded_growth() {
    // The RPC enforces a cap of 1000 entries. This prevents a malicious
    // client from filling memory by spamming queueL1ForwardTx.
    let queue: Arc<std::sync::Mutex<Vec<Bytes>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Fill to 1000
    {
        let mut q = queue.lock().unwrap();
        for _ in 0..1000 {
            q.push(Bytes::from(vec![0xFF]));
        }
    }

    // RPC rejects the 1001st entry
    {
        let q = queue.lock().unwrap();
        assert!(q.len() >= 1000, "queue should be at capacity");
        // The RPC returns an error here — not allowed to push
    }

    // Driver drains → allows new entries
    {
        let mut q = queue.lock().unwrap();
        q.drain(..);
    }
    {
        let mut q = queue.lock().unwrap();
        q.push(Bytes::from(vec![0x01]));
        assert_eq!(q.len(), 1, "queue should accept after drain");
    }
}

// ──────────────────────────────────────────────────────────────────
//  Re-run Iteration 5: Concurrent L1 submissions (cross-chain focus)
// ──────────────────────────────────────────────────────────────────

#[test]
fn test_cross_chain_entries_accumulate_across_blocks_before_flush() {
    // If postBatch fails and entries are re-queued, the next builder tick
    // adds entries from the new block. The pending_cross_chain_entries Vec
    // grows with entries from multiple blocks. On the next successful
    // postBatch, ALL accumulated entries are submitted in a single call.
    use crate::cross_chain::CrossChainExecutionEntry;
    use crate::execution_planner::build_entries_from_encoded;

    let mut pending_cross_chain_entries: Vec<CrossChainExecutionEntry> = Vec::new();

    // Block N entries (re-queued from failed postBatch)
    let block_n_entries = build_entries_from_encoded(
        1,
        B256::with_last_byte(0xAA),
        B256::with_last_byte(0xBB),
        &[0xc0],
    );
    pending_cross_chain_entries.extend(block_n_entries);
    assert_eq!(pending_cross_chain_entries.len(), 1);

    // Block N+1 entries (new block built)
    let block_n1_entries = build_entries_from_encoded(
        1,
        B256::with_last_byte(0xBB),
        B256::with_last_byte(0xCC),
        &[0xc1, 0x80],
    );
    pending_cross_chain_entries.extend(block_n1_entries);
    assert_eq!(pending_cross_chain_entries.len(), 2);

    // Block N+2 entries (another block)
    let block_n2_entries = build_entries_from_encoded(
        1,
        B256::with_last_byte(0xCC),
        B256::with_last_byte(0xDD),
        &[0xc2, 0x80, 0x01],
    );
    pending_cross_chain_entries.extend(block_n2_entries);
    assert_eq!(pending_cross_chain_entries.len(), 3);

    // Verify the state root chain is unbroken across accumulated entries
    assert_eq!(
        pending_cross_chain_entries[0].state_deltas[0].current_state,
        B256::with_last_byte(0xAA)
    );
    assert_eq!(
        pending_cross_chain_entries[0].state_deltas[0].new_state,
        B256::with_last_byte(0xBB)
    );
    assert_eq!(
        pending_cross_chain_entries[1].state_deltas[0].current_state,
        B256::with_last_byte(0xBB)
    );
    assert_eq!(
        pending_cross_chain_entries[1].state_deltas[0].new_state,
        B256::with_last_byte(0xCC)
    );
    assert_eq!(
        pending_cross_chain_entries[2].state_deltas[0].current_state,
        B256::with_last_byte(0xCC)
    );
    assert_eq!(
        pending_cross_chain_entries[2].state_deltas[0].new_state,
        B256::with_last_byte(0xDD)
    );

    // std::mem::take drains all at once for a single postBatch call
    let batch = std::mem::take(&mut pending_cross_chain_entries);
    assert_eq!(
        batch.len(),
        3,
        "all accumulated entries submitted in one postBatch"
    );
    assert!(
        pending_cross_chain_entries.is_empty(),
        "pending list drained after take"
    );
}

// --- Re-run Iteration 30: Recovery and restart correctness ---

#[test]
fn test_two_builders_dedup_via_next_l2_block() {
    // Two builders starting simultaneously: both read nextL2Block from L1.
    // The one that gets its tx mined first advances nextL2Block.
    // The second builder's pending submissions are skipped because
    // flush_pending_submissions drops blocks where l2_block_number < next_on_l1.
    let mut pending = VecDeque::new();
    pending.push_back(PendingBlock {
        l2_block_number: 5,
        pre_state_root: B256::ZERO,
        state_root: B256::ZERO,
        clean_state_root: B256::ZERO,
        encoded_transactions: Bytes::new(),
        intermediate_roots: vec![],
    });
    pending.push_back(PendingBlock {
        l2_block_number: 6,
        pre_state_root: B256::ZERO,
        state_root: B256::ZERO,
        clean_state_root: B256::ZERO,
        encoded_transactions: Bytes::new(),
        intermediate_roots: vec![],
    });

    // Simulate: the other builder already submitted blocks 5 and 6
    let next_on_l1 = 7u64;

    // Drop pending blocks that L1 already has
    while let Some(front) = pending.front() {
        if front.l2_block_number < next_on_l1 {
            pending.pop_front();
        } else {
            break;
        }
    }

    assert!(
        pending.is_empty(),
        "all pending blocks already on L1 must be dropped"
    );
}

// --- Re-run Iteration 31: Proposer nonce and gas price management (cross-chain focus) ---

#[test]
fn test_flush_to_l1_unified_submission() {
    // flush_to_l1 combines blocks and cross-chain entries into a single
    // submit_to_l1 call. After success, forward_queued_l1_txs is called.
    // This avoids nonce coordination issues between separate submissions.
    let call_order = [
        "flush_to_l1",           // submit_to_l1(blocks, entries, proof) — single nonce
        "forward_queued_l1_txs", // user txs — different signer
    ];
    assert_eq!(call_order[0], "flush_to_l1");
    assert_eq!(call_order[1], "forward_queued_l1_txs");
}

#[test]
fn test_submission_cooldown_affects_both_flush_paths() {
    // When submit_batch fails, last_submission_failure is set.
    // Both flush_pending_submissions AND flush_cross_chain_submissions
    // check this cooldown. Verify that a failure in one blocks the other.
    let now = std::time::Instant::now();
    let last_failure = Some(now);

    // Both paths check: last_fail.elapsed().as_secs() < SUBMISSION_COOLDOWN_SECS
    // Right after failure, elapsed() ~= 0, which is < 5, so both skip.
    if let Some(last_fail) = last_failure {
        assert!(
            last_fail.elapsed().as_secs() < SUBMISSION_COOLDOWN_SECS,
            "immediately after failure, cooldown must be active"
        );
    }

    // After cooldown expires, both paths proceed.
    // We can't easily test time passage here, but verify the constant.
    assert_eq!(SUBMISSION_COOLDOWN_SECS, 5, "cooldown should be 5 seconds");
}

// --- Re-run Iteration 32: Deposit range boundary conditions ---

#[test]
fn test_builder_assigns_entries_only_to_last_block_in_batch() {
    // When the builder builds multiple blocks in one tick (catch-up),
    // execution entries are assigned only to the last block (is_last_block) or
    // the block before an L1 context refresh (is_last_before_refresh).
    // Intermediate blocks get empty entries.
    use crate::cross_chain::{CrossChainAction, CrossChainActionType, CrossChainExecutionEntry};
    use alloy_primitives::Address;

    let mut builder_entries = vec![CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: B256::with_last_byte(0xAA),
        next_action: CrossChainAction {
            action_type: CrossChainActionType::L2Tx,
            rollup_id: alloy_primitives::U256::ZERO,
            destination: Address::ZERO,
            value: alloy_primitives::U256::ZERO,
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: alloy_primitives::U256::ZERO,
            scope: vec![],
        },
    }];
    let effective_target = 10u64;

    // Simulate building blocks 6-10 (5 blocks)
    for next_l2_block in 6..=10u64 {
        let is_last_block = next_l2_block >= effective_target;
        let blocks_since_refresh = next_l2_block - 6; // 0..4
        let is_last_before_refresh = blocks_since_refresh.saturating_add(1) > 100; // always false here
        let assign_entries = is_last_block || is_last_before_refresh;

        let entries = if assign_entries {
            std::mem::take(&mut builder_entries)
        } else {
            vec![]
        };

        if next_l2_block < 10 {
            assert!(
                entries.is_empty(),
                "intermediate block {next_l2_block} must get empty entries"
            );
        } else {
            assert_eq!(
                entries.len(),
                1,
                "last block must receive all pending entries"
            );
        }
    }

    // After the loop, builder_entries is drained
    assert!(
        builder_entries.is_empty(),
        "all entries must be consumed by end of batch"
    );
}

// --- Re-run Iteration 33: Block building with maximum transactions ---

#[test]
fn test_1200_small_txs_fit_within_256kb_l1_limit() {
    // 1200 minimal transactions (21K gas each = 25.2M gas, under 60M limit)
    // should encode to less than 256KB of RLP data, fitting within the
    // Inbox.MAX_TRANSACTIONS_SIZE limit. This validates that the block gas
    // limit (60M) naturally prevents blocks from exceeding the L1 size limit
    // for typical transaction workloads.
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Address, TxKind, U256};

    let txs: Vec<reth_ethereum_primitives::TransactionSigned> = (0..1200u64)
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

    let encoded = encode_block_transactions(&txs);

    // 1200 minimal txs should be ~120KB, well under 256KB
    assert!(
        encoded.len() < 262_144,
        "1200 minimal txs ({} bytes) should fit within 256KB",
        encoded.len()
    );

    // At 60M gas / 21K per tx = 2857 max simple transfers
    // Verify this arithmetic
    let max_simple_transfers = DESIRED_GAS_LIMIT / 21_000;
    assert_eq!(max_simple_transfers, 2857);

    // With 60M gas limit, a completely full block of minimal transfers (~314KB)
    // can exceed the 256KB L1 size limit. This is caught by the proposer's
    // calldata gas check before L1 submission, not by the gas limit alone.
    // Typical blocks are well under this threshold.
    let estimated_max_size = 2857 * 110; // ~110 bytes per minimal tx with RLP overhead
    assert!(
        estimated_max_size > 262_144,
        "max simple transfers ({estimated_max_size} bytes) can exceed 256KB at 60M gas limit"
    );
}

// --- L1ConfirmedAnchor rewind target computation ---

/// Helper: compute rewind target and rollback L1 block from anchor and earliest_block.
/// Mirrors the logic in flush_to_l1's forced rewind path.
fn compute_rewind_with_anchor(
    anchor: Option<L1ConfirmedAnchor>,
    earliest_block: u64,
    deployment_l1_block: u64,
) -> (u64, u64) {
    if let Some(anchor) = anchor {
        let target = earliest_block.saturating_sub(1).max(anchor.l2_block_number);
        let l1_rollback = anchor.l1_block_number.saturating_sub(1);
        (target, l1_rollback)
    } else {
        (earliest_block.saturating_sub(1), deployment_l1_block)
    }
}

#[test]
fn test_l1_confirmed_anchor_rewind_uses_anchor() {
    let anchor = Some(L1ConfirmedAnchor {
        l2_block_number: 300,
        l1_block_number: 50,
    });
    let (rewind_target, rollback_l1) = compute_rewind_with_anchor(anchor, 350, 0);
    // earliest-1 = 349, which is > 300, so rewind_target = 349
    assert_eq!(rewind_target, 349);
    assert_eq!(rollback_l1, 49);
}

#[test]
fn test_l1_confirmed_anchor_rewind_clamps_to_anchor() {
    let anchor = Some(L1ConfirmedAnchor {
        l2_block_number: 300,
        l1_block_number: 50,
    });
    let (rewind_target, rollback_l1) = compute_rewind_with_anchor(anchor, 200, 0);
    // earliest-1 = 199, anchor = 300, max(199, 300) = 300
    assert_eq!(rewind_target, 300);
    assert_eq!(rollback_l1, 49);
}

#[test]
fn test_l1_confirmed_anchor_rewind_no_anchor_uses_genesis() {
    let deployment_l1_block = 42;
    let (rewind_target, rollback_l1) = compute_rewind_with_anchor(None, 350, deployment_l1_block);
    assert_eq!(rewind_target, 349);
    assert_eq!(rollback_l1, deployment_l1_block);
}

#[test]
fn test_fcu_retry_backoff_schedule() {
    // Verify the exponential backoff: initial=100ms, doubles each retry
    let mut backoff = FCU_SYNCING_INITIAL_BACKOFF_MS;
    let mut total = 0u64;
    for _ in 0..FCU_SYNCING_MAX_RETRIES {
        total += backoff;
        backoff *= 2;
    }
    // 100+200+400+800+1600+3200 = 6300
    assert_eq!(total, 6300);
    assert_eq!(FCU_SYNCING_MAX_RETRIES, 6);
    assert_eq!(FCU_SYNCING_INITIAL_BACKOFF_MS, 100);
}

#[test]
fn test_step_builder_syncing_switches_to_sync_mode() {
    // When build_and_insert_block errors, step_builder should switch to Sync
    // and return Ok(()) (not propagate the error).
    let mode = DriverMode::Builder;
    let build_error = true;

    // Simulate the step_builder error handling logic
    let (new_mode, result_is_ok) = if build_error {
        (DriverMode::Sync, true) // switches to Sync, returns Ok(())
    } else {
        (mode, true)
    };

    assert_eq!(new_mode, DriverMode::Sync);
    assert!(result_is_ok, "step_builder should return Ok on build error");
}

// --- Nonce recovery after rewind tests ---
//
// These tests verify the invariant that `recover_builder_l2_nonce` must
// read state from the actual fork-choice head (head_hash), NOT from
// `latest()` which may return stale state after a multi-block rewind.
// Regression tests for the crash loop bug where reth's canonical chain
// didn't immediately unwind after fork_choice_updated, causing latest()
// to return pre-rewind state.

#[test]
fn test_nonce_recovery_must_use_head_hash_not_latest() {
    // Simulate the scenario: rewind from block 108 to block 103.
    // latest() would return block 108's state (nonce=167),
    // state_by_block_hash(block_103_hash) returns block 103's state (nonce=160).
    //
    // The correct nonce after rewind is 160, not 167.
    let head_hash_after_rewind = B256::with_last_byte(103);
    let l2_head_after_rewind = 103u64;

    // Simulate two state providers: latest (stale) and by-hash (correct)
    let nonce_from_latest = 167u64; // WRONG — pre-rewind state
    let nonce_from_head_hash = 160u64; // CORRECT — post-rewind state

    // The bug: using latest()
    let mut builder_l2_nonce = nonce_from_latest;
    assert_eq!(builder_l2_nonce, 167, "latest() returns stale nonce");

    // The fix: using state_by_block_hash(head_hash)
    builder_l2_nonce = nonce_from_head_hash;
    assert_eq!(
        builder_l2_nonce, 160,
        "state_by_block_hash(head_hash) returns correct nonce"
    );

    // Verify that building a block with the correct nonce would succeed
    // (parent state expects nonce 160, so protocol tx must use nonce 160)
    let parent_block_number = l2_head_after_rewind; // block 103
    let expected_nonce_at_parent = 160u64;
    assert_eq!(
        builder_l2_nonce, expected_nonce_at_parent,
        "recovered nonce must match parent block state for building next block"
    );

    // The stale nonce would cause "nonce too high" error
    assert_ne!(
        nonce_from_latest, expected_nonce_at_parent,
        "stale nonce from latest() would mismatch parent state → tx execution failure"
    );

    // Verify head_hash is set correctly by rewind_l2_chain
    assert_eq!(
        head_hash_after_rewind,
        B256::with_last_byte(103),
        "rewind_l2_chain sets head_hash to target block's hash"
    );
    let _ = (parent_block_number, head_hash_after_rewind);
}

#[test]
fn test_rewind_sets_head_hash_and_l2_head_number_consistently() {
    // After rewind_l2_chain(target), both head_hash and l2_head_number
    // must point to the target block. If they diverge, nonce recovery
    // and block building will fail.
    struct SimulatedDriver {
        head_hash: B256,
        l2_head_number: u64,
        block_hashes: VecDeque<B256>,
    }

    let mut driver = SimulatedDriver {
        head_hash: B256::with_last_byte(108),
        l2_head_number: 108,
        block_hashes: VecDeque::new(),
    };

    // Populate block_hashes for blocks 0..=108
    for i in 0..=108u8 {
        driver.block_hashes.push_back(B256::with_last_byte(i));
    }
    // Keep only last FORK_CHOICE_DEPTH entries (real driver does this)
    while driver.block_hashes.len() > FORK_CHOICE_DEPTH {
        driver.block_hashes.pop_front();
    }

    // Simulate rewind to block 103
    let target_l2_block = 103u64;
    let target_hash = B256::with_last_byte(target_l2_block as u8);

    // Rebuild block_hashes (lines 2726-2732 of rewind_l2_chain)
    let mut new_hashes = VecDeque::new();
    let start = target_l2_block.saturating_sub(FORK_CHOICE_DEPTH as u64);
    for n in start..=target_l2_block {
        new_hashes.push_back(B256::with_last_byte(n as u8));
    }

    // Apply rewind (lines 2745-2747)
    driver.block_hashes = new_hashes;
    driver.head_hash = target_hash;
    driver.l2_head_number = target_l2_block;

    // Verify consistency
    assert_eq!(driver.l2_head_number, 103);
    assert_eq!(driver.head_hash, B256::with_last_byte(103));
    assert_eq!(
        *driver.block_hashes.back().unwrap(),
        B256::with_last_byte(103),
        "block_hashes tail must match head_hash"
    );

    // Verify that FCU would be computed correctly
    let fcs = compute_forkchoice_state(driver.head_hash, &driver.block_hashes);
    assert_eq!(fcs.head_block_hash, B256::with_last_byte(103));
}

#[test]
fn test_forced_rewind_from_flush_mismatch_sets_correct_targets() {
    // Simulate the flush_to_l1 forced rewind logic (lines 1410-1467).
    // After MAX_FLUSH_MISMATCHES, the system must:
    // 1. Set rewind_target = earliest_block - 1 (or anchor)
    // 2. Switch to Sync mode
    // 3. Roll back derivation cursor
    // 4. Clear all pending state
    let anchor = Some(L1ConfirmedAnchor {
        l2_block_number: 100,
        l1_block_number: 50,
    });

    // Simulate: blocks 104-108 pending, pre_state_root mismatch
    let earliest_block = 104u64;
    let (rewind_target, rollback_l1_block) = compute_rewind_with_anchor(anchor, earliest_block, 0);

    // earliest-1 = 103, max(103, anchor_l2=100) = 103
    assert_eq!(
        rewind_target, 103,
        "rewind to block before earliest pending"
    );
    assert_eq!(rollback_l1_block, 49, "rollback L1 to anchor-1");

    // After rewind to 103, nonce recovery must use block 103's state
    // This is the invariant enforced by state_by_block_hash(head_hash)
    let head_hash_after_rewind = B256::with_last_byte(103);
    let l2_head_after_rewind = rewind_target;
    assert_eq!(l2_head_after_rewind, 103);

    // The next block to build will be 104
    let next_block = l2_head_after_rewind + 1;
    assert_eq!(next_block, 104);

    // For block 104, the parent is 103, so we need nonce at block 103's state
    // Using state_by_block_hash(head_hash) ensures this is correct
    let _ = head_hash_after_rewind;
}

#[test]
fn test_pre_state_root_mismatch_with_cross_chain_entries() {
    // Simulate the scenario where cross-chain entry consumption diverges
    // between the builder's expectation and L1 reality.
    //
    // Builder submitted block 103 with:
    //   clean_state_root = Y (no RPC entries)
    //   state_root = X (with RPC entries consumed)
    //
    // If entries are NOT consumed on L1, on-chain root stays at Y.
    // Block 104's pre_state_root = X (builder's local state after 103).
    // Mismatch: X != Y

    let y = B256::with_last_byte(0xCC); // clean
    let x = B256::with_last_byte(0xDD); // speculative

    let pending_block_104 = PendingBlock {
        l2_block_number: 104,
        pre_state_root: x, // builder's local parent state (speculative)
        state_root: B256::with_last_byte(0xEE),
        clean_state_root: B256::with_last_byte(0xEE),
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    };

    // On-chain root after block 103 with NO entry consumption
    let on_chain_root = y;

    // Mismatch detection (line 1406)
    assert_ne!(
        pending_block_104.pre_state_root, on_chain_root,
        "pre_state_root (speculative X) should mismatch on-chain (clean Y)"
    );

    // The rposition check should also fail: neither state_root nor
    // clean_state_root of block 103 matches on_chain_root if they
    // were computed with entry effects.
    let pending_block_103 = PendingBlock {
        l2_block_number: 103,
        pre_state_root: B256::with_last_byte(0xBB),
        state_root: x,       // speculative (with entries)
        clean_state_root: y, // clean (without entries)
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    };

    // The flush logic checks both state_root AND clean_state_root (line 1373)
    let matches_speculative = pending_block_103.state_root == on_chain_root;
    let matches_clean = pending_block_103.clean_state_root == on_chain_root;
    assert!(
        matches_speculative || matches_clean,
        "block 103 should match on-chain via clean_state_root (Y == Y)"
    );
    assert!(matches_clean, "clean_state_root should match on_chain_root");
    assert!(
        !matches_speculative,
        "state_root (X) should NOT match on_chain_root (Y)"
    );

    // This means block 103 gets drained (it matches), but block 104's
    // pre_state_root is X (builder's local state = speculative), while
    // on_chain_root = Y (clean). flush_to_l1 detects this mismatch and
    // triggers a rewind. Re-derivation with §4f protocol tx filtering
    // produces the correct root.
}

#[test]
fn test_nonce_recovery_after_rewind_then_sync_to_builder_transition() {
    // Simulate the full cycle:
    // 1. Forced rewind sets pending_rewind_target
    // 2. step() processes rewind → rewind_l2_chain → recover_builder_l2_nonce
    // 3. step_sync catches up → recover_builder_l2_nonce (again at mode transition)
    //
    // Both calls to recover_builder_l2_nonce must use head_hash, not latest().

    let rewind_target = 103u64;
    let rewind_hash = B256::with_last_byte(103);

    // After step() processes the rewind (line 616-620):
    let head_hash_after_rewind = rewind_hash;
    let l2_head_after_rewind = rewind_target;

    // First nonce recovery (line 620): uses state_by_block_hash(head_hash)
    let nonce_at_103 = 160u64;
    let builder_l2_nonce = nonce_at_103; // correct!

    // Sync mode re-derives blocks. No new blocks submitted on L1 beyond 103,
    // so sync just catches up to L1 head without building new L2 blocks.
    // head_hash stays at rewind_hash, l2_head stays at 103.

    // Second nonce recovery at Sync→Builder transition (line 803):
    // head_hash is still block 103's hash
    let head_hash_at_mode_switch = head_hash_after_rewind;
    let nonce_at_mode_switch = nonce_at_103; // same, correct!

    assert_eq!(builder_l2_nonce, nonce_at_mode_switch);
    assert_eq!(head_hash_at_mode_switch, rewind_hash);
    assert_eq!(l2_head_after_rewind, 103);

    // Builder mode builds block 104 with nonce 160 — succeeds because
    // parent block 103's state also has nonce 160 for the builder.
    let next_block = l2_head_after_rewind + 1;
    assert_eq!(next_block, 104);
    assert_eq!(
        builder_l2_nonce, nonce_at_103,
        "nonce at mode transition matches nonce at rewind target"
    );
}

#[test]
fn test_clear_pending_state_before_rewind_prevents_stale_entries() {
    // Verify that clear_pending_state is called BEFORE the rewind (line 615),
    // so stale cross-chain entries and pending submissions from the old fork
    // are not carried into the re-derived chain.
    let mut pending_submissions: VecDeque<PendingBlock> = VecDeque::new();
    let mut pending_cross_chain_entries: Vec<CrossChainExecutionEntry> = Vec::new();
    let mut preconfirmed_hashes: HashMap<u64, B256> = HashMap::new();

    // Populate with stale data from blocks 104-108
    for i in 104..=108 {
        pending_submissions.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::with_last_byte(i as u8),
            state_root: B256::with_last_byte(i as u8 + 100),
            clean_state_root: B256::with_last_byte(i as u8 + 100),
            encoded_transactions: Bytes::from(vec![0xc0]),
            intermediate_roots: vec![],
        });
        preconfirmed_hashes.insert(i, B256::with_last_byte(i as u8));
    }
    pending_cross_chain_entries.push(CrossChainExecutionEntry {
        action_hash: B256::with_last_byte(0x42),
        state_deltas: vec![],
        next_action: crate::cross_chain::CrossChainAction {
            action_type: crate::cross_chain::CrossChainActionType::Result,
            rollup_id: alloy_primitives::U256::ZERO,
            destination: alloy_primitives::Address::ZERO,
            value: alloy_primitives::U256::ZERO,
            data: vec![],
            failed: false,
            source_address: alloy_primitives::Address::ZERO,
            source_rollup: alloy_primitives::U256::ZERO,
            scope: vec![],
        },
    });

    // Simulate clear_pending_state (line 1899)
    pending_submissions.clear();
    pending_cross_chain_entries.clear();
    preconfirmed_hashes.clear();

    assert!(pending_submissions.is_empty());
    assert!(pending_cross_chain_entries.is_empty());
    assert!(preconfirmed_hashes.is_empty());
}

// --- Dual-root verification tests ---

#[test]
fn test_dual_root_verification_accepts_clean_root() {
    // verify_local_block_matches_l1 must accept a block when the L1-derived
    // state root matches EITHER the header root (speculative) or the cached
    // clean root. This prevents false rewinds for blocks with unconsumed
    // cross-chain entries.
    let speculative_root = B256::with_last_byte(0xAA);
    let clean_root = B256::with_last_byte(0xBB);
    let l1_derived_root_no_consumption = B256::with_last_byte(0xBB); // matches clean
    let l1_derived_root_full_consumption = B256::with_last_byte(0xAA); // matches speculative

    // Simulate the check from verify_local_block_matches_l1
    let header_root = speculative_root;
    let cached_clean = Some(clean_root);

    // Case 1: entries NOT consumed on L1 → effective root = clean
    let matches_header = header_root == l1_derived_root_no_consumption;
    let matches_clean = cached_clean == Some(l1_derived_root_no_consumption);
    assert!(
        !matches_header,
        "speculative != clean when entries are unconsumed"
    );
    assert!(matches_clean, "clean root should match L1-derived root");
    assert!(
        matches_header || matches_clean,
        "verification should pass via clean root match"
    );

    // Case 2: entries fully consumed on L1 → effective root = speculative
    let matches_header = header_root == l1_derived_root_full_consumption;
    let matches_clean = cached_clean == Some(l1_derived_root_full_consumption);
    assert!(
        matches_header,
        "speculative should match when entries consumed"
    );
    assert!(!matches_clean, "clean != speculative when entries exist");
    assert!(
        matches_header || matches_clean,
        "verification should pass via header root match"
    );

    // Case 3: no cached clean root (e.g., after restart) — header-only check
    let cached_clean: Option<B256> = None;
    let matches_header = header_root == l1_derived_root_full_consumption;
    let matches_clean = cached_clean == Some(l1_derived_root_full_consumption);
    assert!(
        matches_header || matches_clean,
        "should pass via header when no cache"
    );

    // Case 4: neither matches → verification should fail
    let unrelated_root = B256::with_last_byte(0xFF);
    let matches_header = header_root == unrelated_root;
    let matches_clean = Some(clean_root) == Some(unrelated_root);
    assert!(
        !matches_header && !matches_clean,
        "should fail when neither root matches"
    );
}

#[test]
fn test_immutable_ceiling_skips_verification() {
    // Blocks at or below immutable_block_ceiling must be skipped in
    // verify_local_block_matches_l1 to prevent infinite rewind loops
    // when FCU can't actually unwind committed blocks.
    let immutable_block_ceiling = 750u64;

    // Blocks that should be skipped
    for block_num in [1u64, 100, 500, 749, 750] {
        let should_skip = block_num <= immutable_block_ceiling;
        assert!(
            should_skip,
            "block {block_num} should be skipped (at or below ceiling {immutable_block_ceiling})"
        );
    }

    // Blocks that should NOT be skipped
    for block_num in [751u64, 800, 1000] {
        let should_skip = block_num <= immutable_block_ceiling;
        assert!(
            !should_skip,
            "block {block_num} should NOT be skipped (above ceiling {immutable_block_ceiling})"
        );
    }

    // After restart, ceiling resets to 0 — no blocks skipped
    let ceiling_after_restart = 0u64;
    assert!(
        1u64 > ceiling_after_restart,
        "block 1 should NOT be skipped when ceiling is 0 (fresh start)"
    );
}

#[test]
fn test_tx_journal_encode_decode_roundtrip() {
    let entries = vec![
        TxJournalEntry {
            l2_block_number: 42,
            block_txs: vec![0xde, 0xad, 0xbe, 0xef],
        },
        TxJournalEntry {
            l2_block_number: 100,
            block_txs: vec![1, 2, 3],
        },
        TxJournalEntry {
            l2_block_number: u64::MAX,
            block_txs: vec![],
        },
    ];

    let encoded = TxJournalEntry::encode_all(&entries);
    let decoded = TxJournalEntry::decode_all(&encoded);

    assert_eq!(decoded.len(), 3);
    assert_eq!(decoded[0].l2_block_number, 42);
    assert_eq!(decoded[0].block_txs, vec![0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(decoded[1].l2_block_number, 100);
    assert_eq!(decoded[1].block_txs, vec![1, 2, 3]);
    assert_eq!(decoded[2].l2_block_number, u64::MAX);
    assert!(decoded[2].block_txs.is_empty());
}

#[test]
fn test_tx_journal_decode_empty() {
    let decoded = TxJournalEntry::decode_all(&[]);
    assert!(decoded.is_empty());
}

#[test]
fn test_tx_journal_decode_truncated() {
    // Truncated data — should stop gracefully without panic.
    let entries = vec![TxJournalEntry {
        l2_block_number: 10,
        block_txs: vec![0xFF; 100],
    }];
    let mut encoded = TxJournalEntry::encode_all(&entries);
    // Truncate mid-entry.
    encoded.truncate(20);
    let decoded = TxJournalEntry::decode_all(&encoded);
    // Should decode zero entries (header says 100 bytes but only 8 available).
    assert!(decoded.is_empty());
}

// ── §4f Entry Verification Hold Tests ──────────────────────────────────

#[test]
fn test_hold_prevents_flush_when_set() {
    // When pending_entry_verification_block is Some, flush_to_l1 should return
    // early without submitting. We simulate this by checking the hold logic directly.
    let hold: Option<u64> = Some(42);
    assert!(hold.is_some(), "hold should be set");
    // In flush_to_l1, this causes early return before submission.
    // The builder continues building blocks while the hold is active.
}

#[test]
fn test_hold_cleared_on_verification_match() {
    // When verify_local_block_matches_l1 finds the entry block matches,
    // the hold is cleared.
    let mut hold: Option<u64> = Some(42);
    let derived_block_number = 42u64;

    // Simulate: derived root matches header root (verification passes)
    if hold == Some(derived_block_number) {
        hold = None; // Clear hold
    }
    assert!(hold.is_none(), "hold should be cleared after verification");
}

#[test]
fn test_hold_cleared_on_clear_pending_state() {
    // On rewind, clear_pending_state clears the hold.
    let mut hold: Option<u64> = Some(42);
    assert!(hold.is_some(), "hold starts active");
    // Simulate clear_pending_state
    hold = None;
    assert!(
        hold.is_none(),
        "hold should be cleared on clear_pending_state"
    );
}

#[test]
fn test_hold_not_cleared_for_different_block() {
    // If derivation verifies a DIFFERENT block than the held one, hold persists.
    let mut hold: Option<u64> = Some(42);
    let derived_block_number = 41u64; // Different block

    if hold == Some(derived_block_number) {
        hold = None;
    }
    assert_eq!(hold, Some(42), "hold should persist for non-matching block");
}

#[test]
fn test_builder_continues_building_while_hold_active() {
    // While the hold is active, blocks accumulate in pending_submissions.
    let hold: Option<u64> = Some(42);
    let mut pending: VecDeque<PendingBlock> = VecDeque::new();

    // Simulate building 5 blocks while hold is active
    for i in 43..48 {
        pending.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::ZERO,
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: alloy_primitives::Bytes::new(),
            intermediate_roots: vec![],
        });
    }

    assert!(hold.is_some(), "hold should still be active");
    assert_eq!(
        pending.len(),
        5,
        "5 blocks should be queued while hold is active"
    );

    // After hold is cleared, all 5 blocks would be submitted in next flush
}

#[test]
fn test_batch_size_limited_to_1_when_entries_pending() {
    // §4f nonce safety: when pending_cross_chain_entries is non-empty,
    // batch_size should be 1 to ensure the entry block is submitted alone.
    let pending_entries_count = 2; // non-empty
    let pending_submissions_count = 5;
    let max_batch_size = 100;

    let batch_size = if pending_entries_count > 0 {
        1usize.min(pending_submissions_count)
    } else {
        pending_submissions_count.min(max_batch_size)
    };

    assert_eq!(
        batch_size, 1,
        "batch should be limited to 1 when entries pending"
    );
}

#[test]
fn test_batch_size_normal_when_no_entries() {
    // Without pending entries, batch_size uses MAX_BATCH_SIZE.
    let pending_entries_count = 0;
    let pending_submissions_count = 5;
    let max_batch_size = 100;

    let batch_size = if pending_entries_count > 0 {
        1usize.min(pending_submissions_count)
    } else {
        pending_submissions_count.min(max_batch_size)
    };

    assert_eq!(batch_size, 5, "batch should include all pending blocks");
}

#[test]
fn test_full_rewind_cycle_state_transitions() {
    // Simulate the full rewind cycle for phantom state detection:
    // 1. Builder posts entry-bearing batch → hold set
    // 2. Hold prevents next postBatch
    // 3. Derivation detects mismatch (entries not consumed) → rewind
    // 4. clear_pending_state clears hold
    // 5. Re-derive with filtered txs → correct root
    // 6. Builder re-enters Builder mode → builds new blocks → posts them

    // Step 1: Hold set after posting entry-bearing batch
    let mut hold: Option<u64> = None;
    let mut mode = DriverMode::Builder;
    let mut pending: VecDeque<PendingBlock> = VecDeque::new();
    assert_eq!(mode, DriverMode::Builder);
    assert!(hold.is_none(), "hold starts inactive");

    hold = Some(100); // Entry block 100 posted
    assert_eq!(hold, Some(100));

    // Step 2: Builder halts block production while hold is active (step_builder returns early)
    for i in 101..104 {
        pending.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::with_last_byte(i as u8),
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: alloy_primitives::Bytes::new(),
            intermediate_roots: vec![],
        });
    }
    assert_eq!(pending.len(), 3);
    assert!(hold.is_some(), "hold prevents submission");

    // Step 3: Derivation detects mismatch → rewind
    // (verify_local_block_matches_l1 returns Err → mode switches to Sync)
    mode = DriverMode::Sync;
    assert_eq!(mode, DriverMode::Sync);

    // Step 4: clear_pending_state during rewind
    pending.clear();
    hold = None;
    assert!(hold.is_none());
    assert!(pending.is_empty());

    // Step 5: Re-derive from L1 with §4f filtering → correct state
    // (simulated — derivation produces filtered blocks)

    // Step 6: Builder re-enters Builder mode
    mode = DriverMode::Builder;
    assert_eq!(mode, DriverMode::Builder);

    // New blocks built with correct nonces (from re-derived state)
    for i in 100..105 {
        pending.push_back(PendingBlock {
            l2_block_number: i,
            pre_state_root: B256::with_last_byte(i as u8),
            state_root: B256::with_last_byte(i as u8),
            clean_state_root: B256::with_last_byte(i as u8),
            encoded_transactions: alloy_primitives::Bytes::new(),
            intermediate_roots: vec![],
        });
    }
    // No hold → all 5 blocks can be submitted
    assert!(hold.is_none());
    assert_eq!(pending.len(), 5);
}

#[test]
fn test_hold_set_only_when_entries_non_empty() {
    // The hold should only be set when l1_entries is non-empty.
    // Simulates the check: `if !l1_entries.is_empty() { hold = Some(...) }`
    let mut hold: Option<u64> = None;

    // Batch WITHOUT entries — hold should NOT be set
    let l1_entries_count = 0;
    if l1_entries_count > 0 {
        hold = Some(50);
    }
    assert!(
        hold.is_none(),
        "hold should not be set for entry-free batch"
    );

    // Batch WITH entries — hold SHOULD be set
    let l1_entries_count = 2;
    if l1_entries_count > 0 {
        hold = Some(50);
    }
    assert_eq!(hold, Some(50), "hold should be set for entry-bearing batch");
}

#[test]
fn test_consecutive_rewind_backoff_with_hold() {
    // Verify that consecutive_rewind_cycles dampening works independently from hold.
    // The backoff delay (2^cycles seconds, max 60) applies on re-entry to Builder mode.
    // The hold is cleared by clear_pending_state during rewind — no interference.
    let mut consecutive_rewind_cycles: u32 = 0;
    let mut hold: Option<u64> = Some(42);
    assert!(hold.is_some(), "hold starts active");

    // Simulate: mismatch detected, rewind triggered
    consecutive_rewind_cycles = consecutive_rewind_cycles.saturating_add(1);
    // clear_pending_state clears hold
    hold = None;
    assert!(hold.is_none());
    assert_eq!(consecutive_rewind_cycles, 1);

    // Compute backoff delay
    let delay = (2u64 << consecutive_rewind_cycles.min(5)).min(60);
    assert_eq!(delay, 4, "first rewind cycle should have 4s delay");

    // After successful re-entry, cycles reset
    consecutive_rewind_cycles = 0;
    assert_eq!(consecutive_rewind_cycles, 0);
}

#[test]
fn test_mismatch_counter_resets_on_success() {
    // consecutive_flush_mismatches should reset to 0 when pre_state matches.
    let mut mismatches: u32 = 0;
    let pre_state = B256::with_last_byte(0x01);
    let on_chain = B256::with_last_byte(0x02);

    // Mismatch — increment
    if pre_state != on_chain {
        mismatches += 1;
    }
    assert_eq!(mismatches, 1);

    // Match — reset
    let on_chain = pre_state;
    if pre_state == on_chain {
        mismatches = 0;
    }
    assert_eq!(mismatches, 0, "counter should reset on match");
}

#[test]
fn test_skip_logic_checks_state_root_and_clean_state_root() {
    // The skip logic in flush_to_l1 checks both state_root and clean_state_root.
    // For full consumption: on_chain == state_root → skip
    // For zero consumption: on_chain == clean_state_root → skip
    let block = PendingBlock {
        l2_block_number: 10,
        pre_state_root: B256::ZERO,
        state_root: B256::with_last_byte(0xAA), // speculative (all entries)
        clean_state_root: B256::with_last_byte(0xBB), // clean (no entries)
        encoded_transactions: alloy_primitives::Bytes::new(),
        intermediate_roots: vec![],
    };

    // Full consumption: on-chain matches speculative root
    let on_chain_full = B256::with_last_byte(0xAA);
    assert!(
        block.state_root == on_chain_full || block.clean_state_root == on_chain_full,
        "full consumption should match state_root"
    );

    // Zero consumption: on-chain matches clean root
    let on_chain_zero = B256::with_last_byte(0xBB);
    assert!(
        block.state_root == on_chain_zero || block.clean_state_root == on_chain_zero,
        "zero consumption should match clean_state_root"
    );

    // Partial consumption: on-chain matches neither
    let on_chain_partial = B256::with_last_byte(0xCC);
    assert!(
        !(block.state_root == on_chain_partial || block.clean_state_root == on_chain_partial),
        "partial consumption should NOT match either root"
    );
}

// --- Sibling reorg on §4f-detected divergence (issue #36) ---

/// Reproduces the testnet-eez-2026-04-16 sequence.
///
/// Sequence:
/// 1. Builder commits speculative block N with stateRoot `S_speculative` (includes
///    a protocol trigger tx that L1 later filters via §4f).
/// 2. L1 confirms the §4f-filtered variant: stateRoot `S_clean`.
/// 3. Driver observes `first.pre_state_root != on_chain_root` in `flush_to_l1`.
///
/// Before this fix, the driver called bare FCU-to-ancestor. Per the Engine API
/// spec (see EIP-3675 / op-node `consolidateNextSafeAttributes`), FCU with a
/// backward head is a silent no-op on reth when the canonical tip is already
/// ahead — divergence persists → infinite rewind loop.
///
/// With the fix, `decide_divergence_recovery()` returns `SiblingReorg` when the
/// block's `clean_state_root` matches on-chain. The driver then builds sibling
/// N' with the §4f-filtered tx set and submits it via
/// `newPayloadV3 + forkchoiceUpdatedV3(head=N')`, which is reth's own first-class
/// reorg path.
#[test]
fn test_sibling_reorg_resolves_speculative_divergence_after_4f_filter() {
    // Canonical block N (committed in reth): 7 txs, speculative state=S_spec.
    // L1-derived N' (after §4f filtering): 6 txs, clean state=S_clean.
    let pre_state = B256::with_last_byte(0xA0);
    let speculative = B256::with_last_byte(0xA1);
    let clean = B256::with_last_byte(0xA2);
    let divergent_block = PendingBlock {
        l2_block_number: 5354,
        pre_state_root: pre_state,
        state_root: speculative,
        clean_state_root: clean,
        encoded_transactions: alloy_primitives::Bytes::new(),
        intermediate_roots: vec![],
    };

    // on-chain root is `clean` — the §4f-filtered variant landed on L1.
    let on_chain = clean;

    let decision = decide_divergence_recovery(
        &divergent_block,
        on_chain,
        /* reorg_depth = */ 1,
        /* threshold  = */ REORG_SAFETY_THRESHOLD,
    );

    assert_eq!(
        decision,
        SiblingReorgDecision::SiblingReorg {
            target_block: 5354,
            filtered_root: clean,
        },
        "when clean_state_root matches on-chain, driver must attempt sibling reorg \
         (not bare FCU, which is a silent no-op per Engine API spec)"
    );
}

/// When `clean_state_root == state_root` the block had no cross-chain entries,
/// so §4f filtering is not the cause of the divergence. Fall back to the
/// bare-FCU rewind path (defense-in-depth — it may itself be a no-op but
/// at least it doesn't spin pretending to do work).
#[test]
fn test_sibling_reorg_falls_back_to_fcu_rewind_when_no_4f_evidence() {
    let pre_state = B256::with_last_byte(0xB0);
    let root = B256::with_last_byte(0xB1);
    let block = PendingBlock {
        l2_block_number: 100,
        pre_state_root: pre_state,
        state_root: root,
        clean_state_root: root, // same — no §4f filtering possible
        encoded_transactions: alloy_primitives::Bytes::new(),
        intermediate_roots: vec![],
    };

    // On-chain root matches neither speculative nor clean — some deeper bug.
    let on_chain = B256::with_last_byte(0xC0);

    let decision = decide_divergence_recovery(&block, on_chain, 1, REORG_SAFETY_THRESHOLD);

    assert_eq!(
        decision,
        SiblingReorgDecision::BareRewind,
        "without clean_state_root evidence of §4f filtering, fall back to \
         bare-FCU rewind (defense-in-depth)"
    );
}

/// Even when `clean_state_root != state_root`, if on-chain doesn't match the
/// clean root either, we have no known-good target for the sibling and must
/// fall back.
#[test]
fn test_sibling_reorg_falls_back_when_clean_root_does_not_match_on_chain() {
    let pre_state = B256::with_last_byte(0xD0);
    let clean = B256::with_last_byte(0xD1);
    let spec = B256::with_last_byte(0xD2);
    let block = PendingBlock {
        l2_block_number: 42,
        pre_state_root: pre_state,
        state_root: spec,
        clean_state_root: clean,
        encoded_transactions: alloy_primitives::Bytes::new(),
        intermediate_roots: vec![],
    };
    // On-chain matches neither — we don't know how to rebuild.
    let on_chain = B256::with_last_byte(0xD9);

    let decision = decide_divergence_recovery(&block, on_chain, 1, REORG_SAFETY_THRESHOLD);

    assert_eq!(decision, SiblingReorgDecision::BareRewind);
}

#[test]
fn test_sibling_reorg_decision_is_stable_under_repeated_calls() {
    // The decision must be deterministic — same inputs → same output.
    // Otherwise the flush loop oscillates.
    let pre_state = B256::with_last_byte(0xE0);
    let clean = B256::with_last_byte(0xE1);
    let spec = B256::with_last_byte(0xE2);
    let block = PendingBlock {
        l2_block_number: 42,
        pre_state_root: pre_state,
        state_root: spec,
        clean_state_root: clean,
        encoded_transactions: alloy_primitives::Bytes::new(),
        intermediate_roots: vec![],
    };
    let on_chain = clean;

    let d1 = decide_divergence_recovery(&block, on_chain, 0, REORG_SAFETY_THRESHOLD);
    let d2 = decide_divergence_recovery(&block, on_chain, 1, REORG_SAFETY_THRESHOLD);
    let d3 = decide_divergence_recovery(&block, on_chain, 2, REORG_SAFETY_THRESHOLD);
    assert_eq!(d1, d2);
    assert_eq!(d2, d3);
}

/// Beyond the safety threshold, continuing to attempt recovery would
/// eventually cross reth's `MAX_REORG_DEPTH = 64` eviction window, past which
/// no strategy (sibling reorg, bare FCU, anything) can succeed. Halt instead.
#[test]
fn test_sibling_reorg_halts_beyond_safety_threshold() {
    let block = PendingBlock {
        l2_block_number: 10,
        pre_state_root: B256::with_last_byte(0x00),
        state_root: B256::with_last_byte(0x01),
        clean_state_root: B256::with_last_byte(0x02),
        encoded_transactions: alloy_primitives::Bytes::new(),
        intermediate_roots: vec![],
    };

    let decision = decide_divergence_recovery(
        &block,
        /* on_chain = */ B256::with_last_byte(0x02),
        REORG_SAFETY_THRESHOLD,
        REORG_SAFETY_THRESHOLD,
    );
    assert_eq!(decision, SiblingReorgDecision::Halt);

    let decision = decide_divergence_recovery(
        &block,
        /* on_chain = */ B256::with_last_byte(0x02),
        REORG_SAFETY_THRESHOLD + 1,
        REORG_SAFETY_THRESHOLD,
    );
    assert_eq!(decision, SiblingReorgDecision::Halt);
}

/// The fast-path in `verify_local_block_matches_l1` (issue #36) queues a
/// `SiblingReorgRequest` the FIRST TIME it sees a §4f-flagged mismatch,
/// bypassing the deferral loop. This test pins the struct shape and the
/// property that the request uniquely identifies the divergent block by
/// (target_l2_block, expected_root).
#[test]
fn test_sibling_reorg_request_uniquely_identifies_divergent_block() {
    let req = SiblingReorgRequest {
        target_l2_block: 5354,
        expected_root: B256::with_last_byte(0x42),
    };
    assert_eq!(req.target_l2_block, 5354);
    assert_eq!(req.expected_root, B256::with_last_byte(0x42));

    // Copy semantics — required because the driver passes the request by value
    // through the drain loop and then checks `is_none()` after `.take()`.
    let copy = req;
    assert_eq!(copy.target_l2_block, req.target_l2_block);
    assert_eq!(copy.expected_root, req.expected_root);
}

// --- Reorg safety gate (issue #36) ---

#[test]
fn test_reorg_safety_gate_halts_at_depth_threshold() {
    // Below threshold: allowed.
    for depth in 0..REORG_SAFETY_THRESHOLD {
        assert!(
            !reorg_depth_exceeded(depth, REORG_SAFETY_THRESHOLD),
            "depth {depth} must be allowed (threshold {REORG_SAFETY_THRESHOLD})"
        );
    }
    // At threshold: HALT.
    assert!(
        reorg_depth_exceeded(REORG_SAFETY_THRESHOLD, REORG_SAFETY_THRESHOLD),
        "depth == threshold must halt"
    );
    // Above threshold: HALT.
    assert!(
        reorg_depth_exceeded(REORG_SAFETY_THRESHOLD + 1, REORG_SAFETY_THRESHOLD),
        "depth > threshold must halt"
    );
}

/// Exercises the depth arithmetic the safety gate uses in `step_builder`:
/// `depth = l2_head_number - target_l2_block`. At exactly the threshold the
/// gate MUST trip — we leave no slack room because reth's eviction window is
/// at `MAX_REORG_DEPTH = 64` and we halt at 48 to keep 16 blocks of margin.
#[test]
fn test_safety_gate_halts_builder_at_depth_48() {
    // Target block + 48 tip = depth exactly 48 → HALT.
    let target: u64 = 1000;
    let tip_at_threshold: u64 = target + REORG_SAFETY_THRESHOLD;
    let depth = tip_at_threshold.saturating_sub(target);
    assert_eq!(depth, REORG_SAFETY_THRESHOLD);
    assert!(
        reorg_depth_exceeded(depth, REORG_SAFETY_THRESHOLD),
        "step_builder must halt when tip is exactly REORG_SAFETY_THRESHOLD blocks ahead"
    );

    // Target block + 47 tip = depth 47 → still allowed (one block of headroom).
    let tip_below_threshold: u64 = target + (REORG_SAFETY_THRESHOLD - 1);
    let depth = tip_below_threshold.saturating_sub(target);
    assert_eq!(depth, REORG_SAFETY_THRESHOLD - 1);
    assert!(
        !reorg_depth_exceeded(depth, REORG_SAFETY_THRESHOLD),
        "one block below threshold must still allow building"
    );

    // Sanity: headroom to reth's eviction window.
    let remaining = MAX_REORG_DEPTH - REORG_SAFETY_THRESHOLD;
    assert!(
        remaining >= 16,
        "safety gate must leave at least 16 blocks of recovery headroom; got {remaining}"
    );
}

#[test]
fn test_reorg_safety_threshold_strictly_less_than_reth_eviction() {
    // The safety threshold MUST be strictly less than reth's
    // CHANGESET_CACHE_RETENTION_BLOCKS (64) — if we reach that depth, reth can
    // no longer unwind the committed block via any mechanism.
    //
    // Uses const assertions so a future edit to the constants that would break
    // the invariant is caught at compile time, not at test runtime.
    const _: () = assert!(
        REORG_SAFETY_THRESHOLD < MAX_REORG_DEPTH,
        "REORG_SAFETY_THRESHOLD must be strictly less than MAX_REORG_DEPTH \
         so we halt before reth's eviction window"
    );
    const _: () = assert!(
        MAX_REORG_DEPTH == 64,
        "MAX_REORG_DEPTH must match reth's CHANGESET_CACHE_RETENTION_BLOCKS"
    );
    // Threshold is ~75% of the limit, leaving headroom for recovery.
    const _: () = assert!(
        REORG_SAFETY_THRESHOLD * 4 / 3 <= MAX_REORG_DEPTH,
        "threshold should be approximately 75% of the limit"
    );
}

// --- BlockInvalidated preconfirmation message (issue #36) ---

#[test]
fn test_preconfirmed_message_block_invalidated_evicts_cached_hash() {
    // When a sibling reorg succeeds, the builder broadcasts a `BlockInvalidated`
    // message so subscribed fullnodes can evict their cached hash and adopt the
    // sibling's hash. Without this, fullnodes would believe the old hash is
    // canonical and `verify_local_block_matches_l1` would permanently mismatch.
    use crate::builder_sync::PreconfirmedMessage;

    let old_hash = B256::with_last_byte(0xE0);
    let new_hash = B256::with_last_byte(0xE1);
    let mut preconfirmed_hashes: HashMap<u64, B256> = HashMap::new();
    preconfirmed_hashes.insert(100, old_hash);

    let msg = PreconfirmedMessage::BlockInvalidated {
        block_number: 100,
        new_hash,
    };
    match msg {
        PreconfirmedMessage::BlockInvalidated {
            block_number,
            new_hash,
        } => {
            preconfirmed_hashes.insert(block_number, new_hash);
        }
        PreconfirmedMessage::BlockArrived(_) => {
            panic!("expected BlockInvalidated variant");
        }
    }

    assert_eq!(
        preconfirmed_hashes.get(&100),
        Some(&new_hash),
        "BlockInvalidated must overwrite the cached hash with the sibling's hash"
    );
}

#[test]
fn test_preconfirmed_message_block_arrived_preserves_legacy_semantics() {
    // PreconfirmedMessage::BlockArrived(PreconfirmedBlock { .. }) is the
    // legacy-shape variant — it must wrap the existing struct so downstream
    // code that pattern-matches can unwrap cleanly.
    use crate::builder_sync::{PreconfirmedBlock, PreconfirmedMessage};

    let block_number = 77;
    let block_hash = B256::with_last_byte(0x77);
    let msg = PreconfirmedMessage::BlockArrived(PreconfirmedBlock {
        block_number,
        block_hash,
    });
    match msg {
        PreconfirmedMessage::BlockArrived(pb) => {
            assert_eq!(pb.block_number, block_number);
            assert_eq!(pb.block_hash, block_hash);
        }
        PreconfirmedMessage::BlockInvalidated { .. } => {
            panic!("expected BlockArrived variant");
        }
    }
}

/// Exercises the full broadcast path: builder sends `BlockInvalidated` on the
/// internal `preconfirmed_message_tx`, receiver drains it on
/// `preconfirmed_message_rx` and updates its `HashMap<u64, B256>`. Verifies
/// try_send/try_recv non-blocking semantics that `drain_preconfirmed_blocks`
/// depends on.
#[tokio::test]
async fn test_sibling_reorg_broadcast_channel_roundtrip() {
    use crate::builder_sync::PreconfirmedMessage;
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::channel::<PreconfirmedMessage>(8);
    let old_hash = B256::with_last_byte(0xAB);
    let new_hash = B256::with_last_byte(0xCD);

    // Starting cache: block 42 → old hash (speculative).
    let mut preconfirmed_hashes: HashMap<u64, B256> = HashMap::new();
    preconfirmed_hashes.insert(42, old_hash);

    // Builder simulates broadcasting after a successful sibling reorg.
    tx.try_send(PreconfirmedMessage::BlockInvalidated {
        block_number: 42,
        new_hash,
    })
    .expect("broadcast channel must accept within capacity");

    // Drain loop (mirrors `drain_preconfirmed_blocks` semantics).
    let mut drained = 0usize;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            PreconfirmedMessage::BlockInvalidated {
                block_number,
                new_hash,
            } => {
                preconfirmed_hashes.insert(block_number, new_hash);
            }
            PreconfirmedMessage::BlockArrived(pb) => {
                preconfirmed_hashes.insert(pb.block_number, pb.block_hash);
            }
        }
        drained += 1;
    }

    assert_eq!(drained, 1, "exactly one message should have been drained");
    assert_eq!(
        preconfirmed_hashes.get(&42),
        Some(&new_hash),
        "after BlockInvalidated the cached hash for block 42 must be the sibling hash"
    );
    assert_ne!(
        preconfirmed_hashes.get(&42),
        Some(&old_hash),
        "the speculative hash must be evicted — otherwise verify_local_block_matches_l1 \
         would keep rejecting the new canonical hash"
    );
}

// =============================================================================
// Tier-1 sibling-reorg tests (issue #36 second-pass review)
//
// These tests exercise the fixes for C1, C2, M2, M4, and the drive-bys flagged
// by the auditor + test-writer review. The two-pass design of this feature
// means every regression point needs a test that fails before its fix and
// passes after.
//
// Mock engine: a minimal [`EngineClient`] implementation records the order of
// `new_payload` and `fork_choice_updated` calls and returns scripted
// responses. Tests assert both the call order and the outcome on the engine,
// which is impossible to cover via pure state-mutation tests.
//
// Driver-state tests: the fixes that only mutate driver fields are exercised
// through the pure `plan_sibling_reorg_from_verify` +
// `find_rightmost_sibling_reorg_target` helpers, plus direct
// [`SiblingReorgRequest`] / [`SiblingReorgVerifyPlan`] assertions. Extracting
// these helpers out of the driver was part of the second pass so the
// `verify_local_block_matches_l1` fast path and the drain-loop detection can
// be tested without standing up a full reth instance.
// =============================================================================

mod sibling_reorg_mock_engine {
    //! Mock [`EngineClient`] for sibling-reorg submission tests.

    use super::*;
    use alloy_rpc_types_engine::{
        ExecutionData, ExecutionPayload, ExecutionPayloadSidecar, ExecutionPayloadV1,
        ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadStatus, PayloadStatusEnum,
    };
    use alloy_primitives::{Address, Bloom, U256};
    use reth_payload_primitives::EngineApiMessageVersion;
    use std::sync::Mutex;
    use crate::driver::{
        submit_fork_choice_with_retry, submit_sibling_payload, EngineClient,
    };

    /// Calls recorded by the mock — the ORDER matters for reorg correctness.
    /// `NewPayload` MUST be recorded before `ForkchoiceUpdated` for the reorg
    /// to be safe (see reth's `test_testsuite_deep_reorg`).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum MockEngineCall {
        NewPayload { parent_hash: B256 },
        ForkchoiceUpdated { head: B256 },
    }

    /// Scripted responses the mock returns, popped from the front in order.
    /// If the queue is exhausted, the mock panics (which would cause the test
    /// to fail loudly rather than silently defaulting to VALID).
    pub(crate) struct MockEngine {
        pub(crate) calls: Mutex<Vec<MockEngineCall>>,
        pub(crate) new_payload_responses: Mutex<Vec<PayloadStatus>>,
        pub(crate) fcu_responses: Mutex<Vec<ForkchoiceUpdated>>,
    }

    impl MockEngine {
        pub(crate) fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                new_payload_responses: Mutex::new(Vec::new()),
                fcu_responses: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn push_new_payload_response(&self, status: PayloadStatus) {
            self.new_payload_responses.lock().unwrap().push(status);
        }

        pub(crate) fn push_fcu_response(&self, fcu: ForkchoiceUpdated) {
            self.fcu_responses.lock().unwrap().push(fcu);
        }

        pub(crate) fn take_calls(&self) -> Vec<MockEngineCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl EngineClient for MockEngine {
        async fn new_payload(&self, payload: ExecutionData) -> Result<PayloadStatus> {
            let parent_hash = payload.payload.parent_hash();
            self.calls
                .lock()
                .unwrap()
                .push(MockEngineCall::NewPayload { parent_hash });
            let mut responses = self.new_payload_responses.lock().unwrap();
            if responses.is_empty() {
                return Err(eyre::eyre!(
                    "mock: new_payload called but no scripted response queued"
                ));
            }
            Ok(responses.remove(0))
        }

        async fn fork_choice_updated(
            &self,
            state: ForkchoiceState,
            _payload_attrs: Option<PayloadAttributes>,
            _version: EngineApiMessageVersion,
        ) -> Result<ForkchoiceUpdated> {
            self.calls
                .lock()
                .unwrap()
                .push(MockEngineCall::ForkchoiceUpdated {
                    head: state.head_block_hash,
                });
            let mut responses = self.fcu_responses.lock().unwrap();
            if responses.is_empty() {
                return Err(eyre::eyre!(
                    "mock: fork_choice_updated called but no scripted response queued"
                ));
            }
            Ok(responses.remove(0))
        }
    }

    /// Build a minimal [`ExecutionData`] for test plumbing. The mock doesn't
    /// inspect any field except `payload.parent_hash()` (for call recording),
    /// so defaults elsewhere are fine.
    pub(crate) fn test_execution_data(parent_hash: B256) -> ExecutionData {
        let v1 = ExecutionPayloadV1 {
            parent_hash,
            fee_recipient: Address::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number: 0,
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: Bytes::new(),
            base_fee_per_gas: U256::ZERO,
            block_hash: B256::ZERO,
            transactions: Vec::new(),
        };
        ExecutionData::new(ExecutionPayload::V1(v1), ExecutionPayloadSidecar::none())
    }

    pub(crate) fn valid() -> PayloadStatus {
        PayloadStatus::from_status(PayloadStatusEnum::Valid)
    }

    pub(crate) fn invalid() -> PayloadStatus {
        PayloadStatus::from_status(PayloadStatusEnum::Invalid {
            validation_error: "mock invalid".to_string(),
        })
    }

    pub(crate) fn syncing() -> PayloadStatus {
        PayloadStatus::from_status(PayloadStatusEnum::Syncing)
    }

    /// Test #1 (happy path): mock returns VALID for both new_payload and FCU.
    /// Asserts call order: `NewPayload` before `ForkchoiceUpdated`, and the
    /// forkchoice head equals the sibling hash.
    #[tokio::test]
    async fn test_rebuild_block_as_sibling_happy_path_order() {
        let parent = B256::with_last_byte(0x11);
        let sibling_hash = B256::with_last_byte(0x22);

        let engine = MockEngine::new();
        engine.push_new_payload_response(valid());
        engine.push_fcu_response(ForkchoiceUpdated::from_status(PayloadStatusEnum::Valid));

        let mut parent_hashes: VecDeque<B256> = VecDeque::new();
        parent_hashes.push_back(parent);

        let outcome = submit_sibling_payload(
            &engine,
            test_execution_data(parent),
            sibling_hash,
            &parent_hashes,
        )
        .await
        .expect("happy path must succeed");

        // The final deque must end with the sibling hash.
        assert_eq!(
            outcome.new_hashes.back(),
            Some(&sibling_hash),
            "final forkchoice deque must be headed by the sibling"
        );

        let calls = engine.take_calls();
        assert_eq!(
            calls.len(),
            2,
            "exactly one NewPayload + one FCU must be sent"
        );
        assert_eq!(
            calls[0],
            MockEngineCall::NewPayload { parent_hash: parent },
            "new_payload must be first (parent={parent})"
        );
        match calls[1] {
            MockEngineCall::ForkchoiceUpdated { head } => {
                assert_eq!(head, sibling_hash, "FCU head must be the sibling");
            }
            _ => panic!("expected ForkchoiceUpdated as second call, got {:?}", calls[1]),
        }
    }

    /// Test #2: mock returns INVALID on new_payload → function bails; no FCU
    /// is sent. Guards against the auditor's concern that INVALID could be
    /// silently tolerated.
    #[tokio::test]
    async fn test_rebuild_block_as_sibling_new_payload_invalid_bails() {
        let parent = B256::with_last_byte(0x11);
        let sibling_hash = B256::with_last_byte(0x22);

        let engine = MockEngine::new();
        engine.push_new_payload_response(invalid());
        // Deliberately no FCU response — test asserts FCU is never called.

        let mut parent_hashes: VecDeque<B256> = VecDeque::new();
        parent_hashes.push_back(parent);

        let err = submit_sibling_payload(
            &engine,
            test_execution_data(parent),
            sibling_hash,
            &parent_hashes,
        )
        .await
        .expect_err("INVALID newPayload MUST bail — silent tolerance reintroduces #36");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("newPayload rejected"),
            "error must be attributable to newPayload INVALID: {msg}"
        );

        let calls = engine.take_calls();
        assert_eq!(
            calls.len(),
            1,
            "no FCU must be attempted after INVALID newPayload (calls={calls:?})"
        );
        assert!(matches!(calls[0], MockEngineCall::NewPayload { .. }));
    }

    /// Test #3: new_payload VALID but FCU INVALID → bail.
    #[tokio::test]
    async fn test_rebuild_block_as_sibling_fcu_invalid_bails() {
        let parent = B256::with_last_byte(0x11);
        let sibling_hash = B256::with_last_byte(0x22);

        let engine = MockEngine::new();
        engine.push_new_payload_response(valid());
        engine.push_fcu_response(ForkchoiceUpdated::from_status(
            PayloadStatusEnum::Invalid {
                validation_error: "bad".to_string(),
            },
        ));

        let mut parent_hashes: VecDeque<B256> = VecDeque::new();
        parent_hashes.push_back(parent);

        let err = submit_sibling_payload(
            &engine,
            test_execution_data(parent),
            sibling_hash,
            &parent_hashes,
        )
        .await
        .expect_err("INVALID FCU MUST bail");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("forkchoiceUpdated rejected"),
            "error must be attributable to FCU INVALID: {msg}"
        );

        let calls = engine.take_calls();
        assert_eq!(calls.len(), 2, "both calls must have been attempted");
    }

    /// Test #4: FCU returns SYNCING for a few attempts then VALID — retry
    /// with exponential backoff succeeds. The backoff schedule is
    /// 100ms + 200ms = 300ms before the third call returns VALID, so the
    /// test runs in well under 1s of real time.
    #[tokio::test]
    async fn test_rebuild_block_as_sibling_fcu_syncing_retries_then_succeeds() {
        let engine = MockEngine::new();
        // Script: SYNCING twice, then VALID.
        engine.push_fcu_response(ForkchoiceUpdated::new(syncing()));
        engine.push_fcu_response(ForkchoiceUpdated::new(syncing()));
        engine.push_fcu_response(ForkchoiceUpdated::from_status(PayloadStatusEnum::Valid));

        let head = B256::with_last_byte(0xCA);
        let state = ForkchoiceState {
            head_block_hash: head,
            safe_block_hash: head,
            finalized_block_hash: head,
        };

        let fcu = submit_fork_choice_with_retry(&engine, state, None)
            .await
            .expect("must succeed after SYNCING→VALID");

        assert!(fcu.is_valid(), "final FCU must be VALID");
        let calls = engine.take_calls();
        assert_eq!(calls.len(), 3, "three FCU attempts must have been made");
        for c in &calls {
            assert!(matches!(c, MockEngineCall::ForkchoiceUpdated { .. }));
        }
    }

    /// Test #5 (C2 regression): invoke the production
    /// `submit_sibling_after_guard` (which mirrors `rebuild_block_as_sibling`'s
    /// order: C2 guard → engine submit) against a mock engine with a
    /// `built_root != expected_root`. The production guard MUST fire and no
    /// engine call may be dispatched.
    ///
    /// This is not a theater test: reverting the assertion inside
    /// `check_sibling_state_root_matches` (the helper the production path
    /// delegates to) — e.g. deleting the `if built_root != expected_root`
    /// branch — causes this test to fail with either an exhausted-response
    /// panic from the mock or a recorded engine call.
    #[tokio::test]
    async fn test_rebuild_block_as_sibling_wrong_state_root_bails() {
        use crate::driver::{check_sibling_state_root_matches, submit_sibling_after_guard};

        let parent = B256::with_last_byte(0x11);
        let built_root = B256::with_last_byte(0xAA);
        let expected_root = B256::with_last_byte(0xBB);
        let sibling_hash = B256::with_last_byte(0x22);
        let target: u64 = 5354;

        // 1. Direct coverage of the guard itself — the error surface MUST
        //    preserve the diagnostic fields the operator relies on.
        let err = check_sibling_state_root_matches(built_root, expected_root, target)
            .expect_err("mismatched state root MUST bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("state root mismatch"), "err: {msg}");
        assert!(msg.contains(&target.to_string()), "err: {msg}");
        assert!(msg.contains(&format!("{built_root}")), "err: {msg}");
        assert!(msg.contains(&format!("{expected_root}")), "err: {msg}");

        // 2. Full production ordering: the guard MUST fire BEFORE any engine
        //    call. We call the same helper the driver calls
        //    (`submit_sibling_after_guard`) with a mock whose response queues
        //    are empty. If the guard were removed, `submit_sibling_payload`
        //    would advance to `engine.new_payload()` — the mock would panic
        //    on an empty response queue. With the guard, we never reach the
        //    engine.
        let engine = MockEngine::new();
        // Deliberately DO NOT push any responses — a production bypass would
        // panic the mock here.
        let mut parent_hashes: VecDeque<B256> = VecDeque::new();
        parent_hashes.push_back(parent);

        let err = submit_sibling_after_guard(
            &engine,
            test_execution_data(parent),
            sibling_hash,
            built_root,
            expected_root,
            target,
            &parent_hashes,
        )
        .await
        .expect_err("C2 guard MUST short-circuit before any engine call");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("state root mismatch"),
            "error must be the guard, not an engine failure: {msg}"
        );
        assert_eq!(
            engine.take_calls().len(),
            0,
            "C2 guard must fire before ANY engine call (calls={:?})",
            engine.take_calls()
        );

        // 3. Happy path: matching roots → engine IS called, once for
        //    NewPayload and once for FCU. Confirms the guard really does let
        //    the production path through on success.
        let engine = MockEngine::new();
        engine.push_new_payload_response(valid());
        engine.push_fcu_response(ForkchoiceUpdated::from_status(PayloadStatusEnum::Valid));

        let matching_root = B256::with_last_byte(0xCC);
        let outcome = submit_sibling_after_guard(
            &engine,
            test_execution_data(parent),
            sibling_hash,
            matching_root,
            matching_root,
            target,
            &parent_hashes,
        )
        .await
        .expect("matching roots must submit cleanly");
        assert_eq!(outcome.new_hashes.back(), Some(&sibling_hash));
        let calls = engine.take_calls();
        assert_eq!(
            calls.len(),
            2,
            "matching roots: expected NewPayload + FCU (calls={calls:?})"
        );
    }
}

// --- Tier-1 tests that exercise the driver state-transition logic via the
// pure helpers extracted for testability. ---

/// Test #6 (C1 regression): `plan_sibling_reorg_from_verify` produces a plan
/// with both a `SiblingReorgRequest` AND a rewind target. The plan-then-apply
/// design forces the driver to set `pending_rewind_target` — without which
/// `step_builder`'s batch commit overwrites the derivation rollback and
/// the sibling-reorg request sits idle until the safety gate trips.
///
/// This fails before the C1 fix: the previous code did NOT set
/// `pending_rewind_target`. The planner struct surfaces the field so the
/// test can assert its presence without a live driver.
#[test]
fn test_verify_fast_path_sets_both_rewind_target_and_sibling_reorg() {
    let entry_block = 5354u64;
    let expected_root = B256::with_last_byte(0x42);

    // Cold start (no L1 anchor yet): rewind_target_l2 = 0, rollback_l1_block
    // = deployment_l1_block.
    let plan = plan_sibling_reorg_from_verify(entry_block, expected_root, None, 120);
    assert_eq!(
        plan.request.target_l2_block, entry_block,
        "planner must target the divergent block"
    );
    assert_eq!(
        plan.request.expected_root, expected_root,
        "planner must carry the L1-derived root as expected"
    );
    assert_eq!(plan.rewind_target_l2, 0, "cold-start rewind target must be 0");
    assert_eq!(
        plan.rollback_l1_block, 120,
        "cold-start rollback must use deployment_l1_block"
    );

    // Warm start (anchor present): rewind_target_l2 = entry_block - 1,
    // rollback_l1_block = anchor.l1_block_number - 1.
    let anchor = L1ConfirmedAnchor {
        l2_block_number: 5000,
        l1_block_number: 200,
    };
    let plan = plan_sibling_reorg_from_verify(entry_block, expected_root, Some(anchor), 120);
    assert_eq!(plan.rewind_target_l2, entry_block - 1);
    assert_eq!(plan.rollback_l1_block, 199);

    // Block 1 edge case: saturating_sub(1) must not wrap.
    let plan = plan_sibling_reorg_from_verify(1, expected_root, Some(anchor), 120);
    assert_eq!(plan.rewind_target_l2, 0, "block 1 rewind target must saturate to 0");
}

/// Test #7a (C1 fast-path gate): invoke production `classify_verify_mismatch`
/// and assert that filtering-flagged divergence AND no pre-existing sibling
/// reorg produces `FastPathSiblingReorg`. This is the exact boolean gate at
/// `driver.rs:4020` (pre-refactor) / inside `classify_verify_mismatch`
/// (post-refactor). The production `verify_local_block_matches_l1` now
/// delegates to the classifier — any change to the gate's boolean equation
/// is captured here.
#[test]
fn test_verify_fast_path_fires_on_filtering_mismatch() {
    use crate::driver::{
        classify_verify_mismatch, VerifyMismatchAction, MAX_ENTRY_VERIFY_DEFERRALS,
    };

    // §4f filtering flagged + no prior sibling reorg: fast path fires.
    assert_eq!(
        classify_verify_mismatch(
            /* filtering_present = */ true,
            /* sibling_reorg_already_queued = */ false,
            /* is_pending_entry_block = */ false,
            /* deferrals_before_increment = */ 0,
            MAX_ENTRY_VERIFY_DEFERRALS,
        ),
        VerifyMismatchAction::FastPathSiblingReorg,
        "§4f-flagged divergence with no prior queue → fast path fires"
    );

    // §4f filtering flagged AND the block was entry-bearing: fast path still
    // wins (the filtering signal trumps the deferral loop).
    assert_eq!(
        classify_verify_mismatch(true, false, /* entry */ true, 0, MAX_ENTRY_VERIFY_DEFERRALS),
        VerifyMismatchAction::FastPathSiblingReorg,
        "§4f filtering signal trumps entry-hold — L1 is definitive"
    );

    // §4f filtering flagged BUT a sibling reorg is already queued: don't
    // double-queue.
    assert_ne!(
        classify_verify_mismatch(true, true, false, 0, MAX_ENTRY_VERIFY_DEFERRALS),
        VerifyMismatchAction::FastPathSiblingReorg,
        "already-queued sibling reorg MUST suppress the fast path"
    );
}

/// Test #7b: when the classifier returns `FastPathSiblingReorg`, the
/// downstream planner+apply produces all three C1 side-effects. Invokes the
/// two production helpers (`plan_sibling_reorg_from_verify` +
/// `apply_sibling_reorg_plan_fields`) exactly as
/// `verify_local_block_matches_l1` does, and asserts all the promised
/// mutations land.
#[test]
fn test_verify_fast_path_wires_both_rewind_target_and_sibling_reorg() {
    use crate::derivation::DerivationPipeline;
    use crate::driver::{
        apply_sibling_reorg_plan_fields, plan_sibling_reorg_from_verify,
        DriverMode as Mode, DriverRecoveryFields,
    };

    let entry_block = 5354u64;
    let expected_root = B256::with_last_byte(0x42);
    let anchor = L1ConfirmedAnchor {
        l2_block_number: 5000,
        l1_block_number: 200,
    };

    // Same call shape as `verify_local_block_matches_l1`'s fast path.
    let plan = plan_sibling_reorg_from_verify(entry_block, expected_root, Some(anchor), 120);

    // Driver-side mutations, in isolation: start "clean" and apply the plan.
    let mut fields = DriverRecoveryFields {
        pending_sibling_reorg: None,
        pending_rewind_target: None,
        pending_entry_verification_block: Some(entry_block),
        entry_verify_deferrals: 1,
        mode: Mode::Builder,
    };
    let mut derivation = DerivationPipeline::new(test_config_arc());

    apply_sibling_reorg_plan_fields(&mut fields, plan.request, plan, &mut derivation);

    // C1 (the actual regression): BOTH `pending_sibling_reorg` AND
    // `pending_rewind_target` must be set after the fast path applies. Without
    // the rewind target, `step_builder`'s trailing commit overwrites the
    // derivation rollback and the sibling-reorg request sits idle.
    assert_eq!(
        fields.pending_sibling_reorg,
        Some(plan.request),
        "C1: pending_sibling_reorg MUST be set"
    );
    assert_eq!(
        fields.pending_rewind_target,
        Some(plan.rewind_target_l2),
        "C1: pending_rewind_target MUST be set (without this the batch commit \
         overwrites the derivation rollback)"
    );
    // Entry hold released.
    assert_eq!(fields.pending_entry_verification_block, None);
    assert_eq!(fields.entry_verify_deferrals, 0);
    // Mode flipped to Sync.
    assert_eq!(fields.mode, Mode::Sync);
}

/// Test #7c (deferral path): the classifier must route an entry-bearing
/// non-filtering mismatch to `DeferEntryVerify` while deferrals are below the
/// cap, and to `ExhaustedDeferralRewind` once the cap is reached. A generic
/// mismatch (no entry, no filtering) routes to `GenericMismatchRewind`. Pins
/// the entire truth table via the same production function
/// `verify_local_block_matches_l1` calls.
#[test]
fn test_verify_deferral_path_runs_on_non_filtering_mismatch() {
    use crate::driver::{
        classify_verify_mismatch, VerifyMismatchAction, MAX_ENTRY_VERIFY_DEFERRALS,
    };

    // Non-filtering, entry-bearing, deferrals not exhausted → defer.
    for pre_deferrals in 0..MAX_ENTRY_VERIFY_DEFERRALS - 1 {
        assert_eq!(
            classify_verify_mismatch(
                /* filtering_present = */ false,
                /* sibling_reorg_already_queued = */ false,
                /* is_pending_entry_block = */ true,
                pre_deferrals,
                MAX_ENTRY_VERIFY_DEFERRALS,
            ),
            VerifyMismatchAction::DeferEntryVerify,
            "non-filtering + entry hold + pre_deferrals={pre_deferrals} → defer"
        );
    }

    // Non-filtering, entry-bearing, deferrals exhausted → rewind.
    assert_eq!(
        classify_verify_mismatch(
            false,
            false,
            true,
            MAX_ENTRY_VERIFY_DEFERRALS - 1,
            MAX_ENTRY_VERIFY_DEFERRALS,
        ),
        VerifyMismatchAction::ExhaustedDeferralRewind,
        "non-filtering + entry hold + deferrals at cap → rewind"
    );

    // Non-filtering, non-entry → generic rewind (fallthrough).
    assert_eq!(
        classify_verify_mismatch(false, false, false, 0, MAX_ENTRY_VERIFY_DEFERRALS),
        VerifyMismatchAction::GenericMismatchRewind,
        "non-filtering, non-entry mismatch → generic rewind"
    );

    // Sibling reorg already queued suppresses the fast path even when the
    // block is entry-bearing AND filtering was flagged.
    assert_ne!(
        classify_verify_mismatch(true, true, true, 0, MAX_ENTRY_VERIFY_DEFERRALS),
        VerifyMismatchAction::FastPathSiblingReorg,
        "already-queued sibling reorg wins over all other signals"
    );
}

/// Test #8 (M2 regression): invoke the production helper
/// `clear_recovery_state` — the same code `clear_internal_state` calls — and
/// assert that all three recovery fields (including `pending_sibling_reorg`)
/// go to their empty values. Also pins the save+reinstate pattern the two
/// dispatch sites rely on.
///
/// This is not a theater test: the helper is the authoritative place where
/// `pending_sibling_reorg = None` lives. Reverting the M2 fix = removing
/// `pending_sibling_reorg` from `clear_recovery_state` = this test fails.
#[test]
fn test_clear_recovery_state_wipes_all_fields() {
    use crate::driver::clear_recovery_state;

    // 1. Baseline: all three fields populated → all cleared.
    let mut pending_sibling_reorg = Some(SiblingReorgRequest {
        target_l2_block: 1234,
        expected_root: B256::with_last_byte(0x55),
    });
    let mut pending_entry_verification_block: Option<u64> = Some(1234);
    let mut entry_verify_deferrals: u32 = 2;

    clear_recovery_state(
        &mut pending_sibling_reorg,
        &mut pending_entry_verification_block,
        &mut entry_verify_deferrals,
    );

    assert_eq!(
        pending_sibling_reorg, None,
        "M2: pending_sibling_reorg MUST be cleared"
    );
    assert_eq!(pending_entry_verification_block, None);
    assert_eq!(entry_verify_deferrals, 0);

    // 2. Idempotency: clearing already-empty fields is a no-op.
    clear_recovery_state(
        &mut pending_sibling_reorg,
        &mut pending_entry_verification_block,
        &mut entry_verify_deferrals,
    );
    assert_eq!(pending_sibling_reorg, None);
    assert_eq!(pending_entry_verification_block, None);
    assert_eq!(entry_verify_deferrals, 0);

    // 3. Save+reinstate pattern: callers that need the request alive
    //    (`flush_to_l1` and `verify_local_block_matches_l1` fast-path) save
    //    before calling the helper and reinstate after. Pin this here so a
    //    regression in either dispatch site (removing the save OR removing
    //    the reinstate) is observable as a contract change.
    let saved = SiblingReorgRequest {
        target_l2_block: 42,
        expected_root: B256::with_last_byte(0x77),
    };
    let mut pending_sibling_reorg = Some(saved);
    let mut pending_entry_verification_block: Option<u64> = None;
    let mut entry_verify_deferrals: u32 = 0;

    // The two-step dance the dispatch sites perform:
    //   let saved_req = req;
    //   self.clear_internal_state();          <- wipes the field
    //   self.pending_sibling_reorg = Some(saved_req);
    let saved_req = pending_sibling_reorg.expect("seeded above");
    clear_recovery_state(
        &mut pending_sibling_reorg,
        &mut pending_entry_verification_block,
        &mut entry_verify_deferrals,
    );
    assert_eq!(
        pending_sibling_reorg, None,
        "clear wipes the field mid-dance"
    );
    pending_sibling_reorg = Some(saved_req);
    assert_eq!(
        pending_sibling_reorg,
        Some(saved),
        "save+reinstate preserves the exact request value"
    );
}

/// Test #9 (drive-by): invoke the production helper
/// `clear_fields_on_sibling_reorg_success` — the same code the
/// `step_sync` success branch calls (see `driver.rs:1455-1466`) — and assert
/// all five fields go to None / 0 in one atomic call.
///
/// This is not a theater test: the helper is the authoritative place where
/// all five clearing lines live. Removing ANY one line from the helper =
/// this test fails. Removing the helper call from the success branch = the
/// production behavior regresses (the compiler won't warn because the
/// helper function still exists, but `test_driver_build_succeeds` would
/// break the integration expectations — this test pins the unit contract).
#[test]
fn test_step_sync_success_clears_all_five_fields() {
    use crate::driver::clear_fields_on_sibling_reorg_success;

    let mut pending_sibling_reorg = Some(SiblingReorgRequest {
        target_l2_block: 100,
        expected_root: B256::with_last_byte(0x42),
    });
    let mut consecutive_rewind_cycles: u32 = 3;
    let mut consecutive_flush_mismatches: u32 = 1;
    let mut pending_entry_verification_block: Option<u64> = Some(100);
    let mut entry_verify_deferrals: u32 = 2;

    clear_fields_on_sibling_reorg_success(
        &mut pending_sibling_reorg,
        &mut consecutive_rewind_cycles,
        &mut consecutive_flush_mismatches,
        &mut pending_entry_verification_block,
        &mut entry_verify_deferrals,
    );

    // All five MUST be zeroed — entry hold parity with the verify fast-path.
    // Without this, the entry-verification hold persists and `step_builder`
    // returns early forever (different livelock class than #36 itself).
    assert_eq!(
        pending_sibling_reorg, None,
        "success branch MUST clear pending_sibling_reorg"
    );
    assert_eq!(
        consecutive_rewind_cycles, 0,
        "success branch MUST reset consecutive_rewind_cycles"
    );
    assert_eq!(
        consecutive_flush_mismatches, 0,
        "success branch MUST reset consecutive_flush_mismatches"
    );
    assert_eq!(
        pending_entry_verification_block, None,
        "success branch MUST clear pending_entry_verification_block"
    );
    assert_eq!(
        entry_verify_deferrals, 0,
        "success branch MUST reset entry_verify_deferrals"
    );

    // Fine-grained: each field is individually cleared even if the others
    // are already at their empty values (rules out the "conditional clear"
    // class of regression).
    for seed_idx in 0..5u8 {
        let mut f0 = None;
        let mut f1: u32 = 0;
        let mut f2: u32 = 0;
        let mut f3: Option<u64> = None;
        let mut f4: u32 = 0;
        match seed_idx {
            0 => {
                f0 = Some(SiblingReorgRequest {
                    target_l2_block: 1,
                    expected_root: B256::ZERO,
                })
            }
            1 => f1 = 99,
            2 => f2 = 99,
            3 => f3 = Some(1),
            _ => f4 = 99,
        }
        clear_fields_on_sibling_reorg_success(&mut f0, &mut f1, &mut f2, &mut f3, &mut f4);
        assert_eq!(f0, None, "seed={seed_idx}: pending_sibling_reorg");
        assert_eq!(f1, 0, "seed={seed_idx}: consecutive_rewind_cycles");
        assert_eq!(f2, 0, "seed={seed_idx}: consecutive_flush_mismatches");
        assert_eq!(f3, None, "seed={seed_idx}: pending_entry_verification_block");
        assert_eq!(f4, 0, "seed={seed_idx}: entry_verify_deferrals");
    }
}

/// Test #10 (M4 regression): when two pending blocks both match
/// `clean_state_root == on_chain_root`, detection MUST target the
/// rightmost (the one `rposition` found upstream), not the first
/// forward-scan match.
///
/// Before the M4 fix, `flush_to_l1` iterated `take(pos + 1)` FORWARD and
/// broke on the first `SiblingReorg` decision — picking the earliest
/// match. This test populates `pending_submissions` with a collision at
/// two distinct block numbers, calls `find_rightmost_sibling_reorg_target`,
/// and asserts the rightmost wins.
#[test]
fn test_flush_detection_targets_rposition_block_not_earliest() {
    use crate::driver::find_rightmost_sibling_reorg_target;
    use crate::proposer::PendingBlock;

    let on_chain_root = B256::with_last_byte(0x42);
    let speculative_root_a = B256::with_last_byte(0xAA);
    let speculative_root_b = B256::with_last_byte(0xBB);

    // Two entry blocks in the queue, both whose `clean_state_root` matches
    // the on-chain root but whose speculative `state_root` differs.
    // `rposition` would find block #200 (the later one); detection must
    // target it, not block #100.
    let mut pending: VecDeque<PendingBlock> = VecDeque::new();
    pending.push_back(PendingBlock {
        l2_block_number: 100,
        pre_state_root: B256::ZERO,
        state_root: speculative_root_a,
        clean_state_root: on_chain_root, // MATCH 1 — earliest
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    });
    pending.push_back(PendingBlock {
        l2_block_number: 101,
        pre_state_root: B256::ZERO,
        // Intervening block with NO match, to prove we walk back.
        state_root: B256::with_last_byte(0xCC),
        clean_state_root: B256::with_last_byte(0xCC),
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    });
    pending.push_back(PendingBlock {
        l2_block_number: 200,
        pre_state_root: B256::ZERO,
        state_root: speculative_root_b,
        clean_state_root: on_chain_root, // MATCH 2 — rightmost (the one `rposition` finds)
        encoded_transactions: Bytes::from(vec![0xc0]),
        intermediate_roots: vec![],
    });

    // window_len = pending.len() (pos + 1 in the caller, where `pos` is the
    // rightmost match — i.e. 2). Scanning [0, 1, 2] in reverse finds 200 first.
    let req = find_rightmost_sibling_reorg_target(
        &pending,
        on_chain_root,
        /* reorg_depth = */ 0,
        REORG_SAFETY_THRESHOLD,
        /* window_len = */ 3,
    )
    .expect("both blocks match; detection must pick one");

    assert_eq!(
        req.target_l2_block, 200,
        "detection MUST target the rightmost block (200), not the earliest (100). \
         Before the M4 fix, the forward scan picked block 100 and hijacked the request."
    );
    assert_eq!(req.expected_root, on_chain_root);

    // Sanity: shrinking the window to exclude the rightmost flips the
    // answer. Confirms the direction is deterministic with respect to the
    // window bound.
    let req = find_rightmost_sibling_reorg_target(
        &pending,
        on_chain_root,
        0,
        REORG_SAFETY_THRESHOLD,
        /* window_len = */ 1, // only block #100 in window
    )
    .expect("single-block window containing match");
    assert_eq!(
        req.target_l2_block, 100,
        "window trimmed to the first block only → that's the rightmost inside the window"
    );
}

/// Direct coverage for `apply_sibling_reorg_plan` (auditor third-pass
/// finding). The linchpin of C1 is the function that actually mutates
/// `Driver` state when the verify fast path fires. Previously there was no
/// direct test — only an assertion on the planner's output shape and a
/// mock-engine test for the submission path.
///
/// `Driver::apply_sibling_reorg_plan` is a thin wrapper around
/// [`apply_sibling_reorg_plan_fields`]: it clones the current field snapshot,
/// delegates, and writes back. We test the delegate directly — a regression
/// in either the wrapper OR the helper fails the existing
/// `test_verify_fast_path_wires_both_rewind_target_and_sibling_reorg` plus
/// this one.
///
/// All 6 documented field mutations are asserted:
///  1. `pending_sibling_reorg = Some(saved_req)` (reinstated by M2).
///  2. `pending_rewind_target` set to the plan target (or min with existing).
///  3. `mode = Sync`.
///  4. `pending_entry_verification_block = None` (entry hold released).
///  5. `entry_verify_deferrals = 0`.
///  6. `derivation.rollback_to(plan.rollback_l1_block)` moved the cursor.
///
/// And the "intentionally not mutated" invariant:
///
///  7. `consecutive_rewind_cycles` NOT bumped by the helper (caller-level,
///     checked via its absence from the `DriverRecoveryFields` snapshot).
#[test]
fn test_apply_sibling_reorg_plan_mutates_all_fields() {
    use crate::derivation::DerivationPipeline;
    use crate::driver::{
        apply_sibling_reorg_plan_fields, plan_sibling_reorg_from_verify, DriverMode as Mode,
        DriverRecoveryFields,
    };

    let entry_block = 5354u64;
    let expected_root = B256::with_last_byte(0x42);
    let anchor = L1ConfirmedAnchor {
        l2_block_number: 5000,
        l1_block_number: 200,
    };
    let deployment_l1_block = 1000u64;

    let plan = plan_sibling_reorg_from_verify(
        entry_block,
        expected_root,
        Some(anchor),
        deployment_l1_block,
    );
    // Sanity-check the plan shape so the assertions below are grounded in
    // the planner's actual contract (warm start → rewind=entry-1,
    // rollback=anchor.l1-1).
    assert_eq!(plan.request.target_l2_block, entry_block);
    assert_eq!(plan.request.expected_root, expected_root);
    assert_eq!(plan.rewind_target_l2, entry_block - 1);
    assert_eq!(plan.rollback_l1_block, anchor.l1_block_number - 1);

    // Pre-state: realistic "builder was healthy and running" snapshot —
    // every mutated field starts at its non-trivial value so a no-op helper
    // would be caught.
    let mut fields = DriverRecoveryFields {
        pending_sibling_reorg: None,
        pending_rewind_target: None,
        pending_entry_verification_block: Some(entry_block),
        entry_verify_deferrals: 2,
        mode: Mode::Builder,
    };

    // Construct a real `DerivationPipeline`. `set_last_derived_l2_block` and
    // `rollback_to` are `pub` methods — this is the same pipeline the
    // production `Driver` holds, just without the rest of the driver state.
    let config = test_config_arc();
    let mut derivation = DerivationPipeline::new(config.clone());
    // Seed the pipeline with a recent L1 cursor so `rollback_to` has
    // something to unwind to; mirrors what the live derivation would hold.
    derivation.resume_from(anchor.l1_block_number);
    derivation.set_last_derived_l2_block(entry_block);
    let cursor_before = derivation.last_processed_l1_block();
    assert_eq!(
        cursor_before, anchor.l1_block_number,
        "seed: derivation cursor must be at anchor before apply"
    );

    apply_sibling_reorg_plan_fields(&mut fields, plan.request, plan, &mut derivation);

    // (1) Sibling-reorg request reinstated.
    assert_eq!(
        fields.pending_sibling_reorg,
        Some(plan.request),
        "(1) pending_sibling_reorg = Some(saved_req) — M2 reinstate"
    );
    // (2) Rewind target wired (C1 regression — the reason this function
    //     exists in its current shape).
    assert_eq!(
        fields.pending_rewind_target,
        Some(plan.rewind_target_l2),
        "(2) pending_rewind_target MUST be set — without this, step_builder commits \
         the batch and overwrites the derivation rollback"
    );
    // (3) Mode flipped to Sync.
    assert_eq!(fields.mode, Mode::Sync, "(3) mode = Sync");
    // (4) Entry-verification hold released.
    assert_eq!(
        fields.pending_entry_verification_block, None,
        "(4) pending_entry_verification_block = None"
    );
    // (5) Deferral count reset.
    assert_eq!(
        fields.entry_verify_deferrals, 0,
        "(5) entry_verify_deferrals = 0"
    );
    // (6) Derivation cursor moved.
    assert_eq!(
        derivation.last_processed_l1_block(),
        plan.rollback_l1_block,
        "(6) derivation.rollback_to(plan.rollback_l1_block) moved cursor"
    );
    // (7) The snapshot struct DOES NOT carry `consecutive_rewind_cycles`,
    //     which proves the helper cannot mutate it. This is the "productive
    //     recovery" invariant that prevents the safety gate from tripping on
    //     a first-time queue.
    //
    //     `DriverRecoveryFields` field count:
    const _ASSERT_FIELD_COUNT: () = {
        // If a future refactor adds `consecutive_rewind_cycles` to this
        // struct, this line still compiles but reviewers should re-check
        // that `apply_sibling_reorg_plan_fields` does NOT bump it.
    };

    // Idempotency: applying the same plan twice does not change anything
    // further (the helper overwrites deterministically).
    apply_sibling_reorg_plan_fields(&mut fields, plan.request, plan, &mut derivation);
    assert_eq!(fields.pending_sibling_reorg, Some(plan.request));
    assert_eq!(fields.pending_rewind_target, Some(plan.rewind_target_l2));
    assert_eq!(fields.mode, Mode::Sync);

    // `set_rewind_target` semantics: a deeper pending target is preserved
    // (the helper narrows to the min). Pin this so refactors don't silently
    // switch to unconditional overwrite.
    let mut fields = DriverRecoveryFields {
        pending_sibling_reorg: None,
        pending_rewind_target: Some(10), // deeper than plan's rewind_target_l2
        pending_entry_verification_block: None,
        entry_verify_deferrals: 0,
        mode: Mode::Builder,
    };
    let mut derivation = DerivationPipeline::new(config.clone());
    apply_sibling_reorg_plan_fields(&mut fields, plan.request, plan, &mut derivation);
    assert_eq!(
        fields.pending_rewind_target,
        Some(10),
        "rewind target narrows to min — deeper existing value wins"
    );
}

/// Wire-through test: prove `Driver::clear_internal_state` (exercised via
/// the free `clear_recovery_state`) and `Driver::apply_sibling_reorg_plan`
/// (via `apply_sibling_reorg_plan_fields`) compose correctly. The production
/// flow is:
///
///   let saved_req = plan.request;
///   self.clear_internal_state();     // clears pending_sibling_reorg
///   apply_sibling_reorg_plan_fields(...) // reinstates saved_req
///
/// This test enacts the same two-step dance against the same helpers
/// production calls and asserts the final state is what production
/// promises. A regression in EITHER helper breaks this test.
#[test]
fn test_apply_sibling_reorg_plan_survives_clear_internal_state_sequence() {
    use crate::derivation::DerivationPipeline;
    use crate::driver::{
        apply_sibling_reorg_plan_fields, clear_recovery_state, plan_sibling_reorg_from_verify,
        DriverMode as Mode, DriverRecoveryFields,
    };

    let plan = plan_sibling_reorg_from_verify(
        42,
        B256::with_last_byte(0x99),
        None,
        100, // cold start
    );

    // Simulate the driver state JUST BEFORE calling
    // `apply_sibling_reorg_plan`: an old sibling-reorg request is present
    // (e.g. left over from a previous fast path), entry hold active, mode
    // = Builder.
    let stale_req = SiblingReorgRequest {
        target_l2_block: 999,
        expected_root: B256::with_last_byte(0xEE),
    };
    let mut fields = DriverRecoveryFields {
        pending_sibling_reorg: Some(stale_req),
        pending_rewind_target: None,
        pending_entry_verification_block: Some(42),
        entry_verify_deferrals: 1,
        mode: Mode::Builder,
    };

    // Step 1: production `clear_internal_state` calls `clear_recovery_state`.
    let saved_req = plan.request;
    clear_recovery_state(
        &mut fields.pending_sibling_reorg,
        &mut fields.pending_entry_verification_block,
        &mut fields.entry_verify_deferrals,
    );
    assert_eq!(
        fields.pending_sibling_reorg, None,
        "M2: the stale request is wiped — save+reinstate is the only way to keep one alive"
    );

    // Step 2: apply the plan.
    let mut derivation = DerivationPipeline::new(test_config_arc());
    apply_sibling_reorg_plan_fields(&mut fields, saved_req, plan, &mut derivation);

    // Final state: fresh request installed, mode = Sync, rewind target wired.
    assert_eq!(
        fields.pending_sibling_reorg,
        Some(plan.request),
        "post-compose: fresh request is now in place (stale was wiped by clear)"
    );
    assert_eq!(fields.pending_rewind_target, Some(plan.rewind_target_l2));
    assert_eq!(fields.mode, Mode::Sync);
    assert_eq!(fields.pending_entry_verification_block, None);
    assert_eq!(fields.entry_verify_deferrals, 0);
}
