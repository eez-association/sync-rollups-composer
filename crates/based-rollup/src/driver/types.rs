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

use super::hold::EntryVerificationHold;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::{
    ExecutionData, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadStatus,
};
use eyre::{Result, WrapErr};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_payload_primitives::EngineApiMessageVersion;
use reth_stages_types::StageId;
use std::collections::VecDeque;
use std::time::Duration;
use tracing::{error, warn};

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
    /// §4f-flagged mismatch (issue #36): derivation carried a
    /// `DeferredFiltering` marker, so the divergence is the speculative /
    /// clean-root divergence that the deferral loop cannot resolve. The
    /// sibling-reorg plan has been applied inline (`apply_sibling_reorg_plan`)
    /// — `pending_sibling_reorg` + `pending_rewind_target` are set and the
    /// driver has switched to Sync mode. Caller proceeds with `Ok(())`.
    SiblingReorgQueued {
        target_l2_block: u64,
        expected_root: B256,
    },
    /// §4f-flagged mismatch at a block while `pending_sibling_reorg` was
    /// already queued (by Fix 1 / Fix 2 / verify fast-path on a prior tick).
    /// The current divergence is expected to be resolved by the queued reorg
    /// on a subsequent tick (via `flush_precheck` dispatch or `step_sync`'s
    /// `rebuild_block_as_sibling`). The verify path intentionally does NOT
    /// clear internal state (which would wipe the queued request) and does
    /// NOT set `pending_rewind_target` (which would trigger bare FCU rewind).
    /// The caller returns `Err` so the main loop's backoff machinery engages
    /// while the queued reorg completes.
    ///
    /// Mirror telemetry only — the handler returns `Err` directly; this
    /// variant exists so tests and the decision-log `trace!` can name the
    /// branch taken.
    SiblingReorgAlreadyQueued {
        target_l2_block: u64,
        queued_target: Option<u64>,
    },
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
///
/// ## Visibility (issue #36 second-pass review)
///
/// Fields are `pub(crate)` so the sibling-reorg planner
/// ([`plan_sibling_reorg_from_verify`]) can accept it as a parameter from
/// outside this module.
#[derive(Debug, Clone, Copy)]
pub(crate) struct L1ConfirmedAnchor {
    pub(crate) l2_block_number: u64,
    pub(crate) l1_block_number: u64,
}

// ──────────────────────────────────────────────────────────────────────────
// Sibling-reorg recovery (issue #36)
// ──────────────────────────────────────────────────────────────────────────

/// Maximum depth that reth's in-memory changeset cache can unwind. After this
/// many blocks are committed past a divergence point, the execution layer has
/// permanently evicted the historical state needed to rebuild via sibling
/// reorg. Matches reth's `CHANGESET_CACHE_RETENTION_BLOCKS`.
///
/// Consumed by the safety gate in `step_builder`.
pub(crate) const MAX_REORG_DEPTH: u64 = 64;

/// Safety threshold: halt building / pause derivation if the unresolved
/// divergence depth reaches this value. Chosen as ~75% of `MAX_REORG_DEPTH`
/// so we always have headroom to recover via sibling reorg before reth's
/// eviction window closes.
pub(crate) const REORG_SAFETY_THRESHOLD: u64 = 48;

/// Pending sibling-reorg request (issue #36).
///
/// Set by `flush_to_l1` when it detects `pre_state_root != on_chain_root` at a
/// block whose §4f-filtered root (`clean_state_root`) already matches on-chain.
/// Consumed by the step_sync / step_builder derivation loop when block
/// `target_l2_block` re-derives: if the derived state root equals
/// `expected_root`, the driver calls `rebuild_block_as_sibling` instead of
/// `build_and_insert_block` (which would fail with `expected sequential block`
/// because the target is already canonical in reth).
///
/// ## `depth` removed (issue #36 second-pass review)
///
/// A prior design carried a `depth` field initialised from
/// `consecutive_rewind_cycles`. It was never consulted — `step_builder`
/// computes the safety-gate depth from `l2_head_number - target_l2_block`,
/// which is the single source of truth. Removed to reduce API surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SiblingReorgRequest {
    /// L2 block number to rebuild as sibling.
    pub(crate) target_l2_block: u64,
    /// State root the rebuilt block is expected to produce (equal to the
    /// §4f-filtered root observed on L1).
    pub(crate) expected_root: B256,
}

