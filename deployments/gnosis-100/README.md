# Deploying on Gnosis Chain

End-to-end guide for deploying the based rollup with **Gnosis Chain** (chain ID 100) as L1.

All commands in this guide should be run from the **repo root** unless otherwise noted.

**Key differences from local devnet:**

| | Local (reth --dev) | Gnosis Chain |
|---|---|---|
| L1 RPC | `http://l1:8545` (local) | External RPC provider |
| `BLOCK_TIME` | `12` | `5` |
| L1 chain ID | `1337` | `100` |
| Native token | ETH (free) | xDAI (real money) |
| Finality | Instant | ~5 min |
| Verifier | tmpECDSAVerifier | tmpECDSAVerifier (ECDSA-based, dev only) |

---

## 1. Prerequisites

### 1.1 Software

| Tool | Version | Check |
|------|---------|-------|
| Foundry (forge, cast) | Latest | `forge --version` |
| Rust toolchain | Stable | `cargo --version` |
| Docker + Compose | v2+ | `docker compose version` |

```bash
# Build everything
git submodule update --init
cd contracts/sync-rollups && forge build --skip test && cd ../..
cargo build --release
docker build -f deployments/shared/Dockerfile --target runtime -t based-rollup:local .
```

### 1.2 Accounts

Generate two separate key pairs — deployer and builder:

```bash
cast wallet new   # → deployer (funds contracts, then goes offline)
cast wallet new   # → builder  (hot wallet, signs blocks + postBatch)
```

Record the addresses and private keys securely. **Never commit private keys.**

### 1.3 Funding (xDAI)

| Account | Amount | Purpose |
|---------|--------|---------|
| Deployer | ~0.5 xDAI | Contract deployment (tmpECDSAVerifier + Rollups + createRollup) |
| Builder | ~10 xDAI initial | Ongoing postBatch submissions (~0.001–0.01 xDAI each) |

