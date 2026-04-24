#!/usr/bin/env bash
# Deploy Rollups + tmpECDSAVerifier + Bridge contracts to Chiado testnet (chain ID 10200)
# and generate rollup-gnosis.env + genesis.json in this directory.
#
# Usage:
#   deployments/chiado-10200/deploy.sh <CHIADO_RPC_URL> <DEPLOYER_PRIVATE_KEY> [BUILDER_PRIVATE_KEY]
#
# Example:
#   deployments/chiado-10200/deploy.sh http://your-chiado-host:8545 0xDEPLOYER_KEY 0xBUILDER_KEY
#
# If BUILDER_PRIVATE_KEY is omitted, the deployer key is used as the builder key.
# Not recommended for production (see CLAUDE.md "Builder key separation").
#
# This script is chiado-specific. It differs from deployments/gnosis-100/deploy-gnosis.sh
# in two ways that are required for the chiado deployment to actually produce blocks:
#   1. Injects a builder pre-mint into the genesis alloc so the builder can pay gas
#      for block-1 protocol txs (L2Context + CCM + Bridge deploys + Bridge.initialize).
#   2. Persists the modified genesis.json next to rollup-gnosis.env so the
#      docker-compose init-config service can seed it into the shared volume.
#      The baseline genesis baked into the image does NOT match the state root
#      registered on L1 via createRollup() — the container MUST load this file.
#
# Prerequisites:
#   - forge + cast (Foundry) installed
#   - based-rollup binary built (cargo build --release) or on PATH
#   - Deployer account funded with at least 0.1 Chiado xDAI (faucet: https://faucet.chiadochain.net/)
#   - Builder account funded with Chiado xDAI for ongoing postBatch submissions
#   - contracts/sync-rollups-protocol submodule initialized (git submodule update --init)
#
# Environment overrides:
#   CONTRACTS_DIR    — path to contracts/ (default: repo-root/contracts)
#   GENESIS_JSON     — path to baseline L2 genesis.json (default: deployments/shared/genesis.json)
#   OUTPUT_FILE      — output rollup env file path (default: this script's directory / rollup-gnosis.env)
#   ROLLUP_BIN       — path to based-rollup binary (default: auto-detect)
#   SHARED_DIR       — temp dir for genesis injection (default: /tmp/deploy-chiado-$$)
#   SKIP_CHAIN_CHECK — set to 1 to bypass the chain ID 10200 guard (e.g. for a local fork)
#   BOOTSTRAP_ACCOUNTS — comma-separated addr:eth pairs for block-1 funding (default: empty)
set -euo pipefail

CHIADO_RPC="${1:?Usage: deploy.sh <CHIADO_RPC_URL> <DEPLOYER_PRIVATE_KEY> [BUILDER_PRIVATE_KEY]}"
DEPLOYER_KEY="${2:?Usage: deploy.sh <CHIADO_RPC_URL> <DEPLOYER_PRIVATE_KEY> [BUILDER_PRIVATE_KEY]}"
BUILDER_KEY="${3:-$DEPLOYER_KEY}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT_FILE="${OUTPUT_FILE:-${SCRIPT_DIR}/rollup-gnosis.env}"
OUTPUT_DIR="$(dirname "$OUTPUT_FILE")"
OUTPUT_GENESIS="${OUTPUT_DIR}/genesis.json"
CONTRACTS_DIR="${CONTRACTS_DIR:-${SCRIPT_DIR}/../../contracts}"
GENESIS_JSON="${GENESIS_JSON:-${SCRIPT_DIR}/../shared/genesis.json}"

# ── Validate chain connection ──────────────────────────────────────────

echo "Checking chain connection at ${CHIADO_RPC}..."
CHAIN_ID=$(cast chain-id --rpc-url "$CHIADO_RPC" 2>/dev/null || echo "")
if [ -z "$CHAIN_ID" ]; then
    echo "ERROR: Cannot connect to RPC at ${CHIADO_RPC}"
    exit 1
fi

if [ "$CHAIN_ID" != "10200" ] && [ "${SKIP_CHAIN_CHECK:-}" != "1" ]; then
    echo "ERROR: Chain ID is ${CHAIN_ID}, expected 10200 (Chiado testnet)"
    echo "If this is intentional, set SKIP_CHAIN_CHECK=1"
    exit 1
