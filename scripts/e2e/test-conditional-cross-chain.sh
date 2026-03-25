#!/usr/bin/env bash
# test-conditional-cross-chain.sh — E2E regression test for cross-chain atomicity.
#
# Tests that when an L1 contract makes two cross-chain calls to L2 and then
# conditionally reverts, the L2 state changes are properly rolled back.
#
# Contract: ConditionalCallTwice
#   callBothConditional(counterA, counterB, revertThreshold)
#     1. Calls counterA.increment() on L2 (cross-chain)
#     2. Calls counterB.increment() on L2 (cross-chain)
#     3. If counterB's return value >= revertThreshold, reverts everything
#
# Test cases:
#   TEST A (no revert): threshold=100 → both counters should increment
#   TEST B (revert):    threshold=1   → counterB returns 1, triggers revert
#                        → both L2 counters should be rolled back to pre-call values
#
# This tests two things:
#   1. Multiple cross-chain calls from one L1 execution (issue #256)
#   2. Cross-chain atomicity: L2 state changes revert when L1 tx reverts
#
# Test account: dev key #18 (HD mnemonic index 18)
#   Address:     0xdD2FD4581271e230360230F9337D5c0430Bf44C0
#   Private key: 0xde9be858da4a475276426320d5e9262ecfc3ba460bfac56360bfa6c4c28b4ee0
#
# Usage: ./scripts/e2e/test-conditional-cross-chain.sh [--json]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

TEST_KEY="0xde9be858da4a475276426320d5e9262ecfc3ba460bfac56360bfa6c4c28b4ee0"
TEST_ADDR="0xdD2FD4581271e230360230F9337D5c0430Bf44C0"

# Counter bytecode (same as other tests)
COUNTER_BYTECODE="0x6080604052348015600f57600080fd5b5061017f8061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100365760003560e01c806361bc221a1461003b578063d09de08a14610059575b600080fd5b610043610077565b60405161005091906100b7565b60405180910390f35b61006161007d565b60405161006e91906100b7565b60405180910390f35b60005481565b600080600081548092919061009190610101565b9190505550600054905090565b6000819050919050565b6100b18161009e565b82525050565b60006020820190506100cc60008301846100a8565b92915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b600061010c8261009e565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff820361013e5761013d6100d2565b5b60018201905091905056fea26469706673582212203dcec02a2fe7260919dd7cb86d1128a36e74ee651874f6f0a26f8e688fd7407764736f6c63430008210033"

# ConditionalCallTwice bytecode
# Source: contracts/test-multi-call/src/ConditionalCallTwice.sol, solc 0.8.28
CONDITIONAL_BYTECODE="0x6080604052348015600e575f5ffd5b506106948061001c5f395ff3fe608060405234801561000f575f5ffd5b5060043610610029575f3560e01c806332cf97791461002d575b5f5ffd5b610047600480360381019061004291906103c3565b61005e565b604051610055929190610422565b60405180910390f35b5f5f5f5f8673ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527fd09de08a000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff8381831617835250505050604051610109919061049b565b5f604051808303815f865af19150503d805f8114610142576040519150601f19603f3d011682016040523d82523d5f602084013e610147565b606091505b50915091508161018c576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016101839061050b565b60405180910390fd5b808060200190518101906101a0919061053d565b93505f5f8773ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527fd09de08a000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff838183161783525050505060405161024b919061049b565b5f604051808303815f865af19150503d805f8114610284576040519150601f19603f3d011682016040523d82523d5f602084013e610289565b606091505b5091509150816102ce576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016102c5906105b2565b60405180910390fd5b808060200190518101906102e2919061053d565b9450868510610326576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161031d90610640565b60405180910390fd5b50505050935093915050565b5f5ffd5b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f61035f82610336565b9050919050565b61036f81610355565b8114610379575f5ffd5b50565b5f8135905061038a81610366565b92915050565b5f819050919050565b6103a281610390565b81146103ac575f5ffd5b50565b5f813590506103bd81610399565b92915050565b5f5f5f606084860312156103da576103d9610332565b5b5f6103e78682870161037c565b93505060206103f88682870161037c565b9250506040610409868287016103af565b9150509250925092565b61041c81610390565b82525050565b5f6040820190506104355f830185610413565b6104426020830184610413565b9392505050565b5f81519050919050565b5f81905092915050565b8281835e5f83830152505050565b5f61047582610449565b61047f8185610453565b935061048f81856020860161045d565b80840191505092915050565b5f6104a6828461046b565b915081905092915050565b5f82825260208201905092915050565b7f66697273742063616c6c206661696c65640000000000000000000000000000005f82015250565b5f6104f56011836104b1565b9150610500826104c1565b602082019050919050565b5f6020820190508181035f830152610522816104e9565b9050919050565b5f8151905061053781610399565b92915050565b5f6020828403121561055257610551610332565b5b5f61055f84828501610529565b91505092915050565b7f7365636f6e642063616c6c206661696c656400000000000000000000000000005f82015250565b5f61059c6012836104b1565b91506105a782610568565b602082019050919050565b5f6020820190508181035f8301526105c981610590565b9050919050565b7f636f6e646974696f6e616c207265766572743a20636f756e74657242203e3d205f8201527f7468726573686f6c640000000000000000000000000000000000000000000000602082015250565b5f61062a6029836104b1565b9150610635826105d0565b604082019050919050565b5f6020820190508181035f8301526106578161061e565b905091905056fea26469706673582212207485a39d19d5b68b23e23d86b5a758bd9933685f27947f49556ae302fdb21e1064736f6c634300081c0033"

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
echo -e "  CONDITIONAL CROSS-CHAIN TEST (#256)"
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

