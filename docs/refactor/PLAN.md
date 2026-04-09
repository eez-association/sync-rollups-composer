# Refactor Plan — `sync-rollup-composer`

> Final destination: `docs/refactor/PLAN.md` (lives in the repo, on branch `refactor/phase-0-mapping`).
> Reference Codex session: gpt-5.4 / xhigh / read-only. Reviewed across multiple passes (proposal + adversarial).

---

## 1. Context

This refactor exists because the **structural complexity** of the `based-rollup` crate has outgrown what a small team can confidently maintain, even though the code looks clean by surface metrics.

**Metrics measured today** (not speculation):

- `cargo build --release`: clean. `cargo clippy --workspace --all-features`: 0 warnings. 534 tests.
- 48 `unwrap()` in production (most in `composer_rpc/trace.rs`, JSON parsing). Only 1 `TODO` in all production code.
- Crate total: ~40k LOC (~16k production + ~24k tests).
- Mammoth functions: `step_builder` ~760 LOC, `flush_to_l1` ~640 LOC, `verify_local_block_matches_l1` ~253 LOC, `build_builder_protocol_txs` ~280 LOC, `trace_and_detect_l2_internal_calls` ~1693 LOC, `trace_and_detect_internal_calls` ~1817 LOC, `simulate_l1_combined_delivery` ~547 LOC, `simulate_l1_delivery` ~535 LOC.
- `composer_rpc/l1_to_l2.rs` (4345 LOC) and `composer_rpc/l2_to_l1.rs` (5459 LOC) are **near-identical mirrors** by un-refactored design.
- `CLAUDE.md` "Lessons Learned" has **~50 critical rules** that live in human conventions, not in types.

**Diagnosis**: this is not spaghetti — it is **concentrated debt**. The consensus invariants are encoded as "the right order to do things" in long imperative sequences. Any new dev (or agent) who touches `step_builder`, `flush_to_l1`, or the detection functions will silently break them.

**Desired outcome**:

1. The critical invariants of `CLAUDE.md` become impossible to break because they live in the type system or in a dedicated CI test/gate (§6 has 23 invariants assigned).
2. The >500 LOC functions in `driver/` and `composer_rpc/` are broken into explicit pipelines / state machines with sub-functions <200 LOC.
3. The L1↔L2 duplication in `composer_rpc/` is eliminated behind a sealed `Direction` trait — but **only after** the behavior is already encapsulated in testable stages.
4. The plan itself helps anyone reading it **understand the entire project**, not just refactor it.

## 2. Non-goals

- ❌ DO NOT change the spec (`docs/DERIVATION.md`).
- ❌ DO NOT modify the Solidity submodule (`contracts/sync-rollups-protocol/`).
- ❌ DO NOT change observable behavior: derivation, state roots, action hashes, postBatch encoding, and tx ordering per block must remain **byte-identical** (verified via baseline replay in 0.8 / 5.7).
- ❌ DO NOT add new features: no new RPC endpoints, no new opcodes, no new metrics.
- ❌ DO NOT refactor the UI or the E2E scripts (`scripts/e2e/`).
- ❌ DO NOT remove the development ECDSA prover — that is separate work (a prerequisite for production but **not part of this refactor**).
- ❌ DO NOT split `cross_chain.rs` / `table_builder.rs` by size (currently 2410 / 2524 LOC). If you want to attack them, that is an **explicit Phase 6** which is not in this plan today.

## 3. Meta-rules (apply to EVERY commit)

| Rule | Command / Policy |
|---|---|
| Per-commit verification | `cargo build --release` must pass |
| Per-step closing verification | `cargo nextest run -p based-rollup` must pass |
| Per-phase closing verification | `cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings` |
| Per-phase E2E smoke | Bring up devnet-eez (NEVER testnet-eez) and run the E2E subset defined at the end of the phase |
| **Per-phase baseline replay** | `bash scripts/refactor/replay_baseline.sh` (created in 0.8) must produce 0 diffs |
| Branch policy | `refactor/<phase>-<topic>` (e.g. `refactor/phase-1-newtypes`). `refactor/` is an accepted prefix for this workstream (aligned with the `refactor:` conventional commit). "Incremental" steps may share a branch; "dedicated" steps get their own branch and PR. |
| Commits | Conventional commits, atomic: `refactor(driver): introduce EntryVerificationHold typestate (#0)` |
| **Merge policy** | **Merge commit (NO squash)** to preserve per-step revertibility. Squash breaks the rule "every step revertible with `git revert`". |
| No-touch zones | `contracts/sync-rollups-protocol/`, `docs/DERIVATION.md` (only `spec-writer` with audit), `crates/based-rollup/src/evm_config.rs` (delicate passthrough), `deployments/shared/genesis.json` |
| Halt conditions | If a step requires modifying observable behavior or working around a `CLAUDE.md` invariant, **STOP** and open an issue describing the friction before continuing |
| Reversibility | Each step must be revertible with `git revert <merge-commit>` without touching subsequent steps |
| Devnet reset | If a change in entries/postBatch leaves L1 with incompatible state (see CLAUDE.md "Stale L1 state blocks builder recovery"), reset with `down -v` **only with explicit per-step user approval** |

## 4. Glossary (Rust idioms used by this plan)

- **Newtype pattern**: `pub struct ActionHash(pub B256);` — zero-cost wrapper over `B256`, a distinct type for the compiler. Makes it impossible to pass a `RollupId` where a `BlockNumber` is expected.
- **Typestate pattern**: `FlushPlan<Collected>` → `FlushPlan<HoldArmed>` — the same struct with a phantom type parameter. Allows `send_to_l1(plan: FlushPlan<HoldArmed>)` to reject at compile time any call that has not armed the hold first.
- **Sealed trait**: a trait that can only be implemented within the crate (via a private super-trait). We use this for `Direction` so nobody outside can add new directions.
- **Dependency inversion trait**: a trait that abstracts an IO boundary (network, filesystem, clock). Only worth it when there are ≥2 real impls (not "production + invented mock"). **In this refactor we only use `SimulationClient`** (see §4b for the ones we evaluated and discarded).
- **Module-private constructor + boundary wrapper**: `CleanStateRoot(B256)` whose `new` is `pub(crate)` and only called from `compute_clean_root(...)`. At boundaries (ABI decode, logs, serde) explicitly named `from_*_boundary` functions are used so grep finds every suspicious constructor.
- **Phantom data**: `PhantomData<S>` — zero-cost data that exists only for the type system, not at runtime.
- **`#[must_use]`**: annotation that warns if the returned type is not consumed. We use it for `NonceResetRequired` so ignoring it becomes a compile error (with `-D warnings`).
- **Builder pattern**: `XBuilder::new().with_a(a).with_b(b).build()` — alternative to constructors with many `Option`/`bool` fields.
- **`debug_assert!` vs `assert!`**: `debug_assert!` is stripped in release builds. We use it for soft invariants the type system does not capture.

## 4b. Trait strategy (where YES and where NO)

> **Design rule** (Codex, pass 3): *"traits at IO boundaries; enums and concrete structs in the domain; multiple `impl` blocks to split responsibility; no traits with a single internal impl"*.

This refactor uses traits in **exactly three** places. Everything else is modeled with concrete structs, closed enums, or module split + multiple `impl Driver` blocks.

| Trait | Purpose | Criterion | Steps |
|---|---|---|---|
| `Direction` (sealed) | Unify L1↔L2 in composer | ≥2 real symmetric implementations (`L1ToL2`, `L2ToL1`) | 3.1, 3.4-3.7 |
| `Sendable` (sealed marker) | Compile-time gate for `FlushPlan<S>` | Typestate — replaces runtime checks | 1.7 |
| `SimulationClient` | JSON-RPC boundary of the composer | ≥2 real impls: `HttpSimClient` + `InMemorySimClient` with fixtures (enables composer_rpc tests without an upstream) | 3.0 |

**EXPLICITLY rejected** (considered and discarded after Codex review):
- ❌ `EntryQueue` as a trait — the composer→driver queue is shared internal state, not an interchangeable backend. Modeled as a **concrete `EntryQueue` struct** with `push` / `wait_confirmation` / `drain_confirmed` methods.
- ❌ `L1Provider` as a wide trait — alloy already has its `Provider` trait. Double abstraction. Modeled as a **`L1Client` wrapper struct** local to `proposer.rs` with narrow methods. If we later need to mock it, we extract a trait then (not preemptively).
- ❌ Capability traits (`BuilderPhase`, `FlushCoordinator`, `BlockVerifier`, `BlockRewinder`) — with a single impl they enforce nothing that multiple `impl Driver` blocks don't. Doc comments are not enforcement. Real "scope discipline" comes from **concrete structs with narrow borrows** (`BuilderTickContext`, `FlushPrecheck`, `FlushAssembly`, `VerificationDecision`) introduced in steps 2.2-2.7.
- ❌ `SimulationStrategy` as trait + `OrElse` chain — with a fixed set of 3 strategies closed in the crate, an **`enum SimulationPlan` + function** is clearer.

**Operational rule**: if you feel tempted to add a new trait while executing the plan, first answer: (1) Are there ≥2 real impls, not just one "just in case"? (2) Is the second impl a real backend, not a fictional mock? If either is "no", use a concrete struct.

**Technical detail — async in traits**: Rust 1.85 supports native `async fn` in traits. The only trait of this refactor that uses `Arc<dyn Trait>` is `SimulationClient` — for it we add the `async-trait = "0.1"` crate as a dep (per-call allocation, trade-off accepted for the fixture benefit). `Direction` uses generics (static dispatch), so native `async fn` is sufficient.

## 5. Current architecture map (pre-refactor state)

```
                            ┌──────────────────────────────────────────┐
                            │            L1 (reth --dev)               │
                            │  ┌────────────────────────────────────┐  │
                            │  │  Rollups.sol / CCM / Bridge / etc │  │
                            │  └─────────▲──────────┬───────────────┘  │
                            │            │postBatch │BatchPosted        │
                            └────────────┼──────────┼──────────────────┘
                                         │          │
                          ┌──────────────┴──┐    ┌──┴──────────────────┐
                          │   proposer.rs   │    │   derivation.rs     │
                          │  (L1 sender)    │    │  (L1 sync, §4e/§4f)│
                          └──────────────▲──┘    └──────────┬──────────┘
                                         │                  │
                                         │                  ▼
   ┌────────────────────┐   ┌────────────┴──────────────────────────┐
   │ payload_builder.rs │   │              driver.rs                │
   │   block building   │◄──┤ step_builder │ flush_to_l1 │ verify  │
   └────────────────────┘   └────────────┬──────────────────────────┘
                                         │
            ┌────────────────────────────┴────────────────────────────┐
            │                                                          │
            ▼                                                          ▼
   ┌────────────────────────┐                          ┌──────────────────────────┐
   │   composer_rpc/        │                          │   cross_chain.rs +      │
   │  ┌──────────────────┐  │                          │   table_builder.rs     │
   │  │ l1_to_l2.rs (4k) │  │                          │  - ABI types           │
   │  │ l2_to_l1.rs (5k) │  │ ◄──────── builds ─────── │  - entry building      │
   │  │  trace.rs        │  │                          │  - state delta logic   │
   │  │  common.rs       │  │                          └──────────────────────────┘
   │  └──────────────────┘  │                                       ▲
   │  HTTP RPC interceptors │                                       │
   │  (hold-then-forward)   │                                       │
   └─────────▲──────────────┘                                       │
             │                                          ┌───────────┴────────────┐
             │                                          │       rpc.rs            │
             │            JSON-RPC client               │  (jsonrpsee trait)      │
             └──────────────────────────────────────────┤  syncrollups_*          │
                                                         └─────────────────────────┘
```

