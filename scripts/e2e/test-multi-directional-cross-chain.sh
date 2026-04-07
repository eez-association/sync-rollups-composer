#!/usr/bin/env bash
# test-multi-directional-cross-chain.sh — E2E regression test for composer#18.
#
# Tests multi-directional cross-chain calls: bridge deposit (L1→L2) +
# proxy call (L1→L2) + bridge withdrawal (L2→L1) in a single transaction.
#
# Pattern (cross-chain liquidity aggregator):
#   L1 Aggregator.aggregatedSwap(WETH, 5e18, 3e18):
#     1. Swap 3 WETH on L1 AMM (local)
#     2. Bridge 2 WETH to L2 Executor via Bridge.bridgeTokens (L1→L2)
#     3. Call L2 Executor via proxy: swap on L2 AMM + bridge USDC back (L1→L2→L1)
#     4. Return combined USDC to user
#
# Isolation tests (progressive):
#   A: EOA → Bridge.bridgeTokens (baseline, 1 hop)
#   B: EOA → BridgeThenCall (bridge + proxy, 2 hops)
#   C: EOA → Aggregator (bridge + proxy→executor→bridge-back, 3 hops)
#
# Source: contracts/test-multi-call/src/{MockERC20,SimpleAMM,BridgeThenCall,
#         L2Executor,CrossChainAggregator}.sol
#
# Test account: dev key #20
#   Address:     0x09DB0a93B389bEF724429898f539AEB7ac2Dd55f
#   Private key: 0xeaa861a9a01391ed3d587d8a5a84ca56ee277629a8b02c22093a419bf240e65d
#
# Usage: ./scripts/e2e/test-multi-directional-cross-chain.sh [--json]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"
parse_lib_args "$@"

TEST_KEY="0xeaa861a9a01391ed3d587d8a5a84ca56ee277629a8b02c22093a419bf240e65d"
TEST_ADDR="0x09DB0a93B389bEF724429898f539AEB7ac2Dd55f"
CONTRACTS_DIR="$(cd "$SCRIPT_DIR/../../contracts/test-multi-call" && pwd)"

