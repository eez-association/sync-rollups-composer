# Deploying on Chiado Testnet (chain ID 10200)

Chiado is the Gnosis Chain testnet. Use this deployment for staging and testing before promoting to Gnosis mainnet (`deployments/gnosis-100`).

The configuration is identical to the Gnosis mainnet deployment except:

| | Gnosis mainnet | Chiado testnet |
|---|---|---|
| Chain ID | 100 | 10200 |
| RPC | `https://rpc.gnosischain.com` | `https://rpc.chiadochain.net` |
| Native token | xDAI (real value) | Chiado xDAI (test tokens, free) |
| Explorer | gnosisscan.io | gnosis-chiado.blockscout.com |
| Faucet | N/A | [Chiado faucet](https://faucet.chiadochain.net/) |

All commands should be run from the **repo root**.

---

## Quick Start

### 1. Obtain Chiado xDAI

Get test tokens from the [Chiado faucet](https://faucet.chiadochain.net/) for both the deployer and builder accounts.

Verify connectivity and chain ID:
```bash
cast chain-id --rpc-url https://rpc.chiadochain.net
# Must return: 10200
```

### 2. Deploy Contracts

```bash
SKIP_CHAIN_CHECK=1 deployments/chiado-10200/deploy.sh \
    https://rpc.chiadochain.net \
    0xDEPLOYER_PRIVATE_KEY \
    0xBUILDER_PRIVATE_KEY
```

`SKIP_CHAIN_CHECK=1` bypasses the chain ID 100 guard in the deploy script (Chiado uses 10200).

The script writes contract addresses and metadata to `deployments/chiado-10200/rollup-gnosis.env`.

### 3. Configure Environment

```bash
cp deployments/chiado-10200/.env.example deployments/chiado-10200/.env
# Edit .env and set BUILDER_PRIVATE_KEY
```

### 4. Start the Rollup

```bash
docker compose -f deployments/chiado-10200/docker-compose.yml --env-file deployments/chiado-10200/.env up -d
docker compose -f deployments/chiado-10200/docker-compose.yml --env-file deployments/chiado-10200/.env logs -f builder
```

---

## Verification

```bash
# Block production
cast block-number --rpc-url http://localhost:9545

# Fullnode sync
cast block-number --rpc-url http://localhost:9546

# Health endpoint
curl -s http://localhost:9560/health | python3 -m json.tool
```

---

## Notes

- The `rollup-gnosis.env` filename is reused from the Gnosis mainnet deploy script — this is intentional (the `init-config` container expects this filename).
- For full operational details (monitoring, backup, troubleshooting), refer to `deployments/gnosis-100/README.md` — all procedures apply identically, substituting `chiado-10200` for `gnosis-100` and `https://rpc.chiadochain.net` for the L1 RPC URL.
- Docker volume names are prefixed with `chiado-` to avoid conflicts with a simultaneous gnosis-100 deployment on the same host.

---

## File Reference

| File | Purpose |
|------|---------|
| `deployments/chiado-10200/deploy.sh` | Deploy contracts to Chiado, generate `rollup-gnosis.env` |
| `deployments/chiado-10200/docker-compose.yml` | Compose file (builder + fullnode) |
| `deployments/chiado-10200/.env.example` | Template for secrets and runtime overrides |
| `deployments/chiado-10200/rollup-gnosis.env` | Generated config (created by deploy.sh) |
| `deployments/shared/genesis.json` | L2 genesis configuration (shared across all deployments) |
