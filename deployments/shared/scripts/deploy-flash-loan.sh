#!/usr/bin/env bash
# Deploy flash loan contracts on L1 and L2.
# Standalone script — runs AFTER deploy.sh and deploy_l2.sh complete.
# Reads config from /shared/rollup.env (written by deploy.sh).
#
# L1 deployments use dev#0 (deployer key, same as deploy.sh).
# L2 deployments use dev#5 (same as deploy_l2.sh).
#
# WARNING: The private keys below are well-known anvil default keys.
# They are PUBLIC and MUST NEVER be used on mainnet, testnets, or any chain
# where real value is at stake. This script is for LOCAL DEVELOPMENT ONLY.
set -euo pipefail

L1_RPC="${L1_RPC:-http://l1:8545}"
L2_RPC="${L2_RPC:-http://builder:8545}"
SHARED_DIR="${SHARED_DIR:-/shared}"
CONTRACTS_DIR="${CONTRACTS_DIR:-/app/contracts}"

DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
DEPLOYER_ADDR=$(cast wallet address --private-key "$DEPLOYER_KEY")
L2_DEPLOY_KEY="0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba"
DEV5_ADDR="0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"

L2_ROLLUP_ID=1
ZERO="0x0000000000000000000000000000000000000000"

# Idempotency — skip if marker exists
if [ -f "${SHARED_DIR}/flash-loan-deploy.done" ]; then
    echo "Flash loan deployment already done -- skipping."
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
        ROLLUPS_ADDRESS|BRIDGE_ADDRESS|BRIDGE_L1_ADDRESS|BRIDGE_L2_ADDRESS|\
        ROLLUP_ID|DEPLOYER_KEY_UNUSED)
            export "$key=$value"
            ;;
    esac
done < "${SHARED_DIR}/rollup.env"

: "${ROLLUPS_ADDRESS:?ROLLUPS_ADDRESS not set in rollup.env}"
: "${BRIDGE_L2_ADDRESS:?BRIDGE_L2_ADDRESS not set in rollup.env}"
: "${BRIDGE_ADDRESS:?BRIDGE_ADDRESS not set in rollup.env}"

# Helper to extract bytecode.object from forge JSON artifacts (no python3 in container)
_bc() { (grep -o '"object":"0x[0-9a-fA-F]*"' "$1" || true) | head -1 | sed 's/"object":"//;s/"//'; }

echo "=========================================="
echo "  Flash Loan Deployment"
echo "=========================================="
echo "L1 RPC:     $L1_RPC"
echo "L2 RPC:     $L2_RPC"
echo "Rollups:    $ROLLUPS_ADDRESS"
echo "Bridge L1:  $BRIDGE_ADDRESS"
echo "Bridge L2:  $BRIDGE_L2_ADDRESS"

# --- Wait for L1 and L2 to be healthy ---
echo ""
echo "Waiting for L1 at ${L1_RPC}..."
WAIT_COUNT=0
MAX_WAIT=120
until cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ "$WAIT_COUNT" -ge "$MAX_WAIT" ]; then
        echo "ERROR: Timed out waiting for L1 after ${MAX_WAIT}s"
        exit 1
    fi
    sleep 1
done
echo "L1 is ready."

echo "Waiting for L2 at ${L2_RPC}..."
WAIT_COUNT=0
MAX_WAIT=300
until cast block-number --rpc-url "$L2_RPC" >/dev/null 2>&1; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ "$WAIT_COUNT" -ge "$MAX_WAIT" ]; then
        echo "ERROR: Timed out waiting for L2 after ${MAX_WAIT}s"
        exit 1
    fi
    sleep 1
done
echo "L2 is ready (block $(cast block-number --rpc-url "$L2_RPC"))."

# --- Build contracts ---
echo ""
echo "Building contracts..."
cd "$CONTRACTS_DIR/sync-rollups-protocol"
forge build --skip "fv/*"

# --- Extract flash loan bytecodes ---
echo "Extracting flash loan bytecodes from build artifacts..."
TOKEN_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/IntegrationTestFlashLoan.t.sol/TestToken.json")
POOL_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/FlashLoan.sol/FlashLoan.json")
EXECUTOR_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/FlashLoanBridgeExecutor.sol/FlashLoanBridgeExecutor.json")

if [ -z "$TOKEN_BYTECODE" ] || [ "$TOKEN_BYTECODE" = "null" ]; then
    echo "ERROR: TestToken bytecode not found"; exit 1
fi
if [ -z "$POOL_BYTECODE" ] || [ "$POOL_BYTECODE" = "null" ]; then
    echo "ERROR: FlashLoan bytecode not found"; exit 1