if [ -t 1 ]; then CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'; RESET='\033[0m'
else CYAN=''; GREEN=''; RED=''; RESET=''; fi

echo -e "${CYAN}=========================================="
echo -e "  MULTI-DIRECTIONAL CROSS-CHAIN (#18)"
echo -e "==========================================${RESET}"

eval "$($DOCKER_COMPOSE_CMD exec -T builder cat /shared/rollup.env 2>/dev/null)"
[ -z "${ROLLUPS_ADDRESS:-}" ] && { echo -e "${RED}ERROR: rollup.env${RESET}"; exit 1; }

CCM_L2="${CROSS_CHAIN_MANAGER_ADDRESS:-}"
ROLLUP_ID="${ROLLUP_ID:-1}"
BRIDGE="${BRIDGE_L1_ADDRESS:-${BRIDGE_ADDRESS:-}}"
L2_BRIDGE="${BRIDGE_L2_ADDRESS:-}"

echo "ROLLUPS=$ROLLUPS_ADDRESS  BRIDGE=$BRIDGE  L2_BRIDGE=$L2_BRIDGE"

# Helper: deploy contract via forge create
forge_deploy() {
    local rpc="$1" contract="$2"; shift 2
    local result
    result=$(forge create --rpc-url "$rpc" --private-key "$TEST_KEY" --broadcast \
        --root "$CONTRACTS_DIR" "$contract" "$@" 2>&1)
    echo "$result" | grep "Deployed to:" | awk '{print $3}'
}

# ══════════════════════════════════════════
#  PRE-FLIGHT
# ══════════════════════════════════════════
start_timer
MODE=$(wait_for_builder_ready 90)
assert "Builder ready" '[ "$MODE" = "Builder" ]'
$DOCKER_COMPOSE_CMD stop crosschain-tx-sender > /dev/null 2>&1 || true
wait_for_pending_zero 30 >/dev/null || true

# Fund
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
[ "$L1_BAL" = "0" ] || [ "$L1_BAL" = "0x0" ] && \
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" "$TEST_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1

# Bridge ETH to L2
L2_BAL=$(cast balance --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
if [ "$(printf '%d' "$L2_BAL" 2>/dev/null || echo 0)" -lt 100000000000000000 ] 2>/dev/null; then
    cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
        "$BRIDGE" "bridgeEther(uint256,address)" "$ROLLUP_ID" "$TEST_ADDR" \
        --value 2ether --gas-limit 800000 > /dev/null 2>&1
    L2_BLK=$(get_block_number "$L2_RPC")
    wait_for_block_advance "$L2_RPC" "$L2_BLK" 3 60 >/dev/null || true
    wait_for_pending_zero 60 >/dev/null || true
fi
print_elapsed "PRE-FLIGHT"

# ══════════════════════════════════════════
#  DEPLOY
# ══════════════════════════════════════════
echo ""
echo "========================================"
echo "  DEPLOY"
echo "========================================"
start_timer

# Compile contracts
echo "Compiling contracts..."
(cd "$CONTRACTS_DIR" && forge build > /dev/null 2>&1)

# L1: MockERC20 (WETH), MockERC20 (USDC), SimpleAMM
echo "Deploying L1 tokens + AMM..."
TOKEN_L1=$(forge_deploy "$L1_RPC" "src/MockERC20.sol:MockERC20" --constructor-args "Wrapped Ether" "WETH" 18)
USDC_L1=$(forge_deploy "$L1_RPC" "src/MockERC20.sol:MockERC20" --constructor-args "USD Coin" "USDC" 6)
echo "  L1 WETH: $TOKEN_L1"
echo "  L1 USDC: $USDC_L1"
assert "DEPLOY: L1 tokens" '[ -n "$TOKEN_L1" ] && [ -n "$USDC_L1" ]'

# Mint tokens
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$TOKEN_L1" "mint(address,uint256)" "$TEST_ADDR" 200000000000000000000 --gas-limit 100000 > /dev/null 2>&1
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$USDC_L1" "mint(address,uint256)" "$TEST_ADDR" 400000000000 --gas-limit 100000 > /dev/null 2>&1

# L1 AMM
AMM_L1=$(forge_deploy "$L1_RPC" "src/SimpleAMM.sol:SimpleAMM" --constructor-args "$TOKEN_L1" "$USDC_L1")
echo "  L1 AMM: $AMM_L1"
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$TOKEN_L1" "approve(address,uint256)" "$AMM_L1" 100000000000000000000 --gas-limit 100000 > /dev/null 2>&1
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$USDC_L1" "approve(address,uint256)" "$AMM_L1" 200000000000 --gas-limit 100000 > /dev/null 2>&1
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$AMM_L1" "addLiquidity(uint256,uint256)" 100000000000000000000 200000000000 --gas-limit 500000 > /dev/null 2>&1

# Bridge tokens to L2
echo "Bridging tokens to L2..."
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$TOKEN_L1" "approve(address,uint256)" "$BRIDGE" 50000000000000000000 --gas-limit 100000 > /dev/null 2>&1
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$USDC_L1" "approve(address,uint256)" "$BRIDGE" 100000000000 --gas-limit 100000 > /dev/null 2>&1
cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" "$BRIDGE" "bridgeTokens(address,uint256,uint256,address)" "$TOKEN_L1" 50000000000000000000 "$ROLLUP_ID" "$TEST_ADDR" --gas-limit 800000 > /dev/null 2>&1
cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" "$BRIDGE" "bridgeTokens(address,uint256,uint256,address)" "$USDC_L1" 100000000000 "$ROLLUP_ID" "$TEST_ADDR" --gas-limit 800000 > /dev/null 2>&1
echo "  Waiting for bridge settlement..."
wait_for_pending_zero 60 >/dev/null || true
L2_BLK=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true

# Find wrapped tokens on L2 via Bridge_L2.getWrappedToken().
# Bridge.bridgeTokens calls receiveTokens on the destination, which deploys
# WrappedToken via CREATE2 with salt = keccak256(originalToken, originalRollupId).
# originalRollupId=0 because both tokens are native to L1 (mainnet).
echo "Finding wrapped tokens on L2..."
WETH_L2=$(cast call --rpc-url "$L2_RPC" "$L2_BRIDGE" "getWrappedToken(address,uint256)(address)" "$TOKEN_L1" 0 2>/dev/null || echo "")
USDC_L2=$(cast call --rpc-url "$L2_RPC" "$L2_BRIDGE" "getWrappedToken(address,uint256)(address)" "$USDC_L1" 0 2>/dev/null || echo "")
echo "  L2 WETH: $WETH_L2"
echo "  L2 USDC: $USDC_L2"
assert "DEPLOY: Found L2 wrapped tokens" \
    '[ -n "$WETH_L2" ] && [ "$WETH_L2" != "0x0000000000000000000000000000000000000000" ] && [ -n "$USDC_L2" ] && [ "$USDC_L2" != "0x0000000000000000000000000000000000000000" ]'

# L2 AMM
AMM_L2=$(forge_deploy "$L2_RPC" "src/SimpleAMM.sol:SimpleAMM" --constructor-args "$WETH_L2" "$USDC_L2")
echo "  L2 AMM: $AMM_L2"
cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" "$WETH_L2" "approve(address,uint256)" "$AMM_L2" 50000000000000000000 --gas-limit 100000 > /dev/null 2>&1
cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" "$USDC_L2" "approve(address,uint256)" "$AMM_L2" 100000000000 --gas-limit 100000 > /dev/null 2>&1
cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" "$AMM_L2" "addLiquidity(uint256,uint256)" 50000000000000000000 100000000000 --gas-limit 500000 > /dev/null 2>&1

# L2 Executor
L2_EXEC=$(forge_deploy "$L2_RPC" "src/L2Executor.sol:L2Executor" --constructor-args "$AMM_L2" "$L2_BRIDGE" "$WETH_L2" "$USDC_L2")
echo "  L2 Executor: $L2_EXEC"

# L1 Aggregator
AGG_L1=$(forge_deploy "$L1_RPC" "src/CrossChainAggregator.sol:CrossChainAggregator" --constructor-args "$AMM_L1" "$BRIDGE" "$TOKEN_L1" "$USDC_L1" "$ROLLUP_ID")
echo "  L1 Aggregator: $AGG_L1"

# Create proxy for L2 Executor on L1
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
    "$ROLLUPS_ADDRESS" "createCrossChainProxy(address,uint256)" "$L2_EXEC" "$ROLLUP_ID" \
    --gas-limit 500000 > /dev/null 2>&1
L2_EXEC_PROXY=$(cast call --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" "$L2_EXEC" "$ROLLUP_ID" 2>/dev/null)
echo "  L2 Executor proxy on L1: $L2_EXEC_PROXY"

# Configure aggregator
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
    "$AGG_L1" "setL2Executor(address,address)" "$L2_EXEC" "$L2_EXEC_PROXY" \
    --gas-limit 100000 > /dev/null 2>&1

# Also deploy BridgeThenCall + Counter for isolation tests
BTC=$(forge_deploy "$L1_RPC" "src/BridgeThenCall.sol:BridgeThenCall")
C_L2_RESULT=$(cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
    --create "$(forge_deploy "$L2_RPC" "src/Counter.sol:Counter" 2>/dev/null && echo SKIP || \
    cd "$CONTRACTS_DIR" && forge inspect Counter bytecode)" --json 2>&1 || echo "{}")
C_L2=$(forge_deploy "$L2_RPC" "src/Counter.sol:Counter" 2>/dev/null || echo "")
echo "  BridgeThenCall: $BTC"
echo "  L2 Counter: $C_L2"

if [ -n "$C_L2" ]; then
    cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
        "$ROLLUPS_ADDRESS" "createCrossChainProxy(address,uint256)" "$C_L2" "$ROLLUP_ID" \
        --gas-limit 500000 > /dev/null 2>&1
    C_L2_PROXY=$(cast call --rpc-url "$L1_RPC" \
        "$ROLLUPS_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" "$C_L2" "$ROLLUP_ID" 2>/dev/null)
    echo "  L2 Counter proxy: $C_L2_PROXY"
fi

print_elapsed "DEPLOY"

# ══════════════════════════════════════════
#  TEST A: Bridge.bridgeTokens direct (baseline)
# ══════════════════════════════════════════
echo ""
echo "========================================"
echo "  TEST A: bridgeTokens direct (1 hop)"
echo "========================================"
start_timer

cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$TOKEN_L1" "approve(address,uint256)" "$BRIDGE" 1000000000000000000 --gas-limit 100000 > /dev/null 2>&1
STATUS_A=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
    "$BRIDGE" "bridgeTokens(address,uint256,uint256,address)" "$TOKEN_L1" 1000000000000000000 "$ROLLUP_ID" "$TEST_ADDR" \
    --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST_A: bridgeTokens direct" '[ "$STATUS_A" = "1" ]'
print_elapsed "TEST A"

# ══════════════════════════════════════════
#  TEST B: BridgeThenCall (bridge + proxy, 2 hops)
# ══════════════════════════════════════════
echo ""
echo "========================================"
echo "  TEST B: BridgeThenCall (2 hops)"
echo "========================================"
start_timer

if [ -n "$BTC" ] && [ -n "$C_L2_PROXY" ]; then
    COUNTER_BEFORE=$(cast call --rpc-url "$L2_RPC" "$C_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$TOKEN_L1" "approve(address,uint256)" "$BTC" 1000000000000000000 --gas-limit 100000 > /dev/null 2>&1
    STATUS_B=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
        "$BTC" "bridgeThenCallProxy(address,uint256,address,uint256,address,address,bytes)" \
        "$TOKEN_L1" 1000000000000000000 "$BRIDGE" "$ROLLUP_ID" "$TEST_ADDR" "$C_L2_PROXY" "0xd09de08a" \
        --gas-limit 3000000 2>&1 | grep "^status" | awk '{print $2}')
    echo "  TX: $STATUS_B"
    wait_for_pending_zero 60 >/dev/null || true
    L2_BLK=$(get_block_number "$L2_RPC")
    wait_for_block_advance "$L2_RPC" "$L2_BLK" 3 60 >/dev/null || true
    COUNTER_AFTER=$(cast call --rpc-url "$L2_RPC" "$C_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    echo "  Counter: $COUNTER_BEFORE → $COUNTER_AFTER"
    assert "TEST_B: bridge + proxy (2 hops)" '[ "$STATUS_B" = "1" ] && [ "$COUNTER_AFTER" -gt "$COUNTER_BEFORE" ]'
else
    echo "  SKIP (missing contracts)"
fi
print_elapsed "TEST B"

# ══════════════════════════════════════════
#  TEST C (KEY): Aggregated swap (3 hops)
# ══════════════════════════════════════════
echo ""
echo "========================================"
echo "  TEST C: Aggregated swap (3 hops)"
echo "  Bridge deposit + proxy→executor→bridge-back"
echo "========================================"
start_timer

cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$TOKEN_L1" "mint(address,uint256)" "$TEST_ADDR" 10000000000000000000 --gas-limit 100000 > /dev/null 2>&1
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" "$TOKEN_L1" "approve(address,uint256)" "$AGG_L1" 10000000000000000000 --gas-limit 100000 > /dev/null 2>&1

USDC_BEFORE=$(cast call --rpc-url "$L1_RPC" "$USDC_L1" "balanceOf(address)(uint256)" "$TEST_ADDR" 2>/dev/null | awk '{print $1}' || echo "0")

echo "  Swapping 5 WETH: 3 local + 2 remote..."
RESULT_C=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
    "$AGG_L1" "aggregatedSwap(address,uint256,uint256)" "$TOKEN_L1" 5000000000000000000 3000000000000000000 \
    --gas-limit 5000000 --json 2>&1 || echo "{}")
STATUS_C=$(echo "$RESULT_C" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
TX_HASH=$(echo "$RESULT_C" | grep -oP '"transactionHash"\s*:\s*"\K[^"]+' || echo "")
echo "  TX: $STATUS_C  hash: ${TX_HASH:-<none>}"

if [ "$STATUS_C" = "0x1" ]; then
    wait_for_pending_zero 90 >/dev/null || true
    L2_BLK=$(get_block_number "$L2_RPC")
    wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true

    # cast call returns "109643332400 [1.096e11]" — strip the bracketed
    # scientific notation to get the plain integer for arithmetic comparison.
    USDC_AFTER=$(cast call --rpc-url "$L1_RPC" "$USDC_L1" "balanceOf(address)(uint256)" "$TEST_ADDR" 2>/dev/null | awk '{print $1}' || echo "0")
    GAINED=$(echo "scale=6; ($USDC_AFTER - $USDC_BEFORE) / 1000000" | bc 2>/dev/null || echo "?")
    echo "  USDC gained: $GAINED"
    assert "TEST_C: Aggregated swap (3 hops)" '[ "$USDC_AFTER" -gt "$USDC_BEFORE" ]'
else
    echo "  FAILED — 3-hop pattern not supported yet"
    assert "TEST_C: Aggregated swap (3 hops)" 'false'
fi

print_elapsed "TEST C"

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════
echo ""
echo "========================================"
echo "  RESULTS"
echo "========================================"
echo "  Passed: $PASS_COUNT"
echo "  Failed: $FAIL_COUNT"
print_total_elapsed

$DOCKER_COMPOSE_CMD start crosschain-tx-sender > /dev/null 2>&1 || true

if [ "$FAIL_COUNT" -eq 0 ]; then
  echo -e "  ${GREEN}ALL TESTS PASSED${RESET}"
  exit 0
else
  echo -e "  ${RED}$FAIL_COUNT FAILED${RESET}"
  exit 1
fi
