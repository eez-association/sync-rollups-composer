//! Driver-local types, constants, and small helper functions extracted
//! from `driver/mod.rs` in refactor step 2.1e.
//!
//! This module owns the standalone data types that the `impl Driver`
//! block uses:
//!
//! - [`DriverMode`] — Sync / Builder / Fullnode classifier.
//! - [`VerificationDecision`] — terminal path of `verify_local_block_matches_l1`.
//! - [`TriggerExecutionResult`] — `#[must_use]` outcome of trigger receipt checks.
//! - [`BuiltBlock`] — return shape of `build_and_insert_block`.
//! - [`L1ConfirmedAnchor`] — efficient rewind anchor.
//! - [`TxJournalEntry`] — persistent tx replay journal row.
//!
//! Plus helper functions ([`encode_block_transactions`], [`calc_gas_limit`],
//! [`compute_forkchoice_state`]) and all the driver-local tuning constants
//! (`FORK_CHOICE_DEPTH`, `CHECKPOINT_INTERVAL`, etc.).

use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::ForkchoiceState;
use reth_stages_types::StageId;
use std::collections::VecDeque;
use std::time::Duration;

/// The operating mode of the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverMode {
    /// Syncing from L1 events (catching up).
    Sync,
    /// Actively building blocks (caught up).
    Builder,
    /// Fullnode mode — sync only, never sequence.
    Fullnode,
}

/// Classification of `verify_local_block_matches_l1`'s terminal path for a
/// single derived block.
///
/// Each variant names the branch taken; all side effects (rewind target,
/// mode switch, hold transitions) are applied *before* the variant is
/// constructed — the variant is an explicit record consumed by the thin
/// `verify_local_block_matches_l1` wrapper that maps it to `Result<()>` for
/// callers. Fields carry informational payloads surfaced via the `Debug`
/// impl in the `trace!` decision log.
///
/// **Invariants closed by this enum:**
///
/// - **#9 — deferral exhaustion → rewind, not acceptance.** The
///   `MismatchDeferExhausted` variant is the only way to name the
///   exhausted-deferral outcome; the code path that constructs it calls
///   `rewind_to_re_derive` unconditionally before returning. No fallthrough
///   to an "accept" branch exists after `DeferralResult::MustRewind`.
/// - **#10 — rewind target is `entry_block - 1`.** Every terminal path that
///   sets a rewind target either delegates to `Driver::rewind_to_re_derive`
///   (hard rewind) or computes `saturating_sub(1)` inline at a single site
///   (soft L1-context rewind). The formula is not copy-pasted across the
///   file — it lives in exactly the two places that need it.
///
/// The in-progress deferral branch (`DeferralResult::Continue`) is
/// intentionally not represented: it returns `Err(...)` directly to trigger
/// outer-loop backoff and never produces a decision value.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "payload fields are read via the derived Debug impl in the trace! decision log"
)]
pub(super) enum VerificationDecision {
    /// Block matched L1 (state root and L1 context agree). Normal happy path.
    Match,
    /// Verification skipped because the block is below the immutable ceiling
    /// or is missing from local storage. Benign — caller proceeds.
    Skip,
    /// L1 context mismatch detected — soft rewind applied (no mode switch,
    /// no rewind-cycle increment). Caller proceeds with `Ok(())`.
    L1ContextMismatchRewound { target_l2: u64 },
    /// Gap-fill block (state_root == ZERO) matched its L1 context; if the
    /// hold was armed for this block it has been released. Caller proceeds.
    GapFillVerified,
    /// Entry-bearing block mismatch — deferral budget exhausted.
    /// Hard rewind to `rewind_target` has been applied; caller proceeds.
    MismatchDeferExhausted { rewind_target: u64 },
    /// Mismatch with no hold armed (permanent divergence).
    /// Rewind-to-re-derive has been applied and the caller propagates `Err`
    /// so the outer loop transitions to sync-mode backoff.
    MismatchPermanent { rewind_target: u64 },
}

