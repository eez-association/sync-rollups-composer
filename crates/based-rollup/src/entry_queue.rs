//! Typed entry queue with 3-state machine for the hold-then-forward
//! pattern (refactor PLAN step 6, invariant #13).
//!
//! # Lifecycle
//!
//! ```text
//!   push()          drain_pending()     confirm()
//! ───────► Pending ──────────────► Reserved ──────► Confirmed
//!                        ▲                  │
//!                        │    rollback()     │
//!                        └──────────────────┘
//! ```
//!
//! The composer RPC pushes a receipt, then awaits [`EntryQueue::wait_confirmation`].
//! The driver drains pending receipts into Reserved, attempts L1 submission,
//! and either confirms (success) or rolls back (failure). Confirmation
//! returns a [`ForwardPermit`] — a zero-cost token proving the entries
//! landed in a canonical block. The composer must hold the permit to
//! forward the user transaction; dropping it without use triggers a
//! `#[must_use]` warning.
//!
//! # Thread safety
//!
//! [`EntryQueue`] is `Clone + Send + Sync` (shared via inner `Arc`).
//! All mutation goes through a `tokio::sync::Mutex`; waiters park on a
//! `tokio::sync::Notify`.

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

// ────────────────────────────────────────────────────────────────
//  Public types
// ────────────────────────────────────────────────────────────────

/// Stable token emitted by [`EntryQueue::push`].
///
/// Opaque — not a positional index. Two receipts compare equal iff
/// they were returned by the same `push()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueueReceipt(u64);

/// Zero-cost token proving that the entries behind a [`QueueReceipt`]
/// were confirmed in a canonical block.
///
/// Only constructed inside [`EntryQueue::wait_confirmation`] after
/// observing the `Confirmed` state. Consuming code should pass this
/// token to the forwarding step — the `#[must_use]` attribute
/// prevents silent drops.
#[derive(Debug)]
#[must_use = "ForwardPermit must be consumed by the forward step; never drop silently"]
pub struct ForwardPermit {
    /// Private field — prevents external construction.
    _seal: (),
}

// ────────────────────────────────────────────────────────────────
//  Internal types
// ────────────────────────────────────────────────────────────────

/// Three-state machine for a single receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptState {
    /// Just pushed by the composer; not yet picked up by the driver.
    Pending,
    /// Driver has drained it; L1 submission is in progress.
    Reserved,
    /// L1 submission succeeded and the entry is confirmed on-chain.
    Confirmed,
}

/// Interior-mutable state behind the `Arc<Mutex<_>>`.
struct QueueState {
    /// Monotonically increasing counter for receipt IDs.
    next_id: u64,
    /// All live receipts keyed by their ID, in insertion order
    /// (BTreeMap gives deterministic iteration).
    items: BTreeMap<QueueReceipt, ReceiptState>,
}

// ────────────────────────────────────────────────────────────────
//  EntryQueue
// ────────────────────────────────────────────────────────────────

/// Thread-safe entry queue with a 3-state machine per receipt.
///
/// See the [module docs](self) for the full lifecycle diagram.
#[derive(Clone)]
pub struct EntryQueue {
    inner: Arc<Mutex<QueueState>>,
    notify: Arc<Notify>,
}

impl Default for EntryQueue {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(QueueState {
                next_id: 0,
                items: BTreeMap::new(),
            })),
            notify: Arc::new(Notify::new()),
        }
    }
}

impl EntryQueue {
    /// Create a new empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    // ── Composer-side ────────────────────────────────────────────

    /// Push a new entry. Returns a [`QueueReceipt`] in the `Pending`
    /// state. The caller should then call [`wait_confirmation`](Self::wait_confirmation)
    /// to block until the driver confirms or evicts it.
    pub async fn push(&self) -> QueueReceipt {
        let mut state = self.inner.lock().await;
        let id = state.next_id;
        state.next_id += 1;
        let receipt = QueueReceipt(id);
        state.items.insert(receipt, ReceiptState::Pending);
        receipt
    }

