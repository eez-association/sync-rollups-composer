#!/usr/bin/env bash
# Deploy Rollups + tmpECDSAVerifier + Bridge contracts to Gnosis Chain / Chiado testnet
# and generate rollup-gnosis.env.
#
# Usage:
#   deploy-gnosis.sh <GNOSIS_RPC_URL> <DEPLOYER_PRIVATE_KEY> [BUILDER_PRIVATE_KEY]
#
# Example (Chiado testnet):
#   deploy-gnosis.sh https://rpc.chiadochain.net 0xDEPLOYER_KEY 0xBUILDER_KEY
#
# Example (Gnosis mainnet):
#   deploy-gnosis.sh https://rpc.gnosischain.com 0xDEPLOYER_KEY 0xBUILDER_KEY
#
# If BUILDER_PRIVATE_KEY is omitted, the deployer key is used as the builder key.
# This is acceptable for single-operator setups but NOT recommended for production.
#
# Prerequisites:
#   - forge + cast (Foundry) installed
#   - Deployer account funded with at least 0.5 xDAI on Gnosis Chain / Chiado
#   - Builder account funded with xDAI for ongoing postBatch submissions
#   - contracts/sync-rollups-protocol submodule initialized (git submodule update --init)
#
# Environment overrides:
#   CONTRACTS_DIR    — path to contracts/ directory (default: ../contracts relative to script)
#   GENESIS_JSON     — path to L2 genesis.json (default: ../shared/genesis.json relative to script)
#   OUTPUT_FILE      — output config file path (default: rollup-gnosis.env)
#   SHARED_DIR       — shared dir for genesis injection (default: /tmp/deploy-gnosis-$$)
#   BOOTSTRAP_ACCOUNTS — comma-separated addr:eth pairs for block 1 funding (default: empty)
set -euo pipefail

GNOSIS_RPC="${1:?Usage: deploy-gnosis.sh <GNOSIS_RPC_URL> <DEPLOYER_PRIVATE_KEY> [BUILDER_PRIVATE_KEY]}"
DEPLOYER_KEY="${2:?Usage: deploy-gnosis.sh <GNOSIS_RPC_URL> <DEPLOYER_PRIVATE_KEY> [BUILDER_PRIVATE_KEY]}"
BUILDER_KEY="${3:-$DEPLOYER_KEY}"
OUTPUT_FILE="${OUTPUT_FILE:-rollup-gnosis.env}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTRACTS_DIR="${CONTRACTS_DIR:-${SCRIPT_DIR}/../../contracts}"
GENESIS_JSON="${GENESIS_JSON:-${SCRIPT_DIR}/../shared/genesis.json}"

# ── Validate chain connection ──────────────────────────────────────────

echo "Checking chain connection at ${GNOSIS_RPC}..."
CHAIN_ID=$(cast chain-id --rpc-url "$GNOSIS_RPC" 2>/dev/null || echo "")
if [ -z "$CHAIN_ID" ]; then
    echo "ERROR: Cannot connect to RPC at ${GNOSIS_RPC}"
    exit 1
fi

# Accept both Gnosis mainnet (100) and Chiado testnet (10200)
if [ "$CHAIN_ID" != "100" ] && [ "$CHAIN_ID" != "10200" ]; then
    echo "ERROR: Chain ID is ${CHAIN_ID}, expected 100 (Gnosis Chain) or 10200 (Chiado testnet)"
    echo "If this is intentional, set SKIP_CHAIN_CHECK=1"
    if [ "${SKIP_CHAIN_CHECK:-}" != "1" ]; then
        exit 1
    fi
fi

if [ "$CHAIN_ID" = "10200" ]; then
    echo "Connected to Chiado testnet (chain ID 10200)"
elif [ "$CHAIN_ID" = "100" ]; then
    echo "Connected to Gnosis Chain mainnet (chain ID 100)"
