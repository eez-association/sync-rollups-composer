## CRITICAL — Docker Environment Rules

When debugging or investigating issues:

1. **NEVER run `docker compose down -v`** without explicit user approval. This wipes all data and is almost never the right action during debugging.
2. **Always attempt to recover the network first.** Restart individual services, check logs, inspect state. Preserve state for forensic analysis.
3. **Debugging means understanding the problem, not resetting it away.** A fresh deploy destroys evidence.
4. **Before any destructive Docker action** (down -v, volume removal, rebuilds), ask for confirmation.

Debugging order: logs → compare state across nodes → health endpoint → restart individual services → full redeploy ONLY with approval.

## Development Workflow — MANDATORY

```bash
# Standard iteration (devnet)
cargo build --release
sudo docker compose -f deployments/testnet-eez/docker-compose.yml \
     -f deployments/testnet-eez/docker-compose.dev.yml restart builder fullnode1 fullnode2

# First time or after compose changes
cargo build --release
sudo docker compose -f deployments/testnet-eez/docker-compose.yml \
     -f deployments/testnet-eez/docker-compose.dev.yml up -d

# With explorers
sudo docker compose -f deployments/testnet-eez/docker-compose.yml \
     -f deployments/testnet-eez/docker-compose.dev.yml \
     -f deployments/shared/docker-compose.explorer.yml \
     -f deployments/testnet-eez/docker-compose.explorer.yml up -d
```

Rules:
- NEVER `docker compose build` during development — binary comes from host via volume mount
- NEVER modify Dockerfiles or base compose — use docker-compose.dev.yml overlay only
- **ALL devnet docker compose commands MUST include both the main and dev compose files**. Without both files, the container uses the old Docker-built binary.

---

## Lessons Learned — Hard-Won Rules

These rules come from real bugs that took hours to diagnose. Violating them WILL break the system.

### Nonce Management
- **NEVER use alloy auto-nonce for withdrawal triggers or any multi-tx L1 sequence.** Alloy's CachedNonceManager desynchronizes when gas estimation fails, permanently bricking all L1 submissions. Use `send_l1_tx_with_nonce()` with explicit nonces.
- **ALWAYS call `reset_nonce()` after any L1 tx failure.** Recreates the provider and clears corrupted cache. Without this: permanent livelock (builder builds but never submits).
- **Nonce gap = invisible death.** Builder keeps building while submissions are stuck in "queued" pool. Diagnose with `cast rpc txpool_inspect --rpc-url localhost:9555`.

### Cross-Chain Entry Safety
- **NEVER align state roots by overwriting pre_state_root** — NO EXCEPTIONS. If roots don't match, there is a real bug in derivation or filtering. The builder must keep rewinding until the root cause is fixed. Fabricating pre_state_roots produces blocks that fullnodes cannot reproduce.
- **NEVER use all-or-nothing filtering.** §4f is per-executeRemoteCall with prefix counting. All-or-nothing loses consumed entry effects in partial consumption.
- **NEVER fire-and-forget cross-chain detection.** Both composer RPCs (L1→L2 for deposits, L2→L1 for withdrawals) MUST use hold-then-forward: queue entries, await confirmation, THEN forward user tx. Fire-and-forget causes timing races.
- **Deposits and withdrawals can coexist in the same block.** Mutual exclusion was removed. `attach_unified_chained_state_deltas()` builds a single D+W+1 root chain that covers both entry types together.
- **§4f filtering covers both deposits and L2→L1 calls via `filter_block_entries()`.** Single unified call handles `executeRemoteCall` (deposits) and L2→L1 cross-chain txs (identified generically by `CrossChainCallExecuted` receipt events, not Bridge selectors). Replaces the former separate `filter_unconsumed_withdrawal_txs()` path.

### Deposit Minting
- **Deposit minting is no longer needed.** CCM receives 1M ETH via genesis injection (deploy.sh adds the CCM address to a copy of genesis.json at deploy time). `apply_pre_execution_changes` is a pure passthrough to `EthBlockExecutor` — no balance injection, no block body scanning, no Arc<Mutex> state.

### Entry Verification Hold
- **Hold MUST be set BEFORE send_to_l1, not after.** If set after and tx fails, hold is active with no way to clear it. Setting before means failure triggers rewind which clears hold.
- **Builder HALTS block production while hold is active (`step_builder` returns early).** Building during hold would accumulate blocks with advancing L1 context that mismatch after rewind, causing double rewind cycles. Hold prevents both block production and postBatch submission.
- **Withdrawal trigger revert on L1 causes REWIND, not just a log.** If the trigger tx reverts, the block contains unconsumed withdrawal entries; the driver rewinds to strip them. Loops are broken by the `consecutive_rewind_cycles` counter + `MAX_FLUSH_MISMATCHES` threshold.
- **Deferral exhaustion causes REWIND, not acceptance.** After `MAX_ENTRY_VERIFY_DEFERRALS=3` retries, `verify_local_block_matches_l1` returns `Err` to trigger a rewind. Previously mismatches were accepted; now they force re-derivation from `entry_block - 1` so the entry block itself gets re-derived.
- **Rewind target is `entry_block.saturating_sub(1)`.** This ensures the block containing the entry is itself re-derived, not skipped.