fi
echo "Connected to chain ID ${CHAIN_ID}"

# ── Derive builder and deployer addresses ─────────────────────────────

BUILDER_ADDRESS=$(cast wallet address --private-key "$BUILDER_KEY")
DEPLOYER_ADDR=$(cast wallet address --private-key "$DEPLOYER_KEY")
echo "Deployer address: ${DEPLOYER_ADDR}"
echo "Builder address:  ${BUILDER_ADDRESS}"

DEPLOYER_BALANCE=$(cast balance --rpc-url "$CHIADO_RPC" "$DEPLOYER_ADDR" --ether 2>/dev/null || echo "0")
echo "Deployer balance: ${DEPLOYER_BALANCE} xDAI"
if [ "$(echo "$DEPLOYER_BALANCE < 0.1" | bc -l 2>/dev/null || echo "0")" = "1" ]; then
    echo "WARNING: Deployer balance (${DEPLOYER_BALANCE} xDAI) may be too low for deployment"
fi

# ── Idempotency check ─────────────────────────────────────────────────

if [ -f "$OUTPUT_FILE" ]; then
    echo "WARNING: ${OUTPUT_FILE} already exists."
    echo "Delete it (and ${OUTPUT_GENESIS}) manually to force a fresh deployment."
    cat "$OUTPUT_FILE"
    exit 0
fi

# ── Build contracts ───────────────────────────────────────────────────

echo ""
echo "Building sync-rollups-protocol contracts..."
cd "$CONTRACTS_DIR/sync-rollups-protocol"
forge build --skip test

# ── Helper: extract bytecode.object from forge JSON artifacts ─────────
# See CLAUDE.md: "ALL bytecodes MUST come from contracts/sync-rollups-protocol/out/"
_bc() { (grep -o '"object":"0x[0-9a-fA-F]*"' "$1" || true) | head -1 | sed 's/"object":"//;s/"//'; }

# ── Deploy tmpECDSAVerifier ───────────────────────────────────────────

echo ""
echo "Deploying tmpECDSAVerifier (owner=${DEPLOYER_ADDR}, signer=${BUILDER_ADDRESS})..."
VERIFIER_OUTPUT=$(forge create \
    --rpc-url "$CHIADO_RPC" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    src/verifier/tmpECDSAVerifier.sol:tmpECDSAVerifier \
    --constructor-args "$DEPLOYER_ADDR" "$BUILDER_ADDRESS" 2>&1)
echo "$VERIFIER_OUTPUT"
VERIFIER_ADDRESS=$(echo "$VERIFIER_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
[ -n "$VERIFIER_ADDRESS" ] || { echo "ERROR: Failed to deploy tmpECDSAVerifier"; exit 1; }
echo "tmpECDSAVerifier deployed at: ${VERIFIER_ADDRESS}"

# ── Deploy Rollups contract ───────────────────────────────────────────

echo ""
echo "Deploying Rollups contract..."
ROLLUPS_OUTPUT=$(forge create \
    --rpc-url "$CHIADO_RPC" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    src/Rollups.sol:Rollups \
    --constructor-args "$VERIFIER_ADDRESS" 1 2>&1)
echo "$ROLLUPS_OUTPUT"
ROLLUPS_ADDRESS=$(echo "$ROLLUPS_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
DEPLOY_TX=$(echo "$ROLLUPS_OUTPUT" | grep "Transaction hash:" | awk '{print $3}')
[ -n "$ROLLUPS_ADDRESS" ] && [ -n "$DEPLOY_TX" ] || { echo "ERROR: Failed to deploy Rollups"; exit 1; }
echo "Rollups deployed at: ${ROLLUPS_ADDRESS}"

# ── Compute deterministic L2 contract addresses ───────────────────────

L2_CONTEXT_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 0 | awk '{print $NF}')
CROSS_CHAIN_MANAGER_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 1 | awk '{print $NF}')
BRIDGE_L2_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 2 | awk '{print $NF}')
echo "L2Context address (CREATE nonce=0):  ${L2_CONTEXT_ADDRESS}"
echo "CCM address (CREATE nonce=1):        ${CROSS_CHAIN_MANAGER_ADDRESS}"
echo "Bridge L2 address (CREATE nonce=2):  ${BRIDGE_L2_ADDRESS}"

