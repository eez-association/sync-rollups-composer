#!/usr/bin/env bash
# Deploy cross-chain aggregator contracts on L1 and L2.
# Standalone script — runs AFTER deploy-flash-loan.sh completes.
# Reads config from /shared/rollup.env (written by deploy.sh).
#
# L1 deployments use dev#22 (dedicated aggregator deployer, avoids nonce
# conflicts with the builder which uses dev#0 for postBatch).
# L2 deployments use dev#5 (shared with deploy_l2.sh / deploy-flash-loan.sh;
# starts from current nonce, not 0).
# Funding: dev#9 funds dev#22 on L1 before deployment.
#
# Deploys: WETH, MockERC20 (USDC), SimpleAMM (L1+L2), L2Executor,
#          CrossChainAggregator, and creates a CrossChainProxy for L2Executor.
#
# WARNING: The private keys below are well-known anvil default keys.
# They are PUBLIC and MUST NEVER be used on mainnet, testnets, or any chain
# where real value is at stake. This script is for LOCAL DEVELOPMENT ONLY.
set -euo pipefail

L1_RPC="${L1_RPC:-http://l1:8545}"
L2_RPC="${L2_RPC:-http://builder:8545}"
L1_PROXY="${L1_PROXY:-http://builder:9556}"
SHARED_DIR="${SHARED_DIR:-/shared}"
CONTRACTS_DIR="${CONTRACTS_DIR:-/app/contracts}"

# dev#22 — dedicated aggregator L1 deployer
DEPLOYER_KEY="0x224b7eb7449992aac96d631d9677f7bf5888245eef6d6eeda31e62d2f29a83e4"
DEPLOYER_ADDR="0x08135Da0A343E492FA2d4282F2AE34c6c5CC1BbE"
# dev#9 — L1 funder
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
# dev#5 — L2 deployer (shared — start from current nonce)
L2_DEPLOY_KEY="0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba"
DEV5_ADDR="0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"

L2_ROLLUP_ID=1
ZERO="0x0000000000000000000000000000000000000000"

# Liquidity amounts
L1_WETH_LIQUIDITY="100000000000000000000"    # 100 WETH (= 100 ETH deposited)
L1_USDC_LIQUIDITY="200000000000"             # 200,000 USDC (6 decimals)
L2_WETH_LIQUIDITY="50000000000000000000"     # 50 wWETH
L2_USDC_LIQUIDITY="100000000000"             # 100,000 wUSDC
BRIDGE_WETH_AMOUNT="60000000000000000000"    # 60 WETH to bridge (50 for AMM + buffer)
BRIDGE_USDC_AMOUNT="120000000000"            # 120,000 USDC to bridge

# Idempotency — skip if marker exists
if [ -f "${SHARED_DIR}/aggregator-deploy.done" ]; then
    echo "Aggregator deployment already done -- skipping."
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
        ROLLUPS_ADDRESS|BRIDGE_ADDRESS|BRIDGE_L1_ADDRESS|BRIDGE_L2_ADDRESS|ROLLUP_ID)
            export "$key=$value"
            ;;
    esac
done < "${SHARED_DIR}/rollup.env"

: "${ROLLUPS_ADDRESS:?ROLLUPS_ADDRESS not set in rollup.env}"
: "${BRIDGE_L2_ADDRESS:?BRIDGE_L2_ADDRESS not set in rollup.env}"
: "${BRIDGE_ADDRESS:?BRIDGE_ADDRESS not set in rollup.env}"

# Helper to extract bytecode.object from forge JSON artifacts
_bc() { (grep -o '"object":"0x[0-9a-fA-F]*"' "$1" || true) | head -1 | sed 's/"object":"//;s/"//'; }

echo "=========================================="
echo "  Aggregator Deployment"
echo "=========================================="
echo "L1 RPC:     $L1_RPC"
echo "L2 RPC:     $L2_RPC"
echo "L1 Proxy:   $L1_PROXY"
echo "Rollups:    $ROLLUPS_ADDRESS"
echo "Bridge L1:  $BRIDGE_ADDRESS"
echo "Bridge L2:  $BRIDGE_L2_ADDRESS"

