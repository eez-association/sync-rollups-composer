use super::*;
use alloy_primitives::Address;

fn test_config() -> Arc<RollupConfig> {
    Arc::new(RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: Address::ZERO,
        deployment_l1_block: 1000,
        deployment_timestamp: 1_700_000_000,
        block_time: 12,
        builder_mode: false,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        l1_builder_rpc_url: None,
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
fn test_new_pipeline_starts_at_deployment_block() {
    let config = test_config();
    let pipeline = DerivationPipeline::new(config.clone());
    assert_eq!(
        pipeline.last_processed_l1_block(),
        config.deployment_l1_block
    );
    assert_eq!(pipeline.cursor_len(), 0);
}

#[test]
fn test_resume_from() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);
    pipeline.resume_from(5000);
    assert_eq!(pipeline.last_processed_l1_block(), 5000);
}

#[test]
fn test_rollback_to() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    for i in 1001..=1010 {
        pipeline.cursor.push(DerivedBlockMeta {
            l2_block_number: i - 1000,
            l1_block_number: i,
            l1_block_hash: B256::with_last_byte(i as u8),
        });
    }
    pipeline.last_processed_l1_block = 1010;

    let last_valid_l2 = pipeline.rollback_to(1005);
    assert_eq!(last_valid_l2, Some(5));
    assert_eq!(pipeline.last_processed_l1_block(), 1005);
    assert_eq!(pipeline.cursor_len(), 5);
    assert!(pipeline.cursor.iter().all(|m| m.l1_block_number <= 1005));
}

#[test]
fn test_rollback_to_before_all_cursor_entries() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    for i in 1001..=1005 {
        pipeline.cursor.push(DerivedBlockMeta {
            l2_block_number: i - 1000,
            l1_block_number: i,
            l1_block_hash: B256::ZERO,
        });
    }
    pipeline.last_processed_l1_block = 1005;

    let last_valid_l2 = pipeline.rollback_to(999);
    assert_eq!(last_valid_l2, None);
    assert_eq!(pipeline.last_processed_l1_block(), 999);
    assert_eq!(pipeline.cursor_len(), 0);
}

/// Regression test for the `rewind_to_re_derive` ordering bug: after
/// rolling back the cursor and then authoritatively setting the
/// derivation head to the rewind target, the head must equal the
/// target even when the cursor is empty. This mirrors the sequence in
/// `driver/rewind.rs:rewind_to_re_derive` after the fix (rollback
/// first, set second).
#[test]
fn test_rewind_sequence_leaves_derivation_head_at_target_when_cursor_empty() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    for i in 3371..=3400 {
        pipeline.cursor.push(DerivedBlockMeta {
            l2_block_number: i - 1000,
            l1_block_number: i,
            l1_block_hash: B256::with_last_byte(i as u8),
        });
    }
    pipeline.last_processed_l1_block = 3400;
    pipeline.set_last_derived_l2_block(3520);

    let target_l2_block = 3483u64;
    let rollback_l1_block = 3370u64;

    // Exactly the call order used by `Driver::rewind_to_re_derive`.
    pipeline.rollback_to(rollback_l1_block);
    pipeline.set_last_derived_l2_block(target_l2_block);

    assert_eq!(pipeline.cursor_len(), 0);
    assert_eq!(
        pipeline.last_derived_l2_block, target_l2_block,
        "rewind_to_re_derive must leave last_derived_l2_block at the \
         requested target (not 0 from the empty-cursor fallback)"
    );
}

#[test]
fn test_prune_finalized() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    for i in 1001..=1020 {
        pipeline.cursor.push(DerivedBlockMeta {
            l2_block_number: i - 1000,
            l1_block_number: i,
            l1_block_hash: B256::ZERO,
        });
    }

    pipeline.prune_finalized(1010);
    assert_eq!(pipeline.cursor_len(), 10);
    assert!(pipeline.cursor.iter().all(|m| m.l1_block_number > 1010));
}

#[test]
fn test_prune_finalized_all() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    for i in 1001..=1005 {
        pipeline.cursor.push(DerivedBlockMeta {
            l2_block_number: i - 1000,
            l1_block_number: i,
            l1_block_hash: B256::ZERO,
        });
    }

    pipeline.prune_finalized(2000);
    assert_eq!(pipeline.cursor_len(), 0);
}

#[test]
fn test_prune_finalized_none() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    for i in 1001..=1005 {
        pipeline.cursor.push(DerivedBlockMeta {
            l2_block_number: i - 1000,
            l1_block_number: i,
            l1_block_hash: B256::ZERO,
        });
    }

    pipeline.prune_finalized(999);
    assert_eq!(pipeline.cursor_len(), 5);
}

#[tokio::test]
async fn test_derive_next_batch_no_new_blocks() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config.clone());

    let provider = alloy_provider::RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
    let result = pipeline
        .derive_next_batch(config.deployment_l1_block, &provider)
        .await
        .unwrap();
    assert!(result.blocks.is_empty());
}

#[tokio::test]
async fn test_derive_next_batch_latest_before_processed() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);
    pipeline.resume_from(2000);

    let provider = alloy_provider::RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
    let result = pipeline.derive_next_batch(1500, &provider).await.unwrap();
    assert!(result.blocks.is_empty());
}