fi
if [ -z "$EXECUTOR_BYTECODE" ] || [ "$EXECUTOR_BYTECODE" = "null" ]; then
    echo "ERROR: FlashLoanBridgeExecutor bytecode not found"; exit 1
fi
echo "  TestToken bytecode length:              ${#TOKEN_BYTECODE}"
echo "  FlashLoan bytecode length:              ${#POOL_BYTECODE}"
echo "  FlashLoanBridgeExecutor bytecode length: ${#EXECUTOR_BYTECODE}"

# ==========================================================================
# L1 DEPLOYMENT
#
# Deployer nonce map (relative to current nonce DN):
#   DN+0: CREATE  TestToken
#   DN+1: CREATE  FlashLoan pool
#   DN+2: CALL    token.transfer(pool, 10000e18)
#   DN+3: CALL    token.transfer(dev5, 10000e18)
#   DN+4: CALL    Rollups.createCrossChainProxy(ExecutorL2, 1)
#   DN+5: CREATE  FlashLoanBridgeExecutor (needs EXECUTOR_L2_PROXY -- sent after phase 1)
# ==========================================================================

echo ""
echo "=== Phase 1: L1 Pre-computation ==="

# Pre-compute L2 flash loan contract addresses (dev#5 at nonce 0 and 1)
EXECUTOR_L2=$(cast compute-address "$DEV5_ADDR" --nonce 0 | awk '{print $NF}')
FLASH_NFT=$(cast compute-address "$DEV5_ADDR" --nonce 1 | awk '{print $NF}')

# Get deployer starting nonce on L1
DN=$(cast nonce --rpc-url "$L1_RPC" "$DEPLOYER_ADDR")
echo "Deployer L1 starting nonce: $DN"

# Pre-compute L1 flash loan contract addresses
FLASH_TOKEN_ADDRESS=$(cast compute-address "$DEPLOYER_ADDR" --nonce $((DN+0)) | awk '{print $NF}')
FLASH_POOL_ADDRESS=$(cast compute-address "$DEPLOYER_ADDR" --nonce $((DN+1)) | awk '{print $NF}')
# DN+2, DN+3, DN+4 are CALLs (not CREATEs)
FLASH_EXECUTOR_L1_ADDRESS=$(cast compute-address "$DEPLOYER_ADDR" --nonce $((DN+5)) | awk '{print $NF}')

echo "Pre-computed addresses:"
echo "  ExecutorL2 (dev#5 nonce=0):  $EXECUTOR_L2"
echo "  FlashNFT   (dev#5 nonce=1):  $FLASH_NFT"
echo "  TestToken  (DN+0):           $FLASH_TOKEN_ADDRESS"
echo "  FlashPool  (DN+1):           $FLASH_POOL_ADDRESS"
echo "  ExecutorL1 (DN+5):           $FLASH_EXECUTOR_L1_ADDRESS"

# Build calldata for token transfers
TRANSFER_POOL_CALLDATA=$(cast calldata "transfer(address,uint256)" \
    "$FLASH_POOL_ADDRESS" "10000000000000000000000")
TRANSFER_DEV5_CALLDATA=$(cast calldata "transfer(address,uint256)" \
    "$DEV5_ADDR" "10000000000000000000000")

# Build calldata for createCrossChainProxy
CREATE_PROXY_CALLDATA=$(cast calldata "createCrossChainProxy(address,uint256)" \
    "$EXECUTOR_L2" "$L2_ROLLUP_ID")