## 6. Critical invariants → target type table (23 rules)

This table is the contract of the refactor. Every critical rule of `CLAUDE.md > Lessons Learned` that we kill via types is documented here. **Step 0.2 produces this expanded table in the repo as `docs/refactor/INVARIANT_MAP.md`.**

| # | Invariant (CLAUDE.md) | Current owner (human) | Future owner (type) | Phase |
|---|---|---|---|---|
| 1 | Hold MUST be set BEFORE send_to_l1 | Comment in `flush_to_l1` | `FlushPlan<HoldArmed>` typestate with `entry_block` inside the plan | 1.7 |
| 2 | NEVER use auto-nonce, always reset on failure | Comment in `proposer.rs` | `L1NonceReservation` + `#[must_use] NonceResetRequired` | 1.8 |
| 3 | NEVER align state roots by overwriting pre_state_root | Comment in `flush_to_l1` | `CleanStateRoot` newtype with module-private constructor + explicit `from_bytes_at_boundary` | 1.2 |
| 4 | §4f filtering is per-call prefix counting, never all-or-nothing | Comment in `derivation.rs` | `ConsumedPrefix(usize)` + monotonicity (property) test | 0.3 |
| 5 | Continuation entries are NOT triggers (`hash(next_action) != action_hash`) | Comment in `partition_entries` | `enum EntryClass { Trigger, Continuation, Result, RevertContinue }` | 1.4 |
| 6 | Result entry skipped when `extra_l2_entries` non-empty | Double check in driver and rpc | `enum QueuedCallRequest::{Simple, WithContinuations}` — the Simple variant carries Result, the WithContinuations variant does not allow it | 1.4b |
| 7 | `parent_call_index` must be rebased after combined_delivery | Comment in `l2_to_l1.rs` | `ParentLink { Root, Child(AbsoluteCallIndex) }` + single helper `rebase_parent_links` | 1.3, 3.3 |
| 8 | First TRIGGER entry needs currentState=clean (post swap-and-pop reorder) | Imperative logic | `first_trigger_idx` computed by `ImmediateEntryBuilder` with dedicated test | 1.9b |
| 9 | Deferral exhaustion → rewind, not accept | Imperative logic in `verify_local_block_matches_l1` | `VerificationDecision::MismatchRewind { target: entry_block - 1 }` | 2.5 |
| 10 | Rewind target is `entry_block - 1` | Comment | Single method `Driver::rewind_to_re_derive(entry_block: u64)` that computes target inside | 2.5 |
| 11 | Deposits + withdrawals can coexist in same block | Removed mutual exclusion | `enum BlockEntryMix { Empty, OnlyD, OnlyW, Mixed }` exported by `PendingL1SubmissionQueue` | 1.5 |
| 12 | Multi-call L2→L1 must use scope navigation on Entry 1 | Comment in `table_builder.rs` | `L2ToL1ContinuationBuilder::with_scope_return(scope)` required | 1.9c |
| 13 | **Hold-then-forward: both composer RPCs MUST await queue confirmation** | Comment in `composer_rpc/*` | `ForwardPermit` token only returned when `EntryQueue` transitions the receipt to `Confirmed` (= L2 block with the entries was built locally) | 1.6b+c |
| 14 | **Builder HALTS block production while hold is active** | Comment in `step_builder` | `EntryVerificationHold` exposes `is_blocking_build() -> bool`; `BuilderStage::Build` cannot run if `is_blocking_build` | 1.6 |
| 15 | **Withdrawal trigger revert on L1 causes REWIND, not log** | Logic in `flush_to_l1` post-submit | `TriggerExecutionResult::{Confirmed, RevertedNeedsRewind(entry_block)}` | 2.7b |
| 16 | **§4f filtering is generic (CrossChainCallExecuted events), NOT Bridge selectors** | `filter_block_entries` + `extract_l2_to_l1_tx_indices` | Single function with type signature `(receipts, ccm_address) -> Vec<usize>`; NO Bridge parameter | 0.3 |
| 17 | **NEVER per-call simulate_l1_delivery for multi-call L2→L1** | Comment in `l2_to_l1.rs` | `enum SimulationPlan { Single, CombinedThenAnalytical }` + single function `simulate_delivery()` that picks the plan via `simulation_plan_for(calls, promotion_decision)` | 3.6 |
| 18 | **L1 and L2 entry structures must MIRROR** | CLAUDE.md comment | Mirror tests (0.5) + shared `model.rs` in composer (3.2). Not a type, but a mandatory test and shared code | 0.5, 3.2 |
| 19 | **NEVER swap (dest, source) for L1→L2 return call children** | Comment in `table_builder.rs` | `enum CallOrientation { Forward, Return }` + single function `address_pair_for(orientation)` | 1.9a |
| 20 | **Return data shape: Void = 0 bytes; delivery_return_data → hashes; l2_return_data → scope resolution** | Multiple comments in CLAUDE.md #245, #246 | `enum ReturnData { Void, NonVoid(Bytes) }` propagated by all builders | 1.10 |
| 21 | **Single L2→L1 + terminal return still promotes to multi-call continuation** | Bool condition in `l2_to_l1.rs` | `enum PromotionDecision { KeepSingle, PromoteToContinuation }` with explicit rules | 3.6 |
| 22 | **`publicInputsHash` uses block.timestamp, not block.number** | Code in `proposer.rs` + proxy sim | `ProofContext { block_timestamp: U256, … }` typed in proposer; sim uses `blockOverride.time` mandatorily | 1.8 |
| 23 | **NEVER hardcode function selectors — `sol!` only** | Convention | Local clippy lint `disallowed_methods` + CI grep gate that searches for `0x[a-f0-9]{8}` in strings | 4.4 |

## 7. The MVP path (priority order)

If the refactor is interrupted, do it in this order:

```
0.3 → 0.4 → 0.5 → 0.6 → 0.7 → 0.8           (safety net + BASELINE)
1.4 → 1.4b → 1.5 → 1.6 → 1.6b+c → 1.7 → 1.8 → 1.10
                                              (driver typestate + return data + queued enums)
2.4 → 2.5 → 2.6 → 2.7 → 2.7b → 2.8           (break flush_to_l1 and verify with explicit forward+triggers)
3.0 → 3.1 → ... → 3.7                         (sim_client BEFORE direction trait, only now unify L1↔L2)
```

> "The biggest return is in typing the driver state and making the flush/verify state machine explicit. The `Direction` trait is worth it, but not before the behavior is already encapsulated." — Codex (pass 1)
>
> "Move 5.6 to Phase 1, advance `sim_client` before 3.4, add baseline capture in Phase 0, and fix the DoD so it doesn't promise splitting files without steps to do so." — Codex (pass 2)

---

## 8. Detailed plan by phases

**Convention for each step**:
> **N.M** Imperative description.
> *Files:* `path/file.rs[:Lstart-Lend]` ...
> *Verifies:* specific `cargo …` command.
> *Branch:* `incremental` (may share) or `dedicated` (own PR).

---

### Phase 0 — Mapping, Guardrails & Baseline

**Goal**: install the safety net, the maps, and a **byte-level baseline** of current behavior. This phase does not change production code except for new tests and scripts.

**0.1** Create `docs/refactor/ARCH_MAP.md` with the diagram from §5 + a 1-page walkthrough of each flow (`postBatch` outbound; `derivation` inbound; `composer_rpc` hold-then-forward). `file:line` references to top-level functions.
*Files:* `docs/refactor/ARCH_MAP.md` (new)
*Verifies:* `cargo build --release`
*Branch:* `refactor/phase-0-mapping` (incremental)

**0.2** Create `docs/refactor/INVARIANT_MAP.md` with the expanded §6 table — one row per hard rule from `CLAUDE.md > Lessons Learned` with columns: rule, current owner (`file:line` or "convention"), current test (if any), future type, plan phase.
*Files:* `docs/refactor/INVARIANT_MAP.md` (new)
*Verifies:* `cargo build --release`
*Branch:* incremental

**0.3** Property tests for the invariants that live in filtering, in the right files:
- `cross_chain_tests.rs`: `partition_entries` (`:2022`) is stable and disjoint; `identify_trigger_tx_indices` (`:2172`) dedupes by order.
- **`derivation_tests.rs`** (missing in v1): `compute_consumed_trigger_prefix` (`cross_chain.rs:2237`) is prefix-monotonic, `filter_block_entries` preserves the count of deposits and L2→L1 txs. **This closes invariants #4 and #16.**
Use `proptest` (already in dev-deps).
*Files:* `cross_chain_tests.rs`, `derivation_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup cross_chain:: derivation::`
*Branch:* incremental

**0.4** Property test for `table_builder.rs::reorder_for_swap_and_pop` (`:124`): preserves the multiset, preserves relative order per group, idempotent for groups ≤ 2.
*Files:* `table_builder_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup table_builder::`
*Branch:* incremental

**0.5** **Neutral DSL for mirror tests** — must live in `src/` to be importable from unit tests `*_tests.rs` (Codex p4 fix: tests under `src/` cannot import from `tests/fixtures/`). Create `crates/based-rollup/src/test_support/mirror_case.rs` under the `test-utils` feature (which already exists):
```rust
// crates/based-rollup/src/test_support/mod.rs
#[cfg(any(test, feature = "test-utils"))]
pub mod mirror_case;

// crates/based-rollup/src/test_support/mirror_case.rs
pub struct MirrorCase {
    pub name: &'static str,
    pub calls: Vec<LogicalCall>,        // direction-agnostic
    pub expected_l1_shape: EntryShape,
    pub expected_l2_shape: EntryShape,
    pub expected_action_hashes: Vec<B256>,
}

pub fn canonical_cases() -> Vec<MirrorCase> {
    vec![
        deposit_simple(),
        withdrawal_simple(),
        flash_loan_3_call(),
        ping_pong_depth_2(),
        ping_pong_depth_3(),
    ]
}
```
With this DSL, mirror tests are a loop over `canonical_cases()`. Importable from `table_builder_tests.rs` and `cross_chain_tests.rs` (both are sibling files of `table_builder.rs`/`cross_chain.rs`).
*Files:* `src/test_support/mod.rs`, `src/test_support/mirror_case.rs` (new), `src/lib.rs` (declare the module under cfg), `table_builder_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup table_builder::mirror_`
*Branch:* incremental