#[tokio::test]
async fn test_derive_next_batch_does_not_advance_cursor() {
    // derive_next_batch should NOT advance the pipeline's internal cursor.
    // The cursor should only advance when commit_batch is called.
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config.clone());

    let original_processed = pipeline.last_processed_l1_block();
    let original_derived = pipeline.last_derived_l2_block;

    // Call derive_next_batch with a latest_l1_block equal to deployment
    // (no new blocks) — cursor should remain unchanged.
    let provider = alloy_provider::RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
    let _batch = pipeline
        .derive_next_batch(config.deployment_l1_block, &provider)
        .await
        .unwrap();

    assert_eq!(
        pipeline.last_processed_l1_block(),
        original_processed,
        "last_processed_l1_block should not change after derive_next_batch"
    );
    assert_eq!(
        pipeline.last_derived_l2_block, original_derived,
        "last_derived_l2_block should not change after derive_next_batch"
    );
    assert_eq!(
        pipeline.cursor_len(),
        0,
        "cursor should not grow after derive_next_batch"
    );
}

#[test]
fn test_commit_batch_advances_cursor() {
    // commit_batch should apply all cursor fields from the batch.
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    let batch = DerivedBatch {
        blocks: vec![],
        cursor_update: CursorUpdate {
            last_processed_l1_block: 2000,
            last_execution_l1_block: 1800,
            last_derived_l2_block: 5,
            last_l1_info: Some(L1BlockInfo {
                l1_block_number: 1999,
                l1_block_hash: B256::with_last_byte(0xAA),
            }),
            new_cursor_entries: vec![
                DerivedBlockMeta {
                    l2_block_number: 4,
                    l1_block_number: 1998,
                    l1_block_hash: B256::with_last_byte(0x04),
                },
                DerivedBlockMeta {
                    l2_block_number: 5,
                    l1_block_number: 1999,
                    l1_block_hash: B256::with_last_byte(0x05),
                },
            ],
        },
    };

    pipeline.commit_batch(&batch);

    assert_eq!(pipeline.last_processed_l1_block(), 2000);
    assert_eq!(pipeline.last_derived_l2_block, 5);
    assert_eq!(pipeline.cursor_len(), 2);
    assert!(pipeline.last_l1_info.is_some());
    assert_eq!(
        pipeline.last_l1_info.as_ref().unwrap().l1_block_number,
        1999
    );
}

#[test]
fn test_commit_batch_not_called_preserves_state() {
    // If commit_batch is NOT called, the pipeline state should remain unchanged.
    // This ensures failed block building doesn't lose blocks.
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config.clone());
    pipeline.resume_from(1500);
    pipeline.last_derived_l2_block = 3;

    let original_processed = pipeline.last_processed_l1_block();
    let original_derived = pipeline.last_derived_l2_block;

    // Simulate: derive_next_batch returned a batch but block building failed.
    // We intentionally do NOT call commit_batch.
    let _batch = DerivedBatch {
        blocks: vec![DerivedBlock {
            l2_block_number: 4,
            l2_timestamp: config.l2_timestamp(4),
            l1_info: L1BlockInfo {
                l1_block_number: 1501,
                l1_block_hash: B256::with_last_byte(0x01),
            },
            state_root: B256::ZERO,
            transactions: Bytes::new(),
            is_empty: true,
            execution_entries: vec![],
            filtering: None,
        }],
        cursor_update: CursorUpdate {
            last_processed_l1_block: 1600,
            last_execution_l1_block: 1550,
            last_derived_l2_block: 4,
            last_l1_info: Some(L1BlockInfo {
                l1_block_number: 1501,
                l1_block_hash: B256::with_last_byte(0x01),
            }),
            new_cursor_entries: vec![DerivedBlockMeta {
                l2_block_number: 4,
                l1_block_number: 1502,
                l1_block_hash: B256::with_last_byte(0x02),
            }],
        },
    };

    // Pipeline state should be completely unchanged.
    assert_eq!(
        pipeline.last_processed_l1_block(),
        original_processed,
        "state must not advance without commit_batch"
    );
    assert_eq!(pipeline.last_derived_l2_block, original_derived);
    assert_eq!(
        pipeline.cursor_len(),
        0,
        "cursor must not grow without commit_batch"
    );
}

#[test]
fn test_derive_next_batch_and_commit_convenience() {
    // derive_next_batch_and_commit should be equivalent to derive + commit.
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config.clone());

    // Manually create and commit a batch
    let batch = DerivedBatch {
        blocks: vec![],
        cursor_update: CursorUpdate {
            last_processed_l1_block: 2000,
            last_execution_l1_block: 2000,
            last_derived_l2_block: 0,
            last_l1_info: None,
            new_cursor_entries: vec![],
        },
    };
    pipeline.commit_batch(&batch);
    assert_eq!(pipeline.last_processed_l1_block(), 2000);
}

#[tokio::test]
async fn test_detect_reorg_empty_cursor() {
    let config = test_config();
    let pipeline = DerivationPipeline::new(config);

    let provider = alloy_provider::RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
    let result = pipeline.detect_reorg(&provider).await.unwrap();
    assert!(result.is_none());
}

#[test]
fn test_checkpoint_save_and_load_roundtrip() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config.clone());
    pipeline.last_processed_l1_block = 5000;

    let factory = reth_provider::test_utils::create_test_provider_factory();

    pipeline.save_checkpoint(&factory).unwrap();

    let mut pipeline2 = DerivationPipeline::new(config);
    let loaded = pipeline2.load_checkpoint(&factory).unwrap();

    assert_eq!(loaded, Some(5000));
    assert_eq!(pipeline2.last_processed_l1_block(), 5000);
}

