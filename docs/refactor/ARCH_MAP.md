# Architecture Map — `sync-rollup-composer`

> **Purpose**: pre-refactor snapshot of the `based-rollup` crate. This document is the entry point to the entire project for someone about to execute the refactor described in `docs/refactor/PLAN.md`.
>
> **Generated**: 2026-04-08 (PLAN step 0.1, branch `refactor/phase-0-mapping`).
>
> **Audience**: developer who needs to understand the data flows and entry points before touching code.

---

## 1. Block view

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

## 2. Current state metrics

| File | LOC | Top-level function | Top fn LOC |
|---|---|---|---|
| `driver.rs` | 4 632 | `step_builder` | ~760 |
| `driver.rs` | — | `flush_to_l1` | ~640 |
| `driver.rs` | — | `verify_local_block_matches_l1` | ~253 |
| `driver.rs` | — | `build_builder_protocol_txs` | ~280 |
| `composer_rpc/l1_to_l2.rs` | 4 345 | `trace_and_detect_internal_calls` | ~1 817 |
| `composer_rpc/l1_to_l2.rs` | — | `simulate_l1_to_l2_call_chained_on_l2` | ~491 |
| `composer_rpc/l1_to_l2.rs` | — | `build_and_run_l1_postbatch_trace` | ~327 |
| `composer_rpc/l2_to_l1.rs` | 5 459 | `trace_and_detect_l2_internal_calls` | ~1 693 |
| `composer_rpc/l2_to_l1.rs` | — | `simulate_l1_combined_delivery` | ~547 |
| `composer_rpc/l2_to_l1.rs` | — | `simulate_l1_delivery` | ~535 |
| `composer_rpc/l2_to_l1.rs` | — | `try_chained_l2_enrichment` | ~406 |
| `composer_rpc/l2_to_l1.rs` | — | `enrich_return_calls_via_l2_trace` | ~392 |
| `composer_rpc/trace.rs` | 1 385 | `walk_trace_tree` | ~150 |
| `cross_chain.rs` | 2 410 | `attach_chained_state_deltas` | ~70 |
| `table_builder.rs` | 2 524 | `build_l2_to_l1_continuation_entries` | ~310 |
| `derivation.rs` | 1 172 | `derive_next_batch` | ~573 |
| `proposer.rs` | 558 | `send_to_l1` | ~120 |
| `rpc.rs` | 1 281 | (RPC trait + serde structs) | — |

**Crate totals**: ~16k LOC production + ~24k LOC tests = ~40k LOC. 534 tests, 0 clippy warnings, 48 `unwrap()` in production (most in `composer_rpc/trace.rs`), only 1 `TODO`.

## 3. Critical flows (walkthrough)

### 3.1 `postBatch` outbound (builder → L1)

Path: composer → driver → flush_to_l1 → proposer → L1.

1. A user sends `eth_sendRawTransaction` to `composer_rpc/l2_to_l1.rs:155` (`handle_request`) or `composer_rpc/l1_to_l2.rs:196` (`handle_request`).
2. The composer detects whether it's cross-chain via `composer_rpc/trace.rs::walk_trace_tree`. If it is:
   a. It invokes `debug_traceCallMany` recursively (the `trace_and_detect_*` mammoths).
   b. It calls `cross_chain.rs::build_l2_to_l1_call_entries` (line 819) or `table_builder.rs::build_continuation_entries` (line 264) to build L1+L2 entries.
   c. It pushes to `Driver.queued_cross_chain_calls` or `queued_l2_to_l1_calls` (driver.rs:123, 129) — internal `Arc<Mutex<Vec<_>>>` queues.
   d. It awaits "confirmation" from the driver before forwarding the user tx upstream (hold-then-forward).