**0.6** Minimal reentrant trace fixtures (JSON in `crates/based-rollup/tests/fixtures/traces/`): (a) simple L1→L2 and L2→L1 call, (b) flash loan 3-call, (c) PingPong depth-2 and depth-3, (d) top-level revert, (e) child continuation, (f) multi-call `CallTwice`. Loaded from `composer_rpc/l1_to_l2_tests.rs` and `composer_rpc/l2_to_l1_tests.rs` and also from the DSL in 0.5.
*Files:* `tests/fixtures/traces/*.json`, `composer_rpc/l1_to_l2_tests.rs`, `composer_rpc/l2_to_l1_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* incremental

**0.7** Expand hold/mismatch/rewind coverage around `flush_to_l1` (`driver.rs:1796`) and `verify_local_block_matches_l1` (`:3204`). Required tests: "hold set BEFORE submit", "hold cleared on verify match", "defer 3 times and rewind", "hold not set if no entries", "rewind cycle clamps to anchor", "withdrawal trigger revert causes rewind" (**closes invariant #15**).
*Files:* `driver_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_hold driver::tests::test_full_rewind driver::tests::test_withdrawal_trigger_revert_rewind`
*Branch:* incremental

**0.8** ⭐ **Capture baseline** (new, Codex pass 2 insistence; baseline source revised after Phase 0 audit). Create `scripts/refactor/capture_baseline.sh` that brings up a clean devnet-eez and drives the **protocol E2E suite under `contracts/sync-rollups-protocol/script/e2e/`** in network mode against the running composer/builder. The protocol E2E suite is the right baseline source because:

1. **Canonical expected hashes**: each scenario's `ComputeExpected` contract emits `EXPECTED_L1_HASHES`, `EXPECTED_L2_HASHES`, and `EXPECTED_L2_CALL_HASHES` computed directly from the action structures by the protocol Solidity code. These are the bytes the composer/builder MUST produce — they are deterministic across runs.
2. **Subset-match verifiers**: `script/e2e/shared/Verify.s.sol` (`VerifyL1Batch`, `VerifyL2Blocks`, `VerifyL2Calls`) checks the actual `BatchPosted`, `ExecutionTableLoaded`, and `IncomingCrossChainCallExecuted` events against the expected hashes. The semantics (subset match per block) are the natural baseline contract.
3. **Network mode**: `script/e2e/shared/run-network.sh` already drives each scenario against an existing devnet-eez. It performs the canonical "prepare → deploy → cast send → verify" flow, exercising the SAME composer/builder/derivation code paths that this refactor touches.
4. **Wide coverage**: 21 scenarios vs the 7 ad-hoc bash scripts under `scripts/e2e/`. Includes `revertContinue`, `revertContinueL2`, `deepScopeL2`, `nestedCallRevert`, `siblingScopes`, `multi-call-nested`, `reentrantCrossChainCalls`, and 14 others.

The baseline script wraps `run-network.sh` per scenario and captures, into `tests/baseline/<scenario>.json`:
- `expected_l1_hashes`, `expected_l2_hashes`, `expected_l2_call_hashes` (deterministic, from `ComputeExpected`)
- `target`, `value`, `calldata`, `rlp_encoded_tx` (the user tx generated by `cast mktx`)
- `l1_batch_tx`, `l1_block`, `l2_table_tx`, `l2_call_tx`, `l2_blocks` (actual block/tx ids on the live chains)
- `actual_postbatch_calldata` (hex of the postBatch tx input — fetched via `cast tx <hash> input`)
- `actual_batchposted_logs`, `actual_executiontableloaded_logs`, `actual_crosschaincall_logs` (the on-chain events)

Initial canonical scenarios: `counter`, `counterL2`, `bridge`, `multi-call-twice`, `multi-call-two-diff`, `flash-loan`, `nestedCounter`, `nestedCounterL2`, `revertContinue`, `revertContinueL2`. The remaining 11 scenarios are added on-demand as the refactor uncovers gaps.

**Without this, 5.7 (replay gate) is impossible.**

Note: this step requires a running devnet-eez (Docker) and the `forge`/`cast` binaries already used by the protocol submodule. The first capture run is wall-clock expensive (~20 min for 10 scenarios) but is committed to the repo, so subsequent verification runs only re-execute the scenarios and diff JSON.

*Files:* `scripts/refactor/capture_baseline.sh` (new), `scripts/refactor/baseline_lib.sh` (new helpers), `tests/baseline/*.json` (committed)
*Verifies:* `bash scripts/refactor/capture_baseline.sh && git status tests/baseline/`
*Branch:* incremental

**Phase 0 closing**: `cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings && bash scripts/e2e/bridge-health-check.sh && bash scripts/e2e/crosschain-health-check.sh && bash scripts/refactor/capture_baseline.sh`

---

### Phase 1 — Types for invariants (typestate, newtypes, sealed traits)

**Goal**: encode the critical invariants of §6 in the type system (the ones that admit compile-time gating). The phase with the largest return per LOC touched.

**1.1a** Newtype `RollupId(U256)` in `cross_chain.rs`. Implements `From<U256>` marked `pub(crate)` + `from_bytes_at_boundary(bytes: &[u8])` for ABI decode. **Only the newtype and its impl — do not migrate callsites.**
*Files:* `cross_chain.rs`
*Verifies:* `cargo build --release && cargo nextest run -p based-rollup cross_chain::`
*Branch:* incremental

**1.1b** Migrate callsites from `U256` → `RollupId` in `cross_chain.rs` and `table_builder.rs`. **Dedicated branch** because it touches >20 callsites. Use iterative `cargo check`.
*Files:* `cross_chain.rs`, `table_builder.rs`, `cross_chain_tests.rs`, `table_builder_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup cross_chain:: table_builder::`
*Branch:* `refactor/phase-1-rollup-id` (dedicated)

**1.1c** Newtype `ScopePath(Vec<U256>)` + migration in the callsites that today use `Vec<U256>` for scope. Includes helpers `ScopePath::enter(&mut self, U256)` and `ScopePath::exit(&mut self)`.
*Files:* `cross_chain.rs`, `table_builder.rs`
*Verifies:* `cargo nextest run -p based-rollup cross_chain:: table_builder::`
*Branch:* incremental

**1.2** State root newtypes with **module-private constructors + boundary wrappers** (corrected from v1). **Closes invariant #3.**
```rust
// cross_chain.rs
pub struct CleanStateRoot(B256);
pub struct SpeculativeStateRoot(B256);
pub struct NewStateRoot(B256);
pub struct ActionHash(B256);

impl CleanStateRoot {
    pub(crate) fn new(b: B256) -> Self { Self(b) }       // only inside the module
    pub fn from_abi_boundary(b: B256) -> Self { Self(b) } // explicit name at boundaries
    pub fn from_log_boundary(b: B256) -> Self { Self(b) } // explicit name at boundaries
    pub fn as_bytes(&self) -> B256 { self.0 }
}
```
A grep for `from_*_boundary` lists every entry point — auditable by eye. No `CleanStateRoot(b)` tuple-struct constructor outside the module.
*Files:* `cross_chain.rs`, `driver/`, `derivation.rs`, `proposer.rs`, `rpc.rs`, tests
*Verifies:* `cargo nextest run -p based-rollup`
*Branch:* `refactor/phase-1-state-root-types` (dedicated — touches 6+ files)

**1.3** Replace `Option<usize>` with `enum ParentLink { Root, Child(AbsoluteCallIndex) }` in `table_builder.rs`, `rpc.rs` (`BuildExecutionTableCall`, `BuildL2ToL1Call`), and the composer's local models. **Closes invariant #7 (partial — the single helper lives in 3.3).**
*Files:* `table_builder.rs`, `rpc.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup table_builder:: composer_rpc::`
*Branch:* `refactor/phase-1-parent-link` (dedicated)

**1.4** Three distinct semantic enums (v1 inconsistency fix):
- `enum TxOutcome { Success, Revert }` replaces `tx_reverts: bool`.
- `enum EntryGroupMode { Chained, Independent }` replaces `l1_independent_entries: bool`.
- `enum EntryClass { Trigger, Continuation, Result, RevertContinue }` internal classification.

**Closes invariants #5 (via `EntryClass`) and the chained vs independent semantics (via `EntryGroupMode`).** Invariant #6 (result entry skipped) is closed in **1.4b** with `QueuedCallRequest`.
*Files:* `rpc.rs`, `driver.rs`, `cross_chain.rs`, `table_builder.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup`
*Branch:* `refactor/phase-1-semantic-enums` (dedicated)

**1.4b** ⭐ **MOVED from 1.11** (Codex p4: 1.6c depended on `QueuedCallRequest` which did not exist until 1.11, hidden dependency — advanced to 1.4b). Replace `QueuedCrossChainCall` and `QueuedL2ToL1Call` (in `rpc.rs`) with enums that explicitly separate the simple case from the continuation case. **Closes invariant #6.**
```rust
pub enum QueuedCallRequest {
    Simple {
        call_entry: CrossChainExecutionEntry,
        result_entry: CrossChainExecutionEntry,  // ONLY exists in Simple
        raw_l1_tx: Bytes,
        gas_price: u128,
        tx_outcome: TxOutcome,
        group_mode: EntryGroupMode,
    },
    WithContinuations {
        l2_table_entries: Vec<CrossChainExecutionEntry>,  // NO result_entry
        l1_entries: Vec<CrossChainExecutionEntry>,
        raw_l1_tx: Bytes,
        gas_price: u128,
        tx_outcome: TxOutcome,
        group_mode: EntryGroupMode,
    },
}
```
It becomes IMPOSSIBLE to construct a `WithContinuations` request with a `result_entry`. Same for `QueuedL2ToL1CallRequest` — both are variants of the same unified enum.
*Files:* `rpc.rs`, `driver.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup`
*Branch:* `refactor/phase-1-queued-enums` (dedicated)

**1.5** **Eliminate the 4 parallel vectors** in `driver.rs:130-140` and replace them:
```rust
pub struct PendingL1Group {
    pub entries: Range<usize>,
    pub mode: EntryGroupMode,
    pub trigger: Option<TriggerMetadata>,
}

pub struct PendingL1SubmissionQueue {
    entries: Vec<CrossChainExecutionEntry>,
    groups: Vec<PendingL1Group>,
}

impl PendingL1SubmissionQueue {
    pub fn entry_mix(&self) -> BlockEntryMix { /* closes invariant #11 */ }
    pub fn take_all(&mut self) -> (Vec<CrossChainExecutionEntry>, Vec<PendingL1Group>);
}
```
*Files:* `driver.rs`, `driver_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_flush_to_l1_ driver::tests::test_cross_chain_entries_`
*Branch:* incremental

**1.6** `EntryVerificationHold` with builder gate and idempotent semantics (Codex p2 fix):
```rust
pub enum EntryVerificationHold {
    Clear,
    Armed { entry_block: u64, deferrals: u8 },
}