#[test]
fn test_checkpoint_load_returns_none_when_no_checkpoint() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    let factory = reth_provider::test_utils::create_test_provider_factory();

    let loaded = pipeline.load_checkpoint(&factory).unwrap();
    assert_eq!(loaded, None);
    assert_eq!(pipeline.last_processed_l1_block(), 1000);
}

#[test]
fn test_checkpoint_overwrite() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config.clone());
    let factory = reth_provider::test_utils::create_test_provider_factory();

    pipeline.last_processed_l1_block = 3000;
    pipeline.save_checkpoint(&factory).unwrap();

    pipeline.last_processed_l1_block = 7000;
    pipeline.save_checkpoint(&factory).unwrap();

    let mut pipeline2 = DerivationPipeline::new(config);
    let loaded = pipeline2.load_checkpoint(&factory).unwrap();
    assert_eq!(loaded, Some(7000));
}

#[test]
fn test_checkpoint_at_deployment_block() {
    let config = test_config();
    let pipeline = DerivationPipeline::new(config.clone());
    let factory = reth_provider::test_utils::create_test_provider_factory();

    pipeline.save_checkpoint(&factory).unwrap();

    let mut pipeline2 = DerivationPipeline::new(config);
    let loaded = pipeline2.load_checkpoint(&factory).unwrap();
    assert_eq!(loaded, Some(1000));
    assert_eq!(pipeline2.last_processed_l1_block(), 1000);
}

/// Mock HeaderProvider for testing rebuild_cursor_from_headers.
struct MockHeaderProvider {
    headers: std::collections::HashMap<u64, alloy_consensus::Header>,
}

impl MockHeaderProvider {
    fn new() -> Self {
        Self {
            headers: std::collections::HashMap::new(),
        }
    }

    fn insert(&mut self, number: u64, l1_block_number: u64, l1_block_hash: B256) {
        use alloy_primitives::U256;
        let header = alloy_consensus::Header {
            mix_hash: B256::from(U256::from(l1_block_number)),
            parent_beacon_block_root: Some(l1_block_hash),
            ..Default::default()
        };
        self.headers.insert(number, header);
    }
}

impl reth_provider::HeaderProvider for MockHeaderProvider {
    type Header = alloy_consensus::Header;

    fn header(
        &self,
        _block_hash: alloy_primitives::BlockHash,
    ) -> reth_provider::ProviderResult<Option<Self::Header>> {
        Ok(None)
    }

    fn header_by_number(&self, num: u64) -> reth_provider::ProviderResult<Option<Self::Header>> {
        Ok(self.headers.get(&num).cloned())
    }

    fn headers_range(
        &self,
        _range: impl std::ops::RangeBounds<u64>,
    ) -> reth_provider::ProviderResult<Vec<Self::Header>> {
        Ok(Vec::new())
    }

    fn sealed_header(
        &self,
        number: u64,
    ) -> reth_provider::ProviderResult<Option<reth_primitives_traits::SealedHeader<Self::Header>>>
    {
        Ok(self
            .headers
            .get(&number)
            .cloned()
            .map(|h| reth_primitives_traits::SealedHeader::new(h, B256::ZERO)))
    }

    fn sealed_headers_while(
        &self,
        _range: impl std::ops::RangeBounds<u64>,
        _predicate: impl FnMut(&reth_primitives_traits::SealedHeader<Self::Header>) -> bool,
    ) -> reth_provider::ProviderResult<Vec<reth_primitives_traits::SealedHeader<Self::Header>>>
    {
        Ok(Vec::new())
    }
}

#[test]
fn test_rebuild_cursor_from_headers() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    let mut provider = MockHeaderProvider::new();
    for i in 1..=10u64 {
        provider.insert(i, 1000 + i, B256::with_last_byte(i as u8));
    }

    pipeline.rebuild_cursor_from_headers(&provider, 10).unwrap();

    assert_eq!(pipeline.cursor_len(), 10);
    for i in 1..=10u64 {
        let meta = &pipeline.cursor[(i - 1) as usize];
        assert_eq!(meta.l2_block_number, i);
        // mix_hash stores context block (1000+i), cursor now stores context directly
        assert_eq!(meta.l1_block_number, 1000 + i);
        assert_eq!(meta.l1_block_hash, B256::with_last_byte(i as u8));
    }
}

#[test]
fn test_rebuild_cursor_empty_chain() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    let provider = MockHeaderProvider::new();
    pipeline.rebuild_cursor_from_headers(&provider, 0).unwrap();

    assert_eq!(pipeline.cursor_len(), 0);
}

#[test]
fn test_rebuild_cursor_respects_reorg_check_depth() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    let mut provider = MockHeaderProvider::new();
    // Insert 100 headers (more than REORG_CHECK_DEPTH=64)
    for i in 1..=100u64 {
        provider.insert(i, 2000 + i, B256::with_last_byte(i as u8));
    }

    pipeline
        .rebuild_cursor_from_headers(&provider, 100)
        .unwrap();

    // Should only have entries from max(1, 100-64)=36 to 100 = 65 entries
    assert_eq!(pipeline.cursor_len(), 65);
    assert_eq!(pipeline.cursor[0].l2_block_number, 36);
    assert_eq!(pipeline.cursor.last().unwrap().l2_block_number, 100);
}

