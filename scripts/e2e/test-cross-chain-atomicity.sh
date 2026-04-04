#!/usr/bin/env bash
# test-cross-chain-atomicity.sh — E2E regression test for issue #272.
#
# Tests cross-chain atomicity: when an L2 transaction calls L1 via proxy
# and then reverts, the L1 state changes should also be rolled back.
#
# Contracts:
#   SimpleStorage (L2): stores a uint256 value
#   Orchestrator (L2):  calls L1 Counter proxy, stores result in SimpleStorage,
#                        reverts if result is even
#   DualCaller (L1):    reads L2 Storage proxy (cross-chain) + increments L1 Counter
#   Counter (L1):       simple increment, returns new value
#
# Flow:
#   Step 1: Orchestrator -> L1 Counter (0->1, odd, OK) -> stores 1
#   Step 2: DualCaller reads storage(1) + increments counter(1->2)
#   Step 3: Orchestrator -> L1 Counter (2->3, odd, OK) -> stores 3
#   Step 4: Orchestrator -> L1 Counter (3->4, even, REVERT!)
#           Expected: L1 Counter stays at 3, storage stays at 3
#   Step 5: DualCaller reads storage(3) + increments counter(3->4)
#
# The KEY assertion is Step 4: if the L2 tx reverts, the L1 counter
# increment that was triggered by the cross-chain call should also
# be rolled back. Before the fix, L1 state changes persisted even
# when the L2 tx reverted.
#
# Test account: dev key #19 (HD mnemonic index 19)
#   Address:     0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199
#   Private key: 0xdf57089febbacf7ba0bc227dafbffa9fc08a93fdc68e1e42411a14efcf23656e
#
# Usage: ./scripts/e2e/test-cross-chain-atomicity.sh [--json]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

TEST_KEY="0xdf57089febbacf7ba0bc227dafbffa9fc08a93fdc68e1e42411a14efcf23656e"
TEST_ADDR="0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199"

# Contracts compiled from contracts/test-atomicity/src/ via forge build.
# NEVER hardcode bytecode — use forge create to deploy.
CONTRACTS_DIR="$(cd "$SCRIPT_DIR/../../contracts/test-atomicity" && pwd)"
if [ ! -d "$CONTRACTS_DIR/out" ]; then
    echo "Compiling atomicity test contracts..."
    (cd "$CONTRACTS_DIR" && forge build > /dev/null 2>&1)
fi

# ── Colors ──

if [ -t 1 ]; then
  CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'
  YELLOW='\033[1;33m'; BOLD='\033[1m'; RESET='\033[0m'
else
  CYAN=''; GREEN=''; RED=''; YELLOW=''; BOLD=''; RESET=''
fi

# ── Load rollup.env ──

echo ""
echo -e "${CYAN}========================================"
echo -e "  CROSS-CHAIN ATOMICITY TEST (#272)"
echo -e "========================================${RESET}"
echo ""
echo "Loading rollup.env..."

eval "$($DOCKER_COMPOSE_CMD exec -T builder cat /shared/rollup.env 2>/dev/null)"
if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
  echo -e "${RED}ERROR: Could not load rollup.env${RESET}"
  exit 1
fi

CCM_L2="${CROSS_CHAIN_MANAGER_ADDRESS:-}"
ROLLUP_ID="${ROLLUP_ID:-1}"
BRIDGE_ADDR="${BRIDGE_L1_ADDRESS:-${BRIDGE_ADDRESS:-}}"
echo "ROLLUPS=$ROLLUPS_ADDRESS  CCM=$CCM_L2  ROLLUP_ID=$ROLLUP_ID"
echo "Test account: $TEST_ADDR"
echo ""

# Blockscout
L1_EXPLORER="${L1_EXPLORER:-}"
L2_EXPLORER="${L2_EXPLORER:-}"

