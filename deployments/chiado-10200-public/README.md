# Deploying on Public Chiado (via block-builder RPC)

This deployment targets **public Chiado** (chain ID 10200) as L1 and submits `postBatch` transactions through an `eth_sendBundle`-compatible block-builder RPC. Use this when the rollup needs to run against real-network Chiado, as opposed to the shadowfork setup under `deployments/chiado-10200/`.

All commands should be run from the **repo root**.

---

## Why a separate deployment

`deployments/chiado-10200/` submits postBatch txs via `eth_sendRawTransaction` against a solo-validator shadowfork, which guarantees our tx lands in the very next L1 slot. On public Chiado:

- `eth_sendRawTransaction` puts the tx in the mempool for whichever validator mines next. That validator may be on another host/region — by the time our tx reaches them, the slot we signed against has already been produced and they include us in a later block.
- `publicInputsHash` in `Rollups.sol` commits to `blockhash(block.number - 1)` and `block.timestamp`, so any drift in the containing block causes `InvalidProof()`.

This deployment routes writes through an `eth_sendBundle` endpoint (rbuilder / Flashbots style) that accepts a target L1 block number. The builder either includes the tx in that block or silently drops the bundle. Either outcome is safe — we never have a tx land in a later block with the wrong committed context.

---

## Prerequisites

Same as `chiado-10200` plus:

1. A block-builder RPC URL on Chiado that supports `eth_sendBundle` (see the `L1_BUILDER_RPC_URL` in `.env.example`). The one in `.env.example` (`https://builder.chiado.gcd.ovh/`) is a known rbuilder instance — swap it for whichever you use.
2. A public Chiado RPC for reads (`GNOSIS_RPC_URL`).
3. Foundry, Rust, Docker Compose as per the main README.
4. Deployer + builder keys funded with Chiado xDAI.

---

## 1. Deploy contracts

```bash
deployments/chiado-10200-public/deploy.sh \
    http://37.27.238.19:18545 \
    0xDEPLOYER_PRIVATE_KEY \
    0xBUILDER_PRIVATE_KEY
```

Writes `deployments/chiado-10200-public/rollup-gnosis.env` + `genesis.json` (both gitignored).

## 2. Configure environment

```bash
cp deployments/chiado-10200-public/.env.example deployments/chiado-10200-public/.env
$EDITOR deployments/chiado-10200-public/.env
```

Required in `.env`:
- `BUILDER_PRIVATE_KEY` — the one you registered with `createRollup()`.
- `GNOSIS_RPC_URL` — Chiado RPC for reads.
- `L1_BUILDER_RPC_URL` — block-builder RPC for `eth_sendBundle` writes.

## 3. Start

```bash
docker compose \
  -f deployments/chiado-10200-public/docker-compose.yml \
  -f deployments/chiado-10200-public/docker-compose.dev.yml \
  --env-file deployments/chiado-10200-public/.env \
  up -d
```

Or with the explorer overlays:

```bash
docker compose \
  -f deployments/chiado-10200-public/docker-compose.yml \
  -f deployments/chiado-10200-public/docker-compose.dev.yml \
  -f deployments/shared/docker-compose.explorer.yml \
  -f deployments/chiado-10200-public/docker-compose.explorer.yml \
  -f deployments/chiado-10200-public/docker-compose.explorer.override.yml \
  --env-file deployments/chiado-10200-public/.env \
  up -d
```

## 4. Ports

Offset from `chiado-10200/` so both can run on the same host:

| Service | Port |
|---|---|
| Builder L2 JSON-RPC | 9645 |
| Builder L2 WebSocket (localhost only) | 9650 |
| Builder L2 Proxy RPC | 9648 |
| Builder L1 Proxy RPC | 9656 |
| Builder health | 9660 |
| Fullnode L2 JSON-RPC | 9646 |
| L2 explorer API | 4103 |
| L2 explorer frontend | 4101 |

## 5. Monitor

```bash
# Health
curl -s http://localhost:9660/health | python3 -m json.tool

# Builder logs — look for "submitted bundle to builder RPC" + "postBatch confirmed on L1"
docker compose \
  -f deployments/chiado-10200-public/docker-compose.yml \
  -f deployments/chiado-10200-public/docker-compose.dev.yml \
  --env-file deployments/chiado-10200-public/.env \
  logs -f builder

# L1 nonce (should advance with every confirmed bundle)
source deployments/chiado-10200-public/rollup-gnosis.env
cast nonce --rpc-url "$L1_RPC_URL" "$BUILDER_ADDRESS"

# On-chain state root
cast call --rpc-url "$L1_RPC_URL" "$ROLLUPS_ADDRESS" \
    "rollups(uint256)(address,bytes32,bytes32,uint256)" 1
```

---

## File reference

| File | Purpose |
|------|---------|
| `deploy.sh` | Deploy L1 contracts, generate `rollup-gnosis.env` + `genesis.json` |
| `docker-compose.yml` | Compose (builder + fullnode), bundle-RPC wiring |
| `docker-compose.dev.yml` | Dev overlay — mount locally built binary |
| `docker-compose.explorer.yml` / `.override.yml` | Optional L2 Blockscout overlays |
| `.env.example` | Template for secrets + URLs |
| `rollup-gnosis.env` | Generated (gitignored) |
| `genesis.json` | Generated (gitignored) |
