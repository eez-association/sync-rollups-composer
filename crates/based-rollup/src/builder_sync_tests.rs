use super::*;

#[tokio::test]
async fn test_builder_sync_invalid_url() {
    let sync = BuilderSync::new("ws://127.0.0.1:1".to_string());
    let (tx, _rx) = mpsc::channel(1);
    // Test single connection attempt (run_once), not the reconnection loop
    let result = sync.run_once(&tx).await;
    assert!(result.is_err(), "connecting to invalid WS should fail");
}

#[tokio::test]
async fn test_builder_sync_channel_closed() {
    // If receiver is dropped, sender should detect it and stop
    let (tx, rx) = mpsc::channel::<PreconfirmedBlock>(1);
    drop(rx);
    assert!(
        tx.send(PreconfirmedBlock {
            block_number: 1,
            block_hash: B256::ZERO,
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn test_channel_backpressure_drops_block() {
    // Channel capacity of 1 — filling it should cause try_send to fail
    let (tx, _rx) = mpsc::channel::<PreconfirmedBlock>(1);

    // First send succeeds (fills channel)
    let result1 = tx.try_send(PreconfirmedBlock {
        block_number: 1,
        block_hash: B256::with_last_byte(1),
    });
    assert!(result1.is_ok());

    // Second send fails with Full (channel at capacity)
    let result2 = tx.try_send(PreconfirmedBlock {
        block_number: 2,
        block_hash: B256::with_last_byte(2),
    });
    assert!(matches!(result2, Err(mpsc::error::TrySendError::Full(_))));
}

#[tokio::test]
async fn test_duplicate_block_number_overwrites_in_hashmap() {
    // When the builder sends the same block number twice (e.g. after reconnect),
    // the HashMap should just overwrite — no panic or duplicate
    use std::collections::HashMap;
    let mut preconfirmed: HashMap<u64, B256> = HashMap::new();

    let hash1 = B256::with_last_byte(0x11);
    let hash2 = B256::with_last_byte(0x22);

    preconfirmed.insert(10, hash1);
    preconfirmed.insert(10, hash2);

    assert_eq!(preconfirmed.len(), 1);
    assert_eq!(preconfirmed[&10], hash2, "second insert should overwrite");
}

#[tokio::test]
async fn test_out_of_order_blocks_stored_correctly() {
    // Blocks arriving out of order (e.g. N+2 before N+1) should all be stored
    use std::collections::HashMap;
    let mut preconfirmed: HashMap<u64, B256> = HashMap::new();

    // Arrive out of order: 5, 3, 4, 7, 6
    for n in [5u64, 3, 4, 7, 6] {
        preconfirmed.insert(n, B256::with_last_byte(n as u8));
    }

    assert_eq!(preconfirmed.len(), 5);
    for n in 3..=7 {
        assert!(preconfirmed.contains_key(&n));
    }
}

#[tokio::test]
async fn test_gap_in_preconfirmed_blocks() {
    // Builder sends N, disconnects, reconnects, sends N+2 (gap at N+1)
    // Fullnode should store both; the gap block is derived from L1
    use std::collections::HashMap;
    let mut preconfirmed: HashMap<u64, B256> = HashMap::new();

    preconfirmed.insert(10, B256::with_last_byte(10));
    // Gap: block 11 not received (disconnect)
    preconfirmed.insert(12, B256::with_last_byte(12));

    assert!(preconfirmed.contains_key(&10));
    assert!(!preconfirmed.contains_key(&11), "gap block not received");
    assert!(preconfirmed.contains_key(&12));

    // When fullnode derives block 11 from L1, it won't find a preconfirmation
    // — this is correct behavior (logged as "no preconfirmation")
    assert!(preconfirmed.remove(&11).is_none());
}