# --- Wait for L1 and L2 to be healthy ---
echo ""
echo "Waiting for L1 at ${L1_RPC}..."
WAIT_COUNT=0; MAX_WAIT=120
until cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    [ "$WAIT_COUNT" -ge "$MAX_WAIT" ] && { echo "ERROR: Timed out waiting for L1"; exit 1; }
    sleep 1
done
echo "L1 is ready."

echo "Waiting for L2 at ${L2_RPC}..."
WAIT_COUNT=0; MAX_WAIT=300
until cast block-number --rpc-url "$L2_RPC" >/dev/null 2>&1; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    [ "$WAIT_COUNT" -ge "$MAX_WAIT" ] && { echo "ERROR: Timed out waiting for L2"; exit 1; }
    sleep 1
done
echo "L2 is ready (block $(cast block-number --rpc-url "$L2_RPC"))."

# --- Fund dev#22 on L1 ---
DEPLOYER_BAL=$(cast balance --rpc-url "$L1_RPC" "$DEPLOYER_ADDR" 2>/dev/null || echo "0")
if [ "$(printf '%d' "$DEPLOYER_BAL" 2>/dev/null || echo 0)" -lt 1000000000000000000 ] 2>/dev/null; then
    echo "Funding aggregator deployer ($DEPLOYER_ADDR) from dev#9..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$DEPLOYER_ADDR" --value 200ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
    echo "  Funded: $(cast balance --rpc-url "$L1_RPC" "$DEPLOYER_ADDR") wei"
fi

# --- Build contracts ---
echo ""
echo "Building contracts..."
cd "$CONTRACTS_DIR/test-multi-call"
forge build

# --- Extract bytecodes ---
echo "Extracting bytecodes..."
WETH_BYTECODE=$(_bc "$CONTRACTS_DIR/test-multi-call/out/WETH.sol/WETH.json")
USDC_BYTECODE=$(_bc "$CONTRACTS_DIR/test-multi-call/out/MockERC20.sol/MockERC20.json")
AMM_BYTECODE=$(_bc "$CONTRACTS_DIR/test-multi-call/out/SimpleAMM.sol/SimpleAMM.json")
L2EXEC_BYTECODE=$(_bc "$CONTRACTS_DIR/test-multi-call/out/L2Executor.sol/L2Executor.json")
AGG_BYTECODE=$(_bc "$CONTRACTS_DIR/test-multi-call/out/CrossChainAggregator.sol/CrossChainAggregator.json")

for NAME in WETH USDC AMM L2EXEC AGG; do
    VAR="${NAME}_BYTECODE"
    BC="${!VAR}"
    if [ -z "$BC" ] || [ "$BC" = "null" ]; then
        echo "ERROR: ${NAME} bytecode not found"; exit 1
    fi
    echo "  ${NAME} bytecode length: ${#BC}"
done

# ==========================================================================
# Phase 1: L1 Deployment
# ==========================================================================
echo ""
echo "=== Phase 1: L1 Deployment ==="

DN=$(cast nonce --rpc-url "$L1_RPC" "$DEPLOYER_ADDR")
echo "Deployer L1 starting nonce: $DN"

DEPLOY_GAS=8000000
CALL_GAS=1000000
PROXY_GAS=5000000

# Pre-compute addresses
WETH_ADDR=$(cast compute-address "$DEPLOYER_ADDR" --nonce $((DN+0)) | awk '{print $NF}')
USDC_ADDR=$(cast compute-address "$DEPLOYER_ADDR" --nonce $((DN+1)) | awk '{print $NF}')
# DN+2: USDC constructor args (string,string,uint8) — USDC encoded
USDC_CONSTRUCTOR=$(cast abi-encode "f(string,string,uint8)" "USD Coin" "USDC" 6)

echo "Pre-computed L1 addresses:"
echo "  WETH (DN+0):  $WETH_ADDR"
echo "  USDC (DN+1):  $USDC_ADDR"