# Pre-compute WrappedToken CREATE2 address on L2
# Bridge_L2 deploys WrappedToken via CREATE2 with:
#   salt       = keccak256(abi.encodePacked(token, originRollupId))
#   initCode   = type(WrappedToken).creationCode ++ abi.encode(name, symbol, decimals, bridgeL2)
echo "Computing WrappedToken L2 CREATE2 address from artifacts..."
WT_CREATION_CODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/WrappedToken.sol/WrappedToken.json")
WT_CONSTRUCTOR_ARGS=$(cast abi-encode "f(string,string,uint8,address)" "Test Token" "TT" 18 "$BRIDGE_L2_ADDRESS")
WT_INIT_CODE="${WT_CREATION_CODE}${WT_CONSTRUCTOR_ARGS#0x}"
WT_INIT_HASH=$(cast keccak256 "$WT_INIT_CODE")
# salt = keccak256(abi.encodePacked(address(20 bytes), uint256(32 bytes)))
TOKEN_LOWER=$(echo "${FLASH_TOKEN_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')
WT_SALT=$(cast keccak256 "0x${TOKEN_LOWER}0000000000000000000000000000000000000000000000000000000000000000")
# CREATE2: keccak256(0xff ++ deployer ++ salt ++ initCodeHash)[12:]
CREATE2_DEPLOYER_LOWER=$(echo "${BRIDGE_L2_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')
WT_FULL_HASH=$(cast keccak256 "0xff${CREATE2_DEPLOYER_LOWER}${WT_SALT#0x}${WT_INIT_HASH#0x}")
WRAPPED_TOKEN_L2="0x${WT_FULL_HASH:26}"
echo "  WrappedToken L2 (CREATE2): $WRAPPED_TOKEN_L2"
echo "  salt:     $WT_SALT"
echo "  initHash: $WT_INIT_HASH"

# ==========================================================================
# Phase 2a: Fire L1 txs DN+0 through DN+4 in parallel
# ==========================================================================
echo ""
echo "=== Phase 2a: L1 parallel deployment (DN+0 through DN+4) ==="

TX_DIR=$(mktemp -d)
echo "Tx output dir: $TX_DIR"

DEPLOY_GAS=8000000   # Generous limit for contract creation txs
CALL_GAS=1000000     # Generous limit for contract calls
PROXY_GAS=5000000    # createCrossChainProxy needs more gas (deploys a proxy contract)

# DN+0: Deploy TestToken
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+0)) \
    --gas-limit $DEPLOY_GAS \
    --create "$TOKEN_BYTECODE" > "$TX_DIR/00_token" 2>&1 &

# DN+1: Deploy FlashLoan pool
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+1)) \
    --gas-limit $DEPLOY_GAS \
    --create "$POOL_BYTECODE" > "$TX_DIR/01_pool" 2>&1 &

# DN+2: token.transfer to pool (CALL to pre-computed TOKEN_ADDRESS)
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+2)) \
    --gas-limit $CALL_GAS \
    "$FLASH_TOKEN_ADDRESS" "$TRANSFER_POOL_CALLDATA" > "$TX_DIR/02_transfer_pool" 2>&1 &

# DN+3: token.transfer to dev#5 (CALL to pre-computed TOKEN_ADDRESS)
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+3)) \
    --gas-limit $CALL_GAS \
    "$FLASH_TOKEN_ADDRESS" "$TRANSFER_DEV5_CALLDATA" > "$TX_DIR/03_transfer_dev5" 2>&1 &

# DN+4: createCrossChainProxy (CALL to pre-computed ROLLUPS_ADDRESS)
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+4)) \
    --gas-limit $PROXY_GAS \
    "$ROLLUPS_ADDRESS" "$CREATE_PROXY_CALLDATA" > "$TX_DIR/04_create_proxy" 2>&1 &

echo "All phase 2a transactions submitted. Waiting for confirmations..."
if ! wait; then
    echo "One or more phase 2a transactions may have failed; continuing to receipt verification..."
else
    echo "All phase 2a transactions confirmed."
fi

