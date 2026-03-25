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

# Bytecodes (solc 0.8.28)
# Counter: uint256 public counter; function increment() external returns (uint256)
COUNTER_BYTECODE="0x6080604052348015600f57600080fd5b5061017f8061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100365760003560e01c806361bc221a1461003b578063d09de08a14610059575b600080fd5b610043610077565b60405161005091906100b7565b60405180910390f35b61006161007d565b60405161006e91906100b7565b60405180910390f35b60005481565b600080600081548092919061009190610101565b9190505550600054905090565b6000819050919050565b6100b18161009e565b82525050565b60006020820190506100cc60008301846100a8565b92915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b600061010c8261009e565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff820361013e5761013d6100d2565b5b60018201905091905056fea26469706673582212203dcec02a2fe7260919dd7cb86d1128a36e74ee651874f6f0a26f8e688fd7407764736f6c63430008210033"

# SimpleStorage: uint256 public value; function store(uint256 v) external
STORAGE_BYTECODE="0x$(cat <<'HEXEOF'
6080604052348015600e575f5ffd5b5060f580601a5f395ff3fe6080604052348015600e575f5ffd5b50600436106030575f3560e01c80633fa4f24514603457806360fe47b114604e575b5f5ffd5b603a6066565b60405160459190608e565b60405180910390f35b606460048036038101906060919060c8565b606b565b005b5f5481565b805f8190555050565b5f819050919050565b6088816078565b82525050565b5f60208201905060a15f8301846081565b92915050565b5f5ffd5b60b7816078565b811460c0575f5ffd5b50565b5f8135905060c28160b0565b92915050565b5f6020828403121560da5760d960a7565b5b5f60e68482850160b5565b9150509291505056fea264697066735822122035eda58f11ead6b03e6ffeffd2d458fd9b4107b8c1b9a400e19b40e7b3bbe27f64736f6c634300081c0033
HEXEOF
)"

# Orchestrator: calls L1 counter proxy, stores result, reverts if even
ORCHESTRATOR_BYTECODE="0x$(cat <<'HEXEOF'
6080604052348015600e575f5ffd5b5061058e8061001c5f395ff3fe608060405234801561000f575f5ffd5b5060043610610029575f3560e01c8063c19e5ac41461002d575b5f5ffd5b610047600480360381019061004291906102e1565b61005e565b604051610055919061032e565b60405180910390f35b5f5f5f8473ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527fd09de08a000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff8381831617835250505050604051610109919061039e565b5f604051808303815f865af19150503d805f8114610142576040519150601f19603f3d011682016040523d82523d5f602084013e610147565b606091505b50915091508161018c576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161018390610412565b60405180910390fd5b808060200190518101906101a0919061045a565b92508373ffffffffffffffffffffffffffffffffffffffff166360fe47b1856040518263ffffffff1660e01b81526004016101da919061032e565b5f604051808303815f87803b1580156101f1575f5ffd5b505af1158015610203573d5f5f3e3d5ffd5b5050505060028461021491906104b2565b5f1461024f576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016102469061053d565b60405180910390fd5b5050919050565b5f5ffd5b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f610283826102595b9050919050565b61029381610279565b811461029d575f5ffd5b50565b5f813590506102ae8161028a565b92915050565b5f5f604083850312156102ca576102c9610256565b5b5f6102d7858286016102a0565b92505060206102e8858286016102a0565b9150509250929050565b5f819050919050565b610304816102f2565b82525050565b5f60208201905061031d5f8301846102fb565b92915050565b5f81519050919050565b5f81905092915050565b8281835e5f83830152505050565b5f61034f82610323565b610359818561032d565b9350610369818560208601610337565b80840191505092915050565b5f6103808284610345565b915081905092915050565b5f82825260208201905092915050565b7f4c3120636f756e7465722063616c6c206661696c6564000000000000000000005f82015250565b5f6103ce60168361038b565b91506103d98261039b565b602082019050919050565b5f6020820190508181035f8301526103fb816103c2565b9050919050565b610410816102f2565b811461041a575f5ffd5b50565b5f8151905061042b81610407565b92915050565b5f6020828403121561044657610445610256565b5b5f6104538482850161041d565b91505092915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601260045260245ffd5b5f6104928261027c565b915061049d8361027c565b9250826104ad576104ac61045c565b5b828206905092915050565b7f726573756c7420697320657665e2c2c2207265766572746963e67661006000005f82015250565b5f6105276019836104b8565b9150610532826104b8565b602082019050919050565b5f6020820190508181035f830152610554816104fb565b905091905056fea2646970667358221220a1b2c3d4e5f60718293a4b5c6d7e8f9001122334455667788990aab1bccddeef64736f6c634300081c0033
HEXEOF
)"