# ── Inject genesis pre-mints (CCM + builder) ─────────────────────────
# CCM receives 1M ETH (0xD3C21BCECCEDA1000000 wei) so it can source funds for
# deposits without runtime minting. The builder receives a large balance so it
# can pay gas for the block-1 protocol txs that deploy L2Context, CCM, Bridge,
# and Bridge.initialize (each is a CREATE/call from the builder at nonces 0..3).
# Both injections MUST happen before createRollup() because the state root
# committed on-chain must match the genesis block state root that includes them.
# Mirrors deployments/shared/scripts/deploy.sh (testnet-eez) — the gnosis-100
# deploy-gnosis.sh only injects CCM, which is why Chiado needs its own script.

SHARED_DIR="${SHARED_DIR:-/tmp/deploy-chiado-$$}"
mkdir -p "$SHARED_DIR"
CCM_ADDR_LOWER=$(echo "${CROSS_CHAIN_MANAGER_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')
BUILDER_ADDR_LOWER=$(echo "${BUILDER_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')
echo ""
echo "Injecting CCM pre-mint balance into genesis.json for address ${CCM_ADDR_LOWER}..."
cp "$GENESIS_JSON" "${SHARED_DIR}/genesis.json"
sed -i "/\"alloc\": {/a\\    \"${CCM_ADDR_LOWER}\": { \"balance\": \"0xD3C21BCECCEDA1000000\" }," "${SHARED_DIR}/genesis.json"
echo "Injecting builder pre-mint balance into genesis.json for address ${BUILDER_ADDR_LOWER}..."
sed -i "/\"alloc\": {/a\\    \"${BUILDER_ADDR_LOWER}\": { \"balance\": \"0x200000000000000000000000000000000000000000000000000000000000000\" }," "${SHARED_DIR}/genesis.json"
GENESIS_JSON_MODIFIED="${SHARED_DIR}/genesis.json"
grep -q "$CCM_ADDR_LOWER" "$GENESIS_JSON_MODIFIED" && echo "  CCM address found in genesis" || { echo "  FATAL: CCM address not in genesis"; exit 1; }
grep -q "0xD3C21BCECCEDA1000000" "$GENESIS_JSON_MODIFIED" && echo "  CCM pre-mint balance found" || { echo "  FATAL: CCM pre-mint balance not in genesis"; exit 1; }
grep -q "$BUILDER_ADDR_LOWER" "$GENESIS_JSON_MODIFIED" && echo "  builder address found in genesis" || { echo "  FATAL: builder address not in genesis"; exit 1; }

# ── Compute genesis state root ────────────────────────────────────────

echo ""
echo "Computing genesis state root (includes CCM + builder pre-mints)..."
ROLLUP_BIN="${ROLLUP_BIN:-}"
if [ -n "$ROLLUP_BIN" ] && [ -x "$ROLLUP_BIN" ]; then
    : # User-specified binary
elif command -v based-rollup &>/dev/null; then
    ROLLUP_BIN="based-rollup"
elif [ -x "${SCRIPT_DIR}/../../target/release/based-rollup" ]; then
    ROLLUP_BIN="${SCRIPT_DIR}/../../target/release/based-rollup"
else
    echo "ERROR: based-rollup binary not found on PATH or at target/release/based-rollup"
    echo "Build with: cargo build --release"
    exit 1
fi
GENESIS_STATE_ROOT=$("$ROLLUP_BIN" genesis-state-root --chain "$GENESIS_JSON_MODIFIED")
echo "Genesis state root: ${GENESIS_STATE_ROOT}"

# ── Register rollup (rollup_id = 1) ──────────────────────────────────

echo "Registering rollup (createRollup)..."
REGISTER_OUTPUT=$(cast send --rpc-url "$CHIADO_RPC" --private-key "$DEPLOYER_KEY" \
    "$ROLLUPS_ADDRESS" \
    "createRollup(bytes32,bytes32,address)(uint256)" \
    "$GENESIS_STATE_ROOT" \
    "0x0000000000000000000000000000000000000000000000000000000000000001" \
    "$DEPLOYER_ADDR" 2>&1)
echo "createRollup result: ${REGISTER_OUTPUT}"
ROLLUP_ID=$(cast call --rpc-url "$CHIADO_RPC" "$ROLLUPS_ADDRESS" "rollupCounter()(uint256)" 2>&1)
echo "Rollup counter: ${ROLLUP_ID}"

# ── Get deployment metadata ───────────────────────────────────────────

echo ""
echo "Waiting for deployment receipt..."
RECEIPT_WAIT=0
RECEIPT_MAX=120
DEPLOY_BLOCK=""
while [ $RECEIPT_WAIT -lt $RECEIPT_MAX ]; do
    if DEPLOY_BLOCK=$(cast receipt --rpc-url "$CHIADO_RPC" "$DEPLOY_TX" blockNumber 2>/dev/null); then
        break
    fi
    sleep 2
    RECEIPT_WAIT=$((RECEIPT_WAIT + 2))
done
[ -n "$DEPLOY_BLOCK" ] || { echo "ERROR: Timed out waiting for deployment receipt after ${RECEIPT_MAX}s"; exit 1; }
DEPLOY_TIMESTAMP=$(cast block --rpc-url "$CHIADO_RPC" "$DEPLOY_BLOCK" --field timestamp)

# ── Extract L2 contract bytecodes from build artifacts ────────────────
# See CLAUDE.md: "ALL bytecodes MUST come from contracts/sync-rollups-protocol/out/"

echo ""
echo "Extracting L2 contract bytecodes from build artifacts..."
cd "$CONTRACTS_DIR"
forge build --skip test --skip script --skip "visualizat*"

L2_CONTEXT_BYTECODE=$(_bc "$CONTRACTS_DIR/out/L2Context.sol/L2Context.json")
CCM_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/CrossChainManagerL2.sol/CrossChainManagerL2.json")
BRIDGE_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/Bridge.sol/Bridge.json")

for name in L2_CONTEXT_BYTECODE CCM_BYTECODE BRIDGE_BYTECODE; do
    val="${!name}"
    if [ "$val" = "null" ] || [ -z "$val" ]; then
        echo "ERROR: Failed to extract $name"; exit 1
    fi
done
echo "L2Context bytecode length: ${#L2_CONTEXT_BYTECODE}"
echo "CCM bytecode length:       ${#CCM_BYTECODE}"
echo "Bridge bytecode length:    ${#BRIDGE_BYTECODE}"

# ── Deploy Bridge contract on L1 ──────────────────────────────────────
# Bridge L1 must use the SAME bytecode as L2 so the embedded WrappedToken
# CREATE2 initcode is identical on both sides.

echo ""
echo "Deploying Bridge contract on L1..."
BRIDGE_DEPLOY_OUTPUT=$(cast send --rpc-url "$CHIADO_RPC" --private-key "$DEPLOYER_KEY" \
    --create "$BRIDGE_BYTECODE" 2>&1)
BRIDGE_L1_ADDRESS=$(echo "$BRIDGE_DEPLOY_OUTPUT" | grep "contractAddress" | awk '{print $NF}')
if [ -z "$BRIDGE_L1_ADDRESS" ] || [ "$BRIDGE_L1_ADDRESS" = "null" ]; then
    echo "ERROR: Bridge L1 deployment failed"
    echo "Deploy output: $(echo "$BRIDGE_DEPLOY_OUTPUT" | head -5)"
    exit 1
fi
echo "Bridge L1 deployed at: ${BRIDGE_L1_ADDRESS}"

cast send --rpc-url "$CHIADO_RPC" --private-key "$DEPLOYER_KEY" \
    "$BRIDGE_L1_ADDRESS" \
    "initialize(address,uint256,address)" \
    "$ROLLUPS_ADDRESS" 0 "$DEPLOYER_ADDR" > /dev/null 2>&1
echo "Bridge L1 initialized (manager=${ROLLUPS_ADDRESS}, rollupId=0, admin=${DEPLOYER_ADDR})"

echo "Setting canonicalBridgeAddress on L1 Bridge -> ${BRIDGE_L2_ADDRESS}..."
cast send --rpc-url "$CHIADO_RPC" --private-key "$DEPLOYER_KEY" \
    "$BRIDGE_L1_ADDRESS" \
    "setCanonicalBridgeAddress(address)" \
    "$BRIDGE_L2_ADDRESS" > /dev/null 2>&1
echo "L1 Bridge canonicalBridgeAddress set to ${BRIDGE_L2_ADDRESS}"

# ── Write config ──────────────────────────────────────────────────────

echo ""
echo "=== Chiado Deployment Summary ==="
echo "Chain ID:             ${CHAIN_ID}"
echo "tmpECDSAVerifier:     ${VERIFIER_ADDRESS}"
echo "Rollups:              ${ROLLUPS_ADDRESS}"
echo "Rollup ID:            1"
echo "Builder address:      ${BUILDER_ADDRESS}"
echo "Deployment L1 block:  ${DEPLOY_BLOCK}"
echo "Deployment timestamp: ${DEPLOY_TIMESTAMP}"
echo "L2Context address:    ${L2_CONTEXT_ADDRESS}"
echo "CCM address:          ${CROSS_CHAIN_MANAGER_ADDRESS}"
echo "Bridge L1 address:    ${BRIDGE_L1_ADDRESS}"
echo "Bridge L2 address:    ${BRIDGE_L2_ADDRESS}"
echo ""

cat > "${OUTPUT_FILE}.tmp" <<EOF
# Chiado testnet L1 configuration — generated by deployments/chiado-10200/deploy.sh
# Date: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
# Chain ID: ${CHAIN_ID}
L1_RPC_URL=${CHIADO_RPC}
ROLLUPS_ADDRESS=${ROLLUPS_ADDRESS}
ROLLUP_ID=1
VERIFIER_ADDRESS=${VERIFIER_ADDRESS}
BUILDER_ADDRESS=${BUILDER_ADDRESS}
L2_CONTEXT_ADDRESS=${L2_CONTEXT_ADDRESS}
CROSS_CHAIN_MANAGER_ADDRESS=${CROSS_CHAIN_MANAGER_ADDRESS}
BRIDGE_L1_ADDRESS=${BRIDGE_L1_ADDRESS}
BRIDGE_L2_ADDRESS=${BRIDGE_L2_ADDRESS}
DEPLOYMENT_L1_BLOCK=${DEPLOY_BLOCK}
DEPLOYMENT_TIMESTAMP=${DEPLOY_TIMESTAMP}
BOOTSTRAP_ACCOUNTS=${BOOTSTRAP_ACCOUNTS:-}
# Per-node settings — set in docker-compose or environment, NOT here:
#   BUILDER_MODE=true|false
#   BUILDER_PRIVATE_KEY=0x...
#   BLOCK_TIME=5
#   BUILDER_WS_URL=ws://builder-host:8546
# Bytecodes MUST be last — they are 20KB+ lines that can cause bash read
# to drop subsequent variables if placed before config vars.
L2_CONTEXT_BYTECODE=${L2_CONTEXT_BYTECODE}
CCM_BYTECODE=${CCM_BYTECODE}
BRIDGE_BYTECODE=${BRIDGE_BYTECODE}
EOF
mv "${OUTPUT_FILE}.tmp" "$OUTPUT_FILE"
echo "Rollup config written to ${OUTPUT_FILE}"

# Persist the modified genesis so the builder/fullnode containers load the
# exact genesis whose state root was registered on L1 via createRollup().
cp "$GENESIS_JSON_MODIFIED" "$OUTPUT_GENESIS"
echo "Modified genesis written to ${OUTPUT_GENESIS}"

rm -rf "${SHARED_DIR}"

echo ""
echo "=== Next Steps ==="
echo "  1. cp deployments/chiado-10200/.env.example deployments/chiado-10200/.env"
echo "  2. Set BUILDER_PRIVATE_KEY (and GNOSIS_RPC_URL if different from deploy-time RPC) in .env"
echo "  3. Fund the builder account (${BUILDER_ADDRESS}) with Chiado xDAI: https://faucet.chiadochain.net/"
echo "  4. Start with:"
echo "       docker compose -f deployments/chiado-10200/docker-compose.yml \\"
echo "           --env-file deployments/chiado-10200/.env up -d"
echo "  5. Monitor: curl http://localhost:9560/health"
echo ""
echo "IMPORTANT:"
echo "  - Chiado explorer: https://blockscout.chiadochain.net/address/${ROLLUPS_ADDRESS}"
echo "  - debug_traceCallMany is required for flash loans / multi-call cross-chain; basic block"
echo "    production and ETH bridging work with any standard Chiado RPC."
echo "  - Each postBatch costs ~0.001-0.01 xDAI — monitor the builder balance."
