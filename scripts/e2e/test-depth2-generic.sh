#!/usr/bin/env bash
# test-depth2-generic.sh — E2E regression test for issue #245.
#
# Validates that depth-2 L2→L1→L2 cross-chain calls work with generic
# contracts (Logger.execute → target.call(payload)), not just purpose-built
# PingPong contracts.
#
# Pattern:
#   L2 Logger.execute(L1_Logger_proxy, inner_payload)
#     → L2→L1: L1 Logger.execute(L2_Counter_proxy, increment())
#       → L1→L2 return: L2 Counter.increment()
#
# Before the fix, the builder routed the 5-entry continuation L1 entries
# to the simple withdrawal path (pair-based), causing ExecutionNotFound
# on L1 during scope navigation.
#
# Test account: dev key #16 (HD mnemonic index 16)
#   Address:     0x2546BcD3c84621e976D8185a91A922aE77ECEc30
#   Private key: 0xea6c44ac03bff858b476bba40716402b03e41b8e97e276d1baec7c37d42484a0
#
# Usage: ./scripts/e2e/test-depth2-generic.sh [--json]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

TEST_KEY="0xea6c44ac03bff858b476bba40716402b03e41b8e97e276d1baec7c37d42484a0"
TEST_ADDR="0x2546BcD3c84621e976D8185a91A922aE77ECEc30"

# Counter bytecode: uint256 public counter; function increment() external returns (uint256)
# Compiled with solc 0.8.33, evm-version paris (no PUSH0).
COUNTER_BYTECODE="0x6080604052348015600f57600080fd5b5061017f8061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100365760003560e01c806361bc221a1461003b578063d09de08a14610059575b600080fd5b610043610077565b60405161005091906100b7565b60405180910390f35b61006161007d565b60405161006e91906100b7565b60405180910390f35b60005481565b600080600081548092919061009190610101565b9190505550600054905090565b6000819050919050565b6100b18161009e565b82525050565b60006020820190506100cc60008301846100a8565b92915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b600061010c8261009e565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff820361013e5761013d6100d2565b5b60018201905091905056fea26469706673582212203dcec02a2fe7260919dd7cb86d1128a36e74ee651874f6f0a26f8e688fd7407764736f6c63430008210033"