/// Outcome of verifying L2→L1 trigger receipts after a postBatch lands on L1.
///
/// Produced by `Driver::verify_trigger_receipts` and consumed exactly once by
/// `flush_to_l1`. The `#[must_use]` attribute is the compile-time enforcement
/// for **invariant #15** (withdrawal trigger revert on L1 causes REWIND, not
/// a silent log): with `clippy::must_use_candidate` / `-D warnings` any caller
/// that drops this value without matching on it produces a build error.
///
/// The `Reverted` variant carries the rewind-target hint so callers don't
/// recompute it; the helper method that produces it does not touch driver
/// state beyond querying receipts, so the caller retains control of when
/// the actual rewind fires.
#[derive(Debug, Clone)]
#[must_use = "invariant #15: trigger receipt outcome must be consumed — a reverted \
              trigger MUST cause a rewind, never a silent log"]
#[allow(
    dead_code,
    reason = "payload fields are surfaced via the derived Debug impl in log statements"
)]
pub(super) enum TriggerExecutionResult {
    /// All triggers landed with a successful receipt (status=1).
    AllConfirmed { count: usize },
    /// At least one trigger reverted on L1. The caller MUST initiate a rewind
    /// so the entry-bearing block is re-derived with §4f filtering.
    Reverted { reverted_count: usize, total: usize },
}

/// Result of building and inserting a block via the engine API.
pub struct BuiltBlock {
    /// The block hash.
    pub hash: B256,
    /// The parent's state root (pre-execution).
    pub pre_state_root: B256,
    /// The state root of the block (post-execution).
    pub state_root: B256,
    /// Number of transactions in the block.
    pub tx_count: usize,
    /// RLP-encoded transactions for L1 submission.
    pub encoded_transactions: Bytes,
}

/// Last L1-confirmed batch anchor — used for efficient rollback instead of genesis.
#[derive(Debug, Clone, Copy)]
pub(super) struct L1ConfirmedAnchor {
    pub(super) l2_block_number: u64,
    pub(super) l1_block_number: u64,
}

/// Stage ID for the persistent transaction replay journal.
/// Stores user transaction bytes for recovery after rewinds and crashes.
pub(super) const TX_JOURNAL_STAGE_ID: StageId = StageId::Other("TxJournal");

/// A single entry in the persistent transaction replay journal.
///
/// Stores the L2 block number and the full RLP-encoded transaction list for
/// that block. Written at block build time, pruned after L1 confirmation.
/// Used to recover user transactions after crashes (startup recovery).
#[derive(Clone)]
pub(super) struct TxJournalEntry {
    pub(super) l2_block_number: u64,
    /// Full encoded_transactions bytes (RLP-encoded list, includes protocol txs).
    /// Protocol txs are filtered out on recovery.
    pub(super) block_txs: Vec<u8>,
}

impl TxJournalEntry {
    /// Serialize a list of journal entries to bytes.
    pub(super) fn encode_all(entries: &[TxJournalEntry]) -> Vec<u8> {
        let mut buf = Vec::new();
        for entry in entries {
            buf.extend_from_slice(&entry.l2_block_number.to_le_bytes());
            let len = entry.block_txs.len() as u32;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&entry.block_txs);
        }
        buf
    }

    /// Deserialize a list of journal entries from bytes.
    pub(super) fn decode_all(data: &[u8]) -> Vec<TxJournalEntry> {
        let mut entries = Vec::new();
        let mut pos = 0;
        while pos + 12 <= data.len() {
            let block_bytes: [u8; 8] = match data[pos..pos + 8].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let l2_block_number = u64::from_le_bytes(block_bytes);
            let len_bytes: [u8; 4] = match data[pos + 8..pos + 12].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let tx_len = u32::from_le_bytes(len_bytes) as usize;
            pos += 12;
            if pos + tx_len > data.len() {
                break;
            }
            let block_txs = data[pos..pos + tx_len].to_vec();
            pos += tx_len;
            entries.push(TxJournalEntry {
                l2_block_number,
                block_txs,
            });
        }
        entries
    }
}