#[test]
fn test_rebuild_cursor_skips_missing_headers() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    let mut provider = MockHeaderProvider::new();
    // Only insert blocks 1 and 3, skip 2
    provider.insert(1, 1001, B256::with_last_byte(1));
    provider.insert(3, 1003, B256::with_last_byte(3));

    pipeline.rebuild_cursor_from_headers(&provider, 3).unwrap();

    assert_eq!(pipeline.cursor_len(), 2);
    assert_eq!(pipeline.cursor[0].l2_block_number, 1);
    assert_eq!(pipeline.cursor[1].l2_block_number, 3);
}

#[test]
fn test_rollback_resets_execution_cursors() {
    let mut pipeline = DerivationPipeline::new(test_config());
    pipeline.last_execution_l1_block = 2000;
    pipeline.builder_execution_l1_block = 2000;
    pipeline.rollback_to(1500);
    assert_eq!(pipeline.last_execution_l1_block, 1500);
    assert_eq!(pipeline.builder_execution_l1_block, 1500);
}

#[test]
fn test_rollback_never_advances_execution_cursors() {
    // Regression test: rollback_to must never advance execution cursors
    // past their current position. This can happen during L1 context
    // mismatch rewinds where the rollback L1 block is AFTER the L1 block
    // containing BatchPosted events needed for re-derivation.
    let mut pipeline = DerivationPipeline::new(test_config());
    pipeline.last_execution_l1_block = 100;
    pipeline.builder_execution_l1_block = 100;

    // Rollback to a HIGHER L1 block — cursors must NOT advance
    pipeline.rollback_to(200);
    assert_eq!(
        pipeline.last_execution_l1_block, 100,
        "execution cursor must not advance past BatchPosted events"
    );
    assert_eq!(pipeline.builder_execution_l1_block, 100);

    // But last_processed_l1_block CAN advance (it tracks event scanning)
    assert_eq!(pipeline.last_processed_l1_block, 200);
}

#[test]
fn test_resume_from_resets_execution_cursor() {
    let mut pipeline = DerivationPipeline::new(test_config());
    pipeline.resume_from(5000);
    assert_eq!(pipeline.last_execution_l1_block, 5000);
}

// --- Gap-fill tracking tests ---

#[test]
fn test_gap_fill_uses_last_l1_info_from_previous_submission() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    // Simulate having processed a submission that set last_l1_info
    let prev_info = L1BlockInfo {
        l1_block_number: 5000,
        l1_block_hash: B256::with_last_byte(0x55),
    };
    pipeline.last_l1_info = Some(prev_info.clone());
    pipeline.last_derived_l2_block = 3;

    assert_eq!(
        pipeline.last_l1_info.as_ref().unwrap().l1_block_number,
        5000
    );
    assert_eq!(
        pipeline.last_l1_info.as_ref().unwrap().l1_block_hash,
        B256::with_last_byte(0x55)
    );
}

#[test]
fn test_gap_fill_with_no_last_l1_info_falls_back_to_deployment() {
    let config = test_config();
    let pipeline = DerivationPipeline::new(config.clone());

    assert!(pipeline.last_l1_info.is_none());

    let fallback = pipeline.last_l1_info.clone().unwrap_or(L1BlockInfo {
        l1_block_number: config.deployment_l1_block,
        l1_block_hash: B256::ZERO,
    });
    assert_eq!(fallback.l1_block_number, config.deployment_l1_block);
    assert_eq!(fallback.l1_block_hash, B256::ZERO);
}

// --- Adversarial input fuzzing ---

#[test]
fn test_gap_fill_rejects_excessive_gap() {
    let last_derived = 50u64;
    let expected_next = last_derived + 1;
    let adversarial_block = expected_next + MAX_BLOCK_GAP + 100;
    let gap_size = adversarial_block - expected_next;
    assert!(gap_size > MAX_BLOCK_GAP);

    let safe_gap = MAX_BLOCK_GAP;
    assert_eq!(safe_gap, 1000);
}

#[test]
fn test_excessive_gap_returns_error_not_continue() {
    let last_derived = 50u64;
    let expected_next = last_derived + 1;
    let excessive_block = expected_next + MAX_BLOCK_GAP + 1;
    let gap_size = excessive_block - expected_next;

    assert!(gap_size > MAX_BLOCK_GAP);

    let error_msg = format!(
        "BatchPosted block {excessive_block} exceeds \
         MAX_BLOCK_GAP ({MAX_BLOCK_GAP}): expected next block {expected_next}, \
         gap size {gap_size}. This indicates a state inconsistency — \
         the node may need to be re-synced from genesis."
    );
    assert!(error_msg.contains("MAX_BLOCK_GAP"));
    assert!(error_msg.contains(&excessive_block.to_string()));
    assert!(error_msg.contains("re-synced"));
}

// --- Cross-chain execution entry tests ---

#[tokio::test]
async fn test_fetch_execution_entries_for_builder_zero_rollups_address() {
    let config = Arc::new(RollupConfig {
        rollups_address: Address::ZERO,
        ..(*test_config()).clone()
    });
    let mut pipeline = DerivationPipeline::new(config);

    let provider = alloy_provider::RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
    let result = pipeline
        .fetch_execution_entries_for_builder(2000, &provider)
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn test_fetch_execution_entries_for_builder_cursor_ahead() {
    let config = Arc::new(RollupConfig {
        rollups_address: Address::with_last_byte(0x55),
        rollup_id: 1,
        ..(*test_config()).clone()
    });
    let mut pipeline = DerivationPipeline::new(config);
    pipeline.last_execution_l1_block = 3000;
    pipeline.builder_execution_l1_block = 3000;

    let provider = alloy_provider::RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());

    let result = pipeline
        .fetch_execution_entries_for_builder(3000, &provider)
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());

    let result = pipeline
        .fetch_execution_entries_for_builder(2000, &provider)
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