impl EntryVerificationHold {
    pub fn arm(&mut self, entry_block: u64); // idempotent: arm() of the same block is no-op
    pub fn defer(&mut self) -> DeferralResult; // Continue(deferrals) | MustRewind { target: entry_block - 1 }
    pub fn clear(&mut self);
    pub fn is_armed(&self) -> bool;
    pub fn is_blocking_build(&self) -> bool; // closes invariant #14
    pub fn armed_for(&self) -> Option<u64>;
}
```
Replaces `pending_entry_verification_block: Option<u64>` + `entry_verify_deferrals: u8`. Additionally `BuilderStage::Build` consults `is_blocking_build()` before building — **closes invariant #14 (builder HALTS while hold active)**.
*Files:* `driver.rs`, `driver_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_hold driver::tests::test_consecutive_rewind_backoff driver::tests::test_builder_halts_while_hold_active`
*Branch:* incremental

**1.6b+c** ⭐ **MERGED (Codex p4)**: concrete `EntryQueue` struct + `ForwardPermit` token + explicit 3-state machine. **Closes invariant #13.**

**"Confirmed" semantics** (precised after Codex p6/p8): the `ForwardPermit` token is emitted EXACTLY at the moment the driver completes the `Reserved → Confirmed` transition on a receipt. **Exact code point**: right after `update_fork_choice(block_hash)` returns `Ok(PayloadStatus::Valid)` or equivalent, indicating reth canonized and persisted the block containing the entries. NOT after `build_and_insert_block` finishes (that is just construction), NOT after `fork_choice_updated_with_retry` if it returns `SYNCING`, but specifically when reth marks the block as canonical in its chain state.

This closes the `build OK + crash before FCU` case (inconsistency window) and the `FCU SYNCING` case (not canonized yet). It is the first ordered moment at which the user tx may be forwarded safely: any future tx that reaches the builder will land in an L2 block subsequent to the one already containing the entries in reth → no race with `ExecutionNotFound`.

Replaces `queued_cross_chain_calls: Arc<Mutex<Vec<QueuedCrossChainCall>>>` and `queued_l2_to_l1_calls: Arc<Mutex<Vec<QueuedL2ToL1Call>>>` (driver.rs:123, 129):

```rust
// crates/based-rollup/src/entry_queue.rs (new)
use crate::rpc::QueuedCallRequest;  // unified type from step 1.4b

/// Stable token emitted by push(); opaque, not a positional index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueReceipt(u64);  // monotonic counter

/// State of a receipt in the queue. Transitions:
///   Pending     → Reserved   (driver calls drain_pending when starting block build)
///   Reserved    → Confirmed  (driver calls confirm after successful FCU of the block)
///   Reserved    → Pending    (driver calls rollback if build/FCU fails)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptState { Pending, Reserved, Confirmed }

/// Zero-cost token. Only constructed in wait_confirmation after observing Confirmed.
#[must_use = "ForwardPermit must be passed to forward_user_tx; never drop"]
pub struct ForwardPermit { _seal: () }

#[derive(Clone, Default)]
pub struct EntryQueue {
    inner: Arc<Mutex<QueueState>>,
    notify: Arc<Notify>,
}

struct QueueState {
    next_id: u64,
    items: BTreeMap<QueueReceipt, (QueuedCallRequest, ReceiptState)>,
}

impl EntryQueue {
    /// Composer pushes; receipt starts in Pending.
    pub async fn push(&self, req: QueuedCallRequest) -> QueueReceipt;

    /// Composer waits until the receipt is Confirmed.
    /// - Wakes via `notify`; after wake, re-checks state (idempotent, robust to lost wakeups).
    /// - Returns Err if the receipt was evicted (timeout, shutdown, or invalid).
    /// - The ForwardPermit is the only way to invoke forward_user_tx().
    pub async fn wait_confirmation(&self, receipt: QueueReceipt) -> Result<ForwardPermit>;

    /// Driver: drains up to `max` items from Pending → Reserved.
    /// Returns the items so the driver can include them in the block being built.
    pub fn drain_pending(&self, max: usize) -> Vec<(QueueReceipt, QueuedCallRequest)>;

    /// Driver: confirms receipts (Reserved → Confirmed) after successful FCU of the block.
    /// Wakes wait_confirmation waiters.
    pub fn confirm(&self, receipts: &[QueueReceipt]);

    /// Driver: rollbacks receipts (Reserved → Pending) if build/FCU failed.
    /// Does NOT wake waiters (they keep waiting for Confirmed).
    pub fn rollback(&self, receipts: &[QueueReceipt]);
}

/// Free function that forces consumption of the permit before forwarding.
pub async fn forward_user_tx(permit: ForwardPermit, raw_tx: &str, upstream: &str) -> Result<Response> {
    let _consume = permit;  // moved → cannot forward without having waited for Confirmed
    // ... forward HTTP call
}
```

**Why the precise transition matters**:
- If `confirm` were called in `drain_pending` (when starting the build), the composer would forward before having a guarantee that the block was persisted → race with a crash post-drain pre-FCU.
- If it were called after submitting to L1, the composer would unnecessarily wait for seconds (L1 slot) to forward something that is already locally safe.
- The correct point is **post-FCU**: the block is committed in reth, any future tx will land in a later slot, and even if the driver crashes afterwards, on recovery the block is still there.

**Concurrent risk (§11)**: `Notify` may have lost wakeups in theory, but `wait_confirmation` re-checks state after wake (does not assume wake = confirmed), so it is idempotent. Mandatory test: 1000 concurrent push + 1000 wait from separate tasks, asserting no waiter hangs.

*Files:* `entry_queue.rs` (new), `driver.rs`, `composer_rpc/common.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`, tests
*Verifies:* `cargo nextest run -p based-rollup driver:: composer_rpc:: entry_queue::`
*Branch:* `refactor/phase-1-entry-queue` (dedicated)

**1.7** ⭐ **`FlushPlan<S>` typestate**. **Closes invariant #1.** Codex p2 corrections incorporated:

```rust
// Three variants: NoEntries for blocks-only, Collected for pending entries, HoldArmed for ready-to-send.
pub struct FlushPlan<S> {
    blocks: Vec<PendingBlock>,                    // OWNED (no borrow — async safety)
    entries: Vec<CrossChainExecutionEntry>,       // OWNED
    groups: Vec<PendingL1Group>,                  // OWNED
    entry_block: Option<u64>,                     // carries the entry_block INSIDE the plan
    _marker: PhantomData<S>,
}

pub struct NoEntries;   // blocks-only, no hold needed
pub struct Collected;   // has entries, hold not yet armed
pub struct HoldArmed;   // has entries, hold armed for the correct entry_block

impl FlushPlan<NoEntries> {
    pub fn new_blocks_only(blocks: Vec<PendingBlock>) -> Self { ... }
}

impl FlushPlan<Collected> {
    pub fn new(blocks: Vec<PendingBlock>, queue: PendingL1SubmissionQueue) -> Self {
        // entry_block is computed here from the last block with entries
    }
    pub fn arm_hold(self, hold: &mut EntryVerificationHold) -> FlushPlan<HoldArmed> {
        if let Some(eb) = self.entry_block {
            hold.arm(eb); // idempotent
        }
        FlushPlan { _marker: PhantomData, ..self }
    }
}

// Sendable is a sealed trait for both variants that can be sent
trait Sendable: sealed::Sealed {}
impl Sendable for NoEntries {}
impl Sendable for HoldArmed {}

impl Proposer {
    pub async fn send_to_l1<S: Sendable>(&self, plan: FlushPlan<S>) -> SendResult {
        // NoEntries: just blocks, does not touch hold.
        // HoldArmed: was armed with the correct entry_block before.
        // Collected: DOES NOT COMPILE — the borrow checker rejects it.
    }
}

// Explicit failure path: if send_to_l1 fails after arm, the caller must consume the error.
#[must_use]
pub enum SendResult {
    Ok { consumed_prefix: usize },
    Failed { needs_rollback: RollbackAction },
}

pub enum RollbackAction {
    ClearHoldAndCooldown { entry_block: u64 },
    ResetNonceAndRewind { target: u64 },
}
```
With this:
- You cannot call `send_to_l1` with `Collected` (compile error).
- `NoEntries` passes through without arming the hold (fixes the issue Codex flagged: today `send_to_l1` serves blocks-only).
- `entry_block` travels inside the plan, so `arm_hold` cannot arm for the wrong block.
- `FlushPlan` OWNS everything — no live borrows crossing `.await`.
- `SendResult` is `#[must_use]` with an explicit rollback enum — ignoring it is a warning.
- `arm_hold` is idempotent (if hold was already armed for that block, no-op) because `EntryVerificationHold::arm` is (1.6).
*Files:* `driver/flush.rs`, `proposer.rs`, `driver_tests.rs`, `proposer_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_hold_ driver::tests::test_flush_to_l1_ proposer::tests::`
*Branch:* `refactor/phase-1-flushplan-typestate` (dedicated — central step of the phase)

**1.8** `L1NonceReservation` + `#[must_use] NonceResetRequired` + `ProofContext` + concrete `L1Client` wrapper (revised after Codex p3: originally `trait L1Provider`, discarded — alloy already has its `Provider` trait and double-abstracting it is ceremony). **Closes invariants #2 and #22.**

```rust
// proposer.rs
pub struct L1NonceReservation { nonce: u64, key: Address }

#[must_use = "NonceResetRequired must be consumed by calling proposer.reset_nonce()"]
pub struct NonceResetRequired { _seal: () }

pub struct ProofContext {
    pub block_timestamp: U256,
    pub blob_hashes: Vec<B256>,
    pub entry_hashes: Vec<B256>,
    pub call_data_hash: B256,
}

/// Concrete wrapper local to proposer. Narrow methods — only what proposer actually uses from alloy.
/// If we later need to mock it for tests, we extract a trait then (not preemptively).
pub(crate) struct L1Client {
    inner: RootProvider,
}

impl L1Client {
    pub async fn get_nonce(&self, addr: Address) -> Result<u64>;
    pub async fn send_tx(&self, tx: TransactionRequest) -> Result<B256>;
    pub async fn get_balance(&self, addr: Address) -> Result<U256>;
    pub async fn last_submitted_state_root(&self) -> Result<B256>;
    // ... only what proposer needs, not a wide wrapper over RootProvider
}

impl Proposer {
    pub async fn reserve_nonce(&mut self) -> L1NonceReservation;
    pub async fn send_with(&mut self, res: L1NonceReservation, tx: ...) -> Result<(), NonceResetRequired>;
    pub async fn reset_nonce(&mut self, _token: NonceResetRequired) -> Result<()>;
    pub fn sign_proof(&self, ctx: ProofContext) -> Signature;  // only accepts ProofContext
}
```

`L1Client` consolidates provider accesses in a single place inside `proposer.rs` and leaves the rest of the code (`driver/`, `derivation.rs`) using `RootProvider` directly — that is not touched for now. If end-to-end mocking is eventually needed, the struct already has all the methods ready to be converted to a trait in a single commit.

*Files:* `proposer.rs`, `proposer_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_submission_failure_sets_cooldown driver::tests::test_submission_success_clears_cooldown proposer::tests::`
*Branch:* incremental

**1.9a** `ImmediateEntryBuilder` in `cross_chain.rs`. Encapsulates `build_l2_to_l1_call_entries` (`:819`) and the `address_pair_for(CallOrientation::Forward | Return)` logic — **closes invariant #19**. Migrate the first callsite in the same commit; leave the old function as a `#[deprecated]` wrapper that calls the builder.
*Files:* `cross_chain.rs`, `cross_chain_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup cross_chain::tests::test_build_l2_to_l1_call_entries_`
*Branch:* incremental

**1.9b** `DeferredEntryBuilder` for deferred entries (`build_cross_chain_call_entries`, `:719`) + `RevertGroupBuilder` for `attach_chained_state_deltas` (`:1520`). `RevertGroupBuilder` encapsulates "first trigger needs currentState=clean after swap-and-pop reorder" — **closes invariant #8**.
*Files:* `cross_chain.rs`, `cross_chain_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup cross_chain::tests::test_attach_generic_state_deltas_`
*Branch:* incremental

