#!/usr/bin/env bash
# Deploy L2 contracts: verify canonical bridge address.
# Runs AFTER the builder is healthy (depends on builder in docker-compose).
# Reads config from /shared/rollup.env (written by deploy.sh / deploy_l1).
#
# Flash loan deployment is handled separately by deploy-flash-loan.sh.
#
# WARNING: Uses well-known anvil dev keys — LOCAL DEVELOPMENT ONLY.
set -euo pipefail

L2_RPC="${L2_RPC_URL:-http://builder:8545}"
DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
SHARED_DIR="${SHARED_DIR:-/shared}"
CONTRACTS_DIR="${CONTRACTS_DIR:-/app/contracts}"

# Idempotency — skip if marker exists
if [ -f "${SHARED_DIR}/l2-deploy.done" ]; then
    echo "L2 deployment already done — skipping."
    exit 0
fi

# Load config from L1 deployment
if [ ! -f "${SHARED_DIR}/rollup.env" ]; then
    echo "ERROR: ${SHARED_DIR}/rollup.env not found. L1 deploy must run first."
    exit 1
fi
echo "Loading config from rollup.env..."
while IFS='=' read -r key value; do
    case "$key" in
        BRIDGE_ADDRESS|BRIDGE_L1_ADDRESS|BRIDGE_L2_ADDRESS|ROLLUPS_ADDRESS)
            export "$key=$value"
            ;;
    esac
done < "${SHARED_DIR}/rollup.env"

: "${BRIDGE_L2_ADDRESS:?BRIDGE_L2_ADDRESS not set}"
: "${BRIDGE_ADDRESS:?BRIDGE_ADDRESS not set}"
ZERO="0x0000000000000000000000000000000000000000"

echo "=========================================="
echo "  L2 Deployment"
echo "=========================================="
echo "L2 RPC:     $L2_RPC"
echo "Bridge L2:  $BRIDGE_L2_ADDRESS"
echo "Bridge L1:  $BRIDGE_ADDRESS"

# Wait for L2 to be ready
echo ""
echo "Waiting for L2 builder..."
WAIT=0
MAX_WAIT=300
until cast block-number --rpc-url "$L2_RPC" >/dev/null 2>&1; do
    WAIT=$((WAIT + 1))
    if [ "$WAIT" -ge "$MAX_WAIT" ]; then
        echo "ERROR: L2 not ready after ${MAX_WAIT}s"
        exit 1
    fi
    sleep 1
done
echo "L2 is ready (block $(cast block-number --rpc-url "$L2_RPC"))."

# 1. Verify canonicalBridgeAddress on L2 Bridge
# This is set as a block 2 protocol tx by the builder (driver.rs).
# We just verify it here rather than trying to set it again.
if [ "$BRIDGE_ADDRESS" != "$ZERO" ]; then
    CURRENT_CANONICAL=$(cast call --rpc-url "$L2_RPC" "$BRIDGE_L2_ADDRESS" \
        "canonicalBridgeAddress()(address)" 2>&1 || echo "$ZERO")
    EXPECTED_LOWER=$(echo "$BRIDGE_ADDRESS" | tr '[:upper:]' '[:lower:]')
    ACTUAL_LOWER=$(echo "$CURRENT_CANONICAL" | tr '[:upper:]' '[:lower:]')
    if [ "$ACTUAL_LOWER" = "$EXPECTED_LOWER" ]; then
        echo "  canonicalBridgeAddress already set to ${BRIDGE_ADDRESS} (block 2 protocol tx)"
    else
        echo "  canonicalBridgeAddress is ${CURRENT_CANONICAL}, expected ${BRIDGE_ADDRESS}"
        echo "  Attempting to set..."
        if cast send --rpc-url "$L2_RPC" --private-key "$DEPLOYER_KEY" \
            "$BRIDGE_L2_ADDRESS" \
            "setCanonicalBridgeAddress(address)" \
            "$BRIDGE_ADDRESS" --gas-price 2000000000 > /dev/null 2>&1; then
            echo "  Set successfully"
        else
            echo "  WARNING: Failed to set canonicalBridgeAddress"
        fi
    fi
fi

# Mark as done
touch "${SHARED_DIR}/l2-deploy.done"
echo ""
echo "L2 deployment complete."