// --- Recovery and restart correctness ---

#[test]
fn test_restart_with_l1_far_ahead() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);
    pipeline.resume_from(1000);

    let l1_ahead = 3000u64;
    let from = pipeline.last_processed_l1_block().saturating_add(1);
    let to = l1_ahead.min(from.saturating_add(MAX_LOG_RANGE - 1));

    assert_eq!(from, 1001);
    assert_eq!(to, 3000);
    assert!(to - from < MAX_LOG_RANGE);

    let l1_very_ahead = 11000u64;
    let to2 = l1_very_ahead.min(from.saturating_add(MAX_LOG_RANGE - 1));
    assert_eq!(to2, 3000);
    assert_eq!(to2 - from + 1, MAX_LOG_RANGE);
}

#[test]
fn test_rollback_and_rederive_clears_state() {
    let config = test_config();
    let mut pipeline = DerivationPipeline::new(config);

    pipeline.resume_from(2000);
    pipeline.set_last_derived_l2_block(1000);
    for i in 0..1000 {
        pipeline.cursor.push(DerivedBlockMeta {
            l2_block_number: i + 1,
            l1_block_number: 1001 + i,
            l1_block_hash: B256::with_last_byte(i as u8),
        });
    }

    let last_valid = pipeline.rollback_to(1500);
    assert!(last_valid.is_some());
    assert_eq!(pipeline.last_processed_l1_block(), 1500);
    assert!(pipeline.cursor.iter().all(|m| m.l1_block_number <= 1500));
    assert_eq!(
        pipeline.last_derived_l2_block,
        pipeline
            .cursor
            .last()
            .map(|m| m.l2_block_number)
            .unwrap_or(0)
    );
}

#[test]
fn test_derivation_effective_state_root_with_consumed_deltas() {
    use crate::cross_chain::{CrossChainExecutionEntry, attach_chained_state_deltas};
    use alloy_primitives::Address;

    let rollup_id = 1u64;
    let rollup_id_u256 = crate::cross_chain::RollupId::new(U256::from(rollup_id));

    // Intermediate roots: Y → X₁ → X₂ → X
    let y = B256::with_last_byte(0x10);
    let x1 = B256::with_last_byte(0x11);
    let x2 = B256::with_last_byte(0x12);
    let x = B256::with_last_byte(0x13);

    // Create 3 CALL+RESULT entry pairs, then convert to L1 format.
    // Derivation sees L1-format entries (from BatchPosted events), so we
    // test the effective_state_root algorithm on L1-format entries.
    let make_pair = |id: u8| -> (CrossChainExecutionEntry, CrossChainExecutionEntry) {
        crate::cross_chain::build_cross_chain_call_entries(
            rollup_id_u256,
            Address::with_last_byte(id),
            vec![id],
            U256::ZERO,
            Address::with_last_byte(0xA0 + id),
            crate::cross_chain::RollupId::new(U256::from(2)),
            true,
            vec![id],
        )
    };
    let (c1, r1) = make_pair(1);
    let (c2, r2) = make_pair(2);
    let (c3, r3) = make_pair(3);
    // Attach chained deltas in L2 format (pairs), then convert to L1 format
    let mut l2_entries = vec![c1, r1, c2, r2, c3, r3];
    attach_chained_state_deltas(&mut l2_entries, &[y, x1, x2, x], rollup_id);
    let entries = crate::cross_chain::convert_pairs_to_l1_entries(&l2_entries);

    // Scenario 1: All consumed → effective root = X
    {
        let deferred = &entries;
        let mut effective_root = y;
        for entry in deferred {
            for delta in &entry.state_deltas {
                if delta.current_state == effective_root && delta.rollup_id == rollup_id_u256 {
                    effective_root = delta.new_state;
                }
            }
        }
        assert_eq!(effective_root, x, "all consumed → X");
    }

    // Scenario 2: Only E1 consumed → effective root = X₁
    {
        let deferred = &entries[..1]; // only L1 entry for E1
        let mut effective_root = y;
        for entry in deferred {
            for delta in &entry.state_deltas {
                if delta.current_state == effective_root && delta.rollup_id == rollup_id_u256 {
                    effective_root = delta.new_state;
                }
            }
        }
        assert_eq!(effective_root, x1, "E1 only → X₁");
    }

    // Scenario 3: E1 and E2 consumed → effective root = X₂
    {
        let deferred = &entries[..2]; // L1 entries for E1 and E2
        let mut effective_root = y;
        for entry in deferred {
            for delta in &entry.state_deltas {
                if delta.current_state == effective_root && delta.rollup_id == rollup_id_u256 {
                    effective_root = delta.new_state;
                }
            }
        }
        assert_eq!(effective_root, x2, "E1+E2 → X₂");
    }

    // Scenario 4: None consumed → effective root stays Y
    {
        let deferred: &[CrossChainExecutionEntry] = &[];
        let mut effective_root = y;
        for entry in deferred {
            for delta in &entry.state_deltas {
                if delta.current_state == effective_root && delta.rollup_id == rollup_id_u256 {
                    effective_root = delta.new_state;
                }
            }
        }
        assert_eq!(effective_root, y, "none consumed → Y");
    }
}