else
    echo "Connected to chain ID ${CHAIN_ID} (SKIP_CHAIN_CHECK=1)"
fi

# ── Derive builder and deployer addresses ─────────────────────────────

BUILDER_ADDRESS=$(cast wallet address --private-key "$BUILDER_KEY")
DEPLOYER_ADDR=$(cast wallet address --private-key "$DEPLOYER_KEY")
echo "Deployer address: ${DEPLOYER_ADDR}"
echo "Builder address:  ${BUILDER_ADDRESS}"

# Check deployer balance
DEPLOYER_BALANCE=$(cast balance --rpc-url "$GNOSIS_RPC" "$DEPLOYER_ADDR" --ether 2>/dev/null || echo "0")
echo "Deployer balance: ${DEPLOYER_BALANCE} xDAI"
# Rough check: need at least 0.1 xDAI for deployment gas
if [ "$(echo "$DEPLOYER_BALANCE < 0.1" | bc -l 2>/dev/null || echo "0")" = "1" ]; then
    echo "WARNING: Deployer balance (${DEPLOYER_BALANCE} xDAI) may be too low for deployment"
fi

# ── Idempotency check ─────────────────────────────────────────────────

if [ -f "$OUTPUT_FILE" ]; then
    echo "WARNING: ${OUTPUT_FILE} already exists."
    echo "Delete it manually to force a fresh deployment."
    cat "$OUTPUT_FILE"
    exit 0
fi

# ── Build contracts ───────────────────────────────────────────────────

echo ""
echo "Building sync-rollups-protocol contracts..."
cd "$CONTRACTS_DIR/sync-rollups-protocol"
forge build --skip test

# ── Helper: extract bytecode.object from forge JSON artifacts ─────────
# Uses grep+sed (no python3 in deploy container — only bash/sed/grep/cast/forge).
# Read from out/ artifacts rather than forge inspect to guarantee bytecode consistency.
# See CLAUDE.md: "forge inspect != forge build output" and "ALL bytecodes MUST come from out/"

_bc() { (grep -o '"object":"0x[0-9a-fA-F]*"' "$1" || true) | head -1 | sed 's/"object":"//;s/"//'; }

# ── Deploy tmpECDSAVerifier ───────────────────────────────────────────
# tmpECDSAVerifier requires (owner, signer) constructor args.
# For Chiado/Gnosis deployment: deployer is owner, builder is signer (ecrecover verifies builder key).
# This replaces the former MockZKVerifier which had no verification at all.

echo ""
echo "Deploying tmpECDSAVerifier (owner=${DEPLOYER_ADDR}, signer=${BUILDER_ADDRESS})..."
VERIFIER_OUTPUT=$(forge create \
    --rpc-url "$GNOSIS_RPC" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    src/verifier/tmpECDSAVerifier.sol:tmpECDSAVerifier \
    --constructor-args "$DEPLOYER_ADDR" "$BUILDER_ADDRESS" 2>&1)
echo "$VERIFIER_OUTPUT"