/// Decision returned by [`decide_divergence_recovery`] when `flush_to_l1`
/// observes `first.pre_state_root != on_chain_root`.
///
/// The driver uses the returned variant to dispatch between:
/// - `SiblingReorg`: build N' with the §4f-filtered tx set and submit via
///   `newPayloadV3 + forkchoiceUpdatedV3(head=N')`. This is reth's own
///   first-class reorg path (exercised by reth's `test_testsuite_deep_reorg`).
/// - `BareRewind`: fall back to `rewind_l2_chain` (FCU-to-ancestor). Used
///   when we have no evidence of §4f filtering (no `clean_state_root` match).
///   Known to be a silent no-op on committed blocks; defense-in-depth only.
/// - `Halt`: the unresolved depth exceeds `REORG_SAFETY_THRESHOLD`. Continuing
///   would carry us past reth's `MAX_REORG_DEPTH` eviction window, past which
///   recovery is impossible. The driver must halt and surface a structured
///   error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SiblingReorgDecision {
    /// Attempt sibling reorg. The driver builds a sibling block at
    /// `target_block` with a parent matching `target_block - 1`'s hash and
    /// submits via the engine API. The expected post-execution root is
    /// `filtered_root` (equal to `on_chain_root`).
    SiblingReorg {
        target_block: u64,
        filtered_root: B256,
    },
    /// No evidence of §4f filtering — fall back to bare FCU rewind. (Typically
    /// a no-op on committed blocks; present for defense-in-depth.)
    BareRewind,
    /// Safety gate tripped. Halt instead of attempting recovery.
    Halt,
}

/// Pure-logic decision for how to recover when `flush_to_l1` observes
/// `first.pre_state_root != on_chain_root`.
///
/// Separated from `flush_to_l1` so the dispatch is unit-testable without an
/// engine mock. The function inspects the divergent block and on-chain root
/// alone — no I/O, no mutation.
pub(crate) fn decide_divergence_recovery(
    divergent: &crate::proposer::PendingBlock,
    on_chain_root: B256,
    reorg_depth: u64,
    safety_threshold: u64,
) -> SiblingReorgDecision {
    // Safety gate FIRST: if we've already walked this far without resolving,
    // deeper recovery attempts would eventually cross reth's eviction window.
    if reorg_depth_exceeded(reorg_depth, safety_threshold) {
        return SiblingReorgDecision::Halt;
    }

    // Evidence of §4f filtering: the block's `clean_state_root` (computed by
    // the builder as "state without any cross-chain entry txs") matches what
    // L1 confirmed. If so, the §4f-filtered tx set (strip unconsumed protocol
    // txs) reproduces the on-chain root — we can rebuild N' deterministically.
    //
    // Require `clean_state_root != state_root` to ensure a real choice exists
    // (for plain blocks with no cross-chain entries, the two are equal and
    // sibling reorg would be identical to the current block — pointless).
    let clean_root = divergent.clean_state_root.as_b256();
    if clean_root == on_chain_root && clean_root != divergent.state_root {
        return SiblingReorgDecision::SiblingReorg {
            target_block: divergent.l2_block_number,
            filtered_root: clean_root,
        };
    }

    SiblingReorgDecision::BareRewind
}

/// Pure-logic safety-gate predicate. Returns `true` when the accumulated
/// unresolved divergence depth has reached the halt threshold.
///
/// The halt must fire STRICTLY BEFORE `MAX_REORG_DEPTH`; crossing that
/// boundary means reth has evicted the state needed to rebuild.
pub(crate) fn reorg_depth_exceeded(depth: u64, threshold: u64) -> bool {
    depth >= threshold
}

