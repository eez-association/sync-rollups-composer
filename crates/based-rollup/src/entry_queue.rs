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
//! All mutation goes through a `std::sync::Mutex`; waiters park on a
//! `tokio::sync::Notify`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

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

/// A single slot in the queue: the payload (taken by drain) plus the
/// receipt state (used by confirm/rollback/wait).
///
/// After [`EntryQueue::drain_pending`] the payload is `None` but the
/// receipt stays in the map so `confirm`/`rollback`/`wait_confirmation`
/// can still operate on it.
struct Slot<T> {
    payload: Option<T>,
    state: ReceiptState,
}

/// Interior-mutable state behind the `Arc<Mutex<_>>`.
struct QueueState<T> {
    /// Monotonically increasing counter for receipt IDs.
    next_id: u64,
    /// All live receipts keyed by their ID, in insertion order
    /// (BTreeMap gives deterministic iteration).
    items: BTreeMap<QueueReceipt, Slot<T>>,
}

// ────────────────────────────────────────────────────────────────
//  EntryQueue<T>
// ────────────────────────────────────────────────────────────────

/// Thread-safe entry queue with a 3-state machine per receipt.
///
/// See the [module docs](self) for the full lifecycle diagram.
///
/// Generic over payload type `T`. The payload is stored alongside the
/// receipt state and returned by [`drain_pending`](Self::drain_pending).
#[derive(Clone)]
pub struct EntryQueue<T> {
    inner: Arc<Mutex<QueueState<T>>>,
    notify: Arc<Notify>,
}

impl<T: Send + 'static> Default for EntryQueue<T> {
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