# DN+0: Deploy WETH (no constructor args)
echo "Deploying WETH..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+0)) \
    --gas-limit $DEPLOY_GAS --create "$WETH_BYTECODE" > /dev/null 2>&1
echo "  WETH deployed at: $WETH_ADDR"

# DN+1: Deploy MockERC20 (USDC)
echo "Deploying USDC..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+1)) \
    --gas-limit $DEPLOY_GAS --create "${USDC_BYTECODE}${USDC_CONSTRUCTOR#0x}" > /dev/null 2>&1
echo "  USDC deployed at: $USDC_ADDR"

# DN+2: Mint USDC to deployer (500K USDC)
echo "Minting USDC..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+2)) \
    --gas-limit $CALL_GAS \
    "$USDC_ADDR" "mint(address,uint256)" "$DEPLOYER_ADDR" "500000000000" > /dev/null 2>&1

# DN+3: Deposit ETH → WETH (200 ETH → 200 WETH)
echo "Wrapping ETH → WETH..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+3)) \
    --gas-limit $CALL_GAS --value 200000000000000000000 \
    "$WETH_ADDR" "deposit()" > /dev/null 2>&1

# DN+4: Deploy L1 SimpleAMM(WETH, USDC)
AMM_L1_CONSTRUCTOR=$(cast abi-encode "f(address,address)" "$WETH_ADDR" "$USDC_ADDR")
AMM_L1_ADDR=$(cast compute-address "$DEPLOYER_ADDR" --nonce $((DN+4)) | awk '{print $NF}')
echo "Deploying L1 AMM..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+4)) \
    --gas-limit $DEPLOY_GAS --create "${AMM_BYTECODE}${AMM_L1_CONSTRUCTOR#0x}" > /dev/null 2>&1
echo "  L1 AMM deployed at: $AMM_L1_ADDR"

# DN+5: Approve WETH for L1 AMM
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+5)) \
    --gas-limit $CALL_GAS \
    "$WETH_ADDR" "approve(address,uint256)" "$AMM_L1_ADDR" \
    "115792089237316195423570985008687907853269984665640564039457584007913129639935" > /dev/null 2>&1

# DN+6: Approve USDC for L1 AMM
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+6)) \
    --gas-limit $CALL_GAS \
    "$USDC_ADDR" "approve(address,uint256)" "$AMM_L1_ADDR" \
    "115792089237316195423570985008687907853269984665640564039457584007913129639935" > /dev/null 2>&1

# DN+7: Add liquidity to L1 AMM
echo "Adding L1 AMM liquidity..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN+7)) \
    --gas-limit $CALL_GAS \
    "$AMM_L1_ADDR" "addLiquidity(uint256,uint256)" "$L1_WETH_LIQUIDITY" "$L1_USDC_LIQUIDITY" > /dev/null 2>&1

# Verify L1 AMM reserves
RESERVE_A=$(cast call --rpc-url "$L1_RPC" "$AMM_L1_ADDR" "reserveA()(uint256)" 2>/dev/null)
RESERVE_B=$(cast call --rpc-url "$L1_RPC" "$AMM_L1_ADDR" "reserveB()(uint256)" 2>/dev/null)
echo "  L1 AMM reserves: A=$RESERVE_A B=$RESERVE_B"

# ==========================================================================
# Phase 2: Bridge tokens to L2
# ==========================================================================
echo ""
echo "=== Phase 2: Bridge tokens to L2 ==="

# Approve Bridge for WETH and USDC
DN2=$(cast nonce --rpc-url "$L1_RPC" "$DEPLOYER_ADDR")

cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN2+0)) \
    --gas-limit $CALL_GAS \
    "$WETH_ADDR" "approve(address,uint256)" "$BRIDGE_ADDRESS" \
    "115792089237316195423570985008687907853269984665640564039457584007913129639935" > /dev/null 2>&1

cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN2+1)) \
    --gas-limit $CALL_GAS \
    "$USDC_ADDR" "approve(address,uint256)" "$BRIDGE_ADDRESS" \
    "115792089237316195423570985008687907853269984665640564039457584007913129639935" > /dev/null 2>&1