### Continuation Entry Construction (Flash Loans)
- **NEVER include a RESULT table entry when `extra_l2_entries` (continuations) are present.** `convert_l1_entries_to_l2_pairs` skips the `result_entry` push when `has_continuations=true`. The driver RPC path (`driver.rs`, `rpc_entries` loop) likewise skips `result_entry` when `extra_l2_entries` is non-empty. Including it causes `ExecutionNotFound` — same `actionHash` but wrong `nextAction` conflicts with Entry 0 from the continuation chain.
- **NEVER classify continuation entries as trigger entries in `partition_entries()`.** Continuation entries have `nextAction=CALL_B` targeting our rollup, but their `actionHash=hash(RESULT)` — NOT `hash(CALL_B)`. Only classify as trigger when `hash(next_action) == action_hash` (true for CALL triggers, false for continuations). Mis-classifying sends them to `executeIncomingCrossChainCall` instead of `loadExecutionTable`, causing `ExecutionNotFound` on the scope exit.
- **Canonical reference for continuation L2 entry layout**: `contracts/sync-rollups-protocol/script/flash-loan-test/ExecuteFlashLoan.s.sol`. Verify entry hashes `0x7cee89f0...` (RESULT_L2_void) and `0xe690f92b...` (CALL_bridgeReturn) match derivation output.

### Deploy Split and Docker Dependency Chain
- **NEVER deploy L2 contracts before the builder is healthy.** `deploy_l2.sh` verifies `canonicalBridgeAddress` and deploys flash loan contracts that depend on CCM state written by the builder's block 2 protocol tx. Run `deploy-l2` service only after `builder: healthy`.
- **`complex-tx-sender` MUST depend on `deploy-l2: service_completed_successfully`.** `complex-tx-sender` uses dev#5 key, same as flash loan L2 deployment scripts. If it starts first, it consumes nonces and all flash loan contracts deploy at wrong addresses (silent failure — transactions succeed but addresses are garbage).
- **`canonicalBridgeAddress` is set by the builder as a block 2 protocol tx.** `deploy_l2.sh` verifies the value and falls back to setting it if verification fails.
- **Docker dependency chain for flash loans**: `l1 (healthy) → deploy (L1) → builder (healthy) → deploy-l2 (L2) → complex-tx-sender`.

### Bytecode & CREATE2 Determinism
- **ALL bytecodes MUST come from `contracts/sync-rollups-protocol/out/`.** The project has TWO `out/` directories: `contracts/out/` (L2Context, Counter, MockECDSAVerifier, test contracts) and `contracts/sync-rollups-protocol/out/` (all sync-rollups-protocol contracts). They can produce DIFFERENT bytecodes for the same contract due to separate `forge build` invocations with different metadata. Mixing sources causes CREATE2 address mismatches. The `_bc()` helper in deploy.sh reads exclusively from the correct directory.
- **NEVER compile on host and expect Docker to match.** Host and Docker forge versions differ — bytecodes WILL differ, breaking CREATE2 determinism. All compilation happens inside the Docker deploy container.
- **`forge inspect` ≠ `forge build` output.** `forge inspect` can produce different bytecode than what's in `out/`. Always read from JSON artifact files for CREATE2 computation.

### L2→L1 Multi-Call Continuation
- **L2 entries MUST use scope navigation for flash loan repayment.** Without `callReturn{scope=[0]}` on the second L2→L1 call, tokens burned by `bridgeTokens` never return — the flash loan can't be repaid. This mirrors exactly how L1→L2 flash loan works on L1: scope navigation calls `Bridge_L2.receiveTokens` to mint wrapped tokens back within the same tx.
- **NEVER use per-call `simulate_l1_delivery` return calls for multi-call L2→L1 patterns.** The per-call simulation runs each call in isolation with placeholder entries, producing incorrect proxy addresses. Use `simulate_l1_combined_delivery` which bundles all triggers in one `debug_traceCallMany` so later calls see state effects from earlier ones. Falls back to analytical construction from the forward trip's `receiveTokens` params if combined simulation fails.
- **L1 and L2 entry structures must MIRROR each other.** If L1→L2 flash loan uses scope navigation on L1 entries, L2→L1 must use scope navigation on L2 entries. Any asymmetry is a bug. Correct L2 structure: Entry 0 `hash(CALL_A bridgeTokens) → RESULT(L1,void)` (terminal); Entry 1 `hash(CALL_B claimAndBridgeBack) → callReturn{scope=[0]}`; Entry 2 `hash(RESULT{L2,void}) → RESULT(L1,void)` (scope exit).

### Return Call Address Direction (Depth > 1 / PingPong)
- **NEVER swap (dest, source) for L1→L2 return call children.** In `push_reentrant_child_entries` and `generate_l2_entries_recursive`, detect return calls with `child.call_action.rollup_id == our_rollup_id`. For those children: trigger hashes and L2 `callReturn` entries use NON-swapped (dest, source). For forward L2→L1 children: use swapped addresses. Mixing causes `ExecutionNotFound` on L1 or wrong scope navigation on L2.
- **The direction check is: `child.call_action.rollup_id == our_rollup_id` → L1→L2 return call → no swap.** This is the single canonical gate in both `table_builder.rs` functions. Any path that bypasses this check will produce wrong proxy addresses in multi-depth patterns.