**1.9c** `L2ToL1ContinuationBuilder` for `build_l2_to_l1_continuation_entries` (`table_builder.rs:1612`). API:
```rust
L2ToL1ContinuationBuilder::new()
    .with_scope_return(ScopePath::from([0]))   // MANDATORY — closes invariant #12
    .add_entry(...)
    .build()?
```
`build()` fails if `with_scope_return` was not called — makes it impossible to forget the scope navigation.
*Files:* `table_builder.rs`, `table_builder_tests.rs`
*Verifies:* `cargo nextest run -p based-rollup table_builder::tests::test_build_l2_to_l1_continuation_`
*Branch:* incremental

**1.10** ⭐ **NEW (Codex p2): `ReturnData` enum** — kills several rules at once (#20):
```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ReturnData {
    Void,               // 0 bytes — void function via assembly return
    NonVoid(Bytes),     // raw ABI-encoded return value
}

impl ReturnData {
    pub fn from_bytes(b: Bytes) -> Self {
        if b.is_empty() { Self::Void } else { Self::NonVoid(b) }
    }
    pub fn is_void(&self) -> bool { matches!(self, Self::Void) }
}
```
Propagate through `DetectedReturnCall`, `DetectedCall`, RPC JSON (`L2ReturnCall`), builders. Every site that today checks `.is_empty()` switches to `matches!(_, ReturnData::Void)`. Explicit fix for `delivery_return_data` and `l2_return_data` — the 4 RESULT hash sites in CLAUDE.md #245 are now verified by the type.
*Files:* `cross_chain.rs`, `table_builder.rs`, `rpc.rs`, `composer_rpc/trace.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup`
*Branch:* `refactor/phase-1-return-data` (dedicated — cross-cuts many files)

**Phase 1 closing**: `cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings && bash scripts/e2e/bridge-health-check.sh && bash scripts/e2e/flashloan-health-check.sh && bash scripts/e2e/test-multi-call-cross-chain.sh && bash scripts/refactor/replay_baseline.sh`

---

### Phase 2 — Break mammoth functions into pipelines / state machines

**Goal**: no function >200 LOC in `driver.rs`. Byte-identical behavior (verified vs baseline).

**2.1** **Mechanical split of `driver.rs` into submodules** (revised after Codex p3: originally with capability traits, discarded because with a single impl a trait enforces nothing — the doc comment is not enforcement. The real discipline comes from the concrete structs of 2.2-2.7). Structure:
```
driver/
  mod.rs           // struct Driver + new/run
  types.rs         // public structs (BuiltBlock, PendingBlock, etc.)
  step.rs          // impl Driver { step, step_sync, step_fullnode }
  step_builder.rs  // impl Driver { step_builder + helpers }
  flush.rs         // impl Driver { flush_to_l1 + helpers }
  verify.rs        // impl Driver { verify_local_block_matches_l1, apply_*_filtering }
  protocol_txs.rs  // impl Driver { build_builder_protocol_txs } + ProtocolTxPlan stages (from 2.4)
  rewind.rs        // impl Driver { rewind_l2_chain, set_rewind_target }
  journal.rs       // impl Driver { tx_journal save/load/prune }
```

Multiple `impl Driver` blocks across distinct files — Rust allows this without ceremony. Use `pub(super)` for cross-module methods. No internal traits.

**The real scope discipline** is achieved with the structs of 2.2-2.7: `BuilderTickContext`, `FlushPrecheck`, `FlushAssembly`, `FlushPlan<S>`, `VerificationDecision`, `ProtocolTxPlan<Stage>`, `ForwardAndTriggerPlan`. Each one receives only the sub-fields it needs by parameter, not the full `&mut Driver`.

*Files:* `crates/based-rollup/src/driver/*.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::`
*Branch:* `refactor/phase-2-driver-split` (dedicated)

**2.2** Extract a `BuilderTickContext` from `step_builder` with methods `derive_target_block`, `compute_mode_transition`, `load_l1_context`. Reduces the function's top-level size to ~200 LOC.
*Files:* `driver/step_builder.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_driver_mode_ driver::tests::test_target_l2_block_from_future_timestamp`
*Branch:* incremental

**2.3** Extract a `QueueDrain` with `drain_rpc_queues`, `merge_pending_entries`, `inject_held_l2_txs`. The function `inject_held_l2_txs` (`driver.rs:2970`) already exists.
*Files:* `driver/step_builder.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_pending_cross_chain_entries_accumulate driver::tests::test_cross_chain_entries_accumulate_across_blocks_before_flush`
*Branch:* incremental

**2.4** Extract a `ProtocolTxPlan` from `build_builder_protocol_txs` (`:3947`) **with typed stages** (v2 fix: not `Vec<TransactionSigned>` per stage). Each stage consumes and produces a typed state:
```rust
struct Draft; struct WithContext; struct WithTables; struct WithTriggers;

impl ProtocolTxPlan<Draft> {
    fn bootstrap(self, block_num: u64) -> ProtocolTxPlan<WithContext>;
}
impl ProtocolTxPlan<WithContext> {
    fn set_context(self, l1_block: u64) -> Self;
    fn load_tables(self, entries: &[CrossChainExecutionEntry]) -> ProtocolTxPlan<WithTables>;
}
impl ProtocolTxPlan<WithTables> {
    fn append_triggers(self, triggers: &[TriggerMetadata]) -> ProtocolTxPlan<WithTriggers>;
}
impl ProtocolTxPlan<WithTriggers> {
    fn build(self, nonce_base: u64) -> Vec<TransactionSigned>;
}
```
The stage order becomes impossible to get wrong.
*Files:* `driver/protocol_txs.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_gap_fill_block_at_block_1_uses_deployment_context driver::tests::test_builder_assigns_entries_only_to_last_block_in_batch`
*Branch:* incremental

**2.5** Extract from `verify_local_block_matches_l1` an `enum VerificationDecision { Match, Defer { reason }, MismatchRewind { target }, MismatchImmutable }` + a single method `Driver::rewind_to_re_derive(entry_block: u64)`. **Closes invariants #9 and #10.**
*Files:* `driver/verify.rs`, `driver/rewind.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_hold_cleared_on_verification_match driver::tests::test_immutable_ceiling_skips_verification driver::tests::test_full_rewind_cycle_state_transitions`
*Branch:* incremental

**2.6** Extract from `flush_to_l1` a `FlushPrecheck` with `check_cooldown`, `check_balance`, `drop_l1_confirmed`, `decide_rewind_on_root_mismatch`. Returns `enum PrecheckResult { Proceed, Skip, Rewind { target } }`.
*Files:* `driver/flush.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_flush_to_l1_respects_submission_cooldown driver::tests::test_l1_confirmed_anchor_rewind_uses_anchor`
*Branch:* incremental

**2.7** Extract a `FlushAssembly` with `collect_submission_entries`, `collect_forward_txs`, `collect_trigger_txs`, `compute_group_order`. Produces `FlushPlan<Collected>` (from step 1.7).
*Files:* `driver/flush.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_flush_to_l1_unified_submission driver::tests::test_flush_ordering_includes_forward_queued_l1_txs`
*Branch:* incremental

**2.7b** ⭐ **NEW (Codex p2): `ForwardAndTriggerPlan` + `TriggerExecutionResult`**. Today `flush_to_l1` does: submit postBatch → forward queued user txs → send triggers → await receipts → decide rewind on revert. That is 4 unmodelled responsibilities. **Closes invariant #15.**
```rust
pub struct ForwardAndTriggerPlan {
    pub queued_user_txs: Vec<Bytes>,
    pub triggers: Vec<TriggerMetadata>,
}

#[must_use]
pub enum TriggerExecutionResult {
    AllConfirmed { consumed: usize },
    ForwardFailed { tx_hash: B256, reason: String },
    TriggerReverted { entry_block: u64, needs_rewind: bool }, // invariant #15
    BuilderNonceStale(NonceResetRequired),
}
```
*Files:* `driver/flush.rs`, `proposer.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::tests::test_withdrawal_trigger_revert_rewind driver::tests::test_forward_queued_l1_txs`
*Branch:* incremental

**2.8** **Rewrite** `step_builder` and `flush_to_l1` as orchestrators of ~80 LOC each over two **complete** enums (v2 fix: v1 had FlushStage under-modelled):
```rust
enum BuilderStage {
    CatchUp,          // derive_next_batch and verify
    Drain,            // merge queues + inject held txs
    Build,            // build block (do not run if hold.is_blocking_build())
    MaybeFlush,       // decide whether to call flush_to_l1
    Done,
}

enum FlushStage {
    Precheck,             // 2.6 FlushPrecheck
    Collect,              // 2.7 FlushAssembly -> FlushPlan<Collected>
    ArmHold,              // 1.7 FlushPlan<HoldArmed>
    Submit,               // proposer.send_to_l1 — returns SendResult
    ForwardUserTxs,       // 2.7b forward queued_user_txs
    SendTriggers,         // 2.7b send triggers
    AwaitReceipts,        // 2.7b await + classify
    HandleTriggerResult,  // 2.7b: Confirmed | RevertedNeedsRewind | BuilderNonceStale
    ClearOrRewind,        // terminal
}
```
Byte-identical behavior vs baseline.
*Files:* `driver/step_builder.rs`, `driver/flush.rs`
*Verifies:* `cargo nextest run -p based-rollup driver::` + `bash scripts/refactor/replay_baseline.sh`
*Branch:* `refactor/phase-2-driver-pipelines` (dedicated — final step of the phase)

**Phase 2 closing**: `cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings && bash scripts/e2e/bridge-health-check.sh && bash scripts/e2e/crosschain-health-check.sh && bash scripts/e2e/flashloan-health-check.sh && bash scripts/e2e/double-deposit-withdrawal-trace.sh && bash scripts/refactor/replay_baseline.sh`

---

### Phase 3 — Unify `composer_rpc` L1↔L2 behind `Direction`

**Goal**: turn `l1_to_l2.rs` and `l2_to_l1.rs` into thin adapters over a single direction-parameterized engine. **This phase is only entered when 2.x is stable and the queued enums (1.4b) are merged.**

**3.0** ⭐ **MOVED from 4.7 + dependency inversion**: create `composer_rpc/sim_client.rs` with `trait SimulationClient` + real impl + mock for tests.

```rust
#[async_trait]
pub trait SimulationClient: Send + Sync + 'static {
    async fn eth_call_view(&self, req: CallRequest) -> Result<Bytes>;
    async fn debug_trace_call_many(&self, bundle: CallManyBundle) -> Result<CallManyResponse>;
    async fn get_block_context(&self, block: BlockId) -> Result<BlockContext>;
    async fn get_verification_key(&self, rollup_id: RollupId) -> Result<Bytes>;
    async fn get_rollup_state_root(&self, rollup_id: RollupId, block: BlockId) -> Result<B256>;
}

pub struct HttpSimClient { client: reqwest::Client, upstream: String }
impl SimulationClient for HttpSimClient { ... }  // real impl

#[cfg(any(test, feature = "test-utils"))]
pub struct InMemorySimClient { /* fixture-backed */ }
#[cfg(any(test, feature = "test-utils"))]
impl SimulationClient for InMemorySimClient { ... }
```

Both directions take `Arc<dyn SimulationClient>`. The composer_rpc tests load `InMemorySimClient` with fixtures from `tests/fixtures/traces/` (from 0.6) — they no longer require a real HTTP upstream.

**Rationale (order)**: if we first extract `discover_until_stable` (3.4) over the ad-hoc assembled JSON, we'll re-touch it when typing the client. Type + dependency-invert first.

*Files:* `composer_rpc/sim_client.rs` (new), `composer_rpc/common.rs`, `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`, tests
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* `refactor/phase-3-sim-client-trait` (dedicated)

**3.1** Create `composer_rpc/direction.rs` with sealed trait and associated types. The trait contains **only direction-specific facts and hooks** — no simulation policy (that lives in `simulate.rs` of step 3.6):
```rust
mod sealed { pub trait Sealed {} }

/// Result of classifying a trace call: it's either a forward (we discovered a new cross-chain call)
/// or a return (an edge that closes a previous call). A single function produces both.
pub enum ClassifiedCall {
    Forward(DiscoveredCall),
    Return(ReturnEdge),
}

pub trait Direction: sealed::Sealed {
    type RootCall;
    type ChildCall;
    type SimulationArtifact;

    fn name() -> &'static str;
    fn ccm_address_on_target_chain(&self) -> Address;

    // Direction-specific hooks used by discover_until_stable (3.4):
    /// Classifies a trace node as a forward call, return edge, or neither.
    fn classify_call(trace_call: &TraceCall) -> Option<ClassifiedCall>;

    /// Given a discovery round, produces the next requests for expansion.
    fn expand_round(round: &DiscoveryRound) -> Vec<ExpansionRequest>;

    /// Decides whether the discovered set should be promoted to multi-call continuation.
    /// Closes invariant #21: single L2→L1 + terminal return → PromoteToContinuation even if len()==1.
    fn promotion_rule(calls: &[DiscoveredCall], returns: &[ReturnEdge]) -> PromotionDecision;

    /// Builds the queue payload from the discovered set + simulation artifact.
    /// Used by 3.5. Closes invariant #6 (Simple vs WithContinuations).
    fn build_queue_payload(
        discovered: &DiscoveredSet,
        artifact: &Self::SimulationArtifact,
    ) -> QueuedCallRequest;
}

pub struct L1ToL2;  impl sealed::Sealed for L1ToL2 {}
pub struct L2ToL1;  impl sealed::Sealed for L2ToL1 {}
```
Scaffold + `impl Direction for L1ToL2` and `impl Direction for L2ToL1` with `panic!()` methods until subsequent steps fill them in. No logic migrated.
*Files:* `composer_rpc/direction.rs` (new), `composer_rpc/mod.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* incremental

**3.2** Extract shared models to `composer_rpc/model.rs`: `DiscoveredCall`, `ReturnEdge`, `DiscoveryRound`, `QueuePlan`, `SimulationArtifact`. Two source files, many imports, tests on both sides — requires a dedicated branch.
*Files:* `composer_rpc/model.rs` (new), `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc:: table_builder::`
*Branch:* `refactor/phase-3-composer-model` (dedicated — touches both 4k/5k LOC files)

**3.3** Extract **parent/child index rebasing** to a shared helper `rebase_parent_links(&mut [DiscoveredCall], offset: usize)` and delete the duplicated logic in `l1_to_l2.rs:3337`, `l2_to_l1.rs:4724`, `l2_to_l1.rs:4934`. **Fully closes invariant #7.**
*Files:* `composer_rpc/model.rs`, `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc:: table_builder::`
*Branch:* incremental

**3.4** Factor the **fixed-point discovery loop** of the two >1.6k LOC functions (`l1_to_l2.rs:2280` and `l2_to_l1.rs:3558`) into `discover_until_stable<D: Direction>`. **Complete spec**:
```rust
async fn discover_until_stable<D: Direction>(
    sim: &(dyn SimulationClient),     // trait from step 3.0
    initial: DiscoveryRound,
) -> Result<DiscoveredSet> {
    // 1. Run sim.debug_trace_call_many for the current round
    // 2. Extract cross-chain calls + return calls via D::classify_call
    // 3. Dedupe by (sender, target, calldata, value)
    // 4. Rebase parent_call_index toward absolute index (uses rebase_parent_links from 3.3)
    // 5. Apply D::promotion_rule(calls, returns) -> PromotionDecision
    // 6. Check convergence: did the round add no new calls?
    // 7. If not converged: D::expand_round(round) produces the next bundle
    // 8. Max rounds = MAX_RECURSIVE_DEPTH
}

pub struct DiscoveredSet {
    pub calls: Vec<DiscoveredCall>,
    pub returns: Vec<ReturnEdge>,
    pub promotion: PromotionDecision,  // propagated to simulate_delivery (3.6)
}
```
The hooks `classify_call`, `expand_round`, `promotion_rule` are the ones 3.1 already defines on `Direction`.
*Files:* `composer_rpc/discover.rs` (new), `direction.rs`, `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::` + `bash scripts/e2e/deploy-ping-pong.sh` + `bash scripts/refactor/replay_baseline.sh`
*Branch:* `refactor/phase-3-discover-direction` (dedicated — breaks the two largest functions of the crate)

**3.5** Factor queue payload construction behind `Direction::build_queue_payload` (declared in 3.1), **directly returning the variants of `QueuedCallRequest`** from step 1.4b. Replaces `l1_to_l2.rs:413`, `l2_to_l1.rs:407, :584, :649`.
*Files:* `composer_rpc/queue.rs` (new), `direction.rs`, `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc:: driver::tests::test_cross_chain_entries_`
*Branch:* dedicated

**3.6** Factor simulation with **`enum SimulationPlan` + function** (revised after Codex p3: originally `trait SimulationStrategy` with composable `OrElse` chain, discarded — with a fixed set of 3 strategies closed in the crate, enum + match is clearer). Unifies `l1_to_l2.rs:1144, 1887`, `l2_to_l1.rs:1633, 2477`. **Closes invariants #17 and #21.**

```rust
// composer_rpc/simulate.rs
pub enum SimulationPlan {
    Single,                      // single call, no fallback
    CombinedThenAnalytical,      // multi-call OR single with terminal return: combined + fallback
}

/// Decides the strategy based on input shape AND the `PromotionDecision` from the discover loop.
/// Closes invariants #17 AND #21: multi-call never single-call sim, and single+terminal-return
/// is promoted to continuation (does NOT stay in Single).
pub fn simulation_plan_for(
    calls: &[DiscoveredCall],
    promotion: PromotionDecision,  // comes from step 3.4 via DiscoveredSet
) -> SimulationPlan {
    match promotion {
        PromotionDecision::PromoteToContinuation => SimulationPlan::CombinedThenAnalytical,
        PromotionDecision::KeepSingle => {
            if calls.len() > 1 {
                SimulationPlan::CombinedThenAnalytical
            } else {
                SimulationPlan::Single
            }
        }
    }
}

/// Executes the plan. Single entry point.
pub async fn simulate_delivery<D: Direction>(
    sim: &(dyn SimulationClient),
    calls: &[DiscoveredCall],
    promotion: PromotionDecision,
    ctx: &SimContext,
) -> eyre::Result<SimulationArtifact> {
    match simulation_plan_for(calls, promotion) {
        SimulationPlan::Single => {
            single_call_sim::<D>(sim, &calls[0], ctx).await
        }
        SimulationPlan::CombinedThenAnalytical => {
            match combined_sim::<D>(sim, calls, ctx).await {
                Ok(artifact) => Ok(artifact),
                Err(e) => {
                    tracing::warn!(target: "composer_rpc::sim", %e, "combined sim failed, falling back to analytical");
                    analytical_fallback::<D>(sim, calls, ctx).await
                }
            }
        }
    }
}
```

**Explicitly replaces** the rules "NEVER per-call simulate_l1_delivery for multi-call" (#17) AND "single L2→L1 + terminal return promotes to multi-call" (#21). Both invariants now live in a **single point**: `simulation_plan_for`. The `PromotionDecision` is produced by `Direction::promotion_rule` in 3.1 and travels through the `DiscoveredSet` from `discover_until_stable` (3.4).

The three private functions (`single_call_sim`, `combined_sim`, `analytical_fallback`) live in `simulate.rs` as free functions with identical signatures. Testable in isolation without trait impl ceremony.

*Files:* `composer_rpc/simulate.rs` (new), `direction.rs`, `l1_to_l2.rs`, `l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc:: table_builder::` + `bash scripts/e2e/flashloan-health-check.sh`
*Branch:* dedicated

**3.7** Reduce `l1_to_l2.rs` and `l2_to_l1.rs` to **thin adapters**: HTTP ingress + `impl Direction` + response shaping. Target: each file <800 LOC (down from 4345/5459).
*Files:* `composer_rpc/l1_to_l2.rs`, `composer_rpc/l2_to_l1.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::` + full E2E smoke + `bash scripts/refactor/replay_baseline.sh`
*Branch:* dedicated

**Phase 3 closing**: full smoke (`bridge`, `crosschain`, `flashloan`, `multi-call-cross-chain`, `conditional-cross-chain`, `test-depth2-generic`, `deploy-ping-pong-return`) + `replay_baseline.sh`.

---

### Phase 4 — Layer separation in `composer_rpc/`

**Goal**: no file in `composer_rpc/` mixes more than one responsibility.

**4.1** Mechanical split of each direction into submodules:
```
composer_rpc/
  l1_to_l2/
    mod.rs
    server.rs    // HTTP ingress (delegates to common server.rs from step 4.2)
    direction_impl.rs  // impl Direction for L1ToL2
    response.rs  // response shaping
  l2_to_l1/
    mod.rs ...
```
Mechanical movement.
*Files:* `composer_rpc/l1_to_l2/*`, `composer_rpc/l2_to_l1/*`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* `refactor/phase-4-composer-split` (dedicated)

**4.2** Move JSON-RPC parsing/response to `composer_rpc/server.rs` (generic handler). Each `l1_to_l2/server.rs` and `l2_to_l1/server.rs` is left with the minimal classification of which method belongs to that direction.
*Files:* `composer_rpc/server.rs` (new), `common.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* incremental

**4.3** Move decode/RLP helpers (`l1_to_l2.rs:4198` and equivalents) to `composer_rpc/tx_codec.rs`.
*Files:* `composer_rpc/tx_codec.rs` (new)
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::`
*Branch:* incremental

**4.4** **Owner CHOSEN (Codex p2): `cross_chain.rs`** — consolidate ALL selectors and ABI parsers in the existing `sol!` block. Eliminate duplicated literals. Add a CI gate: `grep -rn "0x[a-f0-9]\{8\}" crates/based-rollup/src/*.rs crates/based-rollup/src/composer_rpc/` MUST NOT find selectors outside the `sol!` block. **Closes invariant #23.**
*Files:* `cross_chain.rs`, every file with hardcoded selectors, `.github/workflows/ci.yml` (grep gate)
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::trace:: cross_chain::`
*Branch:* incremental

**4.5** Split `composer_rpc/trace.rs` into `trace/{walker,proxy,types}.rs` **AND at the same time** replace `serde_json::Value` with typed serde structs `TraceNode`, `TraceCallFrame`, `CallManyResponse` (ex-5.2, moved here by Codex p2). Touching the same subsystem twice (split + typing) is wasted churn — done together.
*Files:* `composer_rpc/trace/{walker,proxy,types}.rs`
*Verifies:* `cargo nextest run -p based-rollup composer_rpc::trace::`
*Branch:* `refactor/phase-4-trace-typed` (dedicated)

**4.6** Create `composer_rpc/entry_builder.rs` as the **single boundary** to the builders of `cross_chain.rs` / `table_builder.rs` from step 1.9. **It is a façade over 1.9**, not a new layer. Both directions call `EntryBuilder::immediate(...)`, `EntryBuilder::deferred(...)`, `EntryBuilder::continuation(...)`, which delegate to `ImmediateEntryBuilder`/`DeferredEntryBuilder`/`L2ToL1ContinuationBuilder`.
*Files:* `composer_rpc/entry_builder.rs` (new)
*Verifies:* `cargo nextest run -p based-rollup table_builder:: cross_chain:: composer_rpc::`
*Branch:* dedicated

**Phase 4 closing**: full smoke + `bash scripts/refactor/replay_baseline.sh`. Measure LOC: `l1_to_l2/` total and `l2_to_l1/` total (target directional — see adjusted §12).

---

### Phase 5 — Hardening (reduced)

**Goal**: most of the hardening already happened in Phases 1-4 (per Codex p2's instruction). This phase only removes the remaining `unwrap()`s, adds proptest/fuzz, and runs the final replay.

**5.1** Remove `unwrap()/expect()` from production. After 4.5, `trace.rs` will no longer have the bulk of unwraps (typed structs eliminate most). We count the remaining ones and attack them with `eyre::WrapErr`.
*Files:* whichever still have `unwrap()` after 4.5
*Verifies:* `cargo nextest run -p based-rollup && cargo clippy --workspace --all-features -- -D warnings -W clippy::unwrap_used`
*Branch:* incremental

**5.4** Add `proptest`/fuzz for: trace parser (post-4.5 typing), parent link rebasing (post-3.3), entry roundtrip (encode → decode → equal), mirror invariant (from the `MirrorCase` DSL of 0.5).
*Files:* `cross_chain_tests.rs`, `table_builder_tests.rs`, `composer_rpc/trace/`, `tests/fixtures/mirror_case.rs`
*Verifies:* `cargo nextest run -p based-rollup`
*Branch:* incremental

**5.7** **Final gate before merging to main**: `scripts/refactor/replay_baseline.sh` (created in 0.8) must produce **0 diffs** against the pre-refactor baseline. The E2Es already cover the happy paths; this gate verifies **byte-equivalence** with the previous system. Commit the final output to `docs/refactor/REPLAY_RESULTS.md`.
*Files:* `docs/refactor/REPLAY_RESULTS.md`
*Verifies:* `cargo build --release && cargo nextest run --workspace && cargo clippy --workspace --all-features -- -D warnings && bash scripts/refactor/replay_baseline.sh`
*Branch:* `refactor/release-prep` (dedicated)

**Note (Codex p2/p4)**: ex-step `5.2` (typed trace nodes) was merged into `4.5`. Ex-`5.3` (typed RPC structs) is covered by sim_client (3.0) + queued enums (1.4b) — no longer needs a separate step. Ex-`5.5` (checked constructors + debug_assert) already lives inside 1.2/1.9 where the types are created. Ex-`5.6` (enums in rpc.rs) was moved to **1.4b** (Codex p4 advanced it to resolve the dependency with EntryQueue from 1.6b+c).

**Phase 5 closing**: refactor complete. Final PR merges with a **merge commit** (not squash — preserves per-step revertibility per §3).

---

## 9. Progress tracker

> Update the checkbox at the close of each step. ⭐ = critical step / change from v1 by Codex p2.

| Phase | # | Step | Branch type | Status | Invariants |
|---|---|---|---|---|---|
| 0 | 0.1 | ARCH_MAP.md | incremental | ☐ | — |
| 0 | 0.2 | INVARIANT_MAP.md | incremental | ☐ | (documents all) |
| 0 | 0.3 | property tests filtering (cross_chain + derivation) | incremental | ☐ | #4, #16 |
| 0 | 0.4 | property test reorder_for_swap_and_pop | incremental | ☐ | — |
| 0 | 0.5 | MirrorCase DSL + mirror tests | incremental | ☐ | #18 |
| 0 | 0.6 | trace fixtures | incremental | ☐ | — |
| 0 | 0.7 | hold/rewind tests (incl. withdrawal revert) | incremental | ☐ | #15 |
| 0 | 0.8 | ⭐ capture baseline script | incremental | ☐ | — |
| 1 | 1.1a | RollupId newtype (scaffold) | incremental | ✅ | — |
| 1 | 1.1b | RollupId migration | dedicated | ✅ | — |
| 1 | 1.1c | ScopePath newtype | incremental | ✅ | — |
| 1 | 1.2 | state root newtypes (module-private + boundary) | dedicated | ✅ | #3 |
| 1 | 1.3 | ParentLink enum | dedicated | ✅ | #7 (partial — single helper lives in 3.3) |
| 1 | 1.4 | TxOutcome + EntryGroupMode + EntryClass | dedicated | ✅ | #5 |
| 1 | 1.4b | ⭐ QueuedCallRequest enums (moved from 1.11, Codex p4) | dedicated | ✅ | #6 |
| 1 | 1.5 | PendingL1SubmissionQueue + BlockEntryMix | incremental | ✅ | #11 |
| 1 | 1.6 | EntryVerificationHold + builder halt gate | incremental | ✅ | #14 |
| 1 | 1.6b+c | ⭐ EntryQueue struct + ForwardPermit token (merged, Codex p4) | dedicated | ⏸ deferred | #13 (behaviorally closed by existing hold-then-forward; type wrapper not yet added) |
| 1 | 1.7 | ⭐ FlushPlan typestate (NoEntries/Collected/HoldArmed + SendResult) | dedicated | ✅ | #1 |
| 1 | 1.8 | L1NonceReservation + ProofContext + L1Client wrapper | incremental | ✅ | #2, #22 (L1Client wrapper deferred — narrow scope) |
| 1 | 1.9a | ImmediateEntryBuilder + CallOrientation | incremental | ✅ (CallOrientation only) | #19 |
| 1 | 1.9b | DeferredEntryBuilder + RevertGroupBuilder | incremental | ⏸ deferred | #8 (logic correct, no type wrapper yet) |
| 1 | 1.9c | L2ToL1ContinuationBuilder (with_scope_return mandatory) | incremental | ⏸ deferred | #12 (logic correct, no type wrapper yet) |
| 1 | 1.10 | ⭐ ReturnData enum (Void / NonVoid) | dedicated | ✅ (scaffold only) | #20 (enum defined, field cascade deferred) |
| 2 | 2.1 | Driver mechanical split (12 commits: mod.rs 5509→971, 11 sibling files) | dedicated | ✅ | — |
| 2 | 2.2 | BuilderTickContext | incremental | ⏸ deferred to Phase 2b | — |
| 2 | 2.3 | QueueDrain | incremental | ⏸ deferred to Phase 2b | — |
| 2 | 2.4 | ProtocolTxPlan with typed stages | incremental | ⏸ deferred to Phase 2b (design work, not mechanical) | — |
| 2 | 2.5 | VerificationDecision + rewind_to_re_derive | incremental | ✅ | #9, #10 |
| 2 | 2.6 | FlushPrecheck | incremental | ✅ (FlushPrecheckResult enum) | — |
| 2 | 2.7 | FlushAssembly → FlushPlan<Collected> | incremental | ✅ folded into 1.7 | — |
| 2 | 2.7b | ⭐ ForwardAndTriggerPlan + TriggerExecutionResult | incremental | ✅ (TriggerExecutionResult) | #15 |
| 2 | 2.8 | step_builder/flush_to_l1 as orchestrators (complete FlushStage) | dedicated | ⏸ deferred to Phase 2b (needs 2.2-2.4 first) | — |
| 3 | 3.0 | ⭐ trait SimulationClient + HttpSimClient + InMemorySimClient | dedicated | ⏸ deferred | — |
| 3 | 3.1 | Sealed trait Direction | incremental | ⏸ deferred | — |
| 3 | 3.2 | shared composer_rpc/model.rs | dedicated | ⏸ deferred | — |
| 3 | 3.3 | rebase_parent_links single helper | incremental | ⏸ deferred | #7 (second half) |
| 3 | 3.4 | discover_until_stable (complete spec) | dedicated | ⏸ deferred | — |
| 3 | 3.5 | build_queue_payload (uses 1.4b enums) | dedicated | ⏸ deferred | — |
| 3 | 3.6 | SimulationPlan enum + simulate_delivery() function | dedicated | ⏸ deferred | #17, #21 |
| 3 | 3.7 | directions as thin adapters | dedicated | ⏸ deferred | — |
| 4 | 4.1 | composer_rpc split | dedicated | ⏸ deferred | — |
| 4 | 4.2 | generic server.rs | incremental | ⏸ deferred | — |
| 4 | 4.3 | tx_codec.rs | incremental | ⏸ deferred | — |
| 4 | 4.4 | selectors in cross_chain.rs (chosen owner) + CI grep gate | incremental | ✅ (CI gate; codebase was already clean) | #23 |
| 4 | 4.5 | ⭐ trace split + typed structs (ex-5.2 merged) | dedicated | ⏸ deferred | — |
| 4 | 4.6 | entry_builder.rs (façade over 1.9) | dedicated | ⏸ deferred | — |
| 5 | 5.1 | remove residual unwraps | incremental | ✅ | — |
| 5 | 5.4 | proptest / fuzz | incremental | ⏸ deferred | — |
| 5 | 5.7 | replay baseline gate vs 0.8 | dedicated | ⏸ deferred | blocks merge to main |

### Current closure status (invariants 1-23)

| # | Invariant | Status | Closure vehicle |
|---|---|---|---|
| 1 | Hold before send_to_l1 | ✅ compile-time | `FlushPlan<Sendable>` typestate (1.7) |
| 2 | Never auto-nonce; reset on failure | ✅ compile-time | `NonceResetRequired` + `#[must_use]` (1.8) |
| 3 | Never fabricate pre_state_root | ✅ compile-time | `CleanStateRoot` + boundary ctors (1.2) |
| 4 | §4f prefix-counting (not all-or-nothing) | ⏸ behavioral | `compute_consumed_trigger_prefix` logic; proptest deferred |
| 5 | Continuation entries ≠ triggers | ✅ compile-time | `EntryClass` + `partition_entries` (1.4) |
| 6 | Result skipped with continuations | ✅ compile-time | `QueuedCrossChainCall::WithContinuations` no `result_entry` field (1.4b) |
| 7 | Rebase parent_call_index | ✅ compile-time (types) + ⏸ (single helper 3.3) | `ParentLink` + `AbsoluteCallIndex` (1.3) |
| 8 | First trigger needs clean root | ⏸ behavioral | reorder_for_swap_and_pop logic |
| 9 | Deferral exhaustion → rewind | ✅ compile-time | `MismatchDeferExhausted` only path (2.5) |
| 10 | Rewind target is entry_block - 1 | ✅ compile-time | `rewind_to_re_derive` helper + single saturating_sub site (2.5) |
| 11 | Deposits+withdrawals coexist | ✅ compile-time | `BlockEntryMix` (1.5) |
| 12 | Scope navigation on continuation Entry 1 | ⏸ behavioral | builder logic (1.9c deferred) |
| 13 | Hold-then-forward awaits confirmation | ⏸ behavioral | composer_rpc hold logic (1.6b+c deferred) |
| 14 | Builder halts during hold | ✅ compile-time | `hold.is_blocking_build()` gate (1.6) |
| 15 | Trigger revert → rewind | ✅ compile-time | `TriggerExecutionResult` + `#[must_use]` (2.7b) |
| 16 | §4f filtering is generic | ⏸ behavioral | unified `filter_block_entries` function |
| 17 | Never per-call sim for multi-call L2→L1 | ⏸ behavioral | `simulate_l1_combined_delivery` routing (3.6 deferred) |
| 18 | L1/L2 structures mirror | ⏸ behavioral | mirror tests deferred (0.5, 3.2) |
| 19 | Never swap (dest, source) for L1→L2 return | ✅ compile-time | `CallOrientation` enum (1.9a) |
| 20 | ReturnData Void vs NonVoid | ✅ compile-time (scaffold) | `ReturnData` enum (1.10) |
| 21 | Single + terminal return → promote | ⏸ behavioral | `bool` condition (3.6 deferred) |
| 22 | publicInputsHash uses timestamp | ✅ compile-time | `ProofContext.block_timestamp` (1.8) |
| 23 | Never hardcode selectors | ✅ CI gate | `scripts/refactor/check-no-hardcoded-selectors.sh` (4.4) |

**Compile-time closures: 14/23** (1, 2, 3, 5, 6, 7, 9, 10, 11, 14, 15, 19, 20, 22) — any violation produces a build error.
**CI gates: 1/23** (23) — any regression breaks the no-hardcoded-selectors job.
**Behavioral-only: 8/23** (4, 8, 12, 13, 16, 17, 18, 21) — invariant is preserved by the code but is not gated by a type or CI check; waiting for deferred refactor steps (primarily Phase 3 composer unification).

---

## 10. Execution steps (what this plan does immediately after approval)

1. Create branch `refactor/phase-0-mapping`.
2. Create `docs/refactor/PLAN.md` with the contents of this file.
3. Create `docs/refactor/ARCH_MAP.md` (step 0.1).
4. Create `docs/refactor/INVARIANT_MAP.md` (step 0.2, with the 23 expanded rows from §6).
5. Initial atomic commit: `docs(refactor): introduce refactor plan, architecture map, and invariant map`.
6. Report to the user and await explicit instruction to start 0.3 (property tests).

> The plan is approved now; the implementation steps are executed afterwards under explicit per-step user approval (not per-phase — each step requires "go").

## 11. Risks and mitigations (12 risks, expanded by Codex p2)

| Risk | Mitigation |
|---|---|
| **Rebase hell** between dedicated branches that touch `driver.rs`, `l1_to_l2.rs`, `l2_to_l1.rs` | Merge dedicated branches to `main` in strict plan order. Do not keep >2 dedicated branches open at the same time. Each branch rebases on `main` before merge — if there's a >1h conflict, STOP and review the order |
| Breaking byte-equivalence of derivation | Baseline capture in 0.8 + replay gate at the close of each phase + final gate in 5.7 |
| A phase ends half-done leaving the code in a worse state | Each step is independently committable; explicit halt conditions; defined in §3 that an incomplete phase rollbacks via `git revert <merge-commit>` in reverse order |
| Viral typestate forcing changes to public APIs | Limit typestate to `FlushPlan`, `EntryVerificationHold`, `ForwardPermit`, `ProtocolTxPlan`, `L1NonceReservation` — all internal to driver/proposer/composer_rpc, not part of the public `SyncRollupsApi` trait |
| **Dynamic dispatch in `SimulationClient`** (the only `dyn Trait` of the refactor, step 3.0) | Negligible: the composer is IO-bound (HTTP), calls are few per block. Optional measurement with `criterion` (the `throughput` benchmark already exists) at the close of Phase 3. If there's a >5% regression, convert to static generic |
| **Temptation to add traits during execution**: while running step 2.x or 3.x someone may think "this would be cleaner with a trait" | Apply the §4b rule: (1) Are there ≥2 real non-mock impls? (2) Is the second one a real backend? If either is "no", use a concrete struct |
| **`EntryQueue` with `Notify` + `BTreeMap`** has delicate concurrent semantics: lost wakeups, double confirmation, stale receipts | (a) `QueueReceipt` is a monotonic counter, not a positional index, so old receipts are detectable. (b) Waiters re-check state after wake (don't assume wake = confirmed). (c) Specific test: 1000 concurrent push + 1000 wait_confirmation from separate tasks, asserting that none hangs nor receives a double permit. (d) `loom` property test (optional, only if a real bug emerges) |
| **Drift between `Direction::promotion_rule` and `simulation_plan_for`**: the promotion rule lives in 3.1, the plan selection rule in 3.6, they may diverge | Cross test: `promotion_rule` and `simulation_plan_for` are called together in `discover_until_stable` (3.4); a test that verifies that for 6 canonical scenarios (from mirror_case.rs) the `(promotion, plan)` combination is correct. This catches drift on the first commit that breaks coherence |
| **`L1Client` wrapper bypass**: someone inside `proposer.rs` adds a direct `RootProvider` access, losing the discipline | Local lint: `clippy::disallowed_methods` with a rule that `RootProvider::*` may only be called inside `impl L1Client { ... }`. Verifiable with `cargo clippy --workspace --all-features -- -D warnings` (already in the phase closing) |
| **Plan text staleness**: during the review loop, stale text remains like `SimulationStrategy`, `SimClient`, `QueueClient` that never exists in code | Convention: each iteration of the loop runs `grep -n` for discarded names (`SimulationStrategy`, `BuilderPhase`, `EntryQueue trait`, `L1Provider trait`) in the plan and verifies 0 matches before closing the iteration |
| Premature over-abstraction of the Direction trait | The plan order respects: guardrails → types → pipelines → only then Direction (MVP path in §7) |
| **Inconsistency revert vs squash**: §3 requires `git revert` per step but v1 closed with squash | Fixed in v2: **mandatory merge commit** at the close of each phase. Squash forbidden except for clearly atomic steps such as 0.1/0.2 |
| **Timing flakiness**: hold-then-forward, receipt polling, `block.timestamp` in `publicInputsHash`, `postBatch/createProxy/trigger` sequencing | (a) E2E with retries on convergence, not on punctual symptoms; (b) `capture_baseline.sh` runs each scenario 3 times and verifies determinism before saving; (c) do not touch proposer timing in Phase 1/2 |
| **Stale devnet state** blocks recovery between steps | Devnet reset (`docker compose down -v`) **only with explicit per-step user approval**, ideally at the close of each phase. Changes to entries/postBatch formats require a prior reset |
| **Development ECDSA prover**: changes to timestamp prediction or proof context may cause intermittent E2E breakage | Step 1.8 (`ProofContext`) centralizes the computation; any timestamp prediction change is audited against `scripts/refactor/capture_baseline.sh`. Do not touch `sign_proof` outside 1.8 |
| **Drift of JSON trace fixtures** if the tracer/reth format changes | Pinned reth version in Docker (`fix/pin-reth-v1.11.3` current branch). Fixtures regenerated with a specific command (`scripts/refactor/regen_trace_fixtures.sh`) when reth changes |
| **Drift of the protocol submodule** would invalidate the baseline | The protocol submodule is pinned (`contracts/sync-rollups-protocol`) and the baseline records the submodule git hash in `tests/baseline/_meta.json`. Replay gate (5.7) refuses to compare against a baseline whose submodule SHA differs from the current checkout |
| **False confidence from filtered `nextest`**: the plan's filters do not cover integrated `driver ↔ rpc ↔ composer` paths | At the close of each phase the full `cargo nextest run --workspace` + E2E suite is run — not just the per-step filters. The filters are for fast iteration |
| Refactor consumes user / agent context | Each step <1 day of work. Small PRs. Merge continuously. Each phase has ≤12 numbered steps maximum |

## 12. Definition of Done (realistic, corrected by Codex p2)

**Refactor scope** (what this plan attacks):
- ✅ No function in `driver/` >200 LOC
- ✅ `composer_rpc/l1_to_l2.rs` + `l1_to_l2/` subdirectory ≤ 2000 LOC total (down from 4345) — **≥50% reduction**
- ✅ `composer_rpc/l2_to_l1.rs` + `l2_to_l1/` subdirectory ≤ 2500 LOC total (down from 5459) — **≥50% reduction**
- ✅ The 23 invariants of §6 encoded with one of these two criteria (Codex p4 fix: not all are compile-time):
  - **Compile-time**: violation produces a compiler error. Applies to: #1 (FlushPlan typestate), #2 (NonceResetRequired), #3 (CleanStateRoot constructors), #5 (EntryClass enum), #6 (QueuedCallRequest variants), #7 (ParentLink), #11 (BlockEntryMix), #12 (mandatory with_scope_return), #13 (ForwardPermit), #14 (is_blocking_build gate), #19 (CallOrientation), #20 (ReturnData), #21 (PromotionDecision), #22 (ProofContext).
  - **Test or CI gate**: violation produces a test/lint failure. Applies to: #4 (prefix monotonic property test), #8 (test "first trigger needs clean root"), #9 + #10 (test "deferral exhaustion rewinds"), #15 (test "withdrawal trigger revert rewinds"), #16 (test "filter is generic, not Bridge selectors"), #17 (test "single-call sim is not picked for multi-call"), #18 (mirror tests with MirrorCase DSL), #23 (CI grep gate against selectors).
- ✅ For each invariant of §6, **at least one of the two criteria passes green**.
- ✅ 0 `unwrap()` / `expect()` in production (`clippy::unwrap_used` deny)
- ✅ `cargo nextest run --workspace` green
- ✅ `cargo clippy --workspace --all-features -- -D warnings` green
- ✅ Full E2E suite (`scripts/e2e/`) green on devnet-eez
- ✅ `scripts/refactor/replay_baseline.sh` 0 diffs against the 0.8 baseline
- ✅ `docs/refactor/INVARIANT_MAP.md` with all rows in the "future type" column = ✓
- ✅ CI anti-hardcoded-selectors grep gate green (4.4)
- ✅ **`SimulationClient` trait** with ≥2 impls (`HttpSimClient` + `InMemorySimClient` with fixtures). `composer_rpc` tests run with `InMemorySimClient` without needing an upstream reth.
- ✅ **`Direction` trait** (sealed) with ≥2 impls (`L1ToL2`, `L2ToL1`) and the `l1_to_l2.rs`/`l2_to_l1.rs` files reduced to thin adapters (see LOC criteria above).
- ✅ **Discarded traits documented**: §4b explicitly lists the considered and rejected traits (`L1Provider` as a wide trait, `EntryQueue` trait, capability traits, `SimulationStrategy` trait + `OrElse`). The §4b operational rule is respected throughout the refactor code.

**Out of scope** (what this plan does NOT promise — requires an explicit Phase 6):
- ❌ Splitting `cross_chain.rs` (currently 2410 LOC)
- ❌ Splitting `table_builder.rs` (currently 2524 LOC)
- ❌ Large functions in those two files (`build_cross_chain_call_entries`, `build_l2_to_l1_call_entries`, `build_continuation_entries`, `build_l2_to_l1_continuation_entries`, `reconstruct_continuation_l2_entries`, `attach_chained_state_deltas`)
- ❌ Removing the development ECDSA prover
- ❌ Splitting `driver.rs` if it ends up >2000 LOC per file after 2.1 (it should stay below, but that is not a DoD criterion)
- ❌ Refactor of `rpc.rs` beyond what is needed for 1.4b

If after the refactor you want to attack what's "out of scope", we open a Phase 6 with its own plan. That is not this work.