/// Pure detection: scan the first `window_len` entries of `pending` looking
/// for a block that triggers a sibling-reorg recovery against `on_chain_root`.
///
/// ## Direction (M4 — issue #36 second-pass review)
///
/// Iterates in REVERSE. `flush_to_l1` already selected the rightmost match via
/// `rposition` (the block whose `state_root / clean_state_root /
/// intermediate_roots` contains `on_chain_root`). The caller passes
/// `window_len = pos + 1`; we scan `pending[0..window_len]` from the back, so
/// the block at `pos` — the one `rposition` found — is tried first. An earlier
/// block that coincidentally has `clean_state_root == on_chain_root` would
/// otherwise hijack the decision.
///
/// Returns the first block's `SiblingReorgRequest` (rightmost within the
/// window) when `decide_divergence_recovery` returns `SiblingReorg`.
pub(crate) fn find_rightmost_sibling_reorg_target(
    pending: &VecDeque<crate::proposer::PendingBlock>,
    on_chain_root: B256,
    reorg_depth: u64,
    safety_threshold: u64,
    window_len: usize,
) -> Option<SiblingReorgRequest> {
    let cap = window_len.min(pending.len());
    for b in pending.iter().take(cap).rev() {
        match decide_divergence_recovery(b, on_chain_root, reorg_depth, safety_threshold) {
            SiblingReorgDecision::SiblingReorg {
                target_block,
                filtered_root,
            } => {
                return Some(SiblingReorgRequest {
                    target_l2_block: target_block,
                    expected_root: filtered_root,
                });
            }
            SiblingReorgDecision::BareRewind | SiblingReorgDecision::Halt => continue,
        }
    }
    None
}

/// The full state transition that the sibling-reorg fast path in
/// `verify_local_block_matches_l1` must apply when it detects a §4f-filtered
/// divergence at an entry-bearing block.
///
/// Kept as a pure data struct so tests can assert field-by-field exactly what
/// the driver is expected to do — in particular that `pending_rewind_target`
/// is set alongside `pending_sibling_reorg` (C1 regression).
///
/// Produced by [`plan_sibling_reorg_from_verify`]; consumed by
/// `Driver::apply_sibling_reorg_plan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SiblingReorgVerifyPlan {
    /// The sibling-reorg request to queue. Target = divergent block; expected
    /// root = L1-derived state root.
    pub(crate) request: SiblingReorgRequest,
    /// L2 block to rewind to (block before the divergent entry block).
    pub(crate) rewind_target_l2: u64,
    /// L1 block to roll derivation back to. Either
    /// `anchor.l1_block_number - 1` (when an L1 anchor exists) or
    /// `deployment_l1_block` (cold start).
    pub(crate) rollback_l1_block: u64,
}

/// Pure planner for the sibling-reorg fast path in
/// `verify_local_block_matches_l1`.
///
/// Separating this computation from the driver's `&mut self` state mutation
/// lets us unit-test that the fast path sets ALL of `pending_sibling_reorg`,
/// `pending_rewind_target`, and the L1 rollback cursor — without spinning up
/// a real reth driver.
///
/// Parameters mirror the driver's internal knobs: `entry_block` is the L2
/// block number where derivation detected the divergence; `expected_root` is
/// the L1-derived root that the re-built sibling must produce; `anchor` is the
/// last L1-confirmed batch anchor (if any); `deployment_l1_block` is the L1
/// block at which the rollup was deployed (used when there's no anchor yet).
pub(crate) fn plan_sibling_reorg_from_verify(
    entry_block: u64,
    expected_root: B256,
    anchor: Option<L1ConfirmedAnchor>,
    deployment_l1_block: u64,
) -> SiblingReorgVerifyPlan {
    let (rewind_target_l2, rollback_l1_block) = if let Some(anchor) = anchor {
        (
            entry_block.saturating_sub(1),
            anchor.l1_block_number.saturating_sub(1),
        )
    } else {
        (0u64, deployment_l1_block)
    };
    SiblingReorgVerifyPlan {
        request: SiblingReorgRequest {
            target_l2_block: entry_block,
            expected_root,
        },
        rewind_target_l2,
        rollback_l1_block,
    }
}

/// Narrow abstraction over the Engine API calls used during a sibling reorg.
///
/// Exists purely so the sibling-reorg submission path can be unit-tested
/// against a mock. The real implementation delegates to
/// `reth_engine_primitives::ConsensusEngineHandle<EthEngineTypes>`; the mock
/// (in `driver_tests.rs`) records calls in order and returns scripted
/// responses. The trait surface is intentionally minimal — only the two
/// methods actually used by `submit_sibling_payload`.
///
/// ## `Send`-ness (issue #36 third-pass review)
///
/// The `#[allow(async_fn_in_trait)]` attribute suppresses the lint that would
/// otherwise warn about async fns in traits being `!Send` by default. The
/// returned futures are therefore implicitly `!Send`, which is acceptable
/// because `Driver` is driven single-task — the engine is only ever awaited
/// from the driver's own `step()` loop, never across tasks. The real impl on
/// `ConsensusEngineHandle<EthEngineTypes>` DOES produce `Send` futures
/// internally, but the trait method signatures don't expose that.
///
/// If a future refactor spawns the driver across tasks (e.g. a work-stealing
/// executor with `Send` bounds), switch to `trait_variant::make(Send)` and
/// re-verify the real impl still compiles. As of this writing, migrating
/// doesn't buy anything and would require a new dependency.
#[allow(async_fn_in_trait)]
pub(crate) trait EngineClient {
    /// Submit a new payload to the engine. Mirrors
    /// `ConsensusEngineHandle::new_payload`.
    async fn new_payload(&self, payload: ExecutionData) -> Result<PayloadStatus>;