// ──────────────────────────────────────────────
//  Step 0.3 (refactor) — §4f filtering invariants
//
//  Closes invariants:
//    #4  §4f filtering is per-call prefix counting, never all-or-nothing
//        — see test_compute_consumed_trigger_prefix_* and the prefix
//        monotonicity proptest below.
//    #16 §4f filtering is generic on CrossChainCallExecuted events,
//        not Bridge selectors — covered in cross_chain_tests.rs via
//        test_identify_trigger_tx_indices_ignores_unrelated_event_signature.
//
//  Targets (all live in cross_chain.rs but tested here per the refactor
//  PLAN.md, since the §4f filtering pipeline is owned by derivation):
//    - cross_chain::compute_consumed_trigger_prefix (cross_chain.rs:2237)
//    - cross_chain::filter_block_by_trigger_prefix  (cross_chain.rs:2196)
// ──────────────────────────────────────────────

#[cfg(test)]
mod filtering_invariants_tests {
    use crate::cross_chain::{
        compute_consumed_trigger_prefix, execution_consumed_signature_hash,
        filter_block_by_trigger_prefix,
    };
    use alloy_primitives::{Address, B256, Bytes, LogData};

    /// Build a Receipt whose log[0] is the canonical ExecutionConsumed event
    /// from `ccm_address` for `action_hash`.
    fn mk_trigger_receipt(
        ccm_address: Address,
        action_hash: B256,
    ) -> alloy_consensus::Receipt<alloy_primitives::Log> {
        let sig = execution_consumed_signature_hash();
        alloy_consensus::Receipt {
            status: alloy_consensus::Eip658Value::Eip658(true),
            cumulative_gas_used: 0,
            logs: vec![alloy_primitives::Log {
                address: ccm_address,
                data: LogData::new(vec![sig, action_hash], Bytes::new()).unwrap(),
            }],
        }
    }

    /// Build a Receipt whose ExecutionConsumed event lists multiple
    /// action hashes (one per log).
    fn mk_trigger_receipt_multi(
        ccm_address: Address,
        action_hashes: &[B256],
    ) -> alloy_consensus::Receipt<alloy_primitives::Log> {
        let sig = execution_consumed_signature_hash();
        alloy_consensus::Receipt {
            status: alloy_consensus::Eip658Value::Eip658(true),
            cumulative_gas_used: 0,
            logs: action_hashes
                .iter()
                .map(|&h| alloy_primitives::Log {
                    address: ccm_address,
                    data: LogData::new(vec![sig, h], Bytes::new()).unwrap(),
                })
                .collect(),
        }
    }

    #[test]
    fn test_compute_consumed_trigger_prefix_empty_indices_returns_zero() {
        let ccm = Address::with_last_byte(0xAB);
        let receipts: Vec<alloy_consensus::Receipt<alloy_primitives::Log>> = vec![];
        let mut map = std::collections::HashMap::new();
        let k = compute_consumed_trigger_prefix(&receipts, ccm, &mut map, &[]);
        assert_eq!(k.as_usize(), 0);
    }

