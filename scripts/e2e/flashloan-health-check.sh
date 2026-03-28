#!/usr/bin/env bash
# Flash Loan E2E Health Check — deploys flash loan contracts and verifies the
# full cross-chain flash loan flow (bridge tokens L1→L2, claim NFT, bridge back).
#
# Requires: forge, cast, L1/L2 running, deploy completed (rollup.env exists).
#
# Usage: ./scripts/e2e/flashloan-health-check.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# lib-health-check.sh requires curl/jq which may not be in foundry container.
# Source it only if curl is available; otherwise define minimal alternatives.
if command -v curl >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    source "$SCRIPT_DIR/lib-health-check.sh"
else
    # Minimal alternatives using cast (foundry container)
    L1_RPC="${L1_RPC:-http://localhost:9555}"
    L2_RPC="${L2_RPC:-http://localhost:9545}"
    FULLNODE1_RPC="${FULLNODE1_RPC:-http://localhost:9546}"
    FULLNODE2_RPC="${FULLNODE2_RPC:-http://localhost:9547}"
    L1_PROXY="${L1_PROXY:-http://localhost:9556}"
    L2_PROXY="${L2_PROXY:-http://localhost:9548}"
    HEALTH_URL="${HEALTH_URL:-http://localhost:9560/health}"
    get_block_number() { cast block-number --rpc-url "$1" 2>/dev/null || echo "0"; }
    get_state_root() { cast block --rpc-url "$1" latest --field stateRoot 2>/dev/null || echo "unknown"; }
    get_health() { echo "health check requires curl"; }
    start_timer() { TIMER_START=$(date +%s); }
    print_elapsed() { local elapsed=$(( $(date +%s) - ${TIMER_START:-$(date +%s)} )); echo "$1 completed in ${elapsed}s"; }
    wait_for_block_advance() {
        local rpc="$1" base="$2" n="${3:-2}" timeout="${4:-60}" elapsed=0
        while [ $elapsed -lt $timeout ]; do
            local current=$(get_block_number "$rpc")
            if [ "$current" -ge $((base + n)) ] 2>/dev/null; then return 0; fi
            sleep 3; elapsed=$((elapsed + 3))
        done
        return 1
    }
fi

# ── Configuration ──
# Use dev#12 for both L1 and L2 deployments (not pre-funded by reth --dev; self-funded via dev#9)
# dev#12 = 0xFABB0ac9d68B0B445fB7357272Ff202C5651694a
DEPLOYER_KEY="0xa267530f49f8280200edf313ee7af6b827f2a8bce2897751d06a843f644967b1"
DEPLOYER_ADDR=$(cast wallet address --private-key "$DEPLOYER_KEY")
# dev#12 used for L2 deployments as well (single key avoids any nonce collision with Docker services)
L2_DEPLOY_KEY="0xa267530f49f8280200edf313ee7af6b827f2a8bce2897751d06a843f644967b1"
L2_DEPLOY_ADDR=$(cast wallet address --private-key "$L2_DEPLOY_KEY")
CONTRACTS_DIR="${CONTRACTS_DIR:-/app/contracts}"
SHARED_DIR="${SHARED_DIR:-/shared}"
L2_ROLLUP_ID=1

# Load rollup.env for existing contract addresses
if [ -f "${SHARED_DIR}/rollup.env" ]; then
    # Source only the config vars (not bytecodes, which are huge)
    while IFS='=' read -r key value; do
        case "$key" in
            ROLLUPS_ADDRESS|BRIDGE_ADDRESS|BRIDGE_L1_ADDRESS|BRIDGE_L2_ADDRESS|\
            CROSS_CHAIN_MANAGER_ADDRESS|BUILDER_ADDRESS)
                export "$key=$value"
                ;;
        esac
    done < "${SHARED_DIR}/rollup.env"
fi

# Ensure required addresses
: "${ROLLUPS_ADDRESS:?ROLLUPS_ADDRESS not set}"
: "${BRIDGE_ADDRESS:?BRIDGE_ADDRESS not set (L1 Bridge)}"
: "${BRIDGE_L2_ADDRESS:?BRIDGE_L2_ADDRESS not set}"
: "${CROSS_CHAIN_MANAGER_ADDRESS:?CROSS_CHAIN_MANAGER_ADDRESS not set}"