# ReturnDataLogger bytecode: execute(address,bytes) does target.call(payload), stores returnData.
LOGGER_BYTECODE="0x6080604052348015600f57600080fd5b506107c98061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100415760003560e01c80631cff79cd1461004657806326a2b98c146100765780639415bc5914610094575b600080fd5b610060600480360381019061005b91906102c9565b6100b2565b60405161006d91906103b9565b60405180910390f35b61007e61015b565b60405161008b91906103b9565b60405180910390f35b61009c6101e9565b6040516100a991906103f6565b60405180910390f35b60606000808573ffffffffffffffffffffffffffffffffffffffff1685856040516100de929190610450565b6000604051808303816000865af19150503d806000811461011b576040519150601f19603f3d011682016040523d82523d6000602084013e610120565b606091505b509150915081600160006101000a81548160ff021916908315150217905550806000908161014e91906106c1565b5080925050509392505050565b60008054610168906104c7565b80601f0160208091040260200160405190810160405280929190818152602001828054610194906104c7565b80156101e15780601f106101b6576101008083540402835291602001916101e1565b820191906000526020600020905b8154815290600101906020018083116101c457829003601f168201915b505050505081565b600160009054906101000a900460ff1681565b600080fd5b600080fd5b600073ffffffffffffffffffffffffffffffffffffffff82169050919050565b600061023182610206565b9050919050565b61024181610226565b811461024c57600080fd5b50565b60008135905061025e81610238565b92915050565b600080fd5b600080fd5b600080fd5b60008083601f84011261028957610288610264565b5b8235905067ffffffffffffffff8111156102a6576102a5610269565b5b6020830191508360018202830111156102c2576102c161026e565b5b9250929050565b6000806000604084860312156102e2576102e16101fc565b5b60006102f08682870161024f565b935050602084013567ffffffffffffffff81111561031157610310610201565b5b61031d86828701610273565b92509250509250925092565b600081519050919050565b600082825260208201905092915050565b60005b83811015610363578082015181840152602081019050610348565b60008484015250505050565b6000601f19601f8301169050919050565b600061038b82610329565b6103958185610334565b93506103a5818560208601610345565b6103ae8161036f565b840191505092915050565b600060208201905081810360008301526103d38184610380565b905092915050565b60008115159050919050565b6103f0816103db565b82525050565b600060208201905061040b60008301846103e7565b92915050565b600081905092915050565b82818337600083830152505050565b60006104378385610411565b935061044483858461041c565b82840190509392505050565b600061045d82848661042b565b91508190509392505050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052604160045260246000fd5b7f4e487b7100000000000000000000000000000000000000000000000000000000600052602260045260246000fd5b600060028204905060018216806104df57607f821691505b6020821081036104f2576104f1610498565b5b50919050565b60008190508160005260206000209050919050565b60006020601f8301049050919050565b600082821b905092915050565b60006008830261055a7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff8261051d565b610564868361051d565b95508019841693508086168417925050509392505050565b6000819050919050565b6000819050919050565b60006105ab6105a66105a18461057c565b610586565b61057c565b9050919050565b6000819050919050565b6105c583610590565b6105d96105d1826105b2565b84845461052a565b825550505050565b600090565b6105ee6105e1565b6105f98184846105bc565b505050565b60005b828110156106215761061660008284016105e6565b600181019050610601565b505050565b601f821115610675578282111561067457610640816104f8565b6106498361050d565b6106528561050d565b602086101561066057600090505b80830161066f828403826105fe565b505050505b5b505050565b600082821c905092915050565b60006106986000198460080261067a565b1980831691505092915050565b60006106b18383610687565b9150826002028217905092915050565b6106ca82610329565b67ffffffffffffffff8111156106e3576106e2610469565b5b6106ed82546104c7565b6106f8828285610626565b600060209050601f83116001811461072b5760008415610719578287015190505b61072385826106a5565b86555061078b565b601f198416610739866104f8565b60005b828110156107615784890151825560018201915060208501945060208101905061073c565b8683101561077e578489015161077a601f891682610687565b8355505b6001600288020188555050505b50505050505056fea2646970667358221220a904bd1c3bec96a31e91a7a303b9b5dfcb9a9185f65c68d188e1fe7090c77dd164736f6c63430008210033"

# ── Colors ──

if [ -t 1 ]; then
  CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'
  YELLOW='\033[1;33m'; BOLD='\033[1m'; RESET='\033[0m'
else
  CYAN=''; GREEN=''; RED=''; YELLOW=''; BOLD=''; DIM=''; RESET=''
fi

# ── Load rollup.env ──

echo ""
echo -e "${CYAN}========================================"
echo -e "  DEPTH-2 GENERIC TEST (issue #245)"
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

# ── Stop crosschain-tx-sender to avoid §4f interference ──
# Depth-2 continuation entries share L1 blocks with the crosschain-tx-sender's
# entries. If the tx-sender's trigger reverts, §4f filters ALL entries in that
# block — including ours. Stopping it ensures our entries land cleanly.
echo "Stopping crosschain-tx-sender (avoids §4f interference)..."
$DOCKER_COMPOSE_CMD stop crosschain-tx-sender > /dev/null 2>&1 || true
# Wait for pending submissions to clear after stopping
wait_for_pending_zero 30 >/dev/null || true

# ── Fund test account on L1 (keys #10+ not pre-funded by reth --dev) ──
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
if [ "$L1_BAL" = "0" ] || [ "$L1_BAL" = "0x0" ]; then
    echo "Funding $TEST_ADDR on L1 with 100 ETH..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$TEST_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
fi