    /// Submit a forkchoice update to the engine. Mirrors
    /// `ConsensusEngineHandle::fork_choice_updated`.
    async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<PayloadAttributes>,
        version: EngineApiMessageVersion,
    ) -> Result<ForkchoiceUpdated>;
}

impl EngineClient for ConsensusEngineHandle<EthEngineTypes> {
    async fn new_payload(&self, payload: ExecutionData) -> Result<PayloadStatus> {
        ConsensusEngineHandle::new_payload(self, payload)
            .await
            .map_err(|e| eyre::eyre!("engine new_payload failed: {e}"))
    }

    async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<PayloadAttributes>,
        version: EngineApiMessageVersion,
    ) -> Result<ForkchoiceUpdated> {
        ConsensusEngineHandle::fork_choice_updated(self, state, payload_attrs, version)
            .await
            .map_err(|e| eyre::eyre!("engine fork_choice_updated failed: {e}"))
    }
}

/// Outcome of `submit_sibling_payload` — reports what the engine did so the
/// caller (the sibling-reorg orchestrator) can mutate driver state precisely.
///
/// Kept as its own struct so the submission helper is a pure function over the
/// engine, with no driver field access. Tests exercise it directly against a
/// mock engine.
#[derive(Debug, Clone)]
pub(crate) struct SiblingSubmitOutcome {
    /// Final forkchoice-state deque after the sibling reorg (head at back).
    pub(crate) new_hashes: VecDeque<B256>,
}

/// Submit a pre-built sibling payload to the engine and drive the fork choice
/// to the new head. Pure over the engine — no driver state mutation.
///
/// Failure modes (all mapped to `Err`):
/// - `new_payload` returns INVALID/SYNCING → bail (INVALID is a genuine build
///   defect; SYNCING shouldn't happen for a payload we just built against the
///   current canonical parent, but we treat it as a failure to be safe).
/// - `fork_choice_updated` returns INVALID → bail.
/// - `fork_choice_updated` returns SYNCING past retry budget → bail (the
///   `fork_choice_updated_with_retry` equivalent is inlined here so the
///   backoff schedule is visible to tests).
pub(crate) async fn submit_sibling_payload<E: EngineClient>(
    engine: &E,
    execution_data: ExecutionData,
    sibling_hash: B256,
    existing_parent_hashes: &VecDeque<B256>,
) -> Result<SiblingSubmitOutcome> {
    let status = engine
        .new_payload(execution_data)
        .await
        .wrap_err("sibling submit: engine_newPayload call failed")?;

    if !status.is_valid() {
        eyre::bail!(
            "sibling submit: newPayload rejected sibling_hash={sibling_hash} status={status:?}"
        );
    }

    // Build the forkchoice state. Caller pre-populates `existing_parent_hashes`
    // with hashes up to and including `target - 1`; we append the sibling hash
    // and cap the deque at `FORK_CHOICE_DEPTH`.
    let mut new_hashes = existing_parent_hashes.clone();
    new_hashes.push_back(sibling_hash);
    if new_hashes.len() > FORK_CHOICE_DEPTH {
        new_hashes.pop_front();
    }

    let fcs = compute_forkchoice_state(sibling_hash, &new_hashes);
    let fcu = submit_fork_choice_with_retry(engine, fcs, None)
        .await
        .wrap_err("sibling submit: forkchoiceUpdated failed")?;

    if fcu.is_invalid() {
        eyre::bail!(
            "sibling submit: forkchoiceUpdated rejected sibling_hash={sibling_hash} status={:?}",
            fcu.payload_status
        );
    }

    Ok(SiblingSubmitOutcome { new_hashes })
}