3. `Driver::step_builder` (driver.rs:1036) drains the queues, merges entries into `pending_l1_entries` (driver.rs:132) using the 4 parallel vectors: `pending_l1_entries`, `pending_l1_group_starts`, `pending_l1_independent`, `pending_l1_trigger_metadata`.
4. `Driver::build_builder_protocol_txs` (driver.rs:3947) builds the L2 protocol txs that load the entries (`loadExecutionTable` + `executeIncomingCrossChainCall`).
5. `Driver::build_and_insert_block` (driver.rs:4328) builds the L2 block via `payload_builder.rs` and persists it in reth (FCU).
6. `Driver::flush_to_l1` (driver.rs:1796) decides whether to post:
   a. Checks `pending_entry_verification_block` (hold gate) and `last_submission_failure` (cooldown).
   b. Compares `pre_state_root` against the on-chain state root to skip already-submitted blocks.
   c. Calls `proposer.send_to_l1(...)` with the accumulated entries.
   d. Marks the block as pending verification (hold).
7. `proposer.rs::send_to_l1` sends the `postBatch` with explicit nonce via `send_l1_tx_with_nonce`. If it fails, it returns an error and `flush_to_l1` records it.

### 3.2 Derivation inbound (L1 → builder & fullnodes)

Path: L1 BatchPosted event → derivation.rs → driver.rs → reth.

1. `DerivationPipeline::derive_next_batch` (derivation.rs:208) polls L1:
   a. Fetches `BatchPosted` logs since the last `last_processed_l1_block`.
   b. For each log, decodes the calldata of the `postBatch` tx (`cross_chain.rs::parse_batch_posted_logs`, line 1661).
   c. Applies §4f filtering: `cross_chain.rs::filter_block_by_trigger_prefix` (line 2196) — prefix counting, not all-or-nothing. Identifies trigger tx indices via `identify_trigger_tx_indices` (line 2172) and consumes the prefix via `compute_consumed_trigger_prefix` (line 2237).
   d. Applies state deltas via `cross_chain.rs::attach_generic_state_deltas` (line 2316) or `attach_chained_state_deltas` (line 1520).
   e. Converts L1 entries to L2 pairs via `cross_chain.rs::convert_l1_entries_to_l2_pairs` (line 1169).
2. Output: `DerivedBatch` with `Vec<DerivedBlockMeta>`.
3. `Driver::step_builder` or `step_sync` consumes the batch:
   a. If the block already exists locally: `verify_local_block_matches_l1` (driver.rs:3204). If it doesn't match: defer (up to `MAX_ENTRY_VERIFY_DEFERRALS=3`) or rewind.
   b. If it doesn't exist: `build_and_insert_block` applying `apply_deferred_filtering` (driver.rs:3457) which filters txs based on receipts.
4. The driver calls `update_fork_choice(block_hash)` (driver.rs:4415) — engine API FCU. When reth returns `Valid`, the block becomes canonical in chain state.

### 3.3 `composer_rpc` hold-then-forward

Path: user → composer RPC → driver queue → confirmation → forward upstream.

**Why it exists**: in a sync rollup, ordering matters. If a user tx that triggers cross-chain is forwarded BEFORE the entries exist in the CCM, there's a race with `ExecutionNotFound`. The solution is: enqueue entries first, wait for the driver to include them in an L2 block, then forward.

**How it works today** (to be refactored in step 1.6b+c):
1. Composer receives `eth_sendRawTransaction`.
2. Detects cross-chain via `walk_trace_tree`.
3. Builds entries via `cross_chain.rs` / `table_builder.rs`.
4. Pushes entries to the driver queue (`Arc<Mutex<Vec<_>>>`) — fire-and-poll style.
5. Polling loop until it detects the driver drained the queue → assumes the entries are included in a block.
6. Forwards the user tx to the upstream URL.

**Current risk**: the polling is timing-sensitive. If the driver is slow or the composer assumes confirmation too early, there's a race condition. This is closed in step 1.6b+c with the `EntryQueue` 3-state machine + `ForwardPermit` token that is only emitted post-FCU.

## 4. Entry points (file:line)

