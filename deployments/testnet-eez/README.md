# Based Rollup Testnet — eez.dev (chain ID 1337)

Local development deployment with reth --dev as L1.

## Quick Start

From repo root:

```bash
# Build binary
cargo build --release

# First time (production image):
sudo docker compose -f deployments/testnet-eez/docker-compose.yml up -d

# Dev mode (mounts local binary, faster iteration):
sudo docker compose -f deployments/testnet-eez/docker-compose.yml \
                    -f deployments/testnet-eez/docker-compose.dev.yml up -d

# Iterate (no rebuild needed):
cargo build --release
sudo docker compose -f deployments/testnet-eez/docker-compose.yml \
                    -f deployments/testnet-eez/docker-compose.dev.yml \
                    restart builder fullnode1 fullnode2
```

## With Explorers

```bash
# Add L1 + L2 Blockscout explorers:
sudo docker compose -f deployments/testnet-eez/docker-compose.yml \
                    -f deployments/shared/docker-compose.explorer.yml \
                    -f deployments/testnet-eez/docker-compose.explorer.yml up -d
```

## Ports

| Host Port | Service | Description |
|-----------|---------|-------------|
| 9555 | l1 | L1 JSON-RPC |
| 9545 | builder | L2 JSON-RPC |
| 9546 | fullnode1 | L2 JSON-RPC |
| 9547 | fullnode2 | L2 JSON-RPC |
| 9548 | builder | L2 RPC Proxy |
| 9550 | builder | L2 WebSocket (localhost) |
| 9556 | builder | L1 RPC Proxy |
| 9560 | builder | Health endpoint |
| 8080 | sync-ui | Dashboard |
| 4000 | l1-frontend | L1 Explorer |
| 4001 | l2-frontend | L2 Explorer |
| 4002 | l1-explorer | L1 Explorer API |
| 4003 | l2-explorer | L2 Explorer API |

## Services

Startup order: `l1 -> deploy -> builder -> deploy-l2 -> deploy-reverse-flash-loan -> complex-tx-sender`

All services start by default (no profiles needed).