### Parent Call Index After Combined Simulation
- **ALWAYS override `parent_call_index` after `simulate_l1_combined_delivery` returns.** The function internally assigns `call_idx=0` relative to its single-call slice, collapsing all return calls onto call[0]. Override with the actual index in `all_l2_calls` (`all_l2_calls.len()-1`). Without this, sequential scopes are created instead of nested scopes, which breaks at depth >= 3 due to swap-and-pop disruption of 4+ same-hash entries.

### State Delta Assignment After Entry Reorder
- **NEVER assign currentState=clean to index 0 blindly.** `reorder_for_swap_and_pop` may move RESULT entries to the front of the L1 entry array. The first TRIGGER entry (which needs currentState=clean to match the on-chain state after the immediate block entry) may be at a later index. Use `first_trigger_idx = entries.iter().position(|e| e.action_hash != result_void_hash).unwrap_or(0)` to find the correct entry. Without this, the trigger's state delta mismatches the on-chain state → ExecutionNotFound on L1.

### Delivery Return Data for L2→L1 Calls
- **`executeOnBehalf` uses assembly return** — for void functions, returnData is empty (0 bytes), NOT ABI-encoded empty bytes (64 bytes). The `result_void()` helper correctly uses `data: vec![]`. For functions that return data, the returnData is the raw ABI-encoded return value (e.g., 32 bytes for uint256). The simple single-call path captures this via direct `eth_call` simulation when the trigger trace has empty output.
- **Continuation path RESULT hashes use `delivery_return_data` when non-empty** (#245/#246). The simulation includes inner CALL→RESULT_VOID entries so the delivery function's return matches real execution. All 4 RESULT hash sites (delivery result, scope resolution, reentrant child, subsequent call) check `delivery_return_data.is_empty()` before choosing result_void vs result_with_data.
- **L2 scope resolution RESULT uses `l2_return_data`** (#245). Captured via `eth_call(from=proxy, to=destination, data=calldata)` in the L2 proxy. Propagated through `DetectedReturnCall.l2_return_data` → RPC JSON → `L2ReturnCall.l2_return_data` → `DetectedCall.l2_return_data` → `generate_l2_entries_recursive`.
- **Depth-2 generic patterns need multi-call promotion even with 1 L2→L1 call** (#245). The condition `all_l2_calls.len() > 1 || !all_return_calls.is_empty()` ensures a single L2→L1 call with a terminal return call (like Logger→Counter) still gets continuation entries. The driver routes these via `trigger_source.is_none()` (not `extra_triggers.is_empty()`).

### Contract ABI Compatibility (feature/contract_updates)
- **`publicInputsHash` uses `block.timestamp`, not `block.number`.** The Rollups.sol contract changed this. The builder predicts timestamp as `latest_timestamp + block_time`. Proxy simulations use `blockOverride.time` for consistency.
- **`computeCrossChainProxyAddress` takes 2 args, not 3.** The `domain` parameter was removed. All callers (`driver.rs`, `composer_rpc/`) must match.
- **ALWAYS verify Solidity ABI against the actual contract code** before implementing Rust callers. Use `grep` on the `.sol` files, not assumptions from old code.

### Debugging Process
- **When a fix introduces a new code path, verify it doesn't break existing flows.** The `computeCrossChainProxyAddress` fix made `simulate_l1_delivery` succeed where it previously failed silently, which changed the entry construction path and broke L2 entries.
- **Stale L1 state from old code blocks builder recovery on restart.** Always use fresh deploy (`down -v`) when testing code changes that affect L1 entries or `postBatch` format.

### Shell Script Gotchas
- **SIGPIPE**: `grep ... | head -1` causes exit code 141 in `set -euo pipefail` scripts. Fix: `(grep ... || true) | head -1`.
- **No python3 in deploy container.** The foundry Docker image only has bash/sed/grep/awk/cast/forge. Use `_bc()` helper (grep+sed) to extract bytecodes from JSON artifacts.

### Protocol-First Implementation
- **NEVER implement a fix for just the specific pattern that broke.** Read the protocol spec and implement the general mechanism.
- **Use protocol identity mechanisms (authorizedProxies, computeCrossChainProxyAddress), not trace heuristics.** Trace output parsing is fragile and breaks with wrapper contracts.
- **The protocol supports arbitrary depth, non-void returns, multiple identical calls, and wrapper-mediated proxy calls.** Every entry construction path must handle ALL of these.
- **Reuse existing generic patterns.** `composer_rpc/trace.rs:walk_trace_tree` is the single generic detection path for all cross-chain proxy detection.
- **Always use `debug_traceCallMany` for simulation, never `eth_call`.** `debug_traceCallMany` provides full call tree visibility and state bundling. `eth_call` only gives flat results. Exception: read-only view calls (`authorizedProxies`, `computeCrossChainProxyAddress`) may use `eth_call`.
- **Derivation is purely from L1 data.** The fullnode derives L2 blocks from L1 `postBatch` entries alone. NEVER add L2 simulation to the derivation path. The L1 entries already contain all necessary information.
- **NEVER hardcode function selectors.** Use typed ABI encoding via `sol!` macros and `SolCall::abi_encode()`. Example: `IRollups::computeCrossChainProxyAddressCall { ... }.abi_encode()`. Hardcoded selectors cause silent failures and cannot be verified at compile time.

---

## Sub-Agent Delegation

Specialized subagents in `.claude/agents/`. The user communicates in Spanish — translate intent and delegate.

### Routing

- Use `core-worker` for any work on `crates/based-rollup/src/` (including `composer_rpc/`): state roots, consensus, cross-chain, filtering, bridge, L1 submission, nonce management, EVM, reorg.
- Use `ui-worker` for visual/layout/CSS/React work in `ui/`.
- Use `general-worker` for research, scripts, Docker, Kurtosis, CI/CD, external repos, GitHub issues.
- Use `auditor` to verify changes against docs/DERIVATION.md. READ-ONLY.
- Use `test-writer` to create `*_tests.rs` tests. Never touches production code.
- Use `spec-writer` to update docs/DERIVATION.md after changes are audited.
- Use `qa` to validate features E2E in Docker: bridge deposits/withdrawals, state convergence, builder recovery, nonce recovery.
- Use `maintainer` after changes land to keep CLAUDE.md, agents, and docs in sync.

### Dispatch Protocol

When delegating, ALWAYS include: (1) context — what was done and why, (2) scope — exact files, (3) success criteria, (4) DERIVATION.md references.

### Pipeline (consensus-critical changes)

```
core-worker → auditor → test-writer → qa → spec-writer → maintainer
```

### Safe Parallel Combinations

- `auditor` + `test-writer` (both read-only on production code)
- `ui-worker` + `core-worker` (different directories)
- `general-worker` + any (research/external repos)
- `qa` + `auditor` (both read-only)

### NEVER Parallel

- `core-worker` + `spec-writer` (docs/DERIVATION.md conflict)
- `core-worker` + `test-writer` (during active edits)
- `maintainer` + anyone writing (reads current state)

---

# Based Rollup — Development Guide

## Project Overview

A minimal based rollup built on reth. L2 blocks follow a deterministic 12-second timestamp schedule. L1 is the sequencer — Rollups.sol on L1 is the canonical source of truth. Read `docs/DERIVATION.md` for the normative spec.

## Project Structure

```
sync-rollup-composer/
├── docs/
│   ├── DERIVATION.md                   # Normative spec
│   └── architecture.excalidraw         # Architecture diagram
├── CLAUDE.md                           # This file
├── .claude/agents/                     # 8 subagent definitions
├── deployments/
│   ├── shared/
│   │   ├── docker-compose.base.yml     # Reusable builder/fullnode templates
│   │   ├── docker-compose.explorer.yml # Shared L2 Blockscout overlay
│   │   ├── Dockerfile / Dockerfile.dev # Build definitions
│   │   ├── genesis.json                # L2 genesis (chain 42069)
│   │   └── scripts/                    # Docker service scripts (mounted into containers)
│   │       ├── start-rollup.sh         # Node entrypoint
│   │       ├── deploy.sh / deploy_l2.sh # L1/L2 contract deployment
│   │       ├── send-*.sh              # tx-sender / crosschain-tx-sender / complex-tx-sender
│   │       ├── deploy-reverse-flash-loan.sh
│   │       └── verify-contracts.sh     # Blockscout verification
│   ├── testnet-eez/                    # Local devnet (reth --dev L1)
│   │   ├── docker-compose.yml          # Main compose (extends base)
│   │   ├── docker-compose.dev.yml      # Dev overlay (mount local binary)
│   │   ├── docker-compose.explorer.yml # Devnet explorer overrides
│   │   ├── deploy.sh                   # Devnet-specific L1 deploy (mounted as deploy-devnet.sh)
│   │   └── README.md
│   ├── devnet-eez/                     # Dev-mode devnet (separate ports from testnet)
│   │   ├── docker-compose.yml          # Main compose (extends base)
│   │   ├── docker-compose.dev.yml      # Dev overlay (mount local binary)
│   │   ├── docker-compose.explorer.yml # Devnet explorer overrides
│   │   ├── genesis-l1.json / genesis-l2.json  # L1 + L2 genesis files
│   │   └── README.md
│   ├── gnosis-100/                     # Gnosis Chain deployment
│   │   ├── docker-compose.yml          # Extends base, gnosis-specific
│   │   ├── docker-compose.explorer.yml # Gnosis explorer overrides
│   │   ├── deploy-gnosis.sh / .env.example  # Deploy script + env template
│   │   └── README.md
│   ├── chiado-10200/                   # Chiado testnet deployment
│   │   ├── docker-compose.yml          # Extends base, chiado-specific
│   │   ├── deploy.sh / .env.example    # Deploy script + env template
│   │   ├── docker-compose.explorer.yml # Chiado explorer overrides
│   │   └── README.md
│   ├── ethereum-1/                     # Placeholder for mainnet
│   └── kurtosis-1337/                  # Kurtosis PoS devnet
│       ├── docker-compose.yml          # Extends base, +1000 port offset
│       ├── start.sh / verify-finality.sh
│       ├── network_params.yaml
│       └── README.md
├── contracts/sync-rollups-protocol/    # Submodule (never modify; includes docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md)
├── contracts/test/                     # Test contracts (MockZKVerifier, RevertOnReceive, WithdrawalSender)
├── contracts/test-depth2/              # PingPong depth test contracts (issue #236)
├── contracts/test-multi-call/          # Multi-call test contracts (CallTwice, CallTwoDifferent, ConditionalCallTwice, Counter)
├── scripts/                            # Host-only scripts (E2E tests, tooling)
│   ├── tools/
│   │   └── inspect-l1-block.sh         # L1 block inspector (decodes postBatch, entries, events)
│   └── e2e/                            # E2E regression tests (run from host)
│       ├── lib-health-check.sh         # Shared test helpers (assert, convergence, timing)
│       ├── bridge-health-check.sh      # L1↔L2 ETH bridging (deposits + withdrawals)
│       ├── crosschain-health-check.sh  # L1→L2 cross-chain calls (§4f prefix counting)
│       ├── test-l2-proxy-call.sh       # L2 proxy symmetric detection
│       ├── test-l2-to-l1-return-data.sh # L2→L1 return data (Counter+Logger, issue #242)
│       ├── deploy-ping-pong.sh         # Configurable-depth PingPong (issue #236)
│       ├── deploy-ping-pong-return.sh  # PingPong with return data (issue #242)
│       ├── test-depth2-generic.sh      # Depth-2 L2→L1→L2 generic bounce (issue #245)
│       ├── flashloan-health-check.sh   # L1→L2 flash loan E2E
│       ├── flashloan-test.sh           # Flash loan trigger on pre-deployed contracts
│       ├── test-l2-to-l1-flash-loan.sh # L2→L1 reverse flash loan
│       ├── double-deposit-withdrawal-trace.sh  # Concurrent deposit+withdrawal
│       ├── test-multi-call-cross-chain.sh      # Multi-call cross-chain (CallTwice, issue #256)
│       └── test-conditional-cross-chain.sh     # Conditional cross-chain (ConditionalCallTwice, issue #256)
├── ui/                                 # React dashboard (port 8080)
└── crates/based-rollup/src/
    ├── driver.rs                       # Mode orchestration, flush_to_l1, hold, withdrawal triggers
    ├── derivation.rs                   # L1 sync, §4e/§4f filtering
    ├── cross_chain.rs                  # Entry types, ABI, filtering, continuation reconstruction
    ├── table_builder.rs                # Flash loan continuation analysis, L1/L2 entry building
    ├── evm_config.rs                   # EVM config, thin executor wrapper (CCM pre-minted in genesis)
    ├── proposer.rs                     # L1 submission, explicit nonces, recovery
    ├── composer_rpc/                   # Cross-chain RPC interception (replaces proxy.rs + l1_proxy.rs)
    │   ├── mod.rs                      # Module root
    │   ├── common.rs                   # Shared HTTP helpers, JSON-RPC parsing, proxy detection
    │   ├── trace.rs                    # Generic trace-based cross-chain call detection (walk_trace_tree)
    │   ├── l2_to_l1.rs                # L2 composer RPC — intercepts L2→L1 calls (hold-then-forward)
    │   ├── l1_to_l2.rs                # L1 composer RPC — intercepts L1→L2 calls (hold-then-forward)
    │   ├── l2_to_l1_tests.rs          # Tests for L2→L1 detection
    │   └── l1_to_l2_tests.rs          # Tests for L1→L2 detection
    ├── rpc.rs                          # syncrollups_* RPC
    ├── execution_planner.rs            # Tx simulation, action hash
    ├── config.rs / consensus.rs        # Config, timestamp validation
    ├── payload_builder.rs              # Block building
    ├── builder_sync.rs                 # WS preconfirmations
    ├── health.rs                       # Health endpoint
    └── lib.rs / main.rs               # Wiring, entrypoint
```

## Key Architecture

- **Protocol TX filtering (§4f)**: `filter_block_entries()` does unified prefix counting for `executeRemoteCall` (deposits) and L2→L1 cross-chain txs (identified generically via `CrossChainCallExecuted` receipt events, not Bridge selectors). `extract_l2_to_l1_tx_indices()` scans receipts for `CrossChainCallExecuted` events emitted by the CCM to identify L2→L1 tx indices. loadExecutionTable always kept. NEVER all-or-nothing.
- **Unified intermediate roots**: `attach_unified_chained_state_deltas()` builds a single D+W+1 root chain for any mix of deposit/withdrawal entries. Stored in `PendingBlock.intermediate_roots`. Deposits and withdrawals can coexist in the same block.
- **CCM genesis pre-mint**: CCM gets 1M ETH via genesis injection (deploy.sh adds the CCM address to a copy of genesis.json at deploy time). No runtime minting needed. `evm_config.rs` is a thin delegation wrapper around `EthBlockExecutor`.
- **Entry verification hold**: set BEFORE send_to_l1. Builder HALTS block production (`step_builder` returns early). Cleared by verify or rewind. After MAX_ENTRY_VERIFY_DEFERRALS=3, rewinds instead of accepting mismatch.
- **Nonce-linked atomicity (§13b)**: postBatch(K), createProxy(K+1), trigger(K+2). Explicit nonces. reset_nonce on failure.
- **Hold-then-forward**: both composer RPCs (`composer_rpc/l2_to_l1.rs`, `composer_rpc/l1_to_l2.rs`) await entry queue confirmation before forwarding user tx.
- **Generic trace-based detection**: `composer_rpc/trace.rs:walk_trace_tree` is the single detection path for both directions. Walks `callTracer` trace trees looking for `executeCrossChainCall` child calls on the CCM — no Bridge-specific selectors. Detects persistent proxies (via `authorizedProxies` lookup) and ephemeral proxies (via `createCrossChainProxy` in the same trace). Works for bridgeEther, bridgeTokens, direct proxy calls, wrapper contracts, flash loans, and any future cross-chain pattern.
- **ECDSA proof signing**: `proposer.rs` `sign_proof()` signs the publicInputsHash with the builder key. The publicInputsHash is keccak256(abi.encodePacked(blockhash, timestamp, encode(entryHashes), encode(blobHashes), keccak256(callData))). `tmpECDSAVerifier` on L1 verifies via ecrecover (raw hash, no EIP-191 prefix). Development-only.
- **Block 1 genesis**: L2Context(nonce=0), CCM(nonce=1), Bridge(nonce=2), Bridge.initialize(nonce=3).
- **L1→L2 flash loan continuation flow**: `composer_rpc/l1_to_l2.rs` detects multi-call txs via iterative `debug_traceCallMany`. `table_builder.rs` analyzes calls and builds L1+L2 entries. On L2, `loadExecutionTable` loads 3 continuation entries; a single `executeIncomingCrossChainCall` triggers the full chain (receiveTokens → claimAndBridgeBack → bridgeTokens return) via CCM `newScope()`. Canonical reference: `contracts/sync-rollups-protocol/script/flash-loan-test/ExecuteFlashLoan.s.sol`.
- **L2→L1 multi-call continuation**: `build_l2_to_l1_continuation_entries()` generates 3 L2 entries (with scope navigation on Entry 1: `callReturn{scope=[0]}`) and 5 L1 entries (with nested delivery on Entry 0). Return calls are constructed analytically from the forward trip's `receiveTokens` params — simulation is not used for the second call due to token availability ordering. Mirrors the L1→L2 pattern: scope navigation is required on both sides so tokens are returned within the same tx. NOTE: `table_builder.rs` still uses `IBridge::receiveTokensCall` for flash loan ABI decode (to be refactored separately to eliminate the last Bridge-specific dependency).
- **Configurable-depth cross-chain (issue #236)**: `composer_rpc/l2_to_l1.rs` supports up to `MAX_RECURSIVE_DEPTH=5` (defined in `composer_rpc/l2_to_l1.rs`) hops via iterative `debug_traceCallMany` expansion. Both the multi-call path and the single-call path use the same constant. PingPong contracts (`contracts/test-depth2/`) use generic `ping(round, maxRounds)` and `pong(round, maxRounds)` signatures; `start(maxRounds)` triggers N rounds of L2→L1 with (N-1) L1→L2 returns. Deployed via `scripts/e2e/deploy-ping-pong.sh` using dev#10.

## Docker Services (Devnet)

- **L1**: port 9555 | **builder**: 9545 (RPC), 9550 (WS), 9548 (L2→L1 composer RPC), 9556 (L1→L2 composer RPC), 9560 (health)
- **fullnode1**: 9546 | **fullnode2**: 9547 | **sync-ui**: 8080
- **deploy**: L1 contracts (Rollups.sol, tmpECDSAVerifier, Bridge, etc.) — runs once at startup
- **deploy-l2**: L2 contracts (canonicalBridgeAddress verify + flash loan contracts) — runs after builder healthy
- **tx-sender**: sends test L1 transactions (funds L2 accounts via dev#1) — runs after deploy
- **crosschain-tx-sender**: sends continuous cross-chain counter increments (dev#4) — runs after deploy

Startup order: `l1 → deploy → builder → deploy-l2 → deploy-reverse-flash-loan → complex-tx-sender`

All core services start by default. Explorers require the explorer overlay files.

### Deployment-specific compose commands

```bash
# Devnet (standard — uses Docker-built binary, CI/production only)
docker compose -f deployments/testnet-eez/docker-compose.yml up -d

# Devnet (dev mode — mount local binary) ← USE THIS FOR DEVELOPMENT
docker compose -f deployments/testnet-eez/docker-compose.yml \
               -f deployments/testnet-eez/docker-compose.dev.yml up -d

# Gnosis
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env up -d

# Kurtosis
docker compose -f deployments/kurtosis-1337/docker-compose.yml up -d  # .env.kurtosis must be created first (see deployments/kurtosis-1337/README.md)
```

## Dev Account Assignments

HD mnemonic dev keys are allocated by role to prevent nonce collisions. Keys #0-#9 are pre-funded by reth --dev; keys #10-#16 plus the CCM CREATE address and other non-mnemonic addresses are funded by deploy.sh from dev#9. Keys #17-#18 are funded at runtime by their respective E2E test scripts.

| Index | Address | Role |
|-------|---------|------|
| #0 | 0xf39F… | Deployer / builder key |
| #1 | 0x7099… | tx-sender (funds L2 accounts) |
| #2 | 0x3C44… | crosschain-health-check test key |
| #3 | 0x90F7… | bridge-health-check test key |
| #4 | 0x15d3… | crosschain-tx-sender (continuous counter increments) |
| #5 | 0x9965… | deploy_l2.sh / deploy-reverse-flash-loan / complex-tx-sender (Docker services only) |
| #6 | 0x976E… | double-deposit-withdrawal-trace user 2 |
| #7 | 0x14dC… | bridge-health-check TEST18 deployer |
| #8 | 0x2361… | test-l2-proxy-call |
| #9 | 0xa0Ee… | L1 funder (deploy.sh funds #10-#18 from this key) |
| #10 | 0xBcd4… | deploy-ping-pong E2E test |
| #11 | 0x71bE… | deploy-ping-pong-return E2E test |
| #12 | 0xFABB… | flashloan-health-check E2E test |
| #13 | 0x1CBd… | double-deposit-withdrawal-trace user 1 |
| #14 | 0xdF3e… | flashloan-test E2E test |
| #15 | 0xcd3B… | test-l2-to-l1-return-data E2E test |
| #16 | 0x2546… | test-depth2-generic E2E test |
| #17 | 0xbDA5… | test-multi-call-cross-chain E2E test |
| #18 | 0xdD2F… | test-conditional-cross-chain E2E test |

**WARNING**: dev#5 is shared between `deploy_l2.sh`, `deploy-reverse-flash-loan`, and `complex-tx-sender`. These are Docker services and MUST NOT run concurrently — enforce via Docker `depends_on`.

All E2E tests (scripts/e2e/) use dedicated keys (#2, #3, #6-#8, #10-#18) except `test-l2-to-l1-flash-loan` which uses #0 (builder key, read-only queries). Run sequentially — the single builder can't handle parallel postBatch load.

## Removed Code (do NOT look for)

- `clean_state_roots: HashMap<u64, Vec<B256>>` — replaced by §4f
- Old `compute_intermediate_state_roots` (N+1 simulation) — replaced by actual-tx filtering
- `build_protocol_txs_for_simulation()` — dead code
- Category 1 state alignment in `flush_to_l1` — all mismatches rewind
- `matches_intermediate` in `verify_local_block_matches_l1` — removed
- `send_l1_tx()` (auto-nonce) — replaced by `send_l1_tx_with_nonce()`
- `pending_deposit_amount: Arc<Mutex<Option<U256>>>` — removed with smart minting (CCM pre-minted in genesis)
- `set_pending_deposit() / take_pending_deposit()` — removed with smart minting
- `set_deposit_for_entries()` in driver.rs — removed with smart minting
- `TRACE_BLOCK_TXS thread_local` — removed with smart minting
- `context_for_block` override in `RollupEvmConfig` that injected deposit balances — removed with smart minting
- `compute_deposit_from_block_body()` — removed with smart minting
- `DatabaseCommitExt::increment_balances()` — removed with smart minting
- Smart minting logic in `apply_pre_execution_changes` — replaced by CCM genesis pre-mint
- Identity delta blocks for withdrawals — replaced by `attach_unified_chained_state_deltas()` (fix #212, EtherDeltaMismatch on concurrent withdrawals). `attach_withdrawal_chained_state_deltas()` and `compute_withdrawal_intermediate_state_roots()` also removed.
- `PendingBlock.withdrawal_intermediate_roots` — renamed to `intermediate_roots` (now covers unified deposit+withdrawal chain)
- Separate `filter_unconsumed_withdrawal_txs()` path in derivation — merged into unified `filter_block_entries()` call
- Mutual exclusion check in `step_builder` — removed; deposits and withdrawals may coexist
- `MockZKVerifier` contract — replaced by `tmpECDSAVerifier` in Docker deployments (ECDSA-based). Note: E2E unit tests (`e2e_anvil.rs`) still use `MockZKVerifier` for Anvil-based testing.
- Deferral-then-accept path in `verify_local_block_matches_l1` — replaced by deferral-then-rewind after `MAX_ENTRY_VERIFY_DEFERRALS=3`
- Spurious RESULT table entry in `convert_l1_entries_to_l2_pairs` when `has_continuations=true` — caused `ExecutionNotFound`; now skipped
- Old `partition_entries` logic that classified continuation entries (`actionHash=hash(RESULT)`, `nextAction=CALL_B`) as triggers — now uses `hash(next_action) == action_hash` guard
- Unconditional `result_entry` push in driver.rs builder RPC path — now skipped when `extra_l2_entries` present
- `query_ccm_pending_entry_count()`, `CCM_PENDING_ENTRY_COUNT_SLOT`, and `skip_entries` logic in driver.rs — stale-entry guard removed after `CrossChainManagerL2.loadExecutionTable()` was fixed to self-clean by deleting existing entries per actionHash before pushing and resetting `pendingEntryCount = entries.length`
- `domain` parameter in `computeCrossChainProxyAddress` — removed in feature/contract_updates; function now takes 2 args (rollupId, deployer). All callers updated.
- `block.number` in `publicInputsHash` — replaced by `block.timestamp` in feature/contract_updates. Builder and proxy simulation code predict/override timestamp accordingly.
- Hardcoded depth-2 PingPong function names `pingAgain()`, `pong()` (no args), and `finalPong()` — replaced by generic `ping(uint256 round, uint256 maxRounds)`, `pong(uint256 round, uint256 maxRounds)`, and `start(uint256 maxRounds)` in `contracts/test-depth2/src/`. The configurable-depth design supports 1 to `MAX_RECURSIVE_DEPTH=5` rounds without new function signatures per depth.
- Per-depth L1 re-trace path in the old `proxy.rs` that used isolated single-call simulation for each depth level — replaced by iterative `debug_traceCallMany` expansion that accumulates children across `MAX_RECURSIVE_DEPTH` passes.
- `proxy.rs` (entire file) — refactored into `composer_rpc/l2_to_l1.rs`. All Bridge-specific selectors removed; detection uses generic `trace::walk_trace_tree`.
- `l1_proxy.rs` (entire file) — refactored into `composer_rpc/l1_to_l2.rs`. Detection uses shared `trace::walk_trace_tree`.
- `build_withdrawal_entries()` in cross_chain.rs — callers now use `build_l2_to_l1_call_entries()` (generic, not Bridge-specific).
- `is_bridge_ether_withdrawal()` / `is_bridge_withdrawal()` in cross_chain.rs — replaced by receipt-based `CrossChainCallExecuted` event scanning via `extract_l2_to_l1_tx_indices()`.
- Bridge-specific selector fast/medium/slow detection paths in the old proxy — replaced by single generic `trace::walk_trace_tree` path that detects `executeCrossChainCall` child calls on the CCM.
- `trigger_user` field as a separate concept from `source_address` — `trigger_user` always equaled `source_address`, so the distinction was eliminated (note: `trigger_user` still exists as a parameter name in `l2_to_l1.rs` internal functions but always receives `source_address`).

## Build & Test

```bash
cargo build --release
cargo nextest run --workspace
cargo clippy --workspace --all-features
cargo +nightly fmt --all
```

## Git & GitHub Workflow

### Branching
- **ALWAYS work on a feature branch**, never commit directly to main.
- Branch naming: `feat/<area>-<description>`, `fix/<area>-<description>` — e.g. `feat/bridge-withdrawal-triggers`, `fix/proposer-nonce-recovery`.
- Create branch before first change: `git checkout -b feat/<name>`.

### Commits
- Conventional commits: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`
- Atomic commits: one logical change per commit. Do NOT bundle unrelated changes.
- Every commit must pass `cargo nextest run --workspace && cargo clippy --workspace --all-features`.
- Commit message body: explain WHY, not just what. Reference docs/DERIVATION.md sections and GitHub issues when relevant.

### GitHub Issues
- **Create an issue BEFORE starting significant work.** The issue is the plan; the PR is the execution.
- Title: `[area] description` — e.g. `[bridge] L1 trigger tx reverts despite matching entry hash`.
- Body: (1) problem description with evidence, (2) root cause analysis, (3) proposed solution, (4) affected files, (5) verification criteria, (6) relevant docs/DERIVATION.md sections.
- Link issues to PRs and PRs to issues.
- Close issues via commit message: `fix(proposer): recover nonce after trigger failure. Closes #207`.

### Pull Requests
- **One PR per feature/fix.** Do NOT combine unrelated changes.
- PR title matches the primary commit convention: `feat(bridge): implement L2→L1 withdrawal triggers`.
- PR description: (1) what changed and why, (2) link to issue, (3) docs/DERIVATION.md sections affected, (4) test results (`cargo nextest` output summary), (5) QA results if applicable (state root convergence, Docker E2E).
- **Before merging**: all tests pass, clippy clean, auditor has reviewed (for consensus-critical), QA has validated in Docker (for bridge/cross-chain).
- Squash merge to main for clean history.

### Agent Git Rules
- `core-worker`, `test-writer`, `ui-worker`: create branch, commit atomically, push. Do NOT open PR (user reviews and merges).
- `auditor`, `qa`: read-only — never commit.
- `spec-writer`: commits to same feature branch as core-worker when spec updates accompany code changes.
- `general-worker`: creates branches for script/Docker changes. Creates issues for findings.
- `maintainer`: commits CLAUDE.md/agent updates to a dedicated `docs/<description>` branch.

## Test Coverage

540 tests across unit, EVM integration, and E2E. Tests in `*_tests.rs` sibling files. The `e2e_anvil` and `evm_executor` integration tests require `anvil` running locally (and compiled contract artifacts in `contracts/sync-rollups-protocol/out/`). No clippy errors, no `unwrap()` in production code.