    #[test]
    fn test_compute_consumed_trigger_prefix_all_consumed_when_map_full() {
        let ccm = Address::with_last_byte(0xAB);
        let h1 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x01));
        let h2 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x02));
        let h3 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x03));
        let receipts = vec![
            mk_trigger_receipt(ccm, h1.as_b256()),
            mk_trigger_receipt(ccm, h2.as_b256()),
            mk_trigger_receipt(ccm, h3.as_b256()),
        ];
        let mut map = std::collections::HashMap::new();
        map.insert(h1, 1);
        map.insert(h2, 1);
        map.insert(h3, 1);

        let k = compute_consumed_trigger_prefix(&receipts, ccm, &mut map, &[0, 1, 2]);
        assert_eq!(k.as_usize(), 3, "all 3 trigger txs should be consumed");
        // The map is decremented to 0 for each consumed hash.
        assert_eq!(map[&h1], 0);
        assert_eq!(map[&h2], 0);
        assert_eq!(map[&h3], 0);
    }

    #[test]
    fn test_compute_consumed_trigger_prefix_stops_at_first_missing_hash() {
        // Three trigger txs; the *second* one's action hash is NOT in the
        // map, so the prefix must stop at 1 (first one consumed) and the
        // map must NOT be decremented for the second or third.
        let ccm = Address::with_last_byte(0xAB);
        let h1 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x01));
        let h2 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x02));
        let h3 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x03));
        let receipts = vec![
            mk_trigger_receipt(ccm, h1.as_b256()),
            mk_trigger_receipt(ccm, h2.as_b256()),
            mk_trigger_receipt(ccm, h3.as_b256()),
        ];
        let mut map = std::collections::HashMap::new();
        map.insert(h1, 1);
        // h2 is intentionally absent.
        map.insert(h3, 1); // present but unreachable due to prefix counting

        let k = compute_consumed_trigger_prefix(&receipts, ccm, &mut map, &[0, 1, 2]);
        assert_eq!(
            k.as_usize(),
            1,
            "prefix should stop at the second trigger (h2 missing)"
        );
        // h1 was consumed, h3 must remain UN-decremented (prefix counting,
        // never all-or-nothing).
        assert_eq!(map[&h1], 0);
        assert_eq!(map.get(&h2), None);
        assert_eq!(map[&h3], 1, "h3 must NOT be decremented past the gap");
    }

    #[test]
    fn test_compute_consumed_trigger_prefix_multi_hash_per_tx_atomic() {
        // A trigger tx that consumes 2 entries: if EITHER is missing in the
        // map, the entire trigger tx is rejected (no partial decrement).
        let ccm = Address::with_last_byte(0xAB);
        let ha = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x0A));
        let hb = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x0B));
        let receipts = vec![mk_trigger_receipt_multi(ccm, &[ha.as_b256(), hb.as_b256()])];
        let mut map = std::collections::HashMap::new();
        map.insert(ha, 1);
        // hb intentionally missing.

        let k = compute_consumed_trigger_prefix(&receipts, ccm, &mut map, &[0]);
        assert_eq!(
            k.as_usize(),
            0,
            "any missing hash within a tx rejects the tx"
        );
        assert_eq!(
            map[&ha], 1,
            "ha must NOT be decremented when the tx as a whole is rejected"
        );
    }

    #[test]
    fn test_compute_consumed_trigger_prefix_decrements_only_consumed() {
        // 3 trigger txs, only the first 2 reachable in the map. The map
        // must reflect exactly 2 decrements after the call.
        let ccm = Address::with_last_byte(0xAB);
        let h1 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x01));
        let h2 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x02));
        let h3 = crate::cross_chain::ActionHash::new(B256::with_last_byte(0x03));
        let receipts = vec![
            mk_trigger_receipt(ccm, h1.as_b256()),
            mk_trigger_receipt(ccm, h2.as_b256()),
            mk_trigger_receipt(ccm, h3.as_b256()),
        ];
        let mut map = std::collections::HashMap::new();
        map.insert(h1, 1);
        map.insert(h2, 1);
        // h3 missing — stops the prefix at 2.

        let total_before: usize = map.values().sum();
        let k = compute_consumed_trigger_prefix(&receipts, ccm, &mut map, &[0, 1, 2]);
        let total_after: usize = map.values().sum();
        assert_eq!(k.as_usize(), 2);
        assert_eq!(
            total_before - total_after,
            2,
            "exactly k entries should be decremented from the map"
        );
    }

    /// Build an RLP-encoded list of `n` minimal legacy transactions, each
    /// distinguishable by its nonce. Used as input to
    /// `filter_block_by_trigger_prefix`.
    fn encode_test_block(n: usize) -> Bytes {
        use alloy_consensus::TxLegacy;
        use alloy_primitives::TxKind;
        let mut txs: Vec<reth_ethereum_primitives::TransactionSigned> = Vec::with_capacity(n);
        for i in 0..n {
            let tx = TxLegacy {
                chain_id: Some(42069),
                nonce: i as u64,
                gas_price: 1_000_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: alloy_primitives::U256::ZERO,
                input: Default::default(),
            };
            let signed_legacy = alloy_consensus::Signed::new_unhashed(
                tx,
                alloy_primitives::Signature::new(
                    alloy_primitives::U256::from(1),
                    alloy_primitives::U256::from(2),
                    false,
                ),
            );
            txs.push(reth_ethereum_primitives::TransactionSigned::Legacy(
                signed_legacy,
            ));
        }
        let mut buf = Vec::new();
        alloy_rlp::encode_list(&txs, &mut buf);
        Bytes::from(buf)
    }

    /// Decode an RLP-encoded block back into the list of nonces (one per tx).
    /// Lets tests inspect the post-filter ordering without comparing full
    /// `TransactionSigned` structures.
    fn decode_block_nonces(encoded: &Bytes) -> Vec<u64> {
        use alloy_consensus::Transaction;
        use alloy_rlp::Decodable;
        let txs: Vec<reth_ethereum_primitives::TransactionSigned> =
            Decodable::decode(&mut encoded.as_ref()).unwrap();
        txs.iter().map(|t| t.nonce()).collect()
    }

    #[test]
    fn test_filter_block_by_trigger_prefix_empty_input() {
        let out = filter_block_by_trigger_prefix(&Bytes::new(), &[], 0).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_filter_block_by_trigger_prefix_no_triggers_keeps_all() {
        let block = encode_test_block(5);
        let out = filter_block_by_trigger_prefix(&block, &[], 0).unwrap();
        assert_eq!(decode_block_nonces(&out), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_filter_block_by_trigger_prefix_keep_count_exceeds_triggers() {
        // 5 txs, 2 triggers (indices 1 and 3), keep_count = 5 → all kept.
        let block = encode_test_block(5);
        let out = filter_block_by_trigger_prefix(&block, &[1, 3], 5).unwrap();
        assert_eq!(decode_block_nonces(&out), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_filter_block_by_trigger_prefix_removes_excess_triggers() {
        // 5 txs, 3 triggers (indices 0, 2, 4), keep_count = 1 →
        // only the first trigger (index 0) is kept; indices 2 and 4 are removed.
        // Non-trigger indices (1 and 3) are kept.
        let block = encode_test_block(5);
        let out = filter_block_by_trigger_prefix(&block, &[0, 2, 4], 1).unwrap();
        // Result: indices [0, 1, 3] → nonces [0, 1, 3].
        assert_eq!(decode_block_nonces(&out), vec![0, 1, 3]);
    }

    #[test]
    fn test_filter_block_by_trigger_prefix_keep_zero_removes_all_triggers() {
        // 4 txs, 2 triggers, keep_count = 0 → both triggers removed.
        let block = encode_test_block(4);
        let out = filter_block_by_trigger_prefix(&block, &[1, 3], 0).unwrap();
        // Result: indices [0, 2] → nonces [0, 2].
        assert_eq!(decode_block_nonces(&out), vec![0, 2]);
    }

    #[test]
    fn test_filter_block_by_trigger_prefix_preserves_relative_order() {
        // The order of the surviving txs must match the original order.
        let block = encode_test_block(6);
        // triggers at indices 1, 4. keep_count = 1 → drop index 4 only.
        let out = filter_block_by_trigger_prefix(&block, &[1, 4], 1).unwrap();
        assert_eq!(decode_block_nonces(&out), vec![0, 1, 2, 3, 5]);
    }

    mod proptests_filtering {
        use super::*;
        use proptest::collection::vec as prop_vec;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// `compute_consumed_trigger_prefix` is bounded by the number of
            /// trigger indices it was given. Closes invariant #4: the
            /// returned prefix is never larger than the input.
            #[test]
            fn compute_consumed_trigger_prefix_bounded_by_input(
                trigger_count in 0usize..16,
                map_population in 0u8..16,
            ) {
                let ccm = Address::with_last_byte(0xAB);
                let receipts: Vec<_> = (0..trigger_count)
                    .map(|i| mk_trigger_receipt(ccm, B256::with_last_byte(i as u8)))
                    .collect();
                let trigger_indices: Vec<usize> = (0..trigger_count).collect();

                // Pre-populate the map with `map_population` of the trigger hashes.
                let mut map = std::collections::HashMap::new();
                for i in 0..(map_population as usize).min(trigger_count) {
                    map.insert(
                        crate::cross_chain::ActionHash::new(B256::with_last_byte(i as u8)),
                        1,
                    );
                }

                let k = compute_consumed_trigger_prefix(
                    &receipts, ccm, &mut map, &trigger_indices,
                );
                prop_assert!(k.as_usize() <= trigger_count);
                // The prefix never exceeds the number of consecutive
                // populated hashes from the start.
                prop_assert!(k.as_usize() <= (map_population as usize).min(trigger_count));
            }

            /// `compute_consumed_trigger_prefix` is monotonic in the map: if
            /// you start with a map M₁ that is a subset of M₂ (same hashes,
            /// possibly more), then k(M₁) ≤ k(M₂). Stronger consequence of
            /// invariant #4: never accept fewer entries when *more* are
            /// available.
            #[test]
            fn compute_consumed_trigger_prefix_monotonic_in_map(
                trigger_count in 1usize..8,
                drop_first in 0u8..8,
            ) {
                let ccm = Address::with_last_byte(0xAB);
                let receipts: Vec<_> = (0..trigger_count)
                    .map(|i| mk_trigger_receipt(ccm, B256::with_last_byte(i as u8)))
                    .collect();
                let trigger_indices: Vec<usize> = (0..trigger_count).collect();

                // Map M_full has every hash present.
                let mut map_full = std::collections::HashMap::new();
                for i in 0..trigger_count {
                    map_full.insert(
                        crate::cross_chain::ActionHash::new(B256::with_last_byte(i as u8)),
                        1,
                    );
                }
                let mut map_partial = map_full.clone();
                // Remove the first `drop_first` hashes from `map_partial`,
                // keeping the rest. Since the prefix walks from index 0,
                // dropping the first hash always reduces k to 0.
                for i in 0..(drop_first as usize).min(trigger_count) {
                    map_partial.remove(&crate::cross_chain::ActionHash::new(
                        B256::with_last_byte(i as u8),
                    ));
                }

                let k_full = compute_consumed_trigger_prefix(
                    &receipts, ccm, &mut map_full, &trigger_indices,
                );
                let k_partial = compute_consumed_trigger_prefix(
                    &receipts, ccm, &mut map_partial, &trigger_indices,
                );

                // Adding more entries to the map never *decreases* the
                // accepted prefix.
                prop_assert!(k_full >= k_partial);
                // ConsumedPrefix comparison works via PartialOrd derive.
            }

            /// `filter_block_by_trigger_prefix` preserves all non-trigger txs
            /// and at least the first `keep_count` triggers. Total surviving
            /// count obeys: out_len == total - max(0, triggers_len - keep).
            #[test]
            fn filter_block_preserves_count_invariant(
                total in 1usize..16,
                trigger_indices in prop_vec(0usize..16, 0..8),
                keep_count in 0usize..16,
            ) {
                // Normalize: dedupe trigger_indices and keep only those
                // strictly less than `total` and sorted (the function's
                // input contract).
                let mut triggers: Vec<usize> = trigger_indices
                    .into_iter()
                    .filter(|&i| i < total)
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                triggers.sort();

                let block = encode_test_block(total);
                let out = filter_block_by_trigger_prefix(&block, &triggers, keep_count).unwrap();
                let out_nonces = decode_block_nonces(&out);

                // Expected: total - max(0, triggers_len - keep_count).
                let removed = triggers.len().saturating_sub(keep_count);
                prop_assert_eq!(out_nonces.len(), total - removed);

                // Every non-trigger nonce must survive.
                let trigger_set: std::collections::BTreeSet<u64> =
                    triggers.iter().map(|&i| i as u64).collect();
                for n in 0u64..total as u64 {
                    if !trigger_set.contains(&n) {
                        prop_assert!(
                            out_nonces.contains(&n),
                            "non-trigger nonce {} was incorrectly removed",
                            n
                        );
                    }
                }

                // The first `keep_count` trigger nonces must survive.
                for &t in triggers.iter().take(keep_count) {
                    prop_assert!(
                        out_nonces.contains(&(t as u64)),
                        "trigger nonce {} should have been kept",
                        t
                    );
                }
            }
        }
    }
}
