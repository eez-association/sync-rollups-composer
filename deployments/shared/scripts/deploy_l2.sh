#!/usr/bin/env bash
# Deploy L2 contracts: canonical bridge address + flash loan contracts.
# Runs AFTER the builder is healthy (depends on builder in docker-compose).
# Reads config from /shared/rollup.env (written by deploy.sh / deploy_l1).
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
        BRIDGE_ADDRESS|BRIDGE_L1_ADDRESS|BRIDGE_L2_ADDRESS|DEPLOYER_KEY|\
        FLASH_TOKEN_ADDRESS|FLASH_EXECUTOR_L2_ADDRESS|FLASH_NFT_ADDRESS|\
        WRAPPED_TOKEN_L2|ROLLUPS_ADDRESS)
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
        echo "  ✓ canonicalBridgeAddress already set to ${BRIDGE_ADDRESS} (block 2 protocol tx)"
    else
        echo "  canonicalBridgeAddress is ${CURRENT_CANONICAL}, expected ${BRIDGE_ADDRESS}"
        echo "  Attempting to set..."
        if cast send --rpc-url "$L2_RPC" --private-key "$DEPLOYER_KEY" \
            "$BRIDGE_L2_ADDRESS" \
            "setCanonicalBridgeAddress(address)" \
            "$BRIDGE_ADDRESS" --gas-price 2000000000 > /dev/null 2>&1; then
            echo "  ✓ Set successfully"
        else
            echo "  ✗ WARNING: Failed to set canonicalBridgeAddress"
        fi
    fi
fi

# 2. Deploy flash loan contracts on L2 (dev#5, nonces 0 and 1)
if [ "${DEPLOY_FLASH_LOAN:-false}" = "true" ] && \
   [ "${FLASH_EXECUTOR_L2_ADDRESS:-$ZERO}" != "$ZERO" ]; then
    echo ""
    echo "=== Deploying Flash Loan Contracts on L2 ==="
    L2_DEPLOY_KEY="0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba"
    DEV5_ADDR="0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
    cd "$CONTRACTS_DIR/sync-rollups-protocol"

    DEV5_NONCE=$(cast nonce --rpc-url "$L2_RPC" "$DEV5_ADDR" 2>&1)
    echo "dev#5 L2 nonce: $DEV5_NONCE"
    if [ "$DEV5_NONCE" != "0" ]; then
        echo "WARNING: dev#5 nonce is $DEV5_NONCE (expected 0) — addresses will differ from pre-computed!"
    fi

    # Deploy FlashLoanBridgeExecutor on L2 at nonce 0 (placeholder args — all zeros)
    echo "Deploying FlashLoanBridgeExecutor on L2 (placeholder, nonce=0)..."
    EXECUTOR_L2_ACTUAL_OUTPUT=$(forge create \
        --rpc-url "$L2_RPC" \
        --private-key "$L2_DEPLOY_KEY" \
        --broadcast \
        --gas-price 2000000000 \
        src/periphery/defiMock/FlashLoanBridgeExecutor.sol:FlashLoanBridgeExecutor \
        --constructor-args "$ZERO" "$ZERO" "$ZERO" "$ZERO" "$ZERO" "$ZERO" "$ZERO" 0 "$ZERO" 2>&1)
    echo "$EXECUTOR_L2_ACTUAL_OUTPUT" | tail -3
    EXECUTOR_L2_ACTUAL=$(echo "$EXECUTOR_L2_ACTUAL_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
    if [ -n "$EXECUTOR_L2_ACTUAL" ]; then
        echo "  ✓ ExecutorL2 deployed at: $EXECUTOR_L2_ACTUAL"
        [ "$EXECUTOR_L2_ACTUAL" != "$FLASH_EXECUTOR_L2_ADDRESS" ] && \
            echo "  WARNING: differs from pre-computed ($FLASH_EXECUTOR_L2_ADDRESS)"
    else
        echo "  ✗ FlashLoanBridgeExecutor L2 deployment failed"
    fi

    # Deploy FlashLoanersNFT on L2 at nonce 1 (with correct WrappedToken address)
    # WRAPPED_TOKEN_L2 was computed by deploy.sh and stored in rollup.env
    WRAPPED="${WRAPPED_TOKEN_L2:-$ZERO}"
    echo "Deploying FlashLoanersNFT on L2 (nonce=1, wrappedToken=$WRAPPED)..."
    FLASH_NFT_ACTUAL_OUTPUT=$(forge create \
        --rpc-url "$L2_RPC" \
        --private-key "$L2_DEPLOY_KEY" \
        --broadcast \
        --gas-price 2000000000 \
        src/periphery/defiMock/FlashLoanersNFT.sol:FlashLoanersNFT \
        --constructor-args "$WRAPPED" 2>&1)
    echo "$FLASH_NFT_ACTUAL_OUTPUT" | tail -3
    FLASH_NFT_ACTUAL=$(echo "$FLASH_NFT_ACTUAL_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
    if [ -n "$FLASH_NFT_ACTUAL" ]; then
        echo "  ✓ FlashLoanersNFT deployed at: $FLASH_NFT_ACTUAL"
        [ "$FLASH_NFT_ACTUAL" != "${FLASH_NFT_ADDRESS:-}" ] && \
            echo "  WARNING: differs from pre-computed (${FLASH_NFT_ADDRESS:-unknown})"
    else
        echo "  ✗ FlashLoanersNFT L2 deployment failed"
    fi

    echo ""
    echo "=== Flash Loan L2 Deployment Complete ==="
    echo "ExecutorL2: ${EXECUTOR_L2_ACTUAL:-FAILED}"
    echo "FlashNFT:   ${FLASH_NFT_ACTUAL:-FAILED}"
fi

# Mark as done
touch "${SHARED_DIR}/l2-deploy.done"
echo ""
echo "L2 deployment complete."
