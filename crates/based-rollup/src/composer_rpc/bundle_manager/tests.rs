//! Unit tests for the bundle manager state machine + helpers.

use super::*;

use alloy_consensus::{SignableTransaction, TxEip1559, TxLegacy};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

// ──────────────────────────────────────────────────────────────────────────────
//  Helpers — build signed raw txs for the tests
// ──────────────────────────────────────────────────────────────────────────────

fn legacy_raw_tx(gas_price: u128, nonce: u64) -> Bytes {
    let signer = PrivateKeySigner::random();
    let tx = TxLegacy {
        chain_id: Some(1),
        nonce,
        gas_price,
        gas_limit: 100_000,
        to: TxKind::Call(Address::repeat_byte(0x11)),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let sig = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope = tx.into_signed(sig);
    let mut buf = Vec::new();
    alloy_eips::eip2718::Encodable2718::encode_2718(&envelope, &mut buf);
    Bytes::from(buf)
}

fn eip1559_raw_tx(priority_fee: u128, max_fee: u128, nonce: u64) -> Bytes {
    let signer = PrivateKeySigner::random();
    let tx = TxEip1559 {
        chain_id: 1,
        nonce,
        max_priority_fee_per_gas: priority_fee,
        max_fee_per_gas: max_fee,
        gas_limit: 100_000,
        to: TxKind::Call(Address::repeat_byte(0x22)),
        value: U256::ZERO,
        input: Bytes::new(),
        access_list: Default::default(),
    };
    let sig = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope = tx.into_signed(sig);
    let mut buf = Vec::new();
    alloy_eips::eip2718::Encodable2718::encode_2718(&envelope, &mut buf);
    Bytes::from(buf)
}

fn mock_tx(gas_price: u128, hash_byte: u8) -> PendingUserTx {
    PendingUserTx {
        raw_tx: Bytes::from(vec![hash_byte; 10]),
        tx_hash: B256::repeat_byte(hash_byte),
        from: Address::repeat_byte(hash_byte),
        to: Address::repeat_byte(0xAA),
        data: Bytes::new(),
        value: U256::ZERO,
        effective_gas_price: gas_price,
        cross_chain_hint: false,
        arrived_at_ms: now_ms(),
    }
}

fn default_config() -> BundleConfig {
    BundleConfig {
        l1_block_time_ms: 12_000,
        close_fraction: 0.7,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
//  1. Config math
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn window_and_grace_sum_to_block_time() {
    let cfg = default_config();
    assert_eq!(cfg.window_ms() + cfg.grace_ms(), cfg.l1_block_time_ms);
    assert_eq!(cfg.window_ms(), 8_400);
    assert_eq!(cfg.grace_ms(), 3_600);
}

#[test]
fn window_with_different_fraction() {
    let cfg = BundleConfig {
        l1_block_time_ms: 5_000,
        close_fraction: 0.5,
    };
    assert_eq!(cfg.window_ms(), 2_500);
    assert_eq!(cfg.grace_ms(), 2_500);
}

// ──────────────────────────────────────────────────────────────────────────────
//  2. Submit — current vs next based on close_deadline
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn submit_before_deadline_goes_to_current() {
    let mgr = BundleManager::new(default_config());
    assert!(mgr.submit(mock_tx(100, 0x01)));
    let drained = mgr.drain_current();
    assert_eq!(drained.txs.len(), 1);
    assert_eq!(drained.txs[0].tx_hash, B256::repeat_byte(0x01));
}

#[test]
fn submit_after_deadline_goes_to_next() {
    let mgr = BundleManager::new(default_config());
    // Forcibly expire the deadline.
    {
        let mut s = mgr.state.lock().unwrap();
        s.close_deadline_ms = now_ms().saturating_sub(1_000);
    }
    assert!(mgr.submit(mock_tx(100, 0x02)));
    let drained = mgr.drain_current();
    assert!(drained.txs.is_empty(), "current must be empty");
    mgr.rotate();
    let after_rot = mgr.drain_current();
    assert_eq!(after_rot.txs.len(), 1);
    assert_eq!(after_rot.txs[0].tx_hash, B256::repeat_byte(0x02));
}

// ──────────────────────────────────────────────────────────────────────────────
//  3. Dedup — same tx_hash doesn't double-insert
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn dedup_rejects_second_insert_of_same_hash() {
    let mgr = BundleManager::new(default_config());
    let tx = mock_tx(100, 0x03);
    assert!(mgr.submit(tx.clone()));
    assert!(!mgr.submit(tx.clone()), "second submit should be deduped");
    let drained = mgr.drain_current();
    assert_eq!(drained.txs.len(), 1);
    assert_eq!(mgr.metrics.tx_deduped_total.load(Ordering::Relaxed), 1);
}

#[test]
fn dedup_checks_next_queue_too() {
    let mgr = BundleManager::new(default_config());
    // First insert in current.
    let tx = mock_tx(100, 0x04);
    assert!(mgr.submit(tx.clone()));
    // Flip the deadline — any further submit goes to next.
    {
        let mut s = mgr.state.lock().unwrap();
        s.close_deadline_ms = now_ms().saturating_sub(1_000);
    }
    // Same tx_hash — must dedup even though the queue differs.
    assert!(!mgr.submit(tx));
    assert_eq!(mgr.metrics.tx_deduped_total.load(Ordering::Relaxed), 1);
}

// ──────────────────────────────────────────────────────────────────────────────
//  4. Rotate — current ← next, next empties
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn rotate_moves_next_to_current() {
    let mgr = BundleManager::new(default_config());
    // Put one in current, one in next (by flipping deadline between submits).
    assert!(mgr.submit(mock_tx(100, 0x05)));
    {
        let mut s = mgr.state.lock().unwrap();
        s.close_deadline_ms = now_ms().saturating_sub(1_000);
    }
    assert!(mgr.submit(mock_tx(100, 0x06)));

    // Drain current — gets only the first.
    let drained = mgr.drain_current();
    assert_eq!(drained.txs.len(), 1);
    assert_eq!(drained.txs[0].tx_hash, B256::repeat_byte(0x05));

    // Rotate — the one in next becomes current.
    mgr.rotate();
    let after_rot = mgr.drain_current();
    assert_eq!(after_rot.txs.len(), 1);
    assert_eq!(after_rot.txs[0].tx_hash, B256::repeat_byte(0x06));

    // Next is empty after rotate.
    mgr.rotate();
    let final_drain = mgr.drain_current();
    assert!(final_drain.txs.is_empty());
    assert_eq!(mgr.metrics.cycles_total.load(Ordering::Relaxed), 2);
}

// ──────────────────────────────────────────────────────────────────────────────
//  5. Sort by gas price descending, stable tiebreaker by tx_hash
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn sort_descending_by_gas_price() {
    let mut bundle = vec![
        mock_tx(50, 0x10),
        mock_tx(200, 0x11),
        mock_tx(100, 0x12),
        mock_tx(200, 0x13),
    ];
    sort_bundle_by_gas_desc(&mut bundle);
    assert_eq!(bundle[0].effective_gas_price, 200);
    assert_eq!(bundle[1].effective_gas_price, 200);
    assert_eq!(bundle[2].effective_gas_price, 100);
    assert_eq!(bundle[3].effective_gas_price, 50);
    // Tiebreaker: lower tx_hash first — but 0x11 < 0x13, so 0x11 should be before 0x13.
    assert_eq!(bundle[0].tx_hash, B256::repeat_byte(0x11));
    assert_eq!(bundle[1].tx_hash, B256::repeat_byte(0x13));
}

// ──────────────────────────────────────────────────────────────────────────────
//  6. effective_gas_price — all tx types
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn effective_gas_price_legacy() {
    let raw = legacy_raw_tx(500, 0);
    assert_eq!(effective_gas_price(&raw), 500);
}

#[test]
fn effective_gas_price_eip1559_returns_priority_fee_not_max_fee() {
    // max_fee is the cap; priority_fee is what actually matters for ordering.
    let raw = eip1559_raw_tx(750, 2_000, 0);
    assert_eq!(effective_gas_price(&raw), 750);
}

#[test]
fn effective_gas_price_malformed_returns_zero() {
    // Garbage bytes that aren't a valid tx.
    let raw = Bytes::from(vec![0xff, 0xff, 0xff]);
    assert_eq!(effective_gas_price(&raw), 0);
}

// ──────────────────────────────────────────────────────────────────────────────
//  7. Bundle id — deterministic + order-invariant
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn bundle_id_invariant_under_tx_ordering() {
    let a = mock_tx(100, 0xA1);
    let b = mock_tx(200, 0xB2);
    let c = mock_tx(150, 0xC3);

    let cycle_start = 1_000_000;
    let id_1 = compute_bundle_id(cycle_start, &[a.clone(), b.clone(), c.clone()]);
    let id_2 = compute_bundle_id(cycle_start, &[c.clone(), a.clone(), b.clone()]);
    assert_eq!(id_1, id_2, "bundle_id must be order-invariant");
}

#[test]
fn bundle_id_differs_by_cycle_start() {
    let a = mock_tx(100, 0xA1);
    let id_1 = compute_bundle_id(1_000_000, std::slice::from_ref(&a));
    let id_2 = compute_bundle_id(1_000_012, &[a]);
    assert_ne!(id_1, id_2);
}

#[test]
fn bundle_id_empty_is_deterministic() {
    let id = compute_bundle_id(1_000_000, &[]);
    // Must not panic, must be deterministic.
    let id2 = compute_bundle_id(1_000_000, &[]);
    assert_eq!(id, id2);
}

// ──────────────────────────────────────────────────────────────────────────────
//  8. Drain empties current
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn drain_empties_current_but_preserves_next() {
    let mgr = BundleManager::new(default_config());
    assert!(mgr.submit(mock_tx(100, 0x20)));
    // Flip deadline → new submits to next.
    {
        let mut s = mgr.state.lock().unwrap();
        s.close_deadline_ms = now_ms().saturating_sub(1_000);
    }
    assert!(mgr.submit(mock_tx(100, 0x21)));
    let drained = mgr.drain_current();
    assert_eq!(drained.txs.len(), 1);

    // current is now empty, next still has 0x21.
    let s = mgr.state.lock().unwrap();
    assert!(s.current.is_empty());
    assert_eq!(s.next.len(), 1);
    assert_eq!(s.next[0].tx_hash, B256::repeat_byte(0x21));
}