# Bridge WETH to L2 (via L1 proxy for cross-chain detection)
echo "Bridging WETH to L2..."
cast send --rpc-url "$L1_PROXY" --private-key "$DEPLOYER_KEY" --nonce $((DN2+2)) \
    --gas-limit 800000 \
    "$BRIDGE_ADDRESS" "bridgeTokens(address,uint256,uint256,address)" \
    "$WETH_ADDR" "$BRIDGE_WETH_AMOUNT" "$L2_ROLLUP_ID" "$DEV5_ADDR" > /dev/null 2>&1

# Bridge USDC to L2
echo "Bridging USDC to L2..."
cast send --rpc-url "$L1_PROXY" --private-key "$DEPLOYER_KEY" --nonce $((DN2+3)) \
    --gas-limit 800000 \
    "$BRIDGE_ADDRESS" "bridgeTokens(address,uint256,uint256,address)" \
    "$USDC_ADDR" "$BRIDGE_USDC_AMOUNT" "$L2_ROLLUP_ID" "$DEV5_ADDR" > /dev/null 2>&1

echo "Waiting for bridge settlement on L2..."
L2_BLK=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null)
WAIT_COUNT=0; MAX_WAIT=120
while true; do
    CURRENT_BLK=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "$L2_BLK")
    if [ "$CURRENT_BLK" -ge "$((L2_BLK + 5))" ] 2>/dev/null; then break; fi
    WAIT_COUNT=$((WAIT_COUNT + 1))
    [ "$WAIT_COUNT" -ge "$MAX_WAIT" ] && { echo "WARNING: L2 block advance timed out"; break; }
    sleep 1
done
echo "L2 advanced to block $(cast block-number --rpc-url "$L2_RPC" 2>/dev/null)"

# ==========================================================================
# Phase 3: Pre-compute wrapped token L2 addresses via CREATE2
# ==========================================================================
echo ""
echo "=== Phase 3: Pre-compute wrapped token addresses ==="

WT_CREATION_CODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/WrappedToken.sol/WrappedToken.json")
if [ -z "$WT_CREATION_CODE" ]; then
    echo "Building sync-rollups-protocol for WrappedToken bytecode..."
    cd "$CONTRACTS_DIR/sync-rollups-protocol"
    forge build --skip "fv/*"
    WT_CREATION_CODE=$(_bc "$CONTRACTS_DIR/sync-rollups-protocol/out/WrappedToken.sol/WrappedToken.json")
fi