/// Engine-agnostic `fork_choice_updated` with the same SYNCING retry schedule
/// as the driver's internal `fork_choice_updated_with_retry`. Kept separate so
/// it can be reused by both the real engine path and the mock-engine tests.
pub(crate) async fn submit_fork_choice_with_retry<E: EngineClient>(
    engine: &E,
    state: ForkchoiceState,
    payload_attrs: Option<PayloadAttributes>,
) -> Result<ForkchoiceUpdated> {
    let mut backoff_ms = FCU_SYNCING_INITIAL_BACKOFF_MS;
    for attempt in 0..FCU_SYNCING_MAX_RETRIES {
        let fcu = engine
            .fork_choice_updated(
                state,
                payload_attrs.clone(),
                EngineApiMessageVersion::default(),
            )
            .await
            .wrap_err("fork choice update failed")?;

        if fcu.is_valid() || fcu.is_invalid() {
            return Ok(fcu);
        }

        if attempt + 1 < FCU_SYNCING_MAX_RETRIES {
            warn!(
                target: "based_rollup::driver",
                attempt = attempt + 1,
                max_retries = FCU_SYNCING_MAX_RETRIES,
                backoff_ms,
                "FCU returned SYNCING, retrying"
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms *= 2;
        }
    }

    eyre::bail!(
        "engine stuck in SYNCING after {} retries",
        FCU_SYNCING_MAX_RETRIES
    )
}

/// C2 guard (issue #36 second-pass review): assert that a freshly-rebuilt
/// sibling's `state_root` matches the `expected_root` the driver promised L1.
///
/// Invoked from `rebuild_block_as_sibling` BEFORE any engine call is made. If
/// the mismatch fires we bail without touching the engine or mutating driver
/// state, and the sibling-reorg request stays in place for retry.
///
/// Extracted so tests can exercise the gate directly — reverting the assertion
/// in production means `rebuild_block_as_sibling` no longer calls this
/// function, and the C2 regression test (which invokes the production submit
/// helper via a mock engine and a synthetic built block with wrong root) sees
/// a successful engine submission instead of a bail.
pub(crate) fn check_sibling_state_root_matches(
    built_root: B256,
    expected_root: B256,
    target: u64,
) -> Result<()> {
    if built_root != expected_root {
        error!(
            target: "based_rollup::driver",
            %built_root,
            %expected_root,
            target_block = target,
            "sibling rebuild produced wrong state root — filter defect, aborting"
        );
        eyre::bail!(
            "sibling rebuild: state root mismatch — built={built_root} expected={expected_root} (filter defect at block {target})"
        );
    }
    Ok(())
}

/// Run the C2 guard and, only on success, submit the pre-built sibling payload
/// to the engine. Pure over the engine — no driver state mutation.
///
/// Mirrors the exact order of operations in `Driver::rebuild_block_as_sibling`
/// around the C2 guard: guard-check, then submit. Exists so a test can assert
/// the "no engine call on guard failure" property against a mock
/// `EngineClient` without instantiating a real driver.
///
/// Kept behind `#[cfg(any(test, feature = "test-utils"))]` so it isn't part of
/// the production binary surface.
#[cfg(any(test, feature = "test-utils"))]
#[allow(dead_code)]
pub(crate) async fn submit_sibling_after_guard<E: EngineClient>(
    engine: &E,
    execution_data: ExecutionData,
    sibling_hash: B256,
    built_root: B256,
    expected_root: B256,
    target: u64,
    existing_parent_hashes: &VecDeque<B256>,
) -> Result<SiblingSubmitOutcome> {
    check_sibling_state_root_matches(built_root, expected_root, target)?;
    submit_sibling_payload(engine, execution_data, sibling_hash, existing_parent_hashes).await
}

/// Outcome classifier for the mismatch branch of
/// `verify_local_block_matches_l1`.
///
/// The branching logic in that method decides — purely from local state — what
/// recovery action to take when the locally-built header's state root does not
/// match the L1-derived root. Extracting this classification step into a free
/// function lets tests exercise the exact boolean gate that was the C1
/// regression: `filtering.is_some() && !sibling_reorg_already_queued`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifyMismatchAction {
    /// §4f-flagged divergence → queue a sibling reorg via
    /// `plan_sibling_reorg_from_verify` + `apply_sibling_reorg_plan`. No
    /// deferral. This is the fast path (issue #36).
    FastPathSiblingReorg,
    /// A sibling-reorg request is already queued (by Fix 1 / Fix 2 / verify
    /// fast-path on a prior tick). The current mismatch is expected to be
    /// resolved by that queued reorg — do NOT call `clear_internal_state`
    /// (which would wipe the queued request), do NOT set
    /// `pending_rewind_target` (which would trigger bare FCU rewind on the
    /// next tick). Return Err so the main loop's backoff machinery engages,
    /// and let the queued reorg complete on a subsequent tick via
    /// `flush_precheck` dispatch or `step_sync`'s `rebuild_block_as_sibling`.
    NoOpPendingSiblingReorg,
    /// Entry-bearing block with a pending hold — defer verification one more
    /// time so L1 has a chance to mine the consumption event.
    DeferEntryVerify,
    /// Entry-bearing block but deferrals are exhausted — rewind to
    /// `entry_block - 1` so the block itself is re-derived with §4f filtering.
    ExhaustedDeferralRewind,
    /// Non-filtering, non-entry divergence — generic rewind.
    GenericMismatchRewind,
}