VERIFIER_ADDRESS=$(echo "$VERIFIER_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
if [ -z "$VERIFIER_ADDRESS" ]; then
    echo "ERROR: Failed to deploy tmpECDSAVerifier"
    exit 1
fi
echo "tmpECDSAVerifier deployed at: ${VERIFIER_ADDRESS}"

# ── Deploy Rollups contract ───────────────────────────────────────────

echo ""
echo "Deploying Rollups contract..."
ROLLUPS_OUTPUT=$(forge create \
    --rpc-url "$GNOSIS_RPC" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    src/Rollups.sol:Rollups \
    --constructor-args "$VERIFIER_ADDRESS" 1 2>&1)
echo "$ROLLUPS_OUTPUT"

ROLLUPS_ADDRESS=$(echo "$ROLLUPS_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
DEPLOY_TX=$(echo "$ROLLUPS_OUTPUT" | grep "Transaction hash:" | awk '{print $3}')
if [ -z "$ROLLUPS_ADDRESS" ] || [ -z "$DEPLOY_TX" ]; then
    echo "ERROR: Failed to deploy Rollups"
    exit 1
fi
echo "Rollups deployed at: ${ROLLUPS_ADDRESS}"

# ── Compute deterministic L2 contract addresses ───────────────────────
# L2Context = CREATE(builder, nonce=0), CCM = CREATE(builder, nonce=1),
# Bridge L2 = CREATE(builder, nonce=2)

L2_CONTEXT_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 0 | awk '{print $NF}')
CROSS_CHAIN_MANAGER_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 1 | awk '{print $NF}')
BRIDGE_L2_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 2 | awk '{print $NF}')
echo "L2Context address (CREATE nonce=0):  ${L2_CONTEXT_ADDRESS}"
echo "CCM address (CREATE nonce=1):        ${CROSS_CHAIN_MANAGER_ADDRESS}"
echo "Bridge L2 address (CREATE nonce=2):  ${BRIDGE_L2_ADDRESS}"

# ── Inject CCM genesis pre-mint into genesis.json ─────────────────────
# CCM receives 1M ETH (0xD3C21BCECCEDA1000000 wei) in genesis.
# This MUST happen before createRollup() because the state root committed on-chain
# must match the genesis block state root which includes this allocation.
# See CLAUDE.md: "CCM gets 1M ETH in genesis.json alloc. No runtime minting needed."