impl<T: Send + 'static> EntryQueue<T> {
    /// Create a new empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    // ── Composer-side ────────────────────────────────────────────

    /// Push a new entry with its payload. Returns a [`QueueReceipt`]
    /// in the `Pending` state. The caller should then call
    /// [`wait_confirmation`](Self::wait_confirmation) to block until
    /// the driver confirms or evicts it.
    pub fn push(&self, payload: T) -> QueueReceipt {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let id = state.next_id;
        state.next_id += 1;
        let receipt = QueueReceipt(id);
        state.items.insert(receipt, Slot {
            payload: Some(payload),
            state: ReceiptState::Pending,
        });
        receipt
    }

    /// Push multiple payloads at once. Returns the receipts in
    /// insertion order. Useful for the rollback path where drained
    /// items need to be re-pushed to the queue.
    pub fn push_many(&self, payloads: impl IntoIterator<Item = T>) -> Vec<QueueReceipt> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut receipts = Vec::new();
        for payload in payloads {
            let id = state.next_id;
            state.next_id += 1;
            let receipt = QueueReceipt(id);
            state.items.insert(receipt, Slot {
                payload: Some(payload),
                state: ReceiptState::Pending,
            });
            receipts.push(receipt);
        }
        receipts
    }

    /// Block until `receipt` reaches the `Confirmed` state, then
    /// return a [`ForwardPermit`].
    ///
    /// Returns `Err` if the receipt is evicted from the queue (e.g.
    /// by [`remove`](Self::remove)) before it reaches `Confirmed`.
    pub async fn wait_confirmation(
        &self,
        receipt: QueueReceipt,
    ) -> eyre::Result<ForwardPermit> {
        loop {
            {
                let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                match state.items.get(&receipt) {
                    Some(slot) if slot.state == ReceiptState::Confirmed => {
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

    /// Drain up to `max` receipts from `Pending` to `Reserved`,
    /// returning the receipt and payload for each.
    ///
    /// Payloads are taken out of the queue (ownership transferred to
    /// the caller). The receipt remains in the map so
    /// `confirm`/`rollback`/`wait_confirmation` can still operate on
    /// it. The payload slot becomes `None` after draining.
    pub fn drain_pending(&self, max: usize) -> Vec<(QueueReceipt, T)> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut drained = Vec::new();
        for (receipt, slot) in state.items.iter_mut() {
            if drained.len() >= max {
                break;
            }
            if slot.state == ReceiptState::Pending {
                slot.state = ReceiptState::Reserved;
                if let Some(payload) = slot.payload.take() {
                    drained.push((*receipt, payload));
                }
            }
        }
        drained
    }

    /// Confirm a set of receipts (`Reserved` -> `Confirmed`).
    ///
    /// Wakes all parked [`wait_confirmation`](Self::wait_confirmation) callers.
    /// Receipts not in `Reserved` state are silently skipped.
    pub fn confirm(&self, receipts: &[QueueReceipt]) {
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            for r in receipts {
                if let Some(slot) = state.items.get_mut(r) {
                    if slot.state == ReceiptState::Reserved {
                        slot.state = ReceiptState::Confirmed;
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
    ///
    /// **Note**: after drain, the payload has been taken. Rolling back
    /// transitions the state but does not restore the payload. If the
    /// caller needs to re-queue the payloads, use [`push_many`](Self::push_many)
    /// instead, which creates fresh receipts with the payloads.
    pub fn rollback(&self, receipts: &[QueueReceipt]) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for r in receipts {
            if let Some(slot) = state.items.get_mut(r) {
                if slot.state == ReceiptState::Reserved {
                    slot.state = ReceiptState::Pending;
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
    pub fn remove(&self, receipt: &QueueReceipt) {
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            state.items.remove(receipt);
        }
        self.notify.notify_waiters();
    }

    // ── Read-only access ────────────────────────────────────────

    /// Iterate over all items in any state, applying a closure.
    ///
    /// Used by the driver to peek at pending items without draining
    /// them (e.g. `compute_gas_overbid` reads gas prices from
    /// queued cross-chain calls). The closure receives a reference to
    /// each payload that is still present (`Some`).
    pub fn for_each_pending<F>(&self, mut f: F)
    where
        F: FnMut(&T),
    {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for slot in state.items.values() {
            if slot.state == ReceiptState::Pending {
                if let Some(ref payload) = slot.payload {
                    f(payload);
                }
            }
        }
    }

    // ── Diagnostics ─────────────────────────────────────────────

    /// Total number of receipts in any state.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).items.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).items.is_empty()
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
        let r1 = q.push("entry-1".to_string());
        let r2 = q.push("entry-2".to_string());
        assert_eq!(q.len(), 2);

        // Driver drains both.
        let drained = q.drain_pending(10);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].0, r1);
        assert_eq!(drained[0].1, "entry-1");
        assert_eq!(drained[1].0, r2);
        assert_eq!(drained[1].1, "entry-2");

        // A second drain yields nothing (both are Reserved now).
        let drained2 = q.drain_pending(10);
        assert!(drained2.is_empty());

        // Driver confirms.
        let receipts: Vec<_> = drained.iter().map(|(r, _)| *r).collect();
        q.confirm(&receipts);

        // Composer waits — should return immediately since already confirmed.
        let permit1 = q.wait_confirmation(r1).await;
        assert!(permit1.is_ok());
        let permit2 = q.wait_confirmation(r2).await;
        assert!(permit2.is_ok());

        // Consume permits (suppress #[must_use]).
        let _p1 = permit1.unwrap();
        let _p2 = permit2.unwrap();

        // Cleanup.
        q.remove(&r1);
        q.remove(&r2);
        assert!(q.is_empty());
    }

    #[tokio::test]
    async fn rollback_returns_to_pending() {
        let q = EntryQueue::new();

        let r = q.push("rollback-me".to_string());

        // Drain -> Reserved.
        let drained = q.drain_pending(10);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, r);

        // Rollback state, but payload was already taken. Re-push fresh.
        // Remove old receipt, push new.
        q.remove(&r);
        let r2 = q.push("rollback-me".to_string());

        // Second drain picks up the re-pushed item.
        let drained2 = q.drain_pending(10);
        assert_eq!(drained2.len(), 1);
        assert_eq!(drained2[0].0, r2);

        // Now confirm it.
        q.confirm(&[r2]);
        let permit = q.wait_confirmation(r2).await;
        assert!(permit.is_ok());
        let _p = permit.unwrap();
    }

    #[tokio::test]
    async fn evicted_receipt_returns_error() {
        let q = EntryQueue::<String>::new();
        let r = q.push("evict-me".to_string());

        // Remove before confirmation.
        q.remove(&r);

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

        let _r1 = q.push("a".to_string());
        let _r2 = q.push("b".to_string());
        let _r3 = q.push("c".to_string());

        // Drain at most 2.
        let drained = q.drain_pending(2);
        assert_eq!(drained.len(), 2);

        // The third is still pending.
        let drained2 = q.drain_pending(10);
        assert_eq!(drained2.len(), 1);
        assert_eq!(drained2[0].1, "c");
    }

    #[tokio::test]
    async fn confirm_skips_non_reserved() {
        let q = EntryQueue::new();
        let r = q.push("skip-me".to_string());

        // Confirm while still Pending — should be a no-op.
        q.confirm(&[r]);

        // Still drainable (still Pending).
        let drained = q.drain_pending(10);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, r);
    }

    #[tokio::test]
    async fn rollback_skips_non_reserved() {
        let q = EntryQueue::new();
        let r = q.push("test".to_string());

        // Rollback while Pending — no-op.
        q.rollback(&[r]);

        // Drain, confirm, then rollback on Confirmed — no-op.
        let drained = q.drain_pending(10);
        let receipts: Vec<_> = drained.iter().map(|(r, _)| *r).collect();
        q.confirm(&receipts);
        q.rollback(&receipts);

        // Still confirmed.
        let permit = q.wait_confirmation(r).await;
        assert!(permit.is_ok());
        let _p = permit.unwrap();
    }

    #[tokio::test]
    async fn concurrent_wait_and_confirm() {
        let q = EntryQueue::new();
        let r = q.push("concurrent".to_string());

        // Drain to Reserved.
        let drained = q.drain_pending(10);
        let receipts: Vec<_> = drained.iter().map(|(r, _)| *r).collect();

        // Spawn a waiter that will park until confirmation.
        let q_clone = q.clone();
        let waiter = tokio::spawn(async move {
            q_clone.wait_confirmation(r).await
        });

        // Give the waiter a chance to park.
        tokio::task::yield_now().await;

        // Confirm — this should wake the waiter.
        q.confirm(&receipts);

        let result = waiter.await.expect("waiter task panicked");
        assert!(result.is_ok());
        let _p = result.unwrap();
    }

    #[tokio::test]
    async fn concurrent_wait_and_evict() {
        let q = EntryQueue::new();
        let r = q.push("evict-concurrent".to_string());

        // Spawn a waiter.
        let q_clone = q.clone();
        let waiter = tokio::spawn(async move {
            q_clone.wait_confirmation(r).await
        });

        // Give the waiter a chance to park.
        tokio::task::yield_now().await;

        // Remove — should wake the waiter with an error.
        q.remove(&r);

        let result = waiter.await.expect("waiter task panicked");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn receipts_are_monotonically_increasing() {
        let q = EntryQueue::new();
        let r1 = q.push("a".to_string());
        let r2 = q.push("b".to_string());
        let r3 = q.push("c".to_string());

        // QueueReceipt implements Ord; IDs should be strictly ordered.
        assert!(r1 < r2);
        assert!(r2 < r3);
    }

    #[tokio::test]
    async fn default_is_empty() {
        let q = EntryQueue::<String>::default();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn push_many_returns_ordered_receipts() {
        let q = EntryQueue::new();
        let items = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        let receipts = q.push_many(items);
        assert_eq!(receipts.len(), 3);
        assert!(receipts[0] < receipts[1]);
        assert!(receipts[1] < receipts[2]);
        assert_eq!(q.len(), 3);

        let drained = q.drain_pending(10);
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].1, "x");
        assert_eq!(drained[1].1, "y");
        assert_eq!(drained[2].1, "z");
    }

    #[tokio::test]
    async fn for_each_pending_reads_without_draining() {
        let q = EntryQueue::new();
        q.push(10u64);
        q.push(20u64);
        q.push(30u64);

        // Drain one to Reserved.
        let drained = q.drain_pending(1);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1, 10);

        // for_each_pending should only see the two still-Pending items.
        let mut seen = Vec::new();
        q.for_each_pending(|val| seen.push(*val));
        assert_eq!(seen, vec![20, 30]);
    }
}