/// Classify the action the verify mismatch branch should take. Pure function
/// over the five inputs that gate the dispatch.
///
/// - `filtering_present`: `derived.filtering.is_some()` — derivation flagged
///   this block as needing §4f filtering (unconsumed entries detected on L1).
/// - `sibling_reorg_already_queued`: `self.pending_sibling_reorg.is_some()`.
/// - `is_pending_entry_block`: the hold is armed for `derived.l2_block_number`.
/// - `deferrals_before_increment`: current value of
///   `self.hold.deferrals()` BEFORE this call bumps it.
/// - `max_deferrals`: the `MAX_ENTRY_VERIFY_DEFERRALS` constant (parameterized
///   so the test doesn't hard-code it).
///
/// Note: this function does not mutate any state. The caller is responsible
/// for the actual state transition (apply plan, defer hold, etc.).
pub(crate) fn classify_verify_mismatch(
    filtering_present: bool,
    sibling_reorg_already_queued: bool,
    is_pending_entry_block: bool,
    deferrals_before_increment: u32,
    max_deferrals: u32,
) -> VerifyMismatchAction {
    // Fast path: the two-pass C1 gate. Both conditions must be true.
    if filtering_present && !sibling_reorg_already_queued {
        return VerifyMismatchAction::FastPathSiblingReorg;
    }
    // §4f-shaped divergence at a block whose queued sibling reorg is about to
    // fix it (production-critical bug from PR #39 soak — 55% of Fix 1
    // recoveries silently fell through to bare FCU rewind because the verify
    // path wiped `pending_sibling_reorg` via `clear_internal_state` and armed
    // `pending_rewind_target` for a bare rewind on the next tick). Returning
    // this variant tells the handler to do NOTHING to driver state and just
    // return `Err` so backoff engages while the queued reorg completes.
    //
    // Must come BEFORE the entry-block check: a sibling reorg can legitimately
    // be queued for an entry-bearing block (deposits/withdrawals divergence),
    // and in that case we still want the queued reorg to win over the
    // deferral/rewind paths.
    if filtering_present && sibling_reorg_already_queued {
        return VerifyMismatchAction::NoOpPendingSiblingReorg;
    }
    // Entry-bearing block with pending verification: defer or rewind depending
    // on how many deferrals we've already spent. The `+ 1` simulates the
    // caller's increment (the caller bumps BEFORE comparing).
    if is_pending_entry_block {
        if deferrals_before_increment + 1 < max_deferrals {
            return VerifyMismatchAction::DeferEntryVerify;
        }
        return VerifyMismatchAction::ExhaustedDeferralRewind;
    }
    // Plain mismatch — generic rewind branch at the end of the mismatch block.
    VerifyMismatchAction::GenericMismatchRewind
}

/// Clear the recovery-state fields that `clear_internal_state` wipes.
///
/// Extracted so:
/// - (M2) the `pending_sibling_reorg = None` line is visible as a named
///   contract that production calls and tests assert.
/// - a regression that removes the field clear from `clear_internal_state`
///   causes the helper-level test to fail (if the helper is still called
///   from production) OR causes the production method to stop using the
///   helper (which the wire-through test would notice).
pub(crate) fn clear_recovery_state(
    pending_sibling_reorg: &mut Option<SiblingReorgRequest>,
    hold: &mut EntryVerificationHold,
) {
    *pending_sibling_reorg = None;
    hold.clear();
}