SHARED_DIR="${SHARED_DIR:-/tmp/deploy-gnosis-$$}"
mkdir -p "$SHARED_DIR"
CCM_ADDR_LOWER=$(echo "${CROSS_CHAIN_MANAGER_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')
echo ""
echo "Injecting CCM pre-mint balance into genesis.json for address ${CCM_ADDR_LOWER}..."
cp "$GENESIS_JSON" "${SHARED_DIR}/genesis.json"
sed -i "/\"alloc\": {/a\\    \"${CCM_ADDR_LOWER}\": { \"balance\": \"0xD3C21BCECCEDA1000000\" }," "${SHARED_DIR}/genesis.json"
GENESIS_JSON_MODIFIED="${SHARED_DIR}/genesis.json"
# Verify injection succeeded
grep -q "$CCM_ADDR_LOWER" "$GENESIS_JSON_MODIFIED" && echo "  CCM address found in genesis" || { echo "  FATAL: CCM address not in genesis"; exit 1; }
grep -q "0xD3C21BCECCEDA1000000" "$GENESIS_JSON_MODIFIED" && echo "  Pre-mint balance found" || { echo "  FATAL: Pre-mint balance not in genesis"; exit 1; }

# ── Compute genesis state root ────────────────────────────────────────

echo ""
echo "Computing genesis state root (includes CCM pre-mint)..."
ROLLUP_BIN="${ROLLUP_BIN:-}"
if [ -n "$ROLLUP_BIN" ] && [ -x "$ROLLUP_BIN" ]; then
    : # User-specified binary
elif command -v based-rollup &>/dev/null; then
    ROLLUP_BIN="based-rollup"
elif [ -x "${SCRIPT_DIR}/../target/release/based-rollup" ]; then
    ROLLUP_BIN="${SCRIPT_DIR}/../target/release/based-rollup"
else
    echo "ERROR: based-rollup binary not found on PATH or at target/release/based-rollup"
    echo "Build with: cargo build --release"
    exit 1
fi
GENESIS_STATE_ROOT=$("$ROLLUP_BIN" genesis-state-root --chain "$GENESIS_JSON_MODIFIED")
echo "Genesis state root: ${GENESIS_STATE_ROOT}"

# ── Register rollup (rollup_id = 1) ──────────────────────────────────

echo "Registering rollup (createRollup)..."
REGISTER_OUTPUT=$(cast send --rpc-url "$GNOSIS_RPC" --private-key "$DEPLOYER_KEY" \
    "$ROLLUPS_ADDRESS" \
    "createRollup(bytes32,bytes32,address)(uint256)" \
    "$GENESIS_STATE_ROOT" \
    "0x0000000000000000000000000000000000000000000000000000000000000001" \
    "$DEPLOYER_ADDR" 2>&1)
echo "createRollup result: ${REGISTER_OUTPUT}"

ROLLUP_ID=$(cast call --rpc-url "$GNOSIS_RPC" "$ROLLUPS_ADDRESS" "rollupCounter()(uint256)" 2>&1)
echo "Rollup counter: ${ROLLUP_ID}"

# ── Get deployment metadata ───────────────────────────────────────────

echo ""
echo "Waiting for deployment receipt..."
RECEIPT_WAIT=0
RECEIPT_MAX=120
DEPLOY_BLOCK=""
while [ $RECEIPT_WAIT -lt $RECEIPT_MAX ]; do
    if DEPLOY_BLOCK=$(cast receipt --rpc-url "$GNOSIS_RPC" "$DEPLOY_TX" blockNumber 2>/dev/null); then
        break
    fi
    sleep 2
    RECEIPT_WAIT=$((RECEIPT_WAIT + 2))
done
if [ -z "$DEPLOY_BLOCK" ]; then
    echo "ERROR: Timed out waiting for deployment receipt after ${RECEIPT_MAX}s"
    exit 1
fi
DEPLOY_TIMESTAMP=$(cast block --rpc-url "$GNOSIS_RPC" "$DEPLOY_BLOCK" --field timestamp)

# ── Extract L2 contract bytecodes from build artifacts ────────────────
# CRITICAL: Read from out/ JSON artifacts, NOT forge inspect.
# forge inspect can produce different bytecodes than forge build artifacts.
# CREATE2 address determinism depends on identical bytecodes across L1 and L2.
# See CLAUDE.md: "ALL bytecodes MUST come from contracts/sync-rollups-protocol/out/"

echo ""
echo "Extracting L2 contract bytecodes from build artifacts..."
cd "$CONTRACTS_DIR"
forge build --skip test --skip script --skip "visualizat*"

L2_CONTEXT_BYTECODE=$(_bc "$CONTRACTS_DIR/out/L2Context.sol/L2Context.json")
CCM_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/CrossChainManagerL2.sol/CrossChainManagerL2.json")
BRIDGE_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/Bridge.sol/Bridge.json")

if [ "$L2_CONTEXT_BYTECODE" = "null" ] || [ -z "$L2_CONTEXT_BYTECODE" ]; then
    echo "ERROR: Failed to extract L2Context bytecode"
    exit 1
fi
if [ "$CCM_BYTECODE" = "null" ] || [ -z "$CCM_BYTECODE" ]; then
    echo "ERROR: Failed to extract CCM bytecode"
    exit 1
fi
if [ "$BRIDGE_BYTECODE" = "null" ] || [ -z "$BRIDGE_BYTECODE" ]; then
    echo "ERROR: Failed to extract Bridge bytecode"
    exit 1
fi
echo "L2Context bytecode length: ${#L2_CONTEXT_BYTECODE}"
echo "CCM bytecode length: ${#CCM_BYTECODE}"
echo "Bridge bytecode length: ${#BRIDGE_BYTECODE}"

# ── Deploy Bridge contract on L1 ──────────────────────────────────────
# Bridge L1 is needed for ETH/token bridging (deposits and withdrawals).
# Deploy using the SAME bytecode that goes into rollup-gnosis.env so that
# the embedded WrappedToken CREATE2 initcode is identical on L1 and L2.

echo ""
echo "Deploying Bridge contract on L1..."
BRIDGE_BYTECODE_FILE="${SHARED_DIR}/bridge_bytecode.txt"
echo "$BRIDGE_BYTECODE" > "$BRIDGE_BYTECODE_FILE"

BRIDGE_DEPLOY_OUTPUT=$(cast send --rpc-url "$GNOSIS_RPC" --private-key "$DEPLOYER_KEY" \
    --create "$BRIDGE_BYTECODE" 2>&1)
BRIDGE_L1_ADDRESS=$(echo "$BRIDGE_DEPLOY_OUTPUT" | grep "contractAddress" | awk '{print $NF}')

if [ -n "$BRIDGE_L1_ADDRESS" ] && [ "$BRIDGE_L1_ADDRESS" != "null" ]; then
    echo "Bridge L1 deployed at: ${BRIDGE_L1_ADDRESS}"

    # Initialize: manager=Rollups, rollupId=0 (L1), admin=deployer
    cast send --rpc-url "$GNOSIS_RPC" --private-key "$DEPLOYER_KEY" \
        "$BRIDGE_L1_ADDRESS" \
        "initialize(address,uint256,address)" \
        "$ROLLUPS_ADDRESS" 0 "$DEPLOYER_ADDR" > /dev/null 2>&1
    echo "Bridge L1 initialized (manager=${ROLLUPS_ADDRESS}, rollupId=0, admin=${DEPLOYER_ADDR})"

    # Set canonical bridge address: L1 Bridge -> L2 Bridge address
    echo "Setting canonicalBridgeAddress on L1 Bridge -> ${BRIDGE_L2_ADDRESS}..."
    cast send --rpc-url "$GNOSIS_RPC" --private-key "$DEPLOYER_KEY" \
        "$BRIDGE_L1_ADDRESS" \
        "setCanonicalBridgeAddress(address)" \
        "$BRIDGE_L2_ADDRESS" > /dev/null 2>&1
    echo "L1 Bridge canonicalBridgeAddress set to ${BRIDGE_L2_ADDRESS}"
else
    echo "ERROR: Bridge L1 deployment failed"
    echo "Deploy output: $(echo "$BRIDGE_DEPLOY_OUTPUT" | head -5)"
    rm -f "$BRIDGE_BYTECODE_FILE"
    exit 1
fi

rm -f "$BRIDGE_BYTECODE_FILE"

# ── Write config ──────────────────────────────────────────────────────

echo ""
echo "=== Gnosis Chain Deployment Summary ==="
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
# Gnosis Chain / Chiado L1 configuration — generated by deploy-gnosis.sh
# Date: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
# Chain ID: ${CHAIN_ID}
L1_RPC_URL=${GNOSIS_RPC}
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

echo "Config written to ${OUTPUT_FILE}"
echo ""
echo "=== Next Steps ==="
echo "  1. Copy ${OUTPUT_FILE} to your deployment host as 'rollup-gnosis.env'"
echo "  2. Create a .env file with your secrets (see .env.gnosis.example)"
echo "  3. Start with: docker compose -f docker-compose.gnosis.yml up -d"
echo "  4. Monitor: curl http://localhost:9560/health"
echo ""
echo "IMPORTANT:"
echo "  - Builder account (${BUILDER_ADDRESS}) needs xDAI for postBatch gas"
if [ "$CHAIN_ID" = "10200" ]; then
    echo "  - Chiado faucet: https://faucet.chiadochain.net/"
    echo "  - Chiado explorer: https://blockscout.chiadochain.net/address/${ROLLUPS_ADDRESS}"
    echo "  - NOTE: debug_traceCallMany not supported on public Chiado RPCs — flash loans require archive node"
else
    echo "  - Verify contracts on Gnosisscan: https://gnosisscan.io/address/${ROLLUPS_ADDRESS}"
fi
echo "  - Monitor builder balance — each postBatch costs ~0.001-0.01 xDAI"

# Clean up temp dir (keep output file which is at OUTPUT_FILE path, not in SHARED_DIR)
rm -rf "${SHARED_DIR}"