verify_on_blockscout() {
    local explorer_url="$1" addr="$2" name="$3" source="$4"
    if [ -z "$explorer_url" ] || [ ! -f "$source" ]; then return 0; fi
    echo "  Verifying $name on Blockscout..."
    local payload
    payload=$(cat <<JSONEOF
{
  "compiler_version": "v0.8.28+commit.7893614a",
  "source_code": $(python3 -c "import json; print(json.dumps(open('$source').read()))" 2>/dev/null || echo '""'),
  "is_optimization_enabled": false,
  "evm_version": "default",
  "contract_name": "$name"
}
JSONEOF
    )
    curl -s -X POST "$explorer_url/api/v2/smart-contracts/$addr/verification/via/flattened-code" \
        -H "Content-Type: application/json" -d "$payload" > /dev/null 2>&1 || true
    sleep 3
    local verified
    verified=$(curl -s "$explorer_url/api/v2/smart-contracts/$addr" 2>/dev/null | \
        grep -oP '"is_verified"\s*:\s*\K[a-z]+' || echo "unknown")
    echo "    verified=$verified"
}

# ══════════════════════════════════════════
#  PRE-FLIGHT
# ══════════════════════════════════════════

echo "========================================"
echo "  PRE-FLIGHT"
echo "========================================"
start_timer

echo "Waiting for builder (up to 90s)..."
MODE=$(wait_for_builder_ready 90)
assert "Builder is in Builder mode" '[ "$MODE" = "Builder" ]'

echo "Stopping crosschain-tx-sender..."
$DOCKER_COMPOSE_CMD stop crosschain-tx-sender > /dev/null 2>&1 || true
wait_for_pending_zero 30 >/dev/null || true

FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
if [ "$L1_BAL" = "0" ] || [ "$L1_BAL" = "0x0" ]; then
    echo "Funding $TEST_ADDR on L1..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$TEST_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
fi