    /// Block until `receipt` reaches the `Confirmed` state, then
    /// return a [`ForwardPermit`].
    ///
    /// Returns `Err` if the receipt is evicted from the queue (e.g.
    /// by [`remove`](Self::remove)) before it reaches `Confirmed`.
    pub async fn wait_confirmation(&self, receipt: QueueReceipt) -> eyre::Result<ForwardPermit> {
        loop {
            {
                let state = self.inner.lock().await;
                match state.items.get(&receipt) {
                    Some(ReceiptState::Confirmed) => {
                        return Ok(ForwardPermit { _seal: () });
                    }
                    Some(_) => { /* keep waiting */ }
                    None => {
                        return Err(eyre::eyre!(
                            "receipt {receipt:?} evicted from queue before confirmation"
                        ));
                    }
                }
            }
            // Park until some state transition fires `notify_waiters`.
            self.notify.notified().await;
        }
    }

    // ── Driver-side ─────────────────────────────────────────────

    /// Drain up to `max` receipts from `Pending` to `Reserved`.
    ///
    /// Returns the receipt IDs that were transitioned so the driver
    /// can associate them with the L1 submission it is about to
    /// attempt.
    pub async fn drain_pending(&self, max: usize) -> Vec<QueueReceipt> {
        let mut state = self.inner.lock().await;
        let mut drained = Vec::new();
        for (receipt, rs) in state.items.iter_mut() {
            if drained.len() >= max {
                break;
            }
            if *rs == ReceiptState::Pending {
                *rs = ReceiptState::Reserved;
                drained.push(*receipt);
            }
        }
        drained
    }

    /// Confirm a set of receipts (`Reserved` -> `Confirmed`).
    ///
    /// Wakes all parked [`wait_confirmation`](Self::wait_confirmation) callers.
    /// Receipts not in `Reserved` state are silently skipped.
    pub async fn confirm(&self, receipts: &[QueueReceipt]) {
        {
            let mut state = self.inner.lock().await;
            for r in receipts {
                if let Some(rs) = state.items.get_mut(r) {
                    if *rs == ReceiptState::Reserved {
                        *rs = ReceiptState::Confirmed;
                    }
                }
            }
        }
        self.notify.notify_waiters();
    }

    /// Roll back a set of receipts (`Reserved` -> `Pending`).
    ///
    /// Used when L1 submission fails and the entries should be
    /// retried in a future flush cycle. Does **not** wake waiters
    /// (there is nothing new to observe yet).
    pub async fn rollback(&self, receipts: &[QueueReceipt]) {
        let mut state = self.inner.lock().await;
        for r in receipts {
            if let Some(rs) = state.items.get_mut(r) {
                if *rs == ReceiptState::Reserved {
                    *rs = ReceiptState::Pending;
                }
            }
        }
    }

    /// Remove a receipt from the queue entirely.
    ///
    /// Typically called after the composer has consumed its
    /// [`ForwardPermit`] and forwarded the user tx. Also wakes
    /// waiters so that any [`wait_confirmation`](Self::wait_confirmation)
    /// call on an evicted receipt returns `Err` promptly.
    pub async fn remove(&self, receipt: &QueueReceipt) {
        {
            let mut state = self.inner.lock().await;
            state.items.remove(receipt);
        }
        self.notify.notify_waiters();
    }

    // ── Diagnostics ─────────────────────────────────────────────

    /// Total number of receipts in any state.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.items.len()
    }

    /// Whether the queue is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.items.is_empty()
    }
}

