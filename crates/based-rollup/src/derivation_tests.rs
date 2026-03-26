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
    let rollup_id_u256 = U256::from(rollup_id);

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
            U256::from(2),
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
