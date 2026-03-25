# Kurtosis L1 Devnet for Based Rollup

Run the based-rollup stack against a **real PoS Ethereum L1** (reth + Lighthouse) with actual finality. Unlike `reth --dev` which finalizes every block immediately, the Kurtosis devnet produces a real head/safe/finalized gap — required for testing rewind-to-finalized behavior.

## Why?

The `reth --dev` L1 in `docker-compose.yml` finalizes every block at submission time (`head == safe == finalized`). This means the rollup's finality-aware logic (reorg rollback to finalized, cursor pruning) is never exercised. The Kurtosis setup provides:

- **Real PoS consensus** (Lighthouse beacon + reth EL)
- **12-second slots** matching the existing block time
- **Head/safe/finalized gap** (~2 epochs behind, ~3 minutes with 8 slots/epoch)
- **Pre-funded dev accounts** identical to the Anvil defaults used by all scripts

## Prerequisites

1. **Docker** (running)
2. **Kurtosis CLI** — [install guide](https://docs.kurtosis.com/install/)
   ```bash
   echo "deb [trusted=yes] https://apt.fury.io/kurtosis-tech/ /" | \
     sudo tee /etc/apt/sources.list.d/kurtosis.list
   sudo apt update && sudo apt install kurtosis-cli
   ```
3. **jq** — `sudo apt-get install jq`
4. **Docker images built** (from repo root):
   ```bash
   docker build -f deployments/shared/Dockerfile --target runtime -t based-rollup:local .
   ```

## Startup Order

### 1. Start the Kurtosis L1

```bash
cd based-rollup
bash deployments/kurtosis-1337/start.sh
```

This will:
- Start the Kurtosis engine (if not running)
- Launch an enclave `based-rollup-l1` with reth + Lighthouse
- Wait for the L1 RPC to be ready
- Verify chain ID (1337) and pre-funded accounts
- Write `deployments/kurtosis-1337/.env.kurtosis` with the RPC/WS URLs

### 2. Start the rollup stack

```bash
docker compose -f deployments/kurtosis-1337/docker-compose.yml --env-file deployments/kurtosis-1337/.env.kurtosis up -d
```

This starts: `deploy` → `builder` → `fullnode1` + `fullnode2` + `tx-sender`

### 3. (Optional) Start profiles

```bash
# Complex transactions
docker compose -f deployments/kurtosis-1337/docker-compose.yml --env-file deployments/kurtosis-1337/.env.kurtosis --profile complex up -d

# Cross-chain + UI
docker compose -f deployments/kurtosis-1337/docker-compose.yml --env-file deployments/kurtosis-1337/.env.kurtosis --profile sync up -d

# Block explorers
docker compose -f deployments/kurtosis-1337/docker-compose.yml --env-file deployments/kurtosis-1337/.env.kurtosis --profile explorer up -d
```

### 4. Verify finality

```bash
bash deployments/kurtosis-1337/verify-finality.sh
# or watch continuously:
bash deployments/kurtosis-1337/verify-finality.sh --watch
```

Expected output after ~25 minutes (4 epochs with 32 slots/epoch, 12s/slot):
```
  Head:      128
  Safe:      96
  Finalized: 64
  Gaps:      head-safe=32  safe-finalized=32  head-finalized=64
  Status:    REAL FINALITY — head/safe/finalized differ!
```

## Ports

All host ports use a **+1000 offset** from the original `docker-compose.yml` so both stacks can run simultaneously:

| Service    | Original | Kurtosis | Description          |
|------------|----------|----------|----------------------|
| Builder    | 9545     | 10545    | L2 JSON-RPC          |
| Builder    | 9548     | 10548    | L2 RPC Proxy         |
| Builder    | 9550     | 10550    | L2 WebSocket (local) |
| Builder    | 9556     | 10556    | L1 RPC Proxy         |
| Builder    | 9560     | 10560    | Health endpoint      |
| Fullnode 1 | 9546     | 10546    | L2 JSON-RPC          |
| Fullnode 2 | 9547     | 10547    | L2 JSON-RPC          |
| Sync UI    | 8080     | 9080     | Dashboard            |
| L1 Expl FE | 4000     | 5000     | Frontend             |
| L1 Expl API| 4002     | 5002     | API                  |
| L2 Expl FE | 4001     | 5001     | Frontend             |
| L2 Expl API| 4003     | 5003     | API                  |

The L1 RPC port is dynamically assigned by Kurtosis — see `deployments/kurtosis-1337/.env.kurtosis` for the actual URL.

## Accounts

The Kurtosis genesis pre-funds the same Anvil dev accounts:

| Account | Address | Role |
|---------|---------|------|
| #0 | `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` | Deployer / Builder |
| #1 | `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` | tx-sender |
| #4 | `0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65` | crosschain-tx-sender |
| #5 | `0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc` | complex-tx-sender |
| Extra | `0xF35960302a07022aBa880DFFaEC2Fdd64d5BF1c1` | Funded by deploy.sh |

All accounts have 10,000 ETH in the Kurtosis genesis.

## Key Differences from docker-compose.yml

| Aspect | Original (reth --dev) | Kurtosis |
|--------|----------------------|----------|
| L1 type | Single reth in dev mode | reth + Lighthouse PoS |
| Finality | Instant (head=finalized) | Real (~2 epoch delay) |
| Chain ID | 1337 | 1337 (configured to match) |
| L1 management | Docker Compose service | Kurtosis enclave (external) |
| L1 RPC | `http://l1:8545` (internal) | Dynamic host port |

## Troubleshooting

### Kurtosis engine won't start
```bash
kurtosis engine restart
```

### Chain ID mismatch
The `network_params.yaml` sets `network_id: "1337"`. If the ethereum-package doesn't honor this, check `deployments/kurtosis-1337/start.sh` output for warnings. The scripts detect chain ID dynamically via `cast chain-id`.

### Finality not appearing
PoS finality requires ~4 epochs (epochs 0-1 are genesis, justification starts at epoch 2, finalization at epoch 4). With mainnet preset (32 slots/epoch) and 12s slots, that's ~25 minutes. Use `--watch`:
```bash
bash deployments/kurtosis-1337/verify-finality.sh --watch
```

### Deploy fails (account not funded)
Verify the prefunded accounts in `network_params.yaml`. Check balance:
```bash
source deployments/kurtosis-1337/.env.kurtosis
cast balance 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url $KURTOSIS_L1_RPC_URL
```

### Rollup nodes can't connect to L1
The Kurtosis L1 runs on the Docker host network, not inside the compose network. The `KURTOSIS_L1_RPC_URL` uses `127.0.0.1:PORT`, which should be accessible from compose services via `host.docker.internal` or `network_mode: host`. If containers can't reach it, you may need to add `extra_hosts: ["host.docker.internal:host-gateway"]` to the compose services or use `network_mode: host`.

### Cleanup
```bash
# Stop rollup stack
docker compose -f deployments/kurtosis-1337/docker-compose.yml --env-file deployments/kurtosis-1337/.env.kurtosis down -v

# Destroy Kurtosis enclave
kurtosis enclave rm -f based-rollup-l1

# Full Kurtosis cleanup
kurtosis clean -a
```