BRIDGE_L1_ADDRESS="${BRIDGE_L1_ADDRESS:-$BRIDGE_ADDRESS}"

echo ""
echo "=========================================="
echo "  Flash Loan E2E Health Check"
echo "=========================================="
echo "L1 RPC:      $L1_RPC"
echo "L2 RPC:      $L2_RPC"
echo "L1 Proxy:    $L1_PROXY"
echo "Rollups:     $ROLLUPS_ADDRESS"
echo "Bridge L1:   $BRIDGE_L1_ADDRESS"
echo "Bridge L2:   $BRIDGE_L2_ADDRESS"
echo "CCM:         $CROSS_CHAIN_MANAGER_ADDRESS"
echo "Deployer:    $DEPLOYER_ADDR"
echo "=========================================="
echo ""

# ── Step 0: Fund deployer accounts ──
echo "=== Step 0: Fund deployer accounts ==="
# Fund L1 deployer (dev#12) from dev#9 (pre-funded, not used by any Docker service)
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L1_BAL_CHECK=$(cast balance --rpc-url "$L1_RPC" "$DEPLOYER_ADDR" 2>/dev/null || echo "0")
if [ "$L1_BAL_CHECK" = "0" ] || [ "$L1_BAL_CHECK" = "0x0" ]; then
    echo "Funding L1 deployer ($DEPLOYER_ADDR) with 100 ETH (dev#9 funder)..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$DEPLOYER_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1 || true
    sleep 2
fi
L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$DEPLOYER_ADDR" --ether 2>&1)
echo "L1 deployer balance: $L1_BAL ETH"

# Fix L2 canonical bridge if not set (deploy.sh may have timed out)
FUNDER_KEY_HEX="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
L2_CANONICAL=$(cast call --rpc-url "$L2_RPC" "$BRIDGE_L2_ADDRESS" "canonicalBridgeAddress()(address)" 2>&1)
if [ "$L2_CANONICAL" = "0x0000000000000000000000000000000000000000" ]; then
    echo "L2 Bridge canonical not set — setting now..."
    # Send via L2 RPC. May take a few blocks since dev#0 nonce is managed by builder.
    cast send --rpc-url "$L2_RPC" --private-key "$FUNDER_KEY_HEX" \
        "$BRIDGE_L2_ADDRESS" \
        "setCanonicalBridgeAddress(address)" \
        "$BRIDGE_L1_ADDRESS" --gas-limit 100000 --timeout 60 > /dev/null 2>&1 || \
        echo "WARNING: Could not set L2 canonical bridge — may affect flash loan return trip"
    L2_CANONICAL=$(cast call --rpc-url "$L2_RPC" "$BRIDGE_L2_ADDRESS" "canonicalBridgeAddress()(address)" 2>&1)
    echo "L2 Bridge canonical after fix: $L2_CANONICAL"
fi

# ── Step 0b: Build contracts ──
echo ""
echo "=== Step 0b: Building contracts ==="
cd "$CONTRACTS_DIR/sync-rollups-protocol"
# Build ALL contracts including test (TestToken is in test/IntegrationTestFlashLoan.t.sol)
forge build 2>&1 | tail -3
echo "Contracts built."

# ── Step 1: Deploy TestToken on L1 ──
echo ""
echo "=== Step 1: Deploy TestToken on L1 ==="
TOKEN_OUTPUT=$(forge create \
    --rpc-url "$L1_RPC" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    test/IntegrationTestFlashLoan.t.sol:TestToken 2>&1)
