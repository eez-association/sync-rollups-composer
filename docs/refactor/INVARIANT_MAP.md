# Invariant Map — `sync-rollup-composer`

> **Purpose**: expanded table of the 23 critical invariants that the refactor must encode in the type system or in dedicated tests/CI gates. Each row documents the current owner (human), the current test (if any), the future type, and the PLAN phase that closes it.
>
> **Generated**: 2026-04-08 (PLAN step 0.2, branch `refactor/phase-0-mapping`).
>
> **Source**: `CLAUDE.md > Lessons Learned — Hard-Won Rules`. This map is the contract of the refactor — when every invariant has ✓ in "future type implemented", the refactor is closed.

---

## Legend

- **Compile-time** = the type system rejects the violation with a compile error.
- **Test/gate** = the violation produces a test failure (unit, property, E2E) or a CI lint (clippy, grep).
- The two final columns (`Future type`, `Status`) are updated as the refactor progresses.

---

## Invariant table

| # | Invariant | Current owner (human) | Current test | Future type | Closure type | Phase | Status |
|---|---|---|---|---|---|---|---|
| 1 | Hold MUST be set BEFORE send_to_l1 | Comment in `flush_to_l1` (`driver.rs:1796`) and discipline | `test_hold_set_only_when_entries_non_empty` | `FlushPlan<HoldArmed>` typestate with `entry_block` inside the plan; `send_to_l1` only accepts `Sendable` | Compile-time | 1.7 | ☐ |
| 2 | NEVER use auto-nonce, always reset on failure | Comment in `proposer.rs` | `test_submission_failure_sets_cooldown` | `L1NonceReservation` + `#[must_use] NonceResetRequired` | Compile-time | 1.8 | ☐ |
| 3 | NEVER align state roots by overwriting pre_state_root | Comment in `flush_to_l1` | (no direct test, only replay baseline) | `CleanStateRoot(B256)` newtype with `pub(crate)` constructor + explicit `from_*_boundary` | Compile-time | 1.2 | ☐ |
| 4 | §4f filtering is per-call prefix counting, never all-or-nothing | Comment in `derivation.rs` and `cross_chain.rs:2237` (`compute_consumed_trigger_prefix`) | (no property test) | `ConsumedPrefix(usize)` + monotonicity property test | Test/gate | 0.3 | ☐ |
| 5 | Continuation entries are NOT triggers (`hash(next_action) != action_hash`) | Comment in `partition_entries` (`cross_chain.rs:2022`) | `test_partition_entries_continuation_not_trigger` | `enum EntryClass { Trigger, Continuation, Result, RevertContinue }` | Compile-time | 1.4 | ☐ |
| 6 | Result entry skipped when `extra_l2_entries` non-empty | Double check in driver and rpc | `test_convert_l1_entries_skips_result_with_continuations` | `enum QueuedCallRequest::{Simple, WithContinuations}` — the `Simple` variant carries `result_entry`, `WithContinuations` does not allow it | Compile-time | 1.4b | ☐ |
| 7 | `parent_call_index` must be rebased after combined_delivery | Comment in `composer_rpc/l2_to_l1.rs:4724` | (no test) | `enum ParentLink { Root, Child(AbsoluteCallIndex) }` + single helper `rebase_parent_links` | Compile-time | 1.3 + 3.3 | ☐ |
| 8 | First TRIGGER entry needs `currentState=clean` (post swap-and-pop reorder) | Imperative logic in `attach_chained_state_deltas` (`cross_chain.rs:1520`) | `test_first_trigger_needs_clean_root_after_reorder` | `RevertGroupBuilder::first_trigger_idx()` correctly computed, with dedicated test | Test/gate | 1.9b | ☐ |
| 9 | Deferral exhaustion → rewind, not accept | Imperative logic in `verify_local_block_matches_l1` (`driver.rs:3204`) | `test_full_rewind_cycle_state_transitions` | `enum VerificationDecision::MismatchRewind { target }` | Test/gate | 2.5 | ☐ |
| 10 | Rewind target is `entry_block - 1` | Comment | (covered by `test_full_rewind_cycle_state_transitions`) | Single method `Driver::rewind_to_re_derive(entry_block)` that computes target inside | Test/gate | 2.5 | ☐ |
| 11 | Deposits + withdrawals can coexist in same block | Removed mutual exclusion check | `test_unified_deposit_withdrawal_block` | `enum BlockEntryMix { Empty, OnlyD, OnlyW, Mixed }` exported by `PendingL1SubmissionQueue` | Compile-time | 1.5 | ☐ |
| 12 | Multi-call L2→L1 must use scope navigation on Entry 1 | Comment in `table_builder.rs:1612` | `test_l2_to_l1_continuation_uses_scope_return` | `L2ToL1ContinuationBuilder::with_scope_return(scope)` required — `build()` fails without it | Compile-time | 1.9c | ☐ |
| 13 | **Hold-then-forward: composer RPCs MUST await queue confirmation** | Comment in `composer_rpc/*` | `test_composer_holds_until_queue_confirmation` | `ForwardPermit` token only constructed on the `Reserved → Confirmed` transition post-FCU | Compile-time | 1.6b+c | ☐ |
| 14 | **Builder HALTS block production while hold is active** | Comment in `step_builder` | `test_builder_halts_while_hold_active` | `EntryVerificationHold::is_blocking_build()` consulted in `BuilderStage::Build` | Compile-time | 1.6 | ☐ |
| 15 | **Withdrawal trigger revert on L1 causes REWIND** | Logic in `flush_to_l1` post-submit | `test_withdrawal_trigger_revert_rewind` (added in PLAN step 0.7) | `enum TriggerExecutionResult::RevertedNeedsRewind(entry_block)` `#[must_use]` (compile-time half lands in step 2.7b) | Test/gate ☑ + Compile-time ☐ | 0.7 (test) + 2.7b (type) | ☐ |
| 16 | **§4f filtering is generic (CrossChainCallExecuted events), NOT Bridge selectors** | `cross_chain.rs:2172` (`identify_trigger_tx_indices`) — already generic, no selectors | `test_filter_uses_event_not_selector` | Test verifying that `extract_l2_to_l1_tx_indices` does not contain hex strings of selectors | Test/gate | 0.3 | ☐ |
| 17 | **NEVER per-call simulate_l1_delivery for multi-call L2→L1** | Comment in `composer_rpc/l2_to_l1.rs` | `test_multi_call_uses_combined_sim` | `enum SimulationPlan` + `simulation_plan_for(calls, promotion)` single decision point | Compile-time | 3.6 | ☐ |
| 18 | **L1 and L2 entry structures must MIRROR** | CLAUDE.md comment | (zero current coverage) | Mirror tests with `MirrorCase` DSL in `src/test_support/mirror_case.rs` | Test/gate | 0.5 | ☐ |
| 19 | **NEVER swap (dest, source) for L1→L2 return call children** | Comment in `table_builder.rs` | `test_l1_to_l2_return_call_no_swap` | `enum CallOrientation { Forward, Return }` + single helper `address_pair_for(orientation)` | Compile-time | 1.9a | ☐ |
| 20 | **Return data shape: Void = 0 bytes; delivery_return_data → hashes; l2_return_data → scope resolution** | Multiple CLAUDE.md comments (#245, #246) | (partially) | `enum ReturnData { Void, NonVoid(Bytes) }` propagated by all builders | Compile-time | 1.10 | ☐ |
| 21 | **Single L2→L1 + terminal return still promotes to multi-call continuation** | Bool condition in `composer_rpc/l2_to_l1.rs` | `test_single_call_terminal_return_promotes` | `enum PromotionDecision { KeepSingle, PromoteToContinuation }` returned by `Direction::promotion_rule` and consumed by `simulation_plan_for` | Compile-time | 3.1 + 3.6 | ☐ |
| 22 | **`publicInputsHash` uses block.timestamp, not block.number** | Code in `proposer.rs` + proxy sim | (covered by replay baseline) | `ProofContext { block_timestamp: U256, … }` mandatorily accepted by `sign_proof` | Compile-time | 1.8 | ☐ |
| 23 | **NEVER hardcode function selectors — `sol!` only** | Convention (CLAUDE.md) | (no current gate) | CI grep gate: `grep -rn "0x[a-f0-9]\{8\}" crates/based-rollup/src/` must return 0 matches outside the `sol!` block | Test/gate | 4.4 | ☐ |

---

## Summary by closure type

| Type | Count | Invariants |
|---|---|---|
| **Compile-time** | 14 | #1, #2, #3, #5, #6, #7, #11, #12, #13, #14, #17, #19, #20, #21, #22 |
| **Test/gate** | 9 | #4, #8, #9, #10, #15, #16, #18, #23 |

**Total**: 23 invariants. (#15 is listed under test/gate but the `enum TriggerExecutionResult` also gives it a partial compile-time check; the E2E test is the canonical closure.)

## Summary by phase

| Phase | Invariants closed |
|---|---|
| 0 (Guardrails) | #4, #16, #18 |
| 1 (Types) | #1, #2, #3, #5, #6, #7, #8, #11, #12, #13, #14, #19, #20, #22 |
| 2 (Pipelines) | #9, #10, #15 |
| 3 (Direction) | #17, #21 |
| 4 (Layer split) | #23 |

## Verification procedure

When each invariant is closed:
1. Create the type / test / gate described in the "Future type" column.
2. Mark the "Status" column as ☑ with a `git commit` referencing the invariant (e.g. `refactor(driver): close invariant #1 with FlushPlan typestate`).
3. If the invariant is "Compile-time": add a negative test in `crates/based-rollup/src/` that ensures `cargo build` fails when violated (a `compile_fail` doctest or a `trybuild` test).
4. If it is "Test/gate": add the test and verify it covers the actual pathological case.

## When all invariants are ☑

The refactor is complete. The DoD criterion of `PLAN.md` §12 includes: "For each invariant in §6, **at least one of the two criteria passes green**". This map is the operational audit of that criterion.