/// RLP-encode a slice of transactions into a single bytes blob for L1 submission.
pub(super) fn encode_block_transactions(
    txs: &[reth_ethereum_primitives::TransactionSigned],
) -> Bytes {
    let mut buf = Vec::new();
    alloy_rlp::encode_list(txs, &mut buf);
    Bytes::from(buf)
}

/// Number of recent block hashes to keep for safe/finalized tracking.
pub(super) const FORK_CHOICE_DEPTH: usize = 64;

/// Save L1 derivation checkpoint to DB every N L1 blocks during sync.
pub(super) const CHECKPOINT_INTERVAL: u64 = 64;

/// Maximum backoff duration on repeated errors (seconds).
pub(super) const MAX_BACKOFF_SECS: u64 = 60;

/// Cooldown after a failed L1 submission before retrying (seconds).
pub(super) const SUBMISSION_COOLDOWN_SECS: u64 = 5;

/// Maximum number of blocks to submit in a single L1 batch transaction.
pub(super) const MAX_BATCH_SIZE: usize = 100;

/// Maximum pending submissions queue size. Prevents unbounded memory growth
/// when L1 transactions are not confirming (e.g., gas too low, stuck nonce).
pub(super) const MAX_PENDING_SUBMISSIONS: usize = 1000;

/// Maximum pending cross-chain entries queue size. Prevents unbounded memory
/// growth when L1 cross-chain submissions are failing or slow.
pub(super) const MAX_PENDING_CROSS_CHAIN_ENTRIES: usize = 1000;

/// Number of consecutive L1 RPC failures before switching to the fallback provider.
pub(super) const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Minimum interval between L1 RPC calls (rate limiting during catchup).
pub(super) const MIN_L1_CALL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum retries when engine returns SYNCING for a fork choice update.
/// Total worst-case wait: 100+200+400+800+1600+3200 = ~6.3s.
pub(super) const FCU_SYNCING_MAX_RETRIES: u32 = 6;

/// Initial backoff for SYNCING retries (doubles each attempt).
pub(super) const FCU_SYNCING_INITIAL_BACKOFF_MS: u64 = 100;

/// Desired gas limit target for block building. Set to 60M to match Ethereum
/// mainnet's current gas limit. Must match the payload builder's default.
pub(super) const DESIRED_GAS_LIMIT: u64 = 60_000_000;

/// Compute the gas limit for the next block, bounded by the EIP-1559 elasticity divisor (1024).
/// Mirrors `alloy_eips::eip1559::helpers::calculate_block_gas_limit` exactly — verified by
/// `test_calc_gas_limit_matches_reth`.
///
/// NOTE: The `saturating_sub(1)` is intentional and matches both alloy's canonical implementation
/// and go-ethereum's `core/block_validator.go`. This means: at parent_gas_limit <= 1024 the delta
/// is 0, effectively locking the gas limit (acceptable since real chains never have limits that low).
pub(super) fn calc_gas_limit(parent_gas_limit: u64, desired_gas_limit: u64) -> u64 {
    let delta = (parent_gas_limit / 1024).saturating_sub(1);
    let min_limit = parent_gas_limit.saturating_sub(delta);
    let max_limit = parent_gas_limit.saturating_add(delta);
    desired_gas_limit.clamp(min_limit, max_limit)
}

/// Compute the fork choice state from a head hash and a deque of recent block hashes.
///
/// - `head`: the latest block hash
/// - `safe`: 32 blocks behind head (or oldest tracked, or head if empty)
/// - `finalized`: the oldest tracked hash (or head if empty)
pub(super) fn compute_forkchoice_state(
    head_hash: B256,
    block_hashes: &VecDeque<B256>,
) -> ForkchoiceState {
    let safe = block_hashes
        .get(block_hashes.len().saturating_sub(32))
        .copied()
        .unwrap_or(head_hash);
    let finalized = block_hashes.front().copied().unwrap_or(head_hash);

    ForkchoiceState {
        head_block_hash: head_hash,
        safe_block_hash: safe,
        finalized_block_hash: finalized,
    }
}