# --- Phase 2a verification: check all receipts succeeded ---
echo ""
echo "=== Verifying phase 2a transaction receipts ==="
DEPLOY_FAILED=false
for f in "$TX_DIR"/*; do
    FNAME=$(basename "$f")
    STATUS=$( (grep -E "^status" "$f" || true) | awk '{print $2}')
    if [ "$STATUS" != "1" ]; then
        echo "FAILED: $FNAME (status=$STATUS)"
        cat "$f"
        DEPLOY_FAILED=true
    else
        echo "  OK: $FNAME"
    fi
done
if [ "$DEPLOY_FAILED" = "true" ]; then
    echo "ERROR: One or more L1 phase 2a transactions failed"
    rm -rf "$TX_DIR"
    exit 1
fi

# ==========================================================================
# Phase 2b: Deploy FlashLoanBridgeExecutor on L1 (needs EXECUTOR_L2_PROXY)
# ==========================================================================
echo ""
echo "=== Phase 2b: L1 FlashLoanBridgeExecutor deployment ==="

# Query computeCrossChainProxyAddress (read-only view call, instant -- Rollups is deployed)
EXECUTOR_L2_PROXY=$(cast call --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" \
    "computeCrossChainProxyAddress(address,uint256)(address)" \
    "$EXECUTOR_L2" "$L2_ROLLUP_ID" 2>&1)
FLASH_EXECUTOR_L2_PROXY_ADDRESS="$EXECUTOR_L2_PROXY"
echo "  ExecutorL2 Proxy (from Rollups): $FLASH_EXECUTOR_L2_PROXY_ADDRESS"

# Build FlashLoanBridgeExecutor constructor args with the now-known proxy address
EXECUTOR_CONSTRUCTOR=$(cast abi-encode "f(address,address,address,address,address,address,address,uint256,address)" \
    "$FLASH_POOL_ADDRESS" \
    "$BRIDGE_ADDRESS" \
    "$EXECUTOR_L2_PROXY" \
    "$EXECUTOR_L2" \
    "$WRAPPED_TOKEN_L2" \
    "$FLASH_NFT" \
    "$BRIDGE_L2_ADDRESS" \
    "$L2_ROLLUP_ID" \
    "$FLASH_TOKEN_ADDRESS")

# DN+5: Deploy FlashLoanBridgeExecutor
EXECUTOR_DEPLOY_OUTPUT=$(cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
    --nonce $((DN+5)) --gas-limit 8000000 \
    --create "${EXECUTOR_BYTECODE}${EXECUTOR_CONSTRUCTOR#0x}" 2>&1)
EXECUTOR_STATUS=$( (echo "$EXECUTOR_DEPLOY_OUTPUT" | grep -E "^status" || true) | awk '{print $2}')
if [ "$EXECUTOR_STATUS" != "1" ]; then
    echo "ERROR: FlashLoanBridgeExecutor L1 deployment failed"
    echo "$EXECUTOR_DEPLOY_OUTPUT"
    rm -rf "$TX_DIR"
    exit 1
fi
echo "  FlashLoanBridgeExecutor deployed at: $FLASH_EXECUTOR_L1_ADDRESS"

# Verify token balances
POOL_BAL=$(cast call --rpc-url "$L1_RPC" "$FLASH_TOKEN_ADDRESS" \
    "balanceOf(address)(uint256)" "$FLASH_POOL_ADDRESS" 2>&1)
DEV5_BAL=$(cast call --rpc-url "$L1_RPC" "$FLASH_TOKEN_ADDRESS" \
    "balanceOf(address)(uint256)" "$DEV5_ADDR" 2>&1)
echo "  Pool token balance: $POOL_BAL"
echo "  Dev#5 token balance: $DEV5_BAL"

rm -rf "$TX_DIR"

# ==========================================================================
# L2 DEPLOYMENT
#
# Wait for Bridge_L2 to have code (builder block 1 protocol txs),
# then deploy FlashLoanBridgeExecutor (nonce 0) and FlashLoanersNFT (nonce 1)
# using dev#5.
# ==========================================================================
echo ""
echo "=== Phase 3: L2 flash loan deployment ==="

# Wait for Bridge_L2 to have code on L2
echo "Waiting for Bridge_L2 code on L2 at $BRIDGE_L2_ADDRESS..."
WAIT_COUNT=0
MAX_WAIT=120
until [ "$(cast code --rpc-url "$L2_RPC" "$BRIDGE_L2_ADDRESS" 2>/dev/null)" != "0x" ]; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ "$WAIT_COUNT" -ge "$MAX_WAIT" ]; then
        echo "ERROR: Bridge_L2 has no code after ${MAX_WAIT}s"
        exit 1
    fi
    sleep 1
done
echo "Bridge_L2 has code on L2."

cd "$CONTRACTS_DIR/sync-rollups-protocol"

DEV5_NONCE=$(cast nonce --rpc-url "$L2_RPC" "$DEV5_ADDR" 2>&1)
echo "dev#5 L2 nonce: $DEV5_NONCE"
if [ "$DEV5_NONCE" != "0" ]; then
    echo "WARNING: dev#5 nonce is $DEV5_NONCE (expected 0) -- addresses will differ from pre-computed!"
fi

# Deploy FlashLoanBridgeExecutor on L2 at nonce 0 (placeholder args -- all zeros)
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
    echo "  ExecutorL2 deployed at: $EXECUTOR_L2_ACTUAL"
    [ "$EXECUTOR_L2_ACTUAL" != "$EXECUTOR_L2" ] && \
        echo "  WARNING: differs from pre-computed ($EXECUTOR_L2)"
else
    echo "ERROR: FlashLoanBridgeExecutor L2 deployment failed"
    exit 1
fi

# Deploy FlashLoanersNFT on L2 at nonce 1 (with correct WrappedToken address)
echo "Deploying FlashLoanersNFT on L2 (nonce=1, wrappedToken=$WRAPPED_TOKEN_L2)..."
FLASH_NFT_ACTUAL_OUTPUT=$(forge create \
    --rpc-url "$L2_RPC" \
    --private-key "$L2_DEPLOY_KEY" \
    --broadcast \
    --gas-price 2000000000 \
    src/periphery/defiMock/FlashLoanersNFT.sol:FlashLoanersNFT \
    --constructor-args "$WRAPPED_TOKEN_L2" 2>&1)
echo "$FLASH_NFT_ACTUAL_OUTPUT" | tail -3
FLASH_NFT_ACTUAL=$(echo "$FLASH_NFT_ACTUAL_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
if [ -n "$FLASH_NFT_ACTUAL" ]; then
    echo "  FlashLoanersNFT deployed at: $FLASH_NFT_ACTUAL"
    [ "$FLASH_NFT_ACTUAL" != "$FLASH_NFT" ] && \
        echo "  WARNING: differs from pre-computed ($FLASH_NFT)"
else
    echo "ERROR: FlashLoanersNFT L2 deployment failed"
    exit 1
fi

# ==========================================================================
# Update rollup.env with flash loan addresses
# ==========================================================================
echo ""
echo "=== Updating rollup.env with flash loan addresses ==="

FLASH_EXECUTOR_L2_ADDRESS="$EXECUTOR_L2"
FLASH_NFT_ADDRESS="$FLASH_NFT"

# Update the FLASH_* variables in rollup.env (they default to 0x000...000)
# Use sed to replace the zero-address defaults with actual addresses
sed -i "s|^FLASH_TOKEN_ADDRESS=.*|FLASH_TOKEN_ADDRESS=${FLASH_TOKEN_ADDRESS}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^FLASH_POOL_ADDRESS=.*|FLASH_POOL_ADDRESS=${FLASH_POOL_ADDRESS}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^FLASH_EXECUTOR_L2_ADDRESS=.*|FLASH_EXECUTOR_L2_ADDRESS=${FLASH_EXECUTOR_L2_ADDRESS}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^FLASH_NFT_ADDRESS=.*|FLASH_NFT_ADDRESS=${FLASH_NFT_ADDRESS}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^FLASH_EXECUTOR_L2_PROXY_ADDRESS=.*|FLASH_EXECUTOR_L2_PROXY_ADDRESS=${FLASH_EXECUTOR_L2_PROXY_ADDRESS}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^FLASH_EXECUTOR_L1_ADDRESS=.*|FLASH_EXECUTOR_L1_ADDRESS=${FLASH_EXECUTOR_L1_ADDRESS}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^WRAPPED_TOKEN_L2=.*|WRAPPED_TOKEN_L2=${WRAPPED_TOKEN_L2}|" "${SHARED_DIR}/rollup.env"

# Verify the updates took effect
echo "Verifying rollup.env updates..."
VERIFY_FAIL=false
for VAR_NAME in FLASH_TOKEN_ADDRESS FLASH_POOL_ADDRESS FLASH_EXECUTOR_L2_ADDRESS \
                FLASH_NFT_ADDRESS FLASH_EXECUTOR_L2_PROXY_ADDRESS \
                FLASH_EXECUTOR_L1_ADDRESS WRAPPED_TOKEN_L2; do
    VAL=$( (grep "^${VAR_NAME}=" "${SHARED_DIR}/rollup.env" || true) | head -1 | cut -d= -f2)
    if [ "$VAL" = "$ZERO" ] || [ -z "$VAL" ]; then
        echo "  ERROR: $VAR_NAME still zero or missing in rollup.env"
        VERIFY_FAIL=true
    else
        echo "  OK: $VAR_NAME=$VAL"
    fi
done
if [ "$VERIFY_FAIL" = "true" ]; then
    echo "ERROR: rollup.env update verification failed"
    exit 1
fi

# Mark as done
touch "${SHARED_DIR}/flash-loan-deploy.done"

# --- Deployment summary ---
echo ""
echo "=== Flash Loan Deployment Summary ==="
echo "TestToken L1:         $FLASH_TOKEN_ADDRESS"
echo "FlashLoan Pool L1:    $FLASH_POOL_ADDRESS"
echo "ExecutorL1:           $FLASH_EXECUTOR_L1_ADDRESS"
echo "ExecutorL2:           $FLASH_EXECUTOR_L2_ADDRESS"
echo "ExecutorL2 Proxy:     $FLASH_EXECUTOR_L2_PROXY_ADDRESS"
echo "FlashNFT L2:          $FLASH_NFT_ADDRESS"
echo "WrappedToken L2:      $WRAPPED_TOKEN_L2"
echo ""
echo "Flash loan deployment complete."