# DualCaller: reads L2 storage proxy + increments L1 counter
DUALCALLER_BYTECODE="0x$(cat <<'HEXEOF'
6080604052348015600e575f5ffd5b5061058e8061001c5f395ff3fe608060405234801561000f575f5ffd5b5060043610610029575f3560e01c80633dbe66c81461002d575b5f5ffd5b610047600480360381019061004291906102e1565b61005e565b604051610055929190610328565b60405180910390f35b5f5f5f5f8573ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527f3fa4f245000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff83818316178352505050506040516101099190610399565b5f604051808303815f865af19150503d805f8114610142576040519150601f19603f3d011682016040523d82523d5f602084013e610147565b606091505b50915091508161018c576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161018390610409565b60405180910390fd5b808060200190518101906101a09190610451565b93505f5f8673ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527fd09de08a000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff838183161783525050505060405161024b9190610399565b5f604051808303815f865af19150503d805f8114610284576040519150601f19603f3d011682016040523d82523d5f602084013e610289565b606091505b5091509150816102ce576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016102c5906104c6565b60405180910390fd5b808060200190518101906102e29190610451565b9450505050509250929050565b5f5ffd5b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f61031c826102f3565b9050919050565b61032c81610312565b82525050565b5f819050919050565b61034481610332565b82525050565b5f60408201905061035d5f830185610323565b61036a602083018461033b565b9392505050565b5f81519050919050565b5f81905092915050565b8281835e5f83830152505050565b5f61039d82610371565b6103a7818561037b565b93506103b7818560208601610385565b80840191505092915050565b5f6103ce8284610393565b915081905092915050565b5f82825260208201905092915050565b7f726561642066726f6d204c322073746f72616765206661696c656400000000005f82015250565b5f61041d601b836103d9565b9150610428826103e9565b602082019050919050565b5f6020820190508181035f83015261044a81610411565b9050919050565b5f6020828403121561046657610465610256565b5b5f61047384828501610288565b91505092915050565b7f4c3120636f756e74657220696e6372656d656e74206661696c656400000000005f82015250565b5f6104b0601b836103d9565b91506104bb8261047c565b602082019050919050565b5f6020820190508181035f8301526104dd816104a4565b905091905056
HEXEOF
)"

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

deploy_contract "$L1_RPC" "$COUNTER_BYTECODE" "Counter (L1)" L1_COUNTER
deploy_contract "$L2_RPC" "$STORAGE_BYTECODE" "SimpleStorage (L2)" L2_STORAGE
deploy_contract "$L2_RPC" "$ORCHESTRATOR_BYTECODE" "Orchestrator (L2)" L2_ORCH
deploy_contract "$L1_RPC" "$DUALCALLER_BYTECODE" "DualCaller (L1)" L1_DUAL

# Verify on Blockscout
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SRC_DIR="$REPO_ROOT/contracts/test-multi-call/src"
if [ -n "$L1_EXPLORER" ] && [ -d "$SRC_DIR" ]; then
    verify_on_blockscout "$L1_EXPLORER" "$L1_COUNTER" "Counter" "$SRC_DIR/Counter.sol" || true
    verify_on_blockscout "$L2_EXPLORER" "$L2_STORAGE" "SimpleStorage" "$SRC_DIR/SimpleStorage.sol" || true
    verify_on_blockscout "$L2_EXPLORER" "$L2_ORCH" "Orchestrator" "$SRC_DIR/Orchestrator.sol" || true
    verify_on_blockscout "$L1_EXPLORER" "$L1_DUAL" "DualCaller" "$SRC_DIR/DualCaller.sol" || true
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