/// Subset of `Driver` recovery-related fields that
/// [`apply_sibling_reorg_plan_fields`] mutates. Exists so a test can assert
/// the full mutation set without constructing a `Driver` (which requires
/// generic P, Pool and a real `ConsensusEngineHandle`).
///
/// Uses [`EntryVerificationHold`] to unify the formerly-separate
/// `pending_entry_verification_block` and `entry_verify_deferrals` fields
/// (the hold state machine supersedes both; see `driver/hold.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DriverRecoveryFields {
    pub(crate) pending_sibling_reorg: Option<SiblingReorgRequest>,
    pub(crate) pending_rewind_target: Option<u64>,
    pub(crate) hold: EntryVerificationHold,
    pub(crate) mode: DriverMode,
}

/// Apply the state transition that `Driver::apply_sibling_reorg_plan` performs
/// on a [`DriverRecoveryFields`] snapshot plus a mutable
/// [`crate::derivation::DerivationPipeline`]. Pure over inputs — tests exercise
/// it directly.
///
/// Fields mutated (also documented on `Driver::apply_sibling_reorg_plan`):
/// 1. `pending_sibling_reorg = Some(saved_req)` — M2 reinstate after the
///    preceding `clear_internal_state` in production.
/// 2. `pending_rewind_target` narrowed to `min(existing, plan.rewind_target_l2)`
///    via the same semantics as `Driver::set_rewind_target` — C1 regression.
/// 3. `mode = Sync`.
/// 4. `hold.clear()` — the entry-verification hold is released.
/// 5. `derivation.set_last_derived_l2_block(plan.rewind_target_l2)` +
///    `derivation.rollback_to(plan.rollback_l1_block)`.
///
/// INTENTIONALLY NOT mutated: `consecutive_rewind_cycles` (sibling reorg is a
/// productive first-time recovery, not a rewind cycle).
///
/// `synced` is not mutated here because it's an `Arc<AtomicBool>` on the
/// driver proper; the production caller handles it after this call returns.
pub(crate) fn apply_sibling_reorg_plan_fields(
    fields: &mut DriverRecoveryFields,
    saved_req: SiblingReorgRequest,
    plan: SiblingReorgVerifyPlan,
    derivation: &mut crate::derivation::DerivationPipeline,
) {
    // M2 reinstate — the caller is responsible for having cleared this before
    // invoking the helper (see `Driver::apply_sibling_reorg_plan`).
    fields.pending_sibling_reorg = Some(saved_req);
    // Rollback derivation: advance last_derived_l2_block back to the rewind
    // target and roll the L1 cursor back so re-derivation picks up the block
    // again with §4f filtering applied.
    derivation.set_last_derived_l2_block(plan.rewind_target_l2);
    derivation.rollback_to(plan.rollback_l1_block);
    fields.mode = DriverMode::Sync;
    // C1: wire the rewind target. Same semantics as `Driver::set_rewind_target`
    // (takes the min with any existing target) so multiple pending mismatches
    // collapse to the earliest one.
    fields.pending_rewind_target = Some(
        fields
            .pending_rewind_target
            .map_or(plan.rewind_target_l2, |t| t.min(plan.rewind_target_l2)),
    );
    // `clear_internal_state` already clears the hold, but we restate it here
    // so the invariant is visible AND so the helper is safe to call without a
    // preceding `clear_internal_state` (e.g. in the unit test).
    fields.hold.clear();
}

/// Clear the fields that the `step_sync` success branch zeros after a
/// sibling-reorg rebuild succeeds.
///
/// Mirrors the verify fast-path semantics: the divergent block may have been
/// entry-bearing, so the hold must be released on successful consumption or
/// `step_builder` returns early forever.
pub(crate) fn clear_fields_on_sibling_reorg_success(
    pending_sibling_reorg: &mut Option<SiblingReorgRequest>,
    consecutive_rewind_cycles: &mut u32,
    consecutive_flush_mismatches: &mut u32,
    hold: &mut EntryVerificationHold,
) {
    *pending_sibling_reorg = None;
    *consecutive_rewind_cycles = 0;
    *consecutive_flush_mismatches = 0;
    hold.clear();
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