Obtain xDAI via [Gnosis Bridge](https://bridge.gnosischain.com/) or exchanges.

Verify balances:
```bash
cast balance --rpc-url https://rpc.gnosischain.com <DEPLOYER_ADDRESS> --ether
cast balance --rpc-url https://rpc.gnosischain.com <BUILDER_ADDRESS> --ether
```

### 1.4 RPC Provider

The public endpoint `https://rpc.gnosischain.com` is rate-limited. For production,
use a paid provider (Blast API, Ankr, QuickNode) that supports both HTTPS and WSS.

**Flash loan / cross-chain note:** Flash loan detection uses `debug_traceCallMany` to analyze
multi-call transactions. Public Chiado and Gnosis RPC endpoints do not support this method —
an archive node (self-hosted or a provider that exposes debug APIs) is required for flash loan
functionality. Basic deposits, withdrawals, and block production work with any standard RPC.

Verify connectivity:
```bash
cast chain-id --rpc-url https://rpc.gnosischain.com
# Must return: 100
```

### 1.5 Genesis State Root

Compute the L2 genesis state root — it must match exactly what `createRollup()` receives:

```bash
./target/release/based-rollup genesis-state-root --chain deployments/shared/genesis.json
# Record this value
```

---

## 2. Contract Deployment

### 2.1 Run the Deploy Script

```bash
deployments/gnosis-100/deploy.sh \
    https://rpc.gnosischain.com \
    0xDEPLOYER_PRIVATE_KEY \
    0xBUILDER_PRIVATE_KEY
```

The script performs these steps:
1. Validates chain ID = 100
2. Checks deployer balance (warns if < 0.1 xDAI)
3. Builds contracts from `contracts/sync-rollups`
4. Deploys **tmpECDSAVerifier** (constructor args: `owner`, `signer`)
5. Deploys **Rollups.sol** (constructor args: verifier address, numRollups=1)
6. Computes genesis state root via `based-rollup genesis-state-root`
7. Calls `createRollup(genesisStateRoot, 0x01, deployer)` on the Rollups contract
8. Extracts deployment block number and timestamp
9. Computes deterministic L2 contract addresses (L2Context at builder nonce 0, CCM at nonce 1)
10. Extracts L2 contract bytecodes (L2Context.sol, CrossChainManagerL2.sol)
11. Writes everything to **`deployments/gnosis-100/rollup-gnosis.env`**

**Idempotency:** If `rollup-gnosis.env` already exists, the script prints it and exits.
Delete the file to force redeployment.

**Environment overrides:**
- `CONTRACTS_DIR` — path to contracts/ (default: `../contracts` relative to script)
- `GENESIS_JSON` — path to genesis.json (default: `deployments/shared/genesis.json`)
- `OUTPUT_FILE` — output path (default: `deployments/gnosis-100/rollup-gnosis.env`)
- `ROLLUP_BIN` — path to based-rollup binary (auto-detected from PATH or `target/release/`)
- `BOOTSTRAP_ACCOUNTS` — comma-separated `addr:eth` pairs for block-1 funding
- `SKIP_CHAIN_CHECK=1` — bypass chain ID 100 check (e.g. for Chiado testnet)

### 2.2 Verify On-Chain

```bash
GNOSIS_RPC=https://rpc.gnosischain.com

# Source the generated config for addresses
source deployments/gnosis-100/rollup-gnosis.env

# Confirm rollup was created (should return 1)
cast call --rpc-url $GNOSIS_RPC $ROLLUPS_ADDRESS "rollupCounter()(uint256)"

# Confirm genesis state root matches
cast call --rpc-url $GNOSIS_RPC $ROLLUPS_ADDRESS \
    "rollups(uint256)(address,bytes32,bytes32,uint256)" 1
# Returns: (owner, verificationKey, stateRoot, etherBalance)
# First value = owner (should be deployer address)
# Third value = stateRoot (should match genesis state root)
```

### 2.3 Verify on Gnosisscan (Optional)

Visit:
- tmpECDSAVerifier: `https://gnosisscan.io/address/<VERIFIER_ADDRESS>`
- Rollups: `https://gnosisscan.io/address/<ROLLUPS_ADDRESS>`

For source verification:
```bash
# Verify tmpECDSAVerifier
forge verify-contract --chain gnosis --etherscan-api-key <GNOSISSCAN_API_KEY> \
    <VERIFIER_ADDRESS> src/tmpECDSAVerifier.sol:tmpECDSAVerifier

# Verify Rollups (constructor args: verifier address + numRollups=1)
forge verify-contract --chain gnosis --etherscan-api-key <GNOSISSCAN_API_KEY> \
    <ROLLUPS_ADDRESS> src/Rollups.sol:Rollups \
    --constructor-args $(cast abi-encode "constructor(address,uint256)" <VERIFIER_ADDRESS> 1)
```

---

## 3. Environment Configuration

### 3.1 Create deployments/gnosis-100/.env

```bash
cp deployments/gnosis-100/.env.example deployments/gnosis-100/.env
```

Edit `deployments/gnosis-100/.env`:

```bash
# REQUIRED — builder's private key (signs blocks + postBatch)
BUILDER_PRIVATE_KEY=0xYOUR_BUILDER_PRIVATE_KEY

# OPTIONAL — override the RPC URL from rollup-gnosis.env
# (e.g. switch providers without redeploying)
GNOSIS_RPC_URL=https://rpc.gnosischain.com

# OPTIONAL tuning
#L1_GAS_OVERBID_PCT=20       # Gas price overbid for postBatch ordering
#HEALTH_PORT=9100             # Health endpoint port (0=disabled)
#BLOCKSCOUT_HOST=your-ip      # For explorer profile
```

**Security:** Verify `deployments/gnosis-100/.env` is gitignored (`.env` and `rollup*.env` are both in `.gitignore`).

### 3.2 Transfer Config to Deployment Host

If deploying on a different server than where you ran `deploy.sh`:
```bash
scp deployments/gnosis-100/rollup-gnosis.env deployments/gnosis-100/.env user@deploy-host:~/based-rollup/deployments/gnosis-100/
```

---

## 4. Start the Rollup

### 4.1 Launch Services

```bash
# Start builder + fullnode (default services)
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env up -d

# Watch builder logs
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env logs -f builder
```

**Startup sequence:**
1. `init-config` copies `rollup-gnosis.env` → `/shared/rollup.env` in the shared volume
2. Builder waits for config, initializes reth database from `deployments/shared/genesis.json`
3. Builder enters **Sync mode**, scans from `DEPLOYMENT_L1_BLOCK` for `BatchPosted` events
4. No batches found → transitions to **Builder mode**
5. **Block 1**: deploys L2Context + CrossChainManagerL2 via CREATE transactions, calls `setContext`
6. Continues building blocks every 5 seconds (`BLOCK_TIME=5`)
7. Fullnode starts after builder is healthy, derives from L1 + preconfirmation WS

### 4.2 Port Mapping

| Host Port | Container Port | Service | Description |
|-----------|---------------|---------|-------------|
| 9545 | 8545 | builder | L2 JSON-RPC |
| 9548 | 8547 | builder | L2 RPC Proxy (MetaMask, simulation) |
| 9550 | 8546 | builder | L2 WebSocket (localhost only) |
| 9556 | 9556 | builder | L1 RPC Proxy (cross-chain detection) |
| 9560 | 9100 | builder | Health endpoint |
| 9546 | 8545 | fullnode1 | L2 JSON-RPC |

---

## 5. Post-Deployment Verification

Run these checks in order after startup:

### 5.1 Block Production

```bash
# Wait ~30s for block 1, then check
cast block-number --rpc-url http://localhost:9545
# Should return ≥ 1
```

### 5.2 L2 Contract Deployment

```bash
source deployments/gnosis-100/rollup-gnosis.env

# L2Context should have code
cast code --rpc-url http://localhost:9545 $L2_CONTEXT_ADDRESS
# Must be non-empty (0x6080...)

# CrossChainManagerL2 should have code
cast code --rpc-url http://localhost:9545 $CROSS_CHAIN_MANAGER_ADDRESS
# Must be non-empty

# Verify L2Context stores block 1 context
cast call --rpc-url http://localhost:9545 $L2_CONTEXT_ADDRESS \
    "getContext(uint256)(uint256,uint256,uint256,bytes32)" 1
```

### 5.3 Fullnode Sync

```bash
# Fullnode block number should match builder
cast block-number --rpc-url http://localhost:9546

# Compare with builder
cast block-number --rpc-url http://localhost:9545
```

### 5.4 L1 Submission

```bash
# Health endpoint shows submission status
curl -s http://localhost:9560/health | python3 -m json.tool

# Check: mode should be "Builder", pending_submissions should decrease over time
# Wait for first batch confirmation, then verify on-chain:
source deployments/gnosis-100/rollup-gnosis.env
cast call --rpc-url https://rpc.gnosischain.com $ROLLUPS_ADDRESS \
    "rollups(uint256)(address,bytes32,bytes32,uint256)" 1
# stateRoot should advance beyond genesis
```

### 5.5 State Root Consistency

```bash
# L2 state root from builder
cast rpc --rpc-url http://localhost:9545 syncrollups_getStateRoot

# Compare with on-chain state root (after L1 confirmation)
source deployments/gnosis-100/rollup-gnosis.env
cast call --rpc-url https://rpc.gnosischain.com $ROLLUPS_ADDRESS \
    "rollups(uint256)(address,bytes32,bytes32,uint256)" 1
```

### 5.6 Smoke Test — L2 Transfer

```bash
# Use any funded L2 account (dev accounts from genesis have funds)
DEV_KEY=0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6

cast send --rpc-url http://localhost:9545 --private-key $DEV_KEY \
    0x0000000000000000000000000000000000000001 --value 0.001ether

# Verify on both builder and fullnode
cast balance --rpc-url http://localhost:9545 0x0000000000000000000000000000000000000001 --ether
cast balance --rpc-url http://localhost:9546 0x0000000000000000000000000000000000000001 --ether
```

### 5.7 Cross-Chain Test (Optional)

If using cross-chain features, point MetaMask to the L1 Proxy (port 9556) and:
1. Deploy a Counter contract on L2
2. Create a CrossChainProxy on L1 via the Rollups contract
3. Send `increment()` via the L1 proxy
4. Verify L2 state updates

See `scripts/send-crosschain-txs.sh` for an automated version.

---

## 6. Operational Runbook

### 6.1 Monitoring

**Health endpoint** (primary monitoring signal):
```bash
curl -s http://localhost:9560/health | python3 -m json.tool
```
Response fields:
- `healthy` — boolean; false if `consecutive_rewind_cycles > 10` or L2 head stale > 120s
- `mode` — "Sync", "Builder", or "Fullnode"
- `l2_head` — latest L2 block number
- `l1_derivation_head` — latest L1 block processed
- `pending_submissions` — blocks built but not yet on L1
- `consecutive_rewind_cycles` — Builder→Sync fallback count (0 = normal)

**Builder balance** (check daily):
```bash
source deployments/gnosis-100/rollup-gnosis.env
cast balance --rpc-url https://rpc.gnosischain.com $BUILDER_ADDRESS --ether
```
The code warns at **0.01 xDAI** (`LOW_BALANCE_THRESHOLD` in `proposer.rs`). For production,
alert at **1 xDAI** to allow time for refilling.

**Key log patterns to watch:**
```
"caught up to L1, switching to builder mode"  — healthy startup
"built and inserted builder block"            — blocks being produced
"postBatch submitted"                         — L1 submission sent
"batch confirmed"                             — L1 inclusion confirmed
"reorg detected"                              — Gnosis L1 reorg (rare)
"low balance"                                 — builder running low on xDAI
"pre_state_root mismatch"                     — state divergence (triggers rewind)
```

**Alert conditions:**
- `healthy: false` in health response
- `consecutive_rewind_cycles > 0` (state divergence)
- `pending_submissions` growing without decreasing
- Builder xDAI balance < 1
- No new L2 blocks for > 60s

### 6.2 Log Management

```bash
# All services
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env logs -f

# Builder only with timestamps
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env logs -f --timestamps builder

# Last 100 lines
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env logs --tail 100 builder
```

### 6.3 Cost Estimation

With `BLOCK_TIME=5` and Gnosis gas prices (~1–3 gwei):
- Each `postBatch()` uses ~100,000–300,000 gas (varies with batch size)
- Cost per batch: ~0.0001–0.001 xDAI
- One batch per L1 block (5s), ~17,280 batches/day
- **Estimated daily cost: ~$2–$17** depending on batch sizes and gas price

### 6.4 Restart Procedures

**Builder crash** — automatic via Docker `restart: unless-stopped`.
Resume point: L1ConfirmedAnchor (last confirmed L1 submission), not genesis.
Checkpoints are persisted every 64 L1 blocks.

**Manual builder restart:**
```bash
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env restart builder
```

**Fullnode desync** — wipe and re-derive from L1:
```bash
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env stop fullnode1
docker volume rm based-rollup_gnosis-fullnode1-data
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env up -d fullnode1
```

**Complete fresh restart** (wipe all L2 state, re-derive from L1; L1 contracts preserved):
```bash
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env down -v
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env up -d
```

### 6.5 Downtime Recovery

If the builder is stopped for a period:
1. L2 blocks are not produced during downtime
2. On restart, the builder enters Sync mode and checks L1 for missed batches
3. If no other builder submitted batches, it transitions to Builder mode
4. Builder produces catch-up blocks with correct timestamps (`deployment_timestamp + block_number * 5`)
5. Multiple blocks are batched into a single `postBatch()` call

### 6.6 Backup and Restore

**Data volumes:**
- `gnosis-builder-data` — reth MDBX database (builder chain state)
- `gnosis-fullnode1-data` — reth MDBX database (fullnode chain state)
- `gnosis-shared` — rollup configuration

**Backup** (while stopped):
```bash
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env stop builder
docker run --rm -v based-rollup_gnosis-builder-data:/data -v $(pwd)/backups:/backup \
    busybox tar czf /backup/builder-data-$(date +%Y%m%d).tar.gz -C /data .
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env start builder
```

**Restore:**
```bash
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env stop builder
docker volume rm based-rollup_gnosis-builder-data
docker volume create based-rollup_gnosis-builder-data
docker run --rm -v based-rollup_gnosis-builder-data:/data -v $(pwd)/backups:/backup \
    busybox tar xzf /backup/builder-data-YYYYMMDD.tar.gz -C /data
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env start builder
```

**Note:** Fullnode data doesn't need backup — it can always be re-derived from L1.

### 6.7 Upgrading

```bash
# Pull latest code and rebuild
git pull
cargo build --release
docker build -f deployments/shared/Dockerfile --target runtime -t based-rollup:local .

# Rolling restart (builder first, then fullnode)
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env up -d --no-deps builder
# Wait for builder to become healthy
docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env up -d --no-deps fullnode1
```

Builder data persists across restarts via the `gnosis-builder-data` volume.

### 6.8 Changing RPC Provider

To switch Gnosis RPC providers without redeploying:

1. Edit `deployments/gnosis-100/.env` and set `GNOSIS_RPC_URL=https://new-provider.com`
2. Restart services:
   ```bash
   docker compose -f deployments/gnosis-100/docker-compose.yml --env-file deployments/gnosis-100/.env up -d
   ```

The `GNOSIS_RPC_URL` from `deployments/gnosis-100/.env` overrides the `L1_RPC_URL` baked into `rollup-gnosis.env`.
If `GNOSIS_RPC_URL` is not set or empty, the original deployment-time URL is used.

---

## 7. Troubleshooting

### Builder stuck in Sync mode

**Symptom:** Health endpoint shows `"mode": "Sync"` indefinitely.

**Causes:**
- L1 RPC is unreachable or rate-limited → check connectivity, switch provider
- `DEPLOYMENT_L1_BLOCK` is wrong → verify against the actual deploy tx block number
- Contracts not deployed correctly → verify on-chain with cast calls from section 2.2

### `pre_state_root mismatch` errors

**Symptom:** Repeated state root mismatches in builder logs, cycling between Builder/Sync.

**Causes:**
- Non-empty `extraData` on some nodes → ensure `--builder.extradata ""` (set by `start-rollup.sh`)
- Different genesis.json between builder and fullnode → all nodes must use the same genesis
- Bug in block building → check `consecutive_rewind_cycles` in health endpoint

**Recovery:** After 5 consecutive mismatches, the builder falls back to L1ConfirmedAnchor
and re-derives. If the issue persists, do a full volume wipe and restart.

### `postBatch` transactions failing

**Symptom:** `pending_submissions` keeps growing, no "batch confirmed" logs.

**Causes:**
- Builder out of xDAI → check balance, refund
- Gas price too low → increase `L1_GAS_OVERBID_PCT` in `deployments/gnosis-100/.env`
- Nonce conflict → restart builder (clears pending nonce state)
- `lastStateUpdateBlock` constraint → only one postBatch per L1 block; if L1 blocks are missed, batches queue

### Fullnode not syncing

**Symptom:** Fullnode block number behind builder.

**Causes:**
- Builder WS not reachable → verify `BUILDER_WS_URL=ws://builder:8546` and builder is healthy
- L1 submissions delayed → fullnode derives from L1, so it lags until batches are confirmed
- Volume corruption → wipe fullnode volume and restart (section 6.4)

### `init-config` fails with "rollup-gnosis.env not found"

**Cause:** `deployments/gnosis-100/rollup-gnosis.env` doesn't exist.

**Fix:** Run `deployments/gnosis-100/deploy.sh` first, or copy the file from wherever it was generated.

---

## 8. Explorer (Optional)

Start Blockscout L2 explorer:
```bash
# Set your server's public IP/domain in .env
echo 'BLOCKSCOUT_HOST=your-server-ip' >> deployments/gnosis-100/.env

# Generate a secret key for Blockscout
echo "BLOCKSCOUT_SECRET_KEY=$(openssl rand -base64 48)" >> deployments/gnosis-100/.env

# Start with explorer overlay files
docker compose -f docker-compose.yml -f ../shared/docker-compose.explorer.yml -f docker-compose.explorer.yml --env-file .env up -d
```

Access:
- Frontend: `http://<BLOCKSCOUT_HOST>:4001`
- API: `http://<BLOCKSCOUT_HOST>:4003`

---

## 9. Security Considerations

1. **tmpECDSAVerifier is development-only.** It verifies proofs via ECDSA ecrecover against the
   configured signer key. Anyone who obtains the signer private key can submit fake state roots.
   Must be replaced with a real ZK verifier for trustless operation.

2. **Private keys.** Never commit `deployments/gnosis-100/.env`. Keep the deployer key offline after deployment.
   The builder key is a hot wallet — use a dedicated account with minimal balance.

3. **RPC exposure.** Restrict ports 9545/9546 (L2 RPC) to trusted IPs via firewall.
   Port 9550 (WS) is already localhost-only in docker-compose. Only open 9548/9556 if
   external users need proxy or cross-chain features.

4. **Single builder.** If the builder goes down, no new L2 blocks are produced. This is by
   design (based rollup with single builder). L2 state is always recoverable from L1.

5. **Blockscout secret.** Generate a unique `BLOCKSCOUT_SECRET_KEY` for production
   (`openssl rand -base64 48`). The default placeholder is insecure.

---

## 10. File Reference

| File | Purpose |
|------|---------|
| `deployments/gnosis-100/deploy.sh` | Deploy contracts to Gnosis, generate `rollup-gnosis.env` |
| `deployments/gnosis-100/docker-compose.yml` | Production compose (builder + fullnode + optional explorer) |
| `deployments/gnosis-100/.env.example` | Template for secrets and runtime overrides |
| `deployments/gnosis-100/rollup-gnosis.env` | Generated config (contract addresses, bytecodes, deployment metadata) |
| `scripts/start-rollup.sh` | Node entrypoint (shared with devnet) |
| `deployments/shared/genesis.json` | L2 genesis configuration (shared with devnet) |
| `crates/based-rollup/src/proposer.rs` | L1 submission logic, balance threshold |
| `crates/based-rollup/src/driver.rs` | Mode orchestration, block building |
| `crates/based-rollup/src/config.rs` | All environment variable definitions |
| `crates/based-rollup/src/health.rs` | Health endpoint implementation |