# ── Bridge ETH to L2 ──
L2_BAL=$(cast balance --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
MIN_BAL=50000000000000000
if [ "$(printf '%d' "$L2_BAL" 2>/dev/null || echo 0)" -lt "$MIN_BAL" ] 2>/dev/null; then
    echo "Bridging 0.5 ETH to L2..."
    DEPOSIT_STATUS=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
        "$BRIDGE_ADDR" "bridgeEther(uint256,address)" "$ROLLUP_ID" "$TEST_ADDR" \
        --value 0.5ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
    assert "Bridge deposit succeeded" '[ "$DEPOSIT_STATUS" = "1" ]'
    echo "Waiting for deposit on L2..."
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
echo "  STEP 1: Deploy Counter (L2) + Logger (L1 & L2)"
echo "========================================"
start_timer

# Deploy Counter on L2
echo "Deploying Counter on L2..."
C_L2_RESULT=$(cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
    --create "$COUNTER_BYTECODE" --json 2>&1 || echo "{}")
C_L2=$(echo "$C_L2_RESULT" | grep -oP '"contractAddress"\s*:\s*"\K[^"]+' || echo "")
C_L2_STATUS=$(echo "$C_L2_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
echo "  Counter L2: $C_L2 (status=$C_L2_STATUS)"
assert "STEP1: Counter L2 deployed" '[ "$C_L2_STATUS" = "0x1" ] && [ -n "$C_L2" ]'

# Deploy Logger on L1
echo "Deploying Logger on L1..."
L_L1_RESULT=$(cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
    --create "$LOGGER_BYTECODE" --json 2>&1 || echo "{}")
L_L1=$(echo "$L_L1_RESULT" | grep -oP '"contractAddress"\s*:\s*"\K[^"]+' || echo "")
L_L1_STATUS=$(echo "$L_L1_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
echo "  Logger L1: $L_L1 (status=$L_L1_STATUS)"
assert "STEP1: Logger L1 deployed" '[ "$L_L1_STATUS" = "0x1" ] && [ -n "$L_L1" ]'

# Deploy Logger on L2
echo "Deploying Logger on L2..."
L_L2_RESULT=$(cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
    --create "$LOGGER_BYTECODE" --json 2>&1 || echo "{}")
L_L2=$(echo "$L_L2_RESULT" | grep -oP '"contractAddress"\s*:\s*"\K[^"]+' || echo "")
L_L2_STATUS=$(echo "$L_L2_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
echo "  Logger L2: $L_L2 (status=$L_L2_STATUS)"
assert "STEP1: Logger L2 deployed" '[ "$L_L2_STATUS" = "0x1" ] && [ -n "$L_L2" ]'

print_elapsed "STEP 1"
echo ""

# ══════════════════════════════════════════
#  STEP 2: Create cross-chain proxies
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 2: Create cross-chain proxies"
echo "========================================"
start_timer

# L1 Logger proxy on L2 (via CCM.createCrossChainProxy, rollupId=0 = L1)
echo "Creating L1 Logger proxy on L2..."
cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
    "$CCM_L2" "createCrossChainProxy(address,uint256)" "$L_L1" 0 \
    --gas-limit 500000 --json > /dev/null 2>&1
L1_LOGGER_PROXY_L2=$(cast call --rpc-url "$L2_RPC" \
    "$CCM_L2" "computeCrossChainProxyAddress(address,uint256)(address)" "$L_L1" 0 2>/dev/null || echo "")
L1_LOGGER_PROXY_CODE=$(cast code --rpc-url "$L2_RPC" "$L1_LOGGER_PROXY_L2" 2>/dev/null || echo "0x")
echo "  L1 Logger proxy on L2: $L1_LOGGER_PROXY_L2"
assert "STEP2: L1 Logger proxy on L2 has code" '[ "$L1_LOGGER_PROXY_CODE" != "0x" ]'

# L2 Counter proxy on L1 (via Rollups.createCrossChainProxy, rollupId=1 = L2)
echo "Creating L2 Counter proxy on L1..."
cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
    "$ROLLUPS_ADDRESS" "createCrossChainProxy(address,uint256)" "$C_L2" "$ROLLUP_ID" \
    --gas-limit 500000 --json > /dev/null 2>&1
L2_COUNTER_PROXY_L1=$(cast call --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" "$C_L2" "$ROLLUP_ID" 2>/dev/null || echo "")
L2_COUNTER_PROXY_CODE=$(cast code --rpc-url "$L1_RPC" "$L2_COUNTER_PROXY_L1" 2>/dev/null || echo "0x")
echo "  L2 Counter proxy on L1: $L2_COUNTER_PROXY_L1"
assert "STEP2: L2 Counter proxy on L1 has code" '[ "$L2_COUNTER_PROXY_CODE" != "0x" ]'

print_elapsed "STEP 2"
echo ""

# ══════════════════════════════════════════
#  STEP 3 (KEY TEST): Depth-2 L2→L1→L2 call
# ══════════════════════════════════════════
#
#   L2 Logger.execute(L1_Logger_proxy_L2, inner_payload)
#     → L2→L1: L1 Logger.execute(L2_Counter_proxy_L1, increment())
#       → L1→L2 return: L2 Counter.increment()

echo "========================================"
echo "  STEP 3 (KEY TEST): Depth-2 L2→L1→L2"
echo "========================================"
start_timer

# Build inner payload: Logger.execute(L2_Counter_proxy_L1, increment())
INNER=$(cast calldata "execute(address,bytes)" "$L2_COUNTER_PROXY_L1" "0xd09de08a")
echo "Inner: Logger_L1.execute($L2_COUNTER_PROXY_L1, increment())"
echo "Inner calldata: ${INNER:0:20}..."

# Get pre-state
COUNTER_BEFORE=$(cast call --rpc-url "$L2_RPC" "$C_L2" "counter()(uint256)" 2>/dev/null || echo "0")
echo "Counter L2 before: $COUNTER_BEFORE"

wait_for_pending_zero 30 >/dev/null || true

echo ""
echo "Sending depth-2 call via L2 proxy..."
RESULT=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY" \
    "$L_L2" "execute(address,bytes)" "$L1_LOGGER_PROXY_L2" "$INNER" \
    --gas-limit 2000000 --json 2>&1 || echo "{}")
TX_HASH=$(echo "$RESULT" | grep -oP '"transactionHash"\s*:\s*"\K[^"]+' || echo "")
TX_STATUS=$(echo "$RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")

echo "L2 tx hash:   ${TX_HASH:-<none>}"
echo "L2 tx status: ${TX_STATUS:-<none>}"
assert "STEP3: L2 tx succeeded" '[ "$TX_STATUS" = "0x1" ]'

# Wait for L1 triggers to execute and state to converge.
# The depth-2 pattern requires: postBatch → trigger → derivation → protocol txs on L2.
# This can take several L1 blocks (12s each), especially if the builder is recovering
# from rewinds caused by other tests running before this one.
echo ""
echo "Waiting for L1 triggers + convergence (up to 120s)..."
wait_for_pending_zero 90 >/dev/null || true
L2_BLK=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true
wait_for_pending_zero 60 >/dev/null || true

# Poll for Counter increment (derivation may need extra blocks after trigger lands)
EXPECTED_COUNTER=$((COUNTER_BEFORE + 1))
COUNTER_AFTER="$COUNTER_BEFORE"
for _poll in $(seq 1 10); do
    COUNTER_AFTER=$(cast call --rpc-url "$L2_RPC" "$C_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    if [ "$COUNTER_AFTER" = "$EXPECTED_COUNTER" ]; then break; fi
    sleep 6
done

print_elapsed "STEP 3"
echo ""

# ══════════════════════════════════════════
#  STEP 4: Verify depth-2 execution
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 4: Verify depth-2 execution"
echo "========================================"
start_timer
echo "Counter L2 after: $COUNTER_AFTER (expected: $EXPECTED_COUNTER)"
echo ""
echo "  This is the KEY assertion — issue #245 regression test."
echo "  Before fix: Counter stays at $COUNTER_BEFORE (inner L1→L2 call fails)."
echo "  After  fix: Counter increments to $EXPECTED_COUNTER."
echo ""
assert "STEP4: L2 Counter incremented (depth-2 L2→L1→L2 worked)" \
    '[ "$COUNTER_AFTER" = "$EXPECTED_COUNTER" ]' \
    "got=$COUNTER_AFTER expected=$EXPECTED_COUNTER"

# Check Logger state
L2_LOGGER_SUCCESS=$(cast call --rpc-url "$L2_RPC" "$L_L2" "lastSuccess()(bool)" 2>/dev/null || echo "?")
echo "L2 Logger.lastSuccess: $L2_LOGGER_SUCCESS"
assert "STEP4: L2 Logger reports success" '[ "$L2_LOGGER_SUCCESS" = "true" ]'

# Check return data is not ExecutionNotFound (0xed6bc750)
L2_LOGGER_DATA=$(cast call --rpc-url "$L2_RPC" "$L_L2" "lastReturnData()(bytes)" 2>/dev/null || echo "")
echo "L2 Logger.lastReturnData: ${L2_LOGGER_DATA:0:42}..."
# Check return data doesn't contain ExecutionNotFound (0xed6bc750) or InvalidRevertData (0xd4bae993)
HAS_ERROR=0
if echo "$L2_LOGGER_DATA" | grep -q "ed6bc750"; then HAS_ERROR=1; fi
if echo "$L2_LOGGER_DATA" | grep -q "d4bae993"; then HAS_ERROR=1; fi
assert "STEP4: Return data has no error selectors" '[ "$HAS_ERROR" = "0" ]' \
    "data=${L2_LOGGER_DATA:0:42}"

# Issue #246: verify return data is non-empty (delivery_return_data propagated).
# L1 Logger.execute wraps Counter.increment() return → bytes with actual data.
# Previously this was always empty (result_void nextAction).
DATA_LEN=${#L2_LOGGER_DATA}
echo "L2 Logger.lastReturnData length: $DATA_LEN chars"
assert "STEP4: Return data is non-empty (#246)" '[ "$DATA_LEN" -gt 2 ]' \
    "len=$DATA_LEN data=${L2_LOGGER_DATA:0:42}"

# Issue #246: verify return data contains Counter's actual return value.
# Logger.execute() returns raw inner call bytes → on L2 the scope resolution
# propagates this through. The return data should be 96 bytes (194 chars with 0x):
# abi.encode(bytes(abi.encode(uint256(EXPECTED_COUNTER))))
# = offset(32) + length(32) + data(32) = 96 bytes
assert "STEP4: Return data is 96 bytes (real data, not void)" '[ "$DATA_LEN" -eq 194 ]' \
    "len=$DATA_LEN expected=194"

# Decode the inner uint256 from the return data.
# Logger wraps: abi.encode(bytes(abi.encode(uint256(N))))
# bytes layout: 0x + offset(64hex) + length(64hex) + data(64hex)
# The uint256 is in the last 64 hex chars of the data
INNER_HEX="${L2_LOGGER_DATA: -64}"
INNER_VAL=$((16#${INNER_HEX}))
echo "Decoded inner return value: $INNER_VAL (expected: $EXPECTED_COUNTER)"
assert "STEP4: Inner return value matches Counter (#246)" \
    '[ "$INNER_VAL" -eq "$EXPECTED_COUNTER" ]' \
    "got=$INNER_VAL expected=$EXPECTED_COUNTER"

print_elapsed "STEP 4"
echo ""

# ══════════════════════════════════════════
#  STEP 5: Health check + convergence
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 5: Health check + convergence"
echo "========================================"
start_timer

ROOTS=$(wait_for_convergence 60)
assert "STEP5: State roots converge" '[ "$ROOTS" = "MATCH" ]'

HEALTH=$(get_health)
FINAL_MODE=$(echo "$HEALTH" | jq -r '.mode // "?"')
FINAL_REWINDS=$(echo "$HEALTH" | jq -r '.consecutive_rewind_cycles // "?"')
FINAL_PENDING=$(echo "$HEALTH" | jq -r '.pending_submissions // "?"')
assert "STEP5: Builder in Builder mode" '[ "$FINAL_MODE" = "Builder" ]'
assert "STEP5: No rewind cycles" '[ "$FINAL_REWINDS" = "0" ]'
assert "STEP5: No pending submissions" '[ "$FINAL_PENDING" = "0" ]'

print_elapsed "STEP 5"
echo ""

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════

echo "========================================"
echo "  DEPTH-2 GENERIC TEST RESULTS"
echo "========================================"
echo ""
echo "  Counter L2:             $C_L2"
echo "  Logger L1:              $L_L1"
echo "  Logger L2:              $L_L2"
echo "  L1 Logger proxy (L2):   $L1_LOGGER_PROXY_L2"
echo "  L2 Counter proxy (L1):  $L2_COUNTER_PROXY_L1"
echo ""
echo "  Counter before: $COUNTER_BEFORE → after: $COUNTER_AFTER"
echo ""
echo "  Passed: $PASS_COUNT"
echo "  Failed: $FAIL_COUNT"
echo "  Total:  $TOTAL_COUNT"
echo ""
print_total_elapsed
echo ""

# Restart crosschain-tx-sender (stopped during pre-flight)
echo "Restarting crosschain-tx-sender..."
$DOCKER_COMPOSE_CMD start crosschain-tx-sender > /dev/null 2>&1 || true

if [ "$FAIL_COUNT" -eq 0 ]; then
  echo -e "  ${GREEN}STATUS: ALL TESTS PASSED${RESET}"
  exit 0
else
  echo -e "  ${RED}STATUS: $FAIL_COUNT TEST(S) FAILED${RESET}"
  exit 1
fi
