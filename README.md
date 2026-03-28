# Sync Rollup Composer

Synchronous cross-chain rollup composer. Atomic L1↔L2 synchronous composability.

A rollup node built on [reth](https://github.com/paradigmxyz/reth) with synchronous cross-chain composability — L2 contracts can call L1 and get return values in the same transaction. L1 is the sequencer, `Rollups.sol` on L1 is the canonical source of truth. Any node can re-derive the full L2 chain from L1 events alone.

> Theoretical basis: [Synchronous composability between rollups via realtime proving](https://ethresear.ch/t/synchronous-composability-between-rollups-via-realtime-proving/23998)

## Overview

L2 blocks follow a deterministic 12-second timestamp schedule. The builder posts batches to `Rollups.sol` via `postBatch(entries, blobCount, callData, proof)` — the sole interface between L1 and L2. Fullnodes derive the canonical chain from `BatchPosted` events independently and verify builder preconfirmations over WebSocket.

Cross-chain composability is native: L1 contracts can call L2 contracts and back within a single synchronous execution context. The protocol tracks state transitions for each cross-chain call as chained `StateDelta` entries in the execution table. Multi-call continuations (flash loans spanning L1 and L2) are supported via scope navigation.

Batch submission carries an ECDSA proof of the `publicInputsHash` (development-grade; production will use ZK proofs). The on-chain `tmpECDSAVerifier` recovers the signer and checks it against the rollup's registered verification key.

## Architecture

```
                         L1 (reth --dev)
                               |
              +----------------+----------------+
              |                                 |
         Rollups.sol                       Bridge L1
         tmpECDSAVerifier              CrossChainProxy L1
              |                                 |
              |  BatchPosted events             | hold-then-forward
              |                                 |
    +---------+---------+           +-----------+-----------+
    |      Builder      |           |   L1 Proxy (9556)     |
    |                   |           | deposit detection,    |
    | builds L2 blocks  |           | debug_traceCallMany   |
    | posts postBatch   |           +-----------+-----------+
    | sends triggers    |                       |
    | signs proofs      |                       v
    +---------+---------+         (entries queued, user tx forwarded)
              |                                 |
              | WS preconfirmations (9550)       |
              |                                 |
    +---------+--------+            +-----------+----------+
    |     Fullnode 1   |            |     L2 (9545)        |
    |     Fullnode 2   |            |  CrossChainManagerL2 |
    |                  |            |  Bridge L2           |
    | derive from L1   |            |  CrossChainProxy L2  |
    | verify against   |            |                      |
    | preconfirmations |            | L2 Proxy (9548)      |
    +------------------+            | withdrawal detection |
                                    +----------------------+
```

### Operating Modes

| Mode | Description |
|------|-------------|
| **Sync** | Catching up: reads `BatchPosted` events from L1, re-derives L2 blocks |
| **Builder** | Caught up: builds blocks from mempool, posts batches to L1, sends cross-chain triggers |
| **Fullnode** | Caught up (non-builder): derives from L1, receives preconfirmations from builder WS |

## Key Features

- **L1→L2 deposits**: `bridgeEther` on L1 → `executeIncomingCrossChainCall` on L2 via `CrossChainManagerL2`. ETH comes from CCM's 1M ETH genesis allocation; no runtime minting.
- **L2→L1 withdrawals**: `bridgeEther(0)` on L2 detected by L2 proxy → withdrawal trigger sent to L1 Bridge via `postBatch` + nonce-linked trigger tx.
- **Multi-call continuations (flash loans)**: L1 proxy traces multi-call transactions via `debug_traceCallMany`, builds L1+L2 entry chains. On L2, `loadExecutionTable` loads continuation entries; a single `executeIncomingCrossChainCall` drives the full chain (receive → process → bridge back) via `newScope()`.
- **Scope navigation**: Nested cross-chain calls navigate scopes hierarchically. `ExecutionEntry.nextAction.scope` carries the path. L1 consumption uses `_processCallAtScope` for withdrawal entries.
- **Hold-then-forward**: Both proxies queue entries, await L1 confirmation, then forward the user tx. Prevents timing races between entry loading and execution.
- **Entry verification with rewind**: After `MAX_ENTRY_VERIFY_DEFERRALS=3` retries, `verify_local_block_matches_l1` returns `Err` to trigger a rewind to `entry_block - 1` for re-derivation.
- **Unified intermediate roots**: `attach_unified_chained_state_deltas()` builds a single D+W+1 root chain covering all deposits and withdrawals in a block. Deposits and withdrawals can coexist.
- **Explicit nonces**: All L1 submissions use `send_l1_tx_with_nonce()`. `reset_nonce()` is called after any L1 tx failure to prevent permanent livelock.

## Quick Start

### Prerequisites

- Rust 1.85+ (nightly for `cargo fmt`)
- Docker and Docker Compose
- [Foundry](https://getfoundry.sh/) (for E2E tests only)

### Build and Run

```bash
# Build the binary
cargo build --release

# Start devnet (first time)
sudo docker compose -f deployments/testnet-eez/docker-compose.yml \
     -f deployments/testnet-eez/docker-compose.dev.yml up -d

# Check health
curl http://localhost:9560/health

# Iterate after code changes (no rebuild of Docker images needed)
cargo build --release
sudo docker compose -f deployments/testnet-eez/docker-compose.yml \
     -f deployments/testnet-eez/docker-compose.dev.yml \
     restart builder fullnode1 fullnode2
```

### Verify Convergence

```bash
# Block numbers should match across nodes
cast block-number --rpc-url http://localhost:9545   # builder
cast block-number --rpc-url http://localhost:9546   # fullnode1
cast block-number --rpc-url http://localhost:9547   # fullnode2

# Block hashes should be identical
cast block --rpc-url http://localhost:9545 latest --field hash
cast block --rpc-url http://localhost:9546 latest --field hash
```

## Project Structure

```
sync-rollup-composer/
├── docs/
│   ├── DERIVATION.md                   # Normative consensus & derivation spec
│   └── architecture.excalidraw         # Architecture diagram
├── CLAUDE.md                           # Development guide (architecture, lessons)
├── .claude/agents/                     # Subagent definitions
├── contracts/sync-rollups-protocol/    # Solidity contracts submodule (includes docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md)
├── deployments/
│   ├── shared/                         # Shared Dockerfiles, genesis, explorer compose, service scripts
│   ├── testnet-eez/                    # Local devnet (reth --dev L1, chain 42069 L2)
│   ├── gnosis-100/                     # Gnosis Chain deployment
│   ├── chiado-10200/                   # Chiado testnet deployment
│   ├── kurtosis-1337/                  # Kurtosis PoS devnet
│   └── ethereum-1/                     # Mainnet placeholder
├── scripts/                            # Host-only: E2E tests, tooling
├── ui/                                 # React dashboard (port 8080)
└── crates/sync-rollup-composer/src/
    ├── driver.rs                       # Mode orchestration, flush_to_l1, hold, triggers
    ├── derivation.rs                   # L1 sync, §4e/§4f filtering
    ├── cross_chain.rs                  # Entry types, ABI, filtering, continuations
    ├── table_builder.rs                # Flash loan analysis, L1/L2 entry building
    ├── evm_config.rs                   # EVM config (thin wrapper around EthBlockExecutor)
    ├── proposer.rs                     # L1 submission, explicit nonces, recovery
    ├── proxy.rs                        # L2 proxy: withdrawal detection, hold-then-forward
    ├── l1_proxy.rs                     # L1 proxy: deposit detection, hold-then-forward
    ├── execution_planner.rs            # Tx simulation, action hash computation
    └── rpc.rs                          # syncrollups_* RPC namespace
```

## Devnet Ports

| Port | Service | Description |
|------|---------|-------------|
| 9555 | l1 | L1 JSON-RPC (reth --dev) |
| 9545 | builder | L2 JSON-RPC |
| 9546 | fullnode1 | L2 JSON-RPC |
| 9547 | fullnode2 | L2 JSON-RPC |
| 9548 | builder | L2 RPC Proxy (cross-chain detection) |
| 9550 | builder | L2 WebSocket (preconfirmations) |
| 9556 | builder | L1 RPC Proxy (deposit detection) |
| 9560 | builder | Health endpoint |
| 8080 | sync-ui | Dashboard |

Explorer ports (requires explorer overlay): L1 frontend 4000, L2 frontend 4001, L1 API 4002, L2 API 4003.

Startup order: `l1 → deploy → builder → deploy-l2 → deploy-reverse-flash-loan → complex-tx-sender`

## Build and Test

```bash
cargo build --release
cargo nextest run --workspace          # ~529 tests across unit, EVM integration, and E2E
cargo clippy --workspace --all-features
cargo +nightly fmt --all
```

The 81 `e2e_anvil` tests and 9 `evm_executor` tests require `anvil` running locally and compiled contract artifacts in `contracts/sync-rollups-protocol/out/`. ~439 unit tests pass standalone.

## L1 Contracts

| Contract | Role |
|----------|------|
| `Rollups.sol` | Sole L1 interface: `postBatch`, `BatchPosted`, state root registry |
| `tmpECDSAVerifier` | Development proof verifier: ecrecover against registered key |
| `Bridge` | ETH bridging between L1 and L2 |
| `CrossChainProxy` (L1) | Authorized proxy for L1-side cross-chain calls (created on demand via `createCrossChainProxy`) |

## L2 Contracts

Deployed by the builder in block 1 protocol transactions (deterministic CREATE addresses from builder nonce 0-3):

| Contract | Address | Role |
|----------|---------|------|
| `L2Context` | `0x5FbDB231…` (nonce 0) | Per-block context: `setContext(l1ParentBlockNumber, l1ParentBlockHash)` |
| `CrossChainManagerL2` | `0xe7f1725E…` (nonce 1) | Execution table, scope navigation, entry consumption |
| `Bridge` (L2) | `0x9fE46736…` (nonce 2) | ETH bridging, `bridgeEther` (initialized at nonce 3) |
| `CrossChainProxy` (L2) | created on demand | Authorized proxy for L2-side cross-chain calls |

## Documentation

| Document | Purpose |
|----------|---------|
| [DERIVATION.md](docs/DERIVATION.md) | Normative spec — consensus rules, block timing, derivation, §4f filtering |
| [SYNC_ROLLUPS_PROTOCOL_SPEC.md](contracts/sync-rollups-protocol/docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md) | Formal protocol spec — data model, entry lifecycle, scope navigation, bridge flows |
| [CLAUDE.md](CLAUDE.md) | Development guide — architecture, Docker workflow, dev accounts, lessons learned |
| [deployments/testnet-eez/README.md](deployments/testnet-eez/README.md) | Devnet setup and port reference |

## License

MIT OR Apache-2.0