TOKEN=$(echo "$TOKEN_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
echo "TestToken deployed at: $TOKEN"

# Verify deployer has tokens
TOKEN_BAL=$(cast call --rpc-url "$L1_RPC" "$TOKEN" "balanceOf(address)(uint256)" "$DEPLOYER_ADDR" 2>&1)
echo "Deployer token balance: $TOKEN_BAL"

# ── Step 2: Deploy FlashLoan pool on L1 ──
echo ""
echo "=== Step 2: Deploy FlashLoan pool on L1 ==="
FLASH_POOL_OUTPUT=$(forge create \
    --rpc-url "$L1_RPC" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    src/periphery/defiMock/FlashLoan.sol:FlashLoan 2>&1)
FLASH_POOL=$(echo "$FLASH_POOL_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
echo "FlashLoan pool at: $FLASH_POOL"

# Fund the flash loan pool with 10,000 tokens
echo "Funding flash loan pool with 10,000 tokens..."
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
    "$TOKEN" "transfer(address,uint256)" "$FLASH_POOL" "10000000000000000000000" > /dev/null 2>&1
POOL_BAL=$(cast call --rpc-url "$L1_RPC" "$TOKEN" "balanceOf(address)(uint256)" "$FLASH_POOL" 2>&1)
echo "Pool balance: $POOL_BAL"

# ── Step 2b: Fund deployer on L2 (bridge from L1 if needed) ──
echo ""
echo "=== Step 2b: Fund deployer on L2 ==="
L2_BAL_CHECK=$(cast balance --rpc-url "$L2_RPC" "$L2_DEPLOY_ADDR" 2>/dev/null || echo "0")
if [ "$L2_BAL_CHECK" = "0" ] || [ "$L2_BAL_CHECK" = "0x0" ]; then
    echo "Funding $L2_DEPLOY_ADDR on L2 via bridge (1 ETH)..."
    BRIDGE_L1_ADDRESS_FUND="${BRIDGE_L1_ADDRESS:-$BRIDGE_ADDRESS}"
    cast send --rpc-url "$L1_PROXY" --private-key "$FUNDER_KEY" \
        "$BRIDGE_L1_ADDRESS_FUND" "bridgeEther(uint256,address)" "$L2_ROLLUP_ID" "$L2_DEPLOY_ADDR" \
        --value 1ether --gas-limit 800000 > /dev/null 2>&1 || true
    echo "  Deposit sent. Waiting 30s for L2 processing..."
    sleep 30
    L2_BAL_CHECK=$(cast balance --rpc-url "$L2_RPC" "$L2_DEPLOY_ADDR" 2>/dev/null || echo "0")
    echo "  L2 balance after bridge: $L2_BAL_CHECK wei"
fi

# ── Step 3: Deploy FlashLoanBridgeExecutor on L2 ──
echo ""
echo "=== Step 3: Deploy FlashLoanBridgeExecutor on L2 ==="
echo "L2 deployer: $L2_DEPLOY_ADDR (dev#12)"
ZERO=0x0000000000000000000000000000000000000000
EXECUTOR_L2_OUTPUT=$(forge create \
    --rpc-url "$L2_RPC" \
    --private-key "$L2_DEPLOY_KEY" \
    --broadcast \
    --gas-price 2000000000 \
    src/periphery/defiMock/FlashLoanBridgeExecutor.sol:FlashLoanBridgeExecutor \
    --constructor-args "$ZERO" "$ZERO" "$ZERO" "$ZERO" "$ZERO" "$ZERO" "$ZERO" 0 "$ZERO" 2>&1)
echo "$EXECUTOR_L2_OUTPUT" | tail -5
EXECUTOR_L2=$(echo "$EXECUTOR_L2_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
echo "ExecutorL2 deployed at: $EXECUTOR_L2"
if [ -z "$EXECUTOR_L2" ]; then
    echo "FATAL: ExecutorL2 deployment failed"
    echo "$EXECUTOR_L2_OUTPUT"
    exit 1
fi

# ── Step 4: Pre-compute WrappedToken address on L2 ──
echo ""
echo "=== Step 4: Pre-compute WrappedToken address ==="
# WrappedToken is deployed by Bridge via CREATE2 when receiveTokens is called.
# Salt = keccak256(abi.encodePacked(token, originRollupId))
# We compute this offline using cast.
WRAPPED_SALT=$(cast keccak256 "$(cast abi-encode "f(address,uint256)" "$TOKEN" 0)" 2>&1 | tr -d '\n')
echo "Wrapped salt: $WRAPPED_SALT"

# Get WrappedToken creation code hash
# The WrappedToken constructor args: (name, symbol, decimals, bridge)
TOKEN_NAME=$(cast call --rpc-url "$L1_RPC" "$TOKEN" "name()(string)" 2>&1)
TOKEN_SYMBOL=$(cast call --rpc-url "$L1_RPC" "$TOKEN" "symbol()(string)" 2>&1)
TOKEN_DECIMALS=$(cast call --rpc-url "$L1_RPC" "$TOKEN" "decimals()(uint8)" 2>&1)

# Compute wrapped token address via Bridge.getWrappedToken
# If not available, we'll get it after the first deposit
echo "Token metadata: name=$TOKEN_NAME symbol=$TOKEN_SYMBOL decimals=$TOKEN_DECIMALS"

# ── Step 5: Deploy FlashLoanersNFT on L2 ──
echo ""
echo "=== Step 5: Deploy FlashLoanersNFT on L2 ==="
# We'll use a placeholder address for now — the NFT checks balanceOf which needs the real address.
# We'll come back and update this after computing the wrapped token address.
# For now, deploy with ZERO and skip the NFT claim check.
# TODO: compute CREATE2 address properly or skip NFT for initial test.
FLASH_NFT_OUTPUT=$(forge create \
    --rpc-url "$L2_RPC" \
    --private-key "$L2_DEPLOY_KEY" \
    --broadcast \
    --gas-price 2000000000 \
    src/periphery/defiMock/FlashLoanersNFT.sol:FlashLoanersNFT \
    --constructor-args "$ZERO" 2>&1)
FLASH_NFT=$(echo "$FLASH_NFT_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
echo "FlashLoanersNFT deployed at: $FLASH_NFT (placeholder token addr)"

# ── Step 6: Create CrossChainProxy for ExecutorL2 on L1 ──
echo ""
echo "=== Step 6: Create CrossChainProxy for ExecutorL2 on L1 ==="
echo "Creating proxy for ExecutorL2=$EXECUTOR_L2 on rollupId=$L2_ROLLUP_ID..."
# Use cast call first to get the return value (proxy address), then send the tx
EXECUTOR_L2_PROXY=$(cast call --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" \
    "createCrossChainProxy(address,uint256)(address)" \
    "$EXECUTOR_L2" "$L2_ROLLUP_ID" 2>&1)
echo "Predicted proxy address: $EXECUTOR_L2_PROXY"
# Actually deploy the proxy
cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
    "$ROLLUPS_ADDRESS" \
    "createCrossChainProxy(address,uint256)" \
    "$EXECUTOR_L2" "$L2_ROLLUP_ID" --gas-limit 5000000 > /dev/null 2>&1
# Verify
PROXY_CODE=$(cast code --rpc-url "$L1_RPC" "$EXECUTOR_L2_PROXY" 2>&1)
echo "ExecutorL2 proxy on L1: $EXECUTOR_L2_PROXY (code=${#PROXY_CODE} chars)"

# ── Step 7: Deploy FlashLoanBridgeExecutor on L1 ──
echo ""
echo "=== Step 7: Deploy FlashLoanBridgeExecutor on L1 ==="
EXECUTOR_L1_OUTPUT=$(forge create \
    --rpc-url "$L1_RPC" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    src/periphery/defiMock/FlashLoanBridgeExecutor.sol:FlashLoanBridgeExecutor \
    --constructor-args \
        "$FLASH_POOL" \
        "$BRIDGE_L1_ADDRESS" \
        "$EXECUTOR_L2_PROXY" \
        "$EXECUTOR_L2" \
        "$ZERO" \
        "$FLASH_NFT" \
        "$BRIDGE_L2_ADDRESS" \
        "$L2_ROLLUP_ID" \
        "$TOKEN" 2>&1)
EXECUTOR_L1=$(echo "$EXECUTOR_L1_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
echo "ExecutorL1 deployed at: $EXECUTOR_L1"

# ── Step 8: Verify canonical bridge addresses ──
echo ""
echo "=== Step 8: Verify canonical bridge setup ==="
L1_CANONICAL=$(cast call --rpc-url "$L1_RPC" "$BRIDGE_L1_ADDRESS" "canonicalBridgeAddress()(address)" 2>&1)
L2_CANONICAL=$(cast call --rpc-url "$L2_RPC" "$BRIDGE_L2_ADDRESS" "canonicalBridgeAddress()(address)" 2>&1)
echo "L1 Bridge canonical: $L1_CANONICAL (expected: $BRIDGE_L2_ADDRESS)"
echo "L2 Bridge canonical: $L2_CANONICAL (expected: $BRIDGE_L1_ADDRESS)"

# ── Summary ──
echo ""
echo "=========================================="
echo "  Deployment Summary"
echo "=========================================="
echo "TestToken:        $TOKEN"
echo "FlashLoan Pool:   $FLASH_POOL"
echo "ExecutorL2:       $EXECUTOR_L2"
echo "FlashLoanersNFT:  $FLASH_NFT"
echo "ExecutorL2Proxy:  $EXECUTOR_L2_PROXY"
echo "ExecutorL1:       $EXECUTOR_L1"
echo "=========================================="

# ── Step 9: Trigger flash loan ──
echo ""
echo "=== Step 9: Trigger executor.execute() ==="
start_timer

L2_BLK_BEFORE=$(get_block_number "$L2_RPC")
BUILDER_ROOT_BEFORE=$(get_state_root "$L2_RPC" "latest")

echo "L2 block before: $L2_BLK_BEFORE"
echo "Builder state root before: $BUILDER_ROOT_BEFORE"

# Mode selection: L1_PROXY (auto-discovery) or DIRECT_RPC (manual entry building)
FLASH_LOAN_MODE="${FLASH_LOAN_MODE:-L1_PROXY}"
echo "Flash loan mode: $FLASH_LOAN_MODE"

if [ "$FLASH_LOAN_MODE" = "DIRECT_RPC" ]; then
    # ── DIRECT_RPC mode: build execution table manually via syncrollups_buildExecutionTable ──
    echo "Using direct RPC to build execution table..."

    # Compute receiveTokens calldata for CALL_A (Bridge_L1 → Bridge_L2)
    RECEIVE_TOKENS_DATA=$(cast calldata "receiveTokens(address,uint256,address,uint256,string,string,uint8,uint256)" \
        "$TOKEN" 0 "$EXECUTOR_L2" "10000000000000000000000" "Test Token" "TT" 18 0 2>&1)
    echo "receiveTokens calldata computed (${#RECEIVE_TOKENS_DATA} chars)"

    # Compute claimAndBridgeBack calldata for CALL_B (executor → executorL2)
    CLAIM_AND_BRIDGE_DATA=$(cast calldata "claimAndBridgeBack(address,address,address,uint256,uint256,address)" \
        "$ZERO" "$FLASH_NFT" "$BRIDGE_L2_ADDRESS" "10000000000000000000000" 0 "$EXECUTOR_L1" 2>&1)
    echo "claimAndBridgeBack calldata computed (${#CLAIM_AND_BRIDGE_DATA} chars)"

    # Build the raw signed L1 tx for executor.execute()
    RAW_L1_TX=$(cast mktx --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
        "$EXECUTOR_L1" "execute()" --gas-limit 2000000 2>&1)
    echo "Raw L1 tx signed (${#RAW_L1_TX} chars)"

    # Call buildExecutionTable RPC on the builder
    BRIDGE_L2_HEX=$(echo "$BRIDGE_L2_ADDRESS" | tr '[:upper:]' '[:lower:]')
    EXECUTOR_L2_HEX=$(echo "$EXECUTOR_L2" | tr '[:upper:]' '[:lower:]')
    BRIDGE_L1_HEX=$(echo "$BRIDGE_L1_ADDRESS" | tr '[:upper:]' '[:lower:]')
    EXECUTOR_L1_HEX=$(echo "$EXECUTOR_L1" | tr '[:upper:]' '[:lower:]')

    PARAMS_FILE="/tmp/build_table_params.json"
    cat > "$PARAMS_FILE" <<PARAMEOF
{
    "calls": [
        {
            "destination": "${BRIDGE_L2_HEX}",
            "data": "0x${RECEIVE_TOKENS_DATA#0x}",
            "value": "0",
            "sourceAddress": "${BRIDGE_L1_HEX}"
        },
        {
            "destination": "${EXECUTOR_L2_HEX}",
            "data": "0x${CLAIM_AND_BRIDGE_DATA#0x}",
            "value": "0",
            "sourceAddress": "${EXECUTOR_L1_HEX}"
        }
    ],
    "gasPrice": 100000,
    "rawL1Tx": "${RAW_L1_TX}"
}
PARAMEOF

    echo "Calling syncrollups_buildExecutionTable..."
    BUILD_RESULT=$(cast rpc --rpc-url "$L2_RPC" syncrollups_buildExecutionTable "$(cat $PARAMS_FILE)" 2>&1)
    echo "Build result: $BUILD_RESULT"
    rm -f "$PARAMS_FILE"

else
    # ── L1_PROXY mode: send executor.execute() via L1 proxy ──
    # The builder auto-discovers ALL cross-chain calls via iterative
    # debug_traceCallMany and builds the execution table automatically.
    echo "Sending executor.execute() via L1 proxy (auto-discovery)..."
    echo "L1 Proxy URL: $L1_PROXY"

    SEND_RESULT=$(cast send --rpc-url "$L1_PROXY" --private-key "$DEPLOYER_KEY" \
        "$EXECUTOR_L1" "execute()" --gas-limit 2000000 2>&1) || true
    echo "Send result: $SEND_RESULT"
fi

# ── Step 10: Wait for L2 processing ──
echo ""
echo "=== Step 10: Waiting for L2 processing ==="
wait_for_block_advance "$L2_RPC" "$L2_BLK_BEFORE" 3 120 >/dev/null || true
sleep 5  # extra buffer for entry verification

L2_BLK_AFTER=$(get_block_number "$L2_RPC")
echo "L2 block after: $L2_BLK_AFTER (advanced $(( L2_BLK_AFTER - L2_BLK_BEFORE )) blocks)"

# ── Step 11: Check results ──
echo ""
echo "=== Step 11: Verify results ==="

# Check token balances
POOL_BAL_AFTER=$(cast call --rpc-url "$L1_RPC" "$TOKEN" "balanceOf(address)(uint256)" "$FLASH_POOL" 2>&1)
EXECUTOR_BAL_AFTER=$(cast call --rpc-url "$L1_RPC" "$TOKEN" "balanceOf(address)(uint256)" "$EXECUTOR_L1" 2>&1)
BRIDGE_BAL_AFTER=$(cast call --rpc-url "$L1_RPC" "$TOKEN" "balanceOf(address)(uint256)" "$BRIDGE_L1_ADDRESS" 2>&1)

echo "Pool balance after:     $POOL_BAL_AFTER (expected: 10000e18 = loan repaid)"
echo "Executor balance after: $EXECUTOR_BAL_AFTER (expected: 0)"
echo "Bridge balance after:   $BRIDGE_BAL_AFTER (expected: 0 = tokens released)"

# Check state root convergence
# State root check

BUILDER_ROOT=$(get_state_root "$L2_RPC" "latest")
FN1_ROOT=$(get_state_root "$FULLNODE1_RPC" "latest")
echo "Builder state root:  $BUILDER_ROOT"
echo "Fullnode1 state root: $FN1_ROOT"

# Check health
echo ""
HEALTH=$(get_health 2>/dev/null || echo "unavailable")
echo "Builder health: $HEALTH"

print_elapsed "FLASH LOAN E2E"

echo ""
echo "=========================================="
echo "  Flash Loan E2E Complete"
echo "=========================================="
echo ""
echo "NOTE: This is the initial flash loan test."
echo "Full assertions will be added once the basic flow is verified."
echo "Check the logs above for: execute tx status, token balances, state roots."