BRIDGE_L2_LOWER=$(echo "${BRIDGE_L2_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')

# Compute wrapped WETH L2 address
WETH_WT_CONSTRUCTOR=$(cast abi-encode "f(string,string,uint8,address)" "Wrapped Ether" "WETH" 18 "$BRIDGE_L2_ADDRESS")
WETH_WT_INIT="${WT_CREATION_CODE}${WETH_WT_CONSTRUCTOR#0x}"
WETH_WT_INIT_HASH=$(cast keccak256 "$WETH_WT_INIT")
WETH_LOWER=$(echo "${WETH_ADDR#0x}" | tr '[:upper:]' '[:lower:]')
WETH_WT_SALT=$(cast keccak256 "0x${WETH_LOWER}0000000000000000000000000000000000000000000000000000000000000000")
WETH_WT_FULL=$(cast keccak256 "0xff${BRIDGE_L2_LOWER}${WETH_WT_SALT#0x}${WETH_WT_INIT_HASH#0x}")
WRAPPED_WETH_L2="0x${WETH_WT_FULL:26}"
echo "  Wrapped WETH L2: $WRAPPED_WETH_L2"

# Compute wrapped USDC L2 address
USDC_WT_CONSTRUCTOR=$(cast abi-encode "f(string,string,uint8,address)" "USD Coin" "USDC" 6 "$BRIDGE_L2_ADDRESS")
USDC_WT_INIT="${WT_CREATION_CODE}${USDC_WT_CONSTRUCTOR#0x}"
USDC_WT_INIT_HASH=$(cast keccak256 "$USDC_WT_INIT")
USDC_LOWER=$(echo "${USDC_ADDR#0x}" | tr '[:upper:]' '[:lower:]')
USDC_WT_SALT=$(cast keccak256 "0x${USDC_LOWER}0000000000000000000000000000000000000000000000000000000000000000")
USDC_WT_FULL=$(cast keccak256 "0xff${BRIDGE_L2_LOWER}${USDC_WT_SALT#0x}${USDC_WT_INIT_HASH#0x}")
WRAPPED_USDC_L2="0x${USDC_WT_FULL:26}"
echo "  Wrapped USDC L2: $WRAPPED_USDC_L2"

# Verify wrapped tokens have code
WETH_L2_CODE=$(cast code --rpc-url "$L2_RPC" "$WRAPPED_WETH_L2" 2>/dev/null | wc -c)
USDC_L2_CODE=$(cast code --rpc-url "$L2_RPC" "$WRAPPED_USDC_L2" 2>/dev/null | wc -c)
echo "  WETH L2 code length: $WETH_L2_CODE"
echo "  USDC L2 code length: $USDC_L2_CODE"

if [ "$WETH_L2_CODE" -lt 10 ] || [ "$USDC_L2_CODE" -lt 10 ]; then
    echo "WARNING: Wrapped tokens may not be deployed yet. Waiting 30s..."
    sleep 30
    WETH_L2_CODE=$(cast code --rpc-url "$L2_RPC" "$WRAPPED_WETH_L2" 2>/dev/null | wc -c)
    USDC_L2_CODE=$(cast code --rpc-url "$L2_RPC" "$WRAPPED_USDC_L2" 2>/dev/null | wc -c)
    if [ "$WETH_L2_CODE" -lt 10 ] || [ "$USDC_L2_CODE" -lt 10 ]; then
        echo "ERROR: Wrapped tokens not deployed on L2"
        exit 1
    fi
fi

# ==========================================================================
# Phase 4: L2 Deployment
# ==========================================================================
echo ""
echo "=== Phase 4: L2 Deployment ==="

cd "$CONTRACTS_DIR/test-multi-call"

DEV5_NONCE=$(cast nonce --rpc-url "$L2_RPC" "$DEV5_ADDR" 2>/dev/null)
echo "dev#5 L2 starting nonce: $DEV5_NONCE"

# Deploy L2 SimpleAMM(wWETH, wUSDC)
L2_AMM_ADDR=$(cast compute-address "$DEV5_ADDR" --nonce "$DEV5_NONCE" | awk '{print $NF}')
L2_AMM_CONSTRUCTOR=$(cast abi-encode "f(address,address)" "$WRAPPED_WETH_L2" "$WRAPPED_USDC_L2")
echo "Deploying L2 AMM..."
cast send --rpc-url "$L2_RPC" --private-key "$L2_DEPLOY_KEY" \
    --gas-limit $DEPLOY_GAS --gas-price 2000000000 \
    --create "${AMM_BYTECODE}${L2_AMM_CONSTRUCTOR#0x}" > /dev/null 2>&1
echo "  L2 AMM deployed at: $L2_AMM_ADDR"

# Deploy L2Executor(amm, bridge_l2, wWETH, wUSDC)
L2_EXEC_NONCE=$((DEV5_NONCE + 1))
L2_EXEC_ADDR=$(cast compute-address "$DEV5_ADDR" --nonce "$L2_EXEC_NONCE" | awk '{print $NF}')
L2_EXEC_CONSTRUCTOR=$(cast abi-encode "f(address,address,address,address)" \
    "$L2_AMM_ADDR" "$BRIDGE_L2_ADDRESS" "$WRAPPED_WETH_L2" "$WRAPPED_USDC_L2")
echo "Deploying L2Executor..."
cast send --rpc-url "$L2_RPC" --private-key "$L2_DEPLOY_KEY" \
    --gas-limit $DEPLOY_GAS --gas-price 2000000000 \
    --create "${L2EXEC_BYTECODE}${L2_EXEC_CONSTRUCTOR#0x}" > /dev/null 2>&1
echo "  L2Executor deployed at: $L2_EXEC_ADDR"

# Add liquidity to L2 AMM
# First approve wrapped tokens for L2 AMM
echo "Setting up L2 AMM liquidity..."
cast send --rpc-url "$L2_RPC" --private-key "$L2_DEPLOY_KEY" \
    --gas-limit $CALL_GAS --gas-price 2000000000 \
    "$WRAPPED_WETH_L2" "approve(address,uint256)" "$L2_AMM_ADDR" \
    "115792089237316195423570985008687907853269984665640564039457584007913129639935" > /dev/null 2>&1

cast send --rpc-url "$L2_RPC" --private-key "$L2_DEPLOY_KEY" \
    --gas-limit $CALL_GAS --gas-price 2000000000 \
    "$WRAPPED_USDC_L2" "approve(address,uint256)" "$L2_AMM_ADDR" \
    "115792089237316195423570985008687907853269984665640564039457584007913129639935" > /dev/null 2>&1

cast send --rpc-url "$L2_RPC" --private-key "$L2_DEPLOY_KEY" \
    --gas-limit $CALL_GAS --gas-price 2000000000 \
    "$L2_AMM_ADDR" "addLiquidity(uint256,uint256)" "$L2_WETH_LIQUIDITY" "$L2_USDC_LIQUIDITY" > /dev/null 2>&1

L2_RESERVE_A=$(cast call --rpc-url "$L2_RPC" "$L2_AMM_ADDR" "reserveA()(uint256)" 2>/dev/null)
L2_RESERVE_B=$(cast call --rpc-url "$L2_RPC" "$L2_AMM_ADDR" "reserveB()(uint256)" 2>/dev/null)
echo "  L2 AMM reserves: A=$L2_RESERVE_A B=$L2_RESERVE_B"

# ==========================================================================
# Phase 5: L1 Aggregator + Proxy
# ==========================================================================
echo ""
echo "=== Phase 5: L1 Aggregator + Proxy ==="

DN5=$(cast nonce --rpc-url "$L1_RPC" "$DEPLOYER_ADDR")

# Create proxy for L2Executor on L1
echo "Creating CrossChainProxy for L2Executor..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN5+0)) \
    --gas-limit $PROXY_GAS \
    "$ROLLUPS_ADDRESS" "createCrossChainProxy(address,uint256)" "$L2_EXEC_ADDR" "$L2_ROLLUP_ID" > /dev/null 2>&1

L2_EXEC_PROXY=$(cast call --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" \
    "$L2_EXEC_ADDR" "$L2_ROLLUP_ID" 2>/dev/null)
echo "  L2Executor proxy on L1: $L2_EXEC_PROXY"

# Deploy CrossChainAggregator(localAMM, bridge, tokenA, tokenB, remoteRollupId)
AGG_CONSTRUCTOR=$(cast abi-encode "f(address,address,address,address,uint256)" \
    "$AMM_L1_ADDR" "$BRIDGE_ADDRESS" "$WETH_ADDR" "$USDC_ADDR" "$L2_ROLLUP_ID")
AGG_ADDR=$(cast compute-address "$DEPLOYER_ADDR" --nonce $((DN5+1)) | awk '{print $NF}')
echo "Deploying CrossChainAggregator..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN5+1)) \
    --gas-limit $DEPLOY_GAS \
    --create "${AGG_BYTECODE}${AGG_CONSTRUCTOR#0x}" > /dev/null 2>&1
echo "  Aggregator deployed at: $AGG_ADDR"

# Configure aggregator with L2Executor + proxy
echo "Configuring aggregator..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" --nonce $((DN5+2)) \
    --gas-limit $CALL_GAS \
    "$AGG_ADDR" "setL2Executor(address,address)" "$L2_EXEC_ADDR" "$L2_EXEC_PROXY" > /dev/null 2>&1

# Verify configuration
L2_EXEC_FROM_AGG=$(cast call --rpc-url "$L1_RPC" "$AGG_ADDR" "l2Executor()(address)" 2>/dev/null)
L2_PROXY_FROM_AGG=$(cast call --rpc-url "$L1_RPC" "$AGG_ADDR" "l2ExecutorProxy()(address)" 2>/dev/null)
echo "  Aggregator.l2Executor:      $L2_EXEC_FROM_AGG"
echo "  Aggregator.l2ExecutorProxy:  $L2_PROXY_FROM_AGG"

# ==========================================================================
# Phase 6: Update rollup.env
# ==========================================================================
echo ""
echo "=== Updating rollup.env with aggregator addresses ==="

sed -i "s|^AGG_WETH_ADDRESS=.*|AGG_WETH_ADDRESS=${WETH_ADDR}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^AGG_USDC_ADDRESS=.*|AGG_USDC_ADDRESS=${USDC_ADDR}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^AGG_L1_AMM_ADDRESS=.*|AGG_L1_AMM_ADDRESS=${AMM_L1_ADDR}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^AGG_AGGREGATOR_ADDRESS=.*|AGG_AGGREGATOR_ADDRESS=${AGG_ADDR}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^AGG_L2_EXECUTOR_ADDRESS=.*|AGG_L2_EXECUTOR_ADDRESS=${L2_EXEC_ADDR}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^AGG_L2_AMM_ADDRESS=.*|AGG_L2_AMM_ADDRESS=${L2_AMM_ADDR}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^AGG_L2_EXECUTOR_PROXY_ADDRESS=.*|AGG_L2_EXECUTOR_PROXY_ADDRESS=${L2_EXEC_PROXY}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^AGG_WRAPPED_WETH_L2=.*|AGG_WRAPPED_WETH_L2=${WRAPPED_WETH_L2}|" "${SHARED_DIR}/rollup.env"
sed -i "s|^AGG_WRAPPED_USDC_L2=.*|AGG_WRAPPED_USDC_L2=${WRAPPED_USDC_L2}|" "${SHARED_DIR}/rollup.env"

# Verify
echo "Verifying rollup.env updates..."
VERIFY_FAIL=false
for VAR_NAME in AGG_WETH_ADDRESS AGG_USDC_ADDRESS AGG_L1_AMM_ADDRESS \
                AGG_AGGREGATOR_ADDRESS AGG_L2_EXECUTOR_ADDRESS AGG_L2_AMM_ADDRESS \
                AGG_L2_EXECUTOR_PROXY_ADDRESS AGG_WRAPPED_WETH_L2 AGG_WRAPPED_USDC_L2; do
    VAL=$( (grep "^${VAR_NAME}=" "${SHARED_DIR}/rollup.env" || true) | head -1 | cut -d= -f2)
    if [ "$VAL" = "$ZERO" ] || [ -z "$VAL" ]; then
        echo "  ERROR: $VAR_NAME still zero or missing"
        VERIFY_FAIL=true
    else
        echo "  OK: $VAR_NAME=$VAL"
    fi
done
if [ "$VERIFY_FAIL" = "true" ]; then
    echo "ERROR: Some aggregator addresses failed to write"
    exit 1
fi

# ==========================================================================
# Done
# ==========================================================================
touch "${SHARED_DIR}/aggregator-deploy.done"

echo ""
echo "=========================================="
echo "  Aggregator Deployment Complete"
echo "=========================================="
echo "  WETH L1:              $WETH_ADDR"
echo "  USDC L1:              $USDC_ADDR"
echo "  L1 AMM:               $AMM_L1_ADDR"
echo "  Aggregator:           $AGG_ADDR"
echo "  L2 Executor:          $L2_EXEC_ADDR"
echo "  L2 AMM:               $L2_AMM_ADDR"
echo "  L2 Executor Proxy:    $L2_EXEC_PROXY"
echo "  Wrapped WETH L2:      $WRAPPED_WETH_L2"
echo "  Wrapped USDC L2:      $WRAPPED_USDC_L2"
echo "=========================================="