echo "Stopping crosschain-tx-sender (avoids §4f interference)..."
$DOCKER_COMPOSE_CMD stop crosschain-tx-sender > /dev/null 2>&1 || true
wait_for_pending_zero 30 >/dev/null || true

FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
if [ "$L1_BAL" = "0" ] || [ "$L1_BAL" = "0x0" ]; then
    echo "Funding $TEST_ADDR on L1 with 100 ETH..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$TEST_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
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

deploy_contract() {
    local rpc="$1" bytecode="$2" label="$3" var="$4"
    local result addr status
    result=$(cast send --rpc-url "$rpc" --private-key "$TEST_KEY" \
        --create "$bytecode" --json 2>&1 || echo "{}")
    addr=$(echo "$result" | grep -oP '"contractAddress"\s*:\s*"\K[^"]+' || echo "")
    status=$(echo "$result" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
    echo "  $label: $addr (status=$status)"
    assert "STEP1: $label deployed" '[ "$status" = "0x1" ] && [ -n "$addr" ]'
    eval "${var}='$addr'"
}

deploy_contract "$L2_RPC" "$COUNTER_BYTECODE" "Counter A (L2)" C_A_L2
deploy_contract "$L2_RPC" "$COUNTER_BYTECODE" "Counter B (L2)" C_B_L2
deploy_contract "$L1_RPC" "$CONDITIONAL_BYTECODE" "ConditionalCallTwice (L1)" COND_L1

# Verify on Blockscout
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SRC_DIR="$REPO_ROOT/contracts/test-multi-call/src"
if [ -n "$L1_EXPLORER" ] && [ -d "$SRC_DIR" ]; then
    verify_on_blockscout "$L2_EXPLORER" "$C_A_L2" "Counter" "$SRC_DIR/Counter.sol" || true
    verify_on_blockscout "$L2_EXPLORER" "$C_B_L2" "Counter" "$SRC_DIR/Counter.sol" || true
    verify_on_blockscout "$L1_EXPLORER" "$COND_L1" "ConditionalCallTwice" "$SRC_DIR/ConditionalCallTwice.sol" || true
fi

print_elapsed "STEP 1"
echo ""

# ══════════════════════════════════════════
#  STEP 2: Create cross-chain proxies
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 2: Create cross-chain proxies"
echo "========================================"
start_timer

create_proxy_l1() {
    local l2_addr="$1" label="$2" var="$3"
    cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
        "$ROLLUPS_ADDRESS" "createCrossChainProxy(address,uint256)" "$l2_addr" "$ROLLUP_ID" \
        --gas-limit 500000 --json > /dev/null 2>&1
    local proxy
    proxy=$(cast call --rpc-url "$L1_RPC" \
        "$ROLLUPS_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" "$l2_addr" "$ROLLUP_ID" 2>/dev/null || echo "")
    echo "  $label: $proxy"
    local code
    code=$(cast code --rpc-url "$L1_RPC" "$proxy" 2>/dev/null || echo "0x")
    assert "STEP2: $label has code" '[ "$code" != "0x" ]'
    eval "${var}='$proxy'"
}

create_proxy_l1 "$C_A_L2" "Counter A proxy on L1" C_A_PROXY_L1
create_proxy_l1 "$C_B_L2" "Counter B proxy on L1" C_B_PROXY_L1

print_elapsed "STEP 2"
echo ""

# ══════════════════════════════════════════
#  TEST A: No revert (threshold=100)
# ══════════════════════════════════════════
#
# ConditionalCallTwice calls both counters. Counter B returns 1 which is < 100.
# No revert → both counters should increment.

echo "========================================"
echo "  TEST A: No revert (threshold=100)"
echo "  Both counters should increment"
echo "========================================"
start_timer

CA_BEFORE=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
CB_BEFORE=$(cast call --rpc-url "$L2_RPC" "$C_B_L2" "counter()(uint256)" 2>/dev/null || echo "0")
echo "Before: Counter A=$CA_BEFORE, Counter B=$CB_BEFORE"

wait_for_pending_zero 30 >/dev/null || true

CALLDATA_A=$(cast calldata "callBothConditional(address,address,uint256)" "$C_A_PROXY_L1" "$C_B_PROXY_L1" 100)
echo "Sending: ConditionalCallTwice(A_proxy, B_proxy, threshold=100)..."
RESULT_A=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
    "$COND_L1" "$CALLDATA_A" \
    --gas-limit 3000000 --json 2>&1 || echo "{}")