| Function | File | Line | Role |
|---|---|---|---|
| `run` (driver main loop) | `driver.rs` | 607 | Main driver loop |
| `step` | `driver.rs` | 724 | One iteration of the loop |
| `step_builder` | `driver.rs` | 1036 | Builder mode: build + flush |
| `step_sync` | `driver.rs` | 925 | Sync mode: derive + verify |
| `step_fullnode` | `driver.rs` | 2698 | Fullnode mode: derive + apply |
| `flush_to_l1` | `driver.rs` | 1796 | Decides and sends postBatch |
| `verify_local_block_matches_l1` | `driver.rs` | 3204 | Verifies local block vs derived |
| `build_builder_protocol_txs` | `driver.rs` | 3947 | Builds L2 protocol txs |
| `build_and_insert_block` | `driver.rs` | 4328 | Builds + persists L2 block |
| `update_fork_choice` | `driver.rs` | 4415 | Engine API FCU |
| `rewind_l2_chain` | `driver.rs` | 4445 | Re-derivation after mismatch |
| `derive_next_batch` | `derivation.rs` | 208 | L1 sync → derived batch |
| `parse_batch_posted_logs` | `cross_chain.rs` | 1661 | Decodes BatchPosted events |
| `filter_block_by_trigger_prefix` | `cross_chain.rs` | 2196 | §4f filtering |
| `attach_chained_state_deltas` | `cross_chain.rs` | 1520 | Chained delta protocol |
| `attach_generic_state_deltas` | `cross_chain.rs` | 2316 | Generic state delta attachment |
| `convert_l1_entries_to_l2_pairs` | `cross_chain.rs` | 1169 | L1 → L2 pair conversion |
| `build_l2_to_l1_call_entries` | `cross_chain.rs` | 819 | L2→L1 entry construction |
| `reorder_for_swap_and_pop` | `table_builder.rs` | 124 | CCM swap-and-pop reorder |
| `build_continuation_entries` | `table_builder.rs` | 264 | L1→L2 continuation entries |
| `analyze_l2_to_l1_continuation_calls` | `table_builder.rs` | 1221 | L2→L1 multi-call analysis |
| `build_l2_to_l1_continuation_entries` | `table_builder.rs` | 1612 | L2→L1 continuation entries |
| `walk_trace_tree` | `composer_rpc/trace.rs` | ~150 | Generic cross-chain detection |
| `trace_and_detect_internal_calls` | `composer_rpc/l1_to_l2.rs` | 2280 | L1→L2 detection (mammoth) |
| `trace_and_detect_l2_internal_calls` | `composer_rpc/l2_to_l1.rs` | 3558 | L2→L1 detection (mammoth) |
| `simulate_l1_combined_delivery` | `composer_rpc/l2_to_l1.rs` | 2477 | Combined sim L2→L1 |
| `send_to_l1` | `proposer.rs` | ~140 | Sends postBatch to L1 |
| `sign_proof` | `proposer.rs` | ~250 | ECDSA signature of publicInputsHash |

## 5. Normative references

- **`docs/DERIVATION.md`** — normative protocol spec. NOT modified in this refactor.
- **`docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md`** — formal spec extracted from Solidity. NOT modified.
- **`CLAUDE.md > Lessons Learned — Hard-Won Rules`** — ~50 critical rules. The refactor encodes them in the type system (see `INVARIANT_MAP.md`).
- **`contracts/sync-rollups-protocol/`** — Solidity submodule. NOT modified.
- **`contracts/sync-rollups-protocol/script/e2e/`** — protocol-side E2E suite (21 scenarios) used by PLAN step 0.8 as the baseline source. Each scenario has `Deploy*`, `ExecuteNetwork[L2]`, and `ComputeExpected` Solidity scripts; `script/e2e/shared/run-network.sh` drives them against the running devnet-eez. The `EXPECTED_L1_HASHES` / `EXPECTED_L2_HASHES` / `EXPECTED_L2_CALL_HASHES` outputs are the canonical "what the composer must produce" specification — they are deterministic across runs because they are computed from the action structures by the protocol Solidity. See `contracts/sync-rollups-protocol/script/e2e/README.md` for the complete catalog and the verification semantics.

## 6. Relationship with the PLAN

This `ARCH_MAP.md` is the output of **step 0.1** of `PLAN.md`. The `file:line` references here are what the plan steps use to anchor their changes. If after the refactor these lines change, this document remains as a historical snapshot — it is NOT updated, because its purpose is to document the "pre-refactor state".

To understand what is going to change in each function of this list, see §8 of `PLAN.md` (detailed plan by phases).