// ────────────────────────────────────────────────────────────────
//  Tests
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn push_drain_confirm_wait_cycle() {
        let q = EntryQueue::new();

        // Composer pushes two entries.
        let r1 = q.push().await;
        let r2 = q.push().await;
        assert_eq!(q.len().await, 2);

        // Driver drains both.
        let drained = q.drain_pending(10).await;
        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&r1));
        assert!(drained.contains(&r2));

        // A second drain yields nothing (both are Reserved now).
        let drained2 = q.drain_pending(10).await;
        assert!(drained2.is_empty());

        // Driver confirms.
        q.confirm(&drained).await;

        // Composer waits — should return immediately since already confirmed.
        let permit1 = q.wait_confirmation(r1).await;
        assert!(permit1.is_ok());
        let permit2 = q.wait_confirmation(r2).await;
        assert!(permit2.is_ok());

        // Consume permits (suppress #[must_use]).
        let _p1 = permit1.unwrap();
        let _p2 = permit2.unwrap();

        // Cleanup.
        q.remove(&r1).await;
        q.remove(&r2).await;
        assert!(q.is_empty().await);
    }

    #[tokio::test]
    async fn rollback_returns_to_pending() {
        let q = EntryQueue::new();

        let r = q.push().await;

        // Drain -> Reserved.
        let drained = q.drain_pending(10).await;
        assert_eq!(drained, vec![r]);

        // Rollback -> Pending again.
        q.rollback(&drained).await;

        // Second drain picks it up again.
        let drained2 = q.drain_pending(10).await;
        assert_eq!(drained2, vec![r]);

        // Now confirm it.
        q.confirm(&drained2).await;
        let permit = q.wait_confirmation(r).await;
        assert!(permit.is_ok());
        let _p = permit.unwrap();
    }

    #[tokio::test]
    async fn evicted_receipt_returns_error() {
        let q = EntryQueue::new();
        let r = q.push().await;

        // Remove before confirmation.
        q.remove(&r).await;

        let result = q.wait_confirmation(r).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("evicted"),
            "expected 'evicted' in error message, got: {msg}"
        );
    }

    #[tokio::test]
    async fn drain_max_limits_output() {
        let q = EntryQueue::new();

        let _r1 = q.push().await;
        let _r2 = q.push().await;
        let _r3 = q.push().await;

        // Drain at most 2.
        let drained = q.drain_pending(2).await;
        assert_eq!(drained.len(), 2);

        // The third is still pending.
        let drained2 = q.drain_pending(10).await;
        assert_eq!(drained2.len(), 1);
    }

    #[tokio::test]
    async fn confirm_skips_non_reserved() {
        let q = EntryQueue::new();
        let r = q.push().await;

        // Confirm while still Pending — should be a no-op.
        q.confirm(&[r]).await;

        // Still drainable (still Pending).
        let drained = q.drain_pending(10).await;
        assert_eq!(drained, vec![r]);
    }

    #[tokio::test]
    async fn rollback_skips_non_reserved() {
        let q = EntryQueue::new();
        let r = q.push().await;

        // Rollback while Pending — no-op.
        q.rollback(&[r]).await;

        // Drain, confirm, then rollback on Confirmed — no-op.
        let drained = q.drain_pending(10).await;
        q.confirm(&drained).await;
        q.rollback(&drained).await;

        // Still confirmed.
        let permit = q.wait_confirmation(r).await;
        assert!(permit.is_ok());
        let _p = permit.unwrap();
    }

    #[tokio::test]
    async fn concurrent_wait_and_confirm() {
        let q = EntryQueue::new();
        let r = q.push().await;

        // Drain to Reserved.
        let drained = q.drain_pending(10).await;

        // Spawn a waiter that will park until confirmation.
        let q_clone = q.clone();
        let waiter = tokio::spawn(async move { q_clone.wait_confirmation(r).await });

        // Give the waiter a chance to park.
        tokio::task::yield_now().await;

        // Confirm — this should wake the waiter.
        q.confirm(&drained).await;

        let result = waiter.await.expect("waiter task panicked");
        assert!(result.is_ok());
        let _p = result.unwrap();
    }

    #[tokio::test]
    async fn concurrent_wait_and_evict() {
        let q = EntryQueue::new();
        let r = q.push().await;

        // Spawn a waiter.
        let q_clone = q.clone();
        let waiter = tokio::spawn(async move { q_clone.wait_confirmation(r).await });

        // Give the waiter a chance to park.
        tokio::task::yield_now().await;

        // Remove — should wake the waiter with an error.
        q.remove(&r).await;

        let result = waiter.await.expect("waiter task panicked");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn receipts_are_monotonically_increasing() {
        let q = EntryQueue::new();
        let r1 = q.push().await;
        let r2 = q.push().await;
        let r3 = q.push().await;

        // QueueReceipt implements Ord; IDs should be strictly ordered.
        assert!(r1 < r2);
        assert!(r2 < r3);
    }

    #[tokio::test]
    async fn default_is_empty() {
        let q = EntryQueue::default();
        assert!(q.is_empty().await);
        assert_eq!(q.len().await, 0);
    }
}