STATUS_A=$(echo "$RESULT_A" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
echo "L1 tx status: $STATUS_A"
assert "TEST_A: L1 tx succeeded" '[ "$STATUS_A" = "0x1" ]'

echo "Waiting for settlement..."
wait_for_pending_zero 90 >/dev/null || true
L2_BLK=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true

EXPECTED_CA=$((CA_BEFORE + 1))
EXPECTED_CB=$((CB_BEFORE + 1))
CA_AFTER="$CA_BEFORE"
CB_AFTER="$CB_BEFORE"
for _poll in $(seq 1 10); do
    CA_AFTER=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    CB_AFTER=$(cast call --rpc-url "$L2_RPC" "$C_B_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    if [ "$CA_AFTER" = "$EXPECTED_CA" ] && [ "$CB_AFTER" = "$EXPECTED_CB" ]; then break; fi
    sleep 6
done
echo "After: Counter A=$CA_AFTER (expected $EXPECTED_CA), Counter B=$CB_AFTER (expected $EXPECTED_CB)"
assert "TEST_A: Counter A incremented" '[ "$CA_AFTER" = "$EXPECTED_CA" ]' \
    "got=$CA_AFTER expected=$EXPECTED_CA"
assert "TEST_A: Counter B incremented" '[ "$CB_AFTER" = "$EXPECTED_CB" ]' \
    "got=$CB_AFTER expected=$EXPECTED_CB"

print_elapsed "TEST A"
echo ""

# ══════════════════════════════════════════
#  TEST B: Revert (threshold=1)
# ══════════════════════════════════════════
#
# ConditionalCallTwice calls both counters. Counter B's new value will be
# >= 1 (since it was already incremented in TEST A, or it's the first call).
# The require(b < 1) fails → L1 tx reverts.
# Expected: both L2 counters should NOT increment (rolled back).
#
# This tests cross-chain atomicity: do L2 state changes revert when the
# L1 execution that triggered them reverts?

echo "========================================"
echo "  TEST B: Revert (threshold=1)"
echo "  Both counters should be rolled back"
echo "========================================"
start_timer

CA_BEFORE_B=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
CB_BEFORE_B=$(cast call --rpc-url "$L2_RPC" "$C_B_L2" "counter()(uint256)" 2>/dev/null || echo "0")
echo "Before: Counter A=$CA_BEFORE_B, Counter B=$CB_BEFORE_B"

wait_for_pending_zero 30 >/dev/null || true

# threshold=1: Counter B returns its new value (>= 1), triggers revert
CALLDATA_B=$(cast calldata "callBothConditional(address,address,uint256)" "$C_A_PROXY_L1" "$C_B_PROXY_L1" 1)
echo "Sending: ConditionalCallTwice(A_proxy, B_proxy, threshold=1)..."
echo "  Expected: L1 tx reverts (counterB >= 1)"
RESULT_B=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
    "$COND_L1" "$CALLDATA_B" \
    --gas-limit 3000000 --json 2>&1 || echo "{}")
STATUS_B=$(echo "$RESULT_B" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
echo "L1 tx status: $STATUS_B"

# The L1 tx SHOULD revert (status=0x0). If the builder correctly detects
# the revert during simulation, it may not even submit the tx.
# Either way, the L2 counters should not change.

echo "Waiting for settlement..."
wait_for_pending_zero 90 >/dev/null || true
L2_BLK=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 60 >/dev/null || true

# Give time for any delayed state changes
sleep 12

CA_AFTER_B=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
CB_AFTER_B=$(cast call --rpc-url "$L2_RPC" "$C_B_L2" "counter()(uint256)" 2>/dev/null || echo "0")
echo "After: Counter A=$CA_AFTER_B (expected $CA_BEFORE_B), Counter B=$CB_AFTER_B (expected $CB_BEFORE_B)"

assert "TEST_B: Counter A unchanged (reverted)" '[ "$CA_AFTER_B" = "$CA_BEFORE_B" ]' \
    "got=$CA_AFTER_B expected=$CA_BEFORE_B"
assert "TEST_B: Counter B unchanged (reverted)" '[ "$CB_AFTER_B" = "$CB_BEFORE_B" ]' \
    "got=$CB_AFTER_B expected=$CB_BEFORE_B"

print_elapsed "TEST B"
echo ""

# ══════════════════════════════════════════
#  Health check + convergence
# ══════════════════════════════════════════

echo "========================================"
echo "  Health check + convergence"
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
echo "  CONDITIONAL CROSS-CHAIN TEST RESULTS"
echo "========================================"
echo ""
echo "  Counter A (L2):              $C_A_L2"
echo "  Counter B (L2):              $C_B_L2"
echo "  ConditionalCallTwice (L1):   $COND_L1"
echo "  Counter A proxy (L1):        $C_A_PROXY_L1"
echo "  Counter B proxy (L1):        $C_B_PROXY_L1"
echo ""
echo "  TEST A (no revert):  Counter A=$CA_AFTER, Counter B=$CB_AFTER"
echo "  TEST B (revert):     Counter A=$CA_AFTER_B, Counter B=$CB_AFTER_B"
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