L2_BAL=$(cast balance --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
MIN_BAL=50000000000000000
if [ "$(printf '%d' "$L2_BAL" 2>/dev/null || echo 0)" -lt "$MIN_BAL" ] 2>/dev/null; then
    echo "Bridging 0.5 ETH to L2..."
    DEPOSIT_STATUS=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
        "$BRIDGE_ADDR" "bridgeEther(uint256,address)" "$ROLLUP_ID" "$TEST_ADDR" \
        --value 0.5ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
    assert "Bridge deposit succeeded" '[ "$DEPOSIT_STATUS" = "1" ]'
    L2_BLK=$(get_block_number "$L2_RPC")
    wait_for_block_advance "$L2_RPC" "$L2_BLK" 3 60 >/dev/null || true
    wait_for_pending_zero 60 >/dev/null || true
fi

print_elapsed "PRE-FLIGHT"
echo ""

# ══════════════════════════════════════════
#  STEP 1: Deploy contracts
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 1: Deploy contracts"
echo "========================================"
start_timer

forge_deploy() {
    local rpc="$1" contract="$2" label="$3" var="$4"
    local addr
    addr=$(forge create --rpc-url "$rpc" --private-key "$TEST_KEY" --broadcast \
        --root "$CONTRACTS_DIR" "$contract" 2>&1 | grep "Deployed to:" | awk '{print $3}')
    echo "  $label: $addr"
    assert "STEP1: $label deployed" '[ -n "$addr" ]'
    eval "${var}='$addr'"
}

forge_deploy "$L1_RPC" "src/Counter.sol:Counter" "Counter (L1)" L1_COUNTER
forge_deploy "$L2_RPC" "src/SimpleStorage.sol:SimpleStorage" "SimpleStorage (L2)" L2_STORAGE
forge_deploy "$L2_RPC" "src/Orchestrator.sol:Orchestrator" "Orchestrator (L2)" L2_ORCH
forge_deploy "$L1_RPC" "src/DualCaller.sol:DualCaller" "DualCaller (L1)" L1_DUAL

print_elapsed "STEP 1"
echo ""

# ══════════════════════════════════════════
#  STEP 2: Create cross-chain proxies
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 2: Create cross-chain proxies"
echo "========================================"
start_timer

# L1 Counter proxy on L2 (for Orchestrator to call L1 Counter)
cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
    "$CCM_L2" "createCrossChainProxy(address,uint256)" "$L1_COUNTER" 0 \
    --gas-limit 500000 --json > /dev/null 2>&1
L1_COUNTER_PROXY_L2=$(cast call --rpc-url "$L2_RPC" \
    "$CCM_L2" "computeCrossChainProxyAddress(address,uint256)(address)" "$L1_COUNTER" 0 2>/dev/null || echo "")
echo "  L1 Counter proxy on L2: $L1_COUNTER_PROXY_L2"
assert "STEP2: L1 Counter proxy on L2 has code" \
    '[ "$(cast code --rpc-url "$L2_RPC" "$L1_COUNTER_PROXY_L2" 2>/dev/null)" != "0x" ]'

# L2 Storage proxy on L1 (for DualCaller to read L2 Storage)
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
    "$ROLLUPS_ADDRESS" "createCrossChainProxy(address,uint256)" "$L2_STORAGE" "$ROLLUP_ID" \
    --gas-limit 500000 --json > /dev/null 2>&1
L2_STORAGE_PROXY_L1=$(cast call --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" "$L2_STORAGE" "$ROLLUP_ID" 2>/dev/null || echo "")
echo "  L2 Storage proxy on L1: $L2_STORAGE_PROXY_L1"
assert "STEP2: L2 Storage proxy on L1 has code" \
    '[ "$(cast code --rpc-url "$L1_RPC" "$L2_STORAGE_PROXY_L1" 2>/dev/null)" != "0x" ]'

print_elapsed "STEP 2"
echo ""

# Helper: call Orchestrator on L2 and wait
call_orchestrator() {
    local label="$1"
    ORCH_CALLDATA=$(cast calldata "executeAndStore(address,address)" "$L1_COUNTER_PROXY_L2" "$L2_STORAGE")
    RESULT=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY" \
        "$L2_ORCH" "$ORCH_CALLDATA" --gas-limit 3000000 --json 2>&1 || echo "{}")
    TX_STATUS=$(echo "$RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
    echo "  $label: L2 tx status=$TX_STATUS"
    wait_for_pending_zero 90 >/dev/null || true
    L2_BLK=$(get_block_number "$L2_RPC")
    wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true
}

# Helper: call DualCaller on L1 and wait
call_dualcaller() {
    local label="$1"
    DUAL_CALLDATA=$(cast calldata "readAndIncrement(address,address)" "$L2_STORAGE_PROXY_L1" "$L1_COUNTER")
    RESULT=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
        "$L1_DUAL" "$DUAL_CALLDATA" --gas-limit 3000000 --json 2>&1 || echo "{}")
    TX_STATUS=$(echo "$RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
    echo "  $label: L1 tx status=$TX_STATUS"
    wait_for_pending_zero 90 >/dev/null || true
    L2_BLK=$(get_block_number "$L2_RPC")
    wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true
}

get_state() {
    COUNTER_VAL=$(cast call --rpc-url "$L1_RPC" "$L1_COUNTER" "counter()(uint256)" 2>/dev/null || echo "?")
    STORAGE_VAL=$(cast call --rpc-url "$L2_RPC" "$L2_STORAGE" "value()(uint256)" 2>/dev/null || echo "?")
    echo "  State: L1 Counter=$COUNTER_VAL, L2 Storage=$STORAGE_VAL"
}

# ══════════════════════════════════════════
#  STEP 3: Orchestrator (0->1, odd, OK)
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 3: Orchestrator (0->1, no revert)"
echo "========================================"
start_timer

call_orchestrator "Orchestrator 0->1"
get_state
assert "STEP3: L1 Counter = 1" '[ "$COUNTER_VAL" = "1" ]' "got=$COUNTER_VAL"
assert "STEP3: L2 Storage = 1" '[ "$STORAGE_VAL" = "1" ]' "got=$STORAGE_VAL"

print_elapsed "STEP 3"
echo ""

# ══════════════════════════════════════════
#  STEP 4: DualCaller (reads 1, counter 1->2)
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 4: DualCaller reads(1) + increment(1->2)"
echo "========================================"
start_timer

call_dualcaller "DualCaller"
get_state
assert "STEP4: L1 Counter = 2" '[ "$COUNTER_VAL" = "2" ]' "got=$COUNTER_VAL"
assert "STEP4: L2 Storage = 1 (unchanged)" '[ "$STORAGE_VAL" = "1" ]' "got=$STORAGE_VAL"

print_elapsed "STEP 4"
echo ""

# ══════════════════════════════════════════
#  STEP 5: Orchestrator (2->3, odd, OK)
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 5: Orchestrator (2->3, no revert)"
echo "========================================"
start_timer

call_orchestrator "Orchestrator 2->3"
get_state
assert "STEP5: L1 Counter = 3" '[ "$COUNTER_VAL" = "3" ]' "got=$COUNTER_VAL"
assert "STEP5: L2 Storage = 3" '[ "$STORAGE_VAL" = "3" ]' "got=$STORAGE_VAL"

print_elapsed "STEP 5"
echo ""

# ══════════════════════════════════════════
#  STEP 6 (KEY TEST): Orchestrator (3->4, even, REVERT!)
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 6 (KEY TEST): Orchestrator (3->4, REVERT)"
echo ""
echo "  L1 Counter is 3. Orchestrator increments to 4 (even)."
echo "  The L2 tx should revert, and BOTH L1 counter"
echo "  increment AND L2 storage write should roll back."
echo "========================================"
start_timer

call_orchestrator "Orchestrator 3->4 (should revert)"

# Poll to allow state to settle (give time for any delayed L1 trigger)
for _poll in $(seq 1 5); do
    COUNTER_VAL=$(cast call --rpc-url "$L1_RPC" "$L1_COUNTER" "counter()(uint256)" 2>/dev/null || echo "?")
    STORAGE_VAL=$(cast call --rpc-url "$L2_RPC" "$L2_STORAGE" "value()(uint256)" 2>/dev/null || echo "?")
    if [ "$COUNTER_VAL" = "3" ]; then break; fi
    sleep 6
done

get_state
echo ""
echo "  This is the KEY assertion — issue #272 regression test."
echo "  Before fix: L1 Counter = 4 (increment persists despite L2 revert)."
echo "  After  fix: L1 Counter = 3 (increment rolled back with L2 revert)."
echo ""
assert "STEP6: L1 Counter = 3 (rolled back)" '[ "$COUNTER_VAL" = "3" ]' \
    "got=$COUNTER_VAL expected=3"
assert "STEP6: L2 Storage = 3 (unchanged)" '[ "$STORAGE_VAL" = "3" ]' \
    "got=$STORAGE_VAL expected=3"

print_elapsed "STEP 6"
echo ""

# ══════════════════════════════════════════
#  STEP 7: DualCaller after revert (reads 3, counter 3->4)
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 7: DualCaller after revert"
echo "========================================"
start_timer

call_dualcaller "DualCaller reads(3) + increment(3->4)"
get_state
assert "STEP7: L1 Counter = 4" '[ "$COUNTER_VAL" = "4" ]' "got=$COUNTER_VAL"
assert "STEP7: L2 Storage = 3 (unchanged)" '[ "$STORAGE_VAL" = "3" ]' "got=$STORAGE_VAL"

print_elapsed "STEP 7"
echo ""

# ══════════════════════════════════════════
#  Health check
# ══════════════════════════════════════════

echo "========================================"
echo "  Health check"
echo "========================================"
start_timer

ROOTS=$(wait_for_convergence 60)
assert "State roots converge" '[ "$ROOTS" = "MATCH" ]'

HEALTH=$(get_health)
FINAL_MODE=$(echo "$HEALTH" | jq -r '.mode // "?"')
FINAL_REWINDS=$(echo "$HEALTH" | jq -r '.consecutive_rewind_cycles // "?"')
assert "Builder in Builder mode" '[ "$FINAL_MODE" = "Builder" ]'
assert "No rewind cycles" '[ "$FINAL_REWINDS" = "0" ]'

print_elapsed "Health check"
echo ""

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════

echo "========================================"
echo "  CROSS-CHAIN ATOMICITY TEST RESULTS"
echo "========================================"
echo ""
echo "  Counter (L1):       $L1_COUNTER"
echo "  SimpleStorage (L2): $L2_STORAGE"
echo "  Orchestrator (L2):  $L2_ORCH"
echo "  DualCaller (L1):    $L1_DUAL"
echo ""
echo "  Passed: $PASS_COUNT"
echo "  Failed: $FAIL_COUNT"
echo "  Total:  $TOTAL_COUNT"
echo ""
print_total_elapsed
echo ""

echo "Restarting crosschain-tx-sender..."
$DOCKER_COMPOSE_CMD start crosschain-tx-sender > /dev/null 2>&1 || true

if [ "$FAIL_COUNT" -eq 0 ]; then
  echo -e "  ${GREEN}STATUS: ALL TESTS PASSED${RESET}"
  exit 0
else
  echo -e "  ${RED}STATUS: $FAIL_COUNT TEST(S) FAILED${RESET}"
  exit 1
fi
