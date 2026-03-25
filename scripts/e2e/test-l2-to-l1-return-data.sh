#!/usr/bin/env bash
# test-l2-to-l1-return-data.sh — E2E regression test for issue #242.
#
# Validates that L2→L1 cross-chain calls propagate return data back to
# the L2 caller. Before the fix, the L2 RESULT entry had empty data,
# so callers always received 0x (silent data loss).
#
# Test flow:
#   1. Deploy Counter on L1 (increment() returns uint256)
#   2. Create L2 proxy for L1 Counter via CCM.createCrossChainProxy
#   3. Deploy ReturnDataLogger on L2 (calls target, stores returnData)
#   4. Call Logger.execute(L2CounterProxy, increment()) via L2 RPC proxy
#   5. Verify Logger.lastReturnData() == abi.encode(uint256(1))
#   6. Verify L1 Counter.counter() == 1
#   7. Final health check + state root convergence
#
# Test account: dev key #15 (HD mnemonic index 15)
#   Address:     0xcd3B766CCDd6AE721141F452C550Ca635964ce71
#   Private key: 0x8166f546bab6da521a8369cab06c5d2b9e46670292d85c875ee9ec20e84ffb61
#
# Usage: ./scripts/e2e/test-l2-to-l1-return-data.sh [--json]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

TEST_KEY="0x8166f546bab6da521a8369cab06c5d2b9e46670292d85c875ee9ec20e84ffb61"
TEST_ADDR="0xcd3B766CCDd6AE721141F452C550Ca635964ce71"

# Counter bytecode: uint256 public counter; function increment() external returns (uint256)
# Compiled with solc 0.8.33, evm-version paris (no PUSH0, works on both L1 and L2).
# NOTE: The crosschain-health-check.sh bytecode uses overlapping init code with PUSH0
# which can fail on reth --dev L1. This version has clean init/runtime separation.
COUNTER_BYTECODE="0x6080604052348015600f57600080fd5b5061017f8061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100365760003560e01c806361bc221a1461003b578063d09de08a14610059575b600080fd5b610043610077565b60405161005091906100b7565b60405180910390f35b61006161007d565b60405161006e91906100b7565b60405180910390f35b60005481565b600080600081548092919061009190610101565b9190505550600054905090565b6000819050919050565b6100b18161009e565b82525050565b60006020820190506100cc60008301846100a8565b92915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b600061010c8261009e565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff820361013e5761013d6100d2565b5b60018201905091905056fea26469706673582212203dcec02a2fe7260919dd7cb86d1128a36e74ee651874f6f0a26f8e688fd7407764736f6c63430008210033"

# ReturnDataLogger bytecode: execute(address,bytes) stores returnData on-chain.
# Compiled from ReturnDataLogger.sol with solc 0.8.33, evm-version paris.
#   function execute(address target, bytes calldata payload) returns (bytes memory)
#   bytes public lastReturnData   — 0x26a2b98c
#   bool public lastSuccess        — 0x9415bc59
LOGGER_BYTECODE="0x6080604052348015600f57600080fd5b506107c98061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100415760003560e01c80631cff79cd1461004657806326a2b98c146100765780639415bc5914610094575b600080fd5b610060600480360381019061005b91906102c9565b6100b2565b60405161006d91906103b9565b60405180910390f35b61007e61015b565b60405161008b91906103b9565b60405180910390f35b61009c6101e9565b6040516100a991906103f6565b60405180910390f35b60606000808573ffffffffffffffffffffffffffffffffffffffff1685856040516100de929190610450565b6000604051808303816000865af19150503d806000811461011b576040519150601f19603f3d011682016040523d82523d6000602084013e610120565b606091505b509150915081600160006101000a81548160ff021916908315150217905550806000908161014e91906106c1565b5080925050509392505050565b60008054610168906104c7565b80601f0160208091040260200160405190810160405280929190818152602001828054610194906104c7565b80156101e15780601f106101b6576101008083540402835291602001916101e1565b820191906000526020600020905b8154815290600101906020018083116101c457829003601f168201915b505050505081565b600160009054906101000a900460ff1681565b600080fd5b600080fd5b600073ffffffffffffffffffffffffffffffffffffffff82169050919050565b600061023182610206565b9050919050565b61024181610226565b811461024c57600080fd5b50565b60008135905061025e81610238565b92915050565b600080fd5b600080fd5b600080fd5b60008083601f84011261028957610288610264565b5b8235905067ffffffffffffffff8111156102a6576102a5610269565b5b6020830191508360018202830111156102c2576102c161026e565b5b9250929050565b6000806000604084860312156102e2576102e16101fc565b5b60006102f08682870161024f565b935050602084013567ffffffffffffffff81111561031157610310610201565b5b61031d86828701610273565b92509250509250925092565b600081519050919050565b600082825260208201905092915050565b60005b83811015610363578082015181840152602081019050610348565b60008484015250505050565b6000601f19601f8301169050919050565b600061038b82610329565b6103958185610334565b93506103a5818560208601610345565b6103ae8161036f565b840191505092915050565b600060208201905081810360008301526103d38184610380565b905092915050565b60008115159050919050565b6103f0816103db565b82525050565b600060208201905061040b60008301846103e7565b92915050565b600081905092915050565b82818337600083830152505050565b60006104378385610411565b935061044483858461041c565b82840190509392505050565b600061045d82848661042b565b91508190509392505050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052604160045260246000fd5b7f4e487b7100000000000000000000000000000000000000000000000000000000600052602260045260246000fd5b600060028204905060018216806104df57607f821691505b6020821081036104f2576104f1610498565b5b50919050565b60008190508160005260206000209050919050565b60006020601f8301049050919050565b600082821b905092915050565b60006008830261055a7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff8261051d565b610564868361051d565b95508019841693508086168417925050509392505050565b6000819050919050565b6000819050919050565b60006105ab6105a66105a18461057c565b610586565b61057c565b9050919050565b6000819050919050565b6105c583610590565b6105d96105d1826105b2565b84845461052a565b825550505050565b600090565b6105ee6105e1565b6105f98184846105bc565b505050565b60005b828110156106215761061660008284016105e6565b600181019050610601565b505050565b601f821115610675578282111561067457610640816104f8565b6106498361050d565b6106528561050d565b602086101561066057600090505b80830161066f828403826105fe565b505050505b5b505050565b600082821c905092915050565b60006106986000198460080261067a565b1980831691505092915050565b60006106b18383610687565b9150826002028217905092915050565b6106ca82610329565b67ffffffffffffffff8111156106e3576106e2610469565b5b6106ed82546104c7565b6106f8828285610626565b600060209050601f83116001811461072b5760008415610719578287015190505b61072385826106a5565b86555061078b565b601f198416610739866104f8565b60005b828110156107615784890151825560018201915060208501945060208101905061073c565b8683101561077e578489015161077a601f891682610687565b8355505b6001600288020188555050505b50505050505056fea2646970667358221220a904bd1c3bec96a31e91a7a303b9b5dfcb9a9185f65c68d188e1fe7090c77dd164736f6c63430008210033"

# ── Colors ──

if [ -t 1 ]; then
  CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'
  YELLOW='\033[1;33m'; BOLD='\033[1m'; DIM='\033[2m'; RESET='\033[0m'
else
  CYAN=''; GREEN=''; RED=''; YELLOW=''; BOLD=''; DIM=''; RESET=''
fi

# ── Load rollup.env ──

echo ""
echo -e "${CYAN}========================================"
echo -e "  L2->L1 RETURN DATA TEST (issue #242)"
echo -e "========================================${RESET}"
echo ""
echo "Loading rollup.env..."

eval "$($DOCKER_COMPOSE_CMD exec -T builder cat /shared/rollup.env 2>/dev/null)"
if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
  echo -e "${RED}ERROR: Could not load rollup.env${RESET}"
  exit 1
fi

CCM_L2_ADDRESS="${CROSS_CHAIN_MANAGER_ADDRESS:-}"
if [ -z "$CCM_L2_ADDRESS" ]; then
  echo -e "${RED}ERROR: CROSS_CHAIN_MANAGER_ADDRESS not set${RESET}"
  exit 1
fi

ROLLUP_ID="${ROLLUP_ID:-1}"
echo "ROLLUPS_ADDRESS=$ROLLUPS_ADDRESS"
echo "CCM_L2_ADDRESS=$CCM_L2_ADDRESS"
echo "ROLLUP_ID=$ROLLUP_ID"
echo "Test account: $TEST_ADDR"
echo ""

# ══════════════════════════════════════════
#  PRE-FLIGHT
# ══════════════════════════════════════════

echo "========================================"
echo "  PRE-FLIGHT"
echo "========================================"
start_timer

echo "Waiting for builder to be ready (up to 90s)..."
MODE=$(wait_for_builder_ready 90)
echo "Builder mode: $MODE"
assert "Builder is in Builder mode" '[ "$MODE" = "Builder" ]'

L1_BLOCK=$(get_block_number "$L1_RPC")
echo "L1 block: $(printf '%d' "$L1_BLOCK")"
assert "L1 producing blocks" '[ "$(printf "%d" "$L1_BLOCK")" -gt 0 ]'

L2_BLOCK=$(get_block_number "$L2_RPC")
echo "L2 block: $(printf '%d' "$L2_BLOCK")"
assert "L2 producing blocks" '[ "$(printf "%d" "$L2_BLOCK")" -gt 0 ]'

# ── Fund test account on L1 (keys #10+ are not pre-funded by reth --dev) ──
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L1_BAL_CHECK=$(cast balance --rpc-url "$L1_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
if [ "$L1_BAL_CHECK" = "0" ] || [ "$L1_BAL_CHECK" = "0x0" ]; then
    echo "Funding $TEST_ADDR on L1 with 100 ETH (dev#9 funder)..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$TEST_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
fi

print_elapsed "PRE-FLIGHT"
echo ""

# ══════════════════════════════════════════
#  STEP 1: Bridge ETH to L2 for test account
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 1: Bridge ETH to L2 (if needed)"
echo "========================================"
start_timer

BRIDGE_ADDR="${BRIDGE_L1_ADDRESS:-${BRIDGE_ADDRESS:-}}"
if [ -z "$BRIDGE_ADDR" ]; then
  echo -e "${RED}ERROR: BRIDGE_L1_ADDRESS not set${RESET}"
  exit 1
fi

L2_BAL_BEFORE=$(get_balance "$L2_RPC" "$TEST_ADDR")
MIN_BALANCE=50000000000000000  # 0.05 ETH

if [ "$(echo "$L2_BAL_BEFORE" | tr -d '[:space:]')" != "0" ] && \
   [ "$(printf '%d' "$L2_BAL_BEFORE" 2>/dev/null || echo 0)" -ge "$MIN_BALANCE" ] 2>/dev/null; then
  echo "Test account has sufficient L2 balance -- skipping deposit."
else
  echo "Bridging 0.5 ETH to L2..."
  DEPOSIT_RESULT=$(cast send \
    --rpc-url "$L1_PROXY" \
    --private-key "$TEST_KEY" \
    "$BRIDGE_ADDR" \
    "bridgeEther(uint256,address)" \
    "$ROLLUP_ID" "$TEST_ADDR" \
    --value 0.5ether \
    --gas-limit 800000 \
    2>&1 || true)
  DEPOSIT_STATUS=$(echo "$DEPOSIT_RESULT" | grep "^status" | awk '{print $2}' || echo "")
  echo "Bridge tx status: $DEPOSIT_STATUS"
  assert "STEP1: bridgeEther succeeded" '[ "$DEPOSIT_STATUS" = "1" ]'

  echo "Waiting for deposit on L2 (up to 60s)..."
  L2_BLK_BEFORE=$(get_block_number "$L2_RPC")
  wait_for_block_advance "$L2_RPC" "$L2_BLK_BEFORE" 3 60 >/dev/null || true
  wait_for_pending_zero 60 >/dev/null || true
fi

print_elapsed "STEP 1"
echo ""

# ══════════════════════════════════════════
#  STEP 2: Deploy Counter on L1
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 2: Deploy Counter on L1"
echo "========================================"
start_timer

L1_NONCE=$(cast nonce --rpc-url "$L1_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
echo "Test account L1 nonce: $L1_NONCE"

# Deploy at a predictable nonce. Key #9 already sent 4 funding txs + possibly
# 1 bridge tx in step 1. Use the current nonce for the deploy.
L1_COUNTER_ADDRESS=""

# Check if a Counter-like contract exists at the expected CREATE address.
PREDICTED_L1_COUNTER=$(cast compute-address "$TEST_ADDR" --nonce "$L1_NONCE" 2>/dev/null \
  | grep -oP '0x[0-9a-fA-F]{40}' || echo "")

# Try previous nonces in case Counter was already deployed in a prior run.
for CHECK_NONCE in $(seq "$L1_NONCE" -1 0 2>/dev/null | head -20); do
  ADDR=$(cast compute-address "$TEST_ADDR" --nonce "$CHECK_NONCE" 2>/dev/null \
    | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
  if [ -n "$ADDR" ]; then
    CODE=$(cast code --rpc-url "$L1_RPC" "$ADDR" 2>/dev/null || echo "0x")
    if [ "$CODE" != "0x" ] && [ -n "$CODE" ]; then
      # Verify it has counter() and increment() selectors.
      COUNTER_VAL=$(cast call --rpc-url "$L1_RPC" "$ADDR" "counter()(uint256)" 2>/dev/null || echo "ERR")
      if [ "$COUNTER_VAL" != "ERR" ]; then
        echo "Counter already deployed at: $ADDR (nonce=$CHECK_NONCE)"
        L1_COUNTER_ADDRESS="$ADDR"
        break
      fi
    fi
  fi
done

if [ -z "$L1_COUNTER_ADDRESS" ]; then
  echo "Deploying Counter on L1..."
  DEPLOY_RESULT=$(cast send \
    --rpc-url "$L1_RPC" \
    --private-key "$TEST_KEY" \
    --create "$COUNTER_BYTECODE" \
    --json 2>&1 || echo "{}")
  DEPLOY_STATUS=$(echo "$DEPLOY_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
  echo "Deploy tx status: $DEPLOY_STATUS"
  assert "STEP2: Counter deploy succeeded" '[ "$DEPLOY_STATUS" = "0x1" ]'

  L1_COUNTER_ADDRESS=$(echo "$DEPLOY_RESULT" | grep -oP '"contractAddress"\s*:\s*"\K[^"]+' || echo "")
  if [ -z "$L1_COUNTER_ADDRESS" ]; then
    L1_COUNTER_ADDRESS=$(cast compute-address "$TEST_ADDR" --nonce "$L1_NONCE" 2>/dev/null \
      | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
  fi
  echo "Counter deployed at: $L1_COUNTER_ADDRESS"
fi

L1_COUNTER_CODE=$(cast code --rpc-url "$L1_RPC" "$L1_COUNTER_ADDRESS" 2>/dev/null || echo "0x")
assert "STEP2: Counter has code on L1" '[ "$L1_COUNTER_CODE" != "0x" ] && [ -n "$L1_COUNTER_CODE" ]'

L1_COUNTER_INITIAL=$(cast call --rpc-url "$L1_RPC" "$L1_COUNTER_ADDRESS" "counter()(uint256)" 2>/dev/null || echo "0")
echo "L1 Counter initial value: $L1_COUNTER_INITIAL"

print_elapsed "STEP 2"
echo ""

# ══════════════════════════════════════════
#  STEP 3: Create L2 CrossChainProxy for L1 Counter
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 3: Create L2 proxy for L1 Counter"
echo "========================================"
start_timer

# Create proxy on L2 via CCM.createCrossChainProxy(L1Counter, rollupId=0).
# rollupId=0 means the original contract is on L1.
L2_COUNTER_PROXY=$(cast call --rpc-url "$L2_RPC" \
  "$CCM_L2_ADDRESS" \
  "computeCrossChainProxyAddress(address,uint256)(address)" \
  "$L1_COUNTER_ADDRESS" 0 2>/dev/null || echo "")
echo "Expected L2 proxy: $L2_COUNTER_PROXY"

L2_PROXY_CODE=$(cast code --rpc-url "$L2_RPC" "${L2_COUNTER_PROXY:-0x0000000000000000000000000000000000000001}" 2>/dev/null || echo "0x")
if [ "$L2_PROXY_CODE" != "0x" ] && [ -n "$L2_PROXY_CODE" ] && [ -n "$L2_COUNTER_PROXY" ]; then
  echo "L2 proxy already exists at: $L2_COUNTER_PROXY"
else
  echo "Creating CrossChainProxy on L2 for L1 Counter..."
  PROXY_RESULT=$(cast send \
    --rpc-url "$L2_RPC" \
    --private-key "$TEST_KEY" \
    "$CCM_L2_ADDRESS" \
    "createCrossChainProxy(address,uint256)" \
    "$L1_COUNTER_ADDRESS" 0 \
    --gas-limit 500000 \
    --json 2>&1 || echo "{}")
  PROXY_STATUS=$(echo "$PROXY_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
  echo "createCrossChainProxy tx status: $PROXY_STATUS"
  assert "STEP3: createCrossChainProxy succeeded" '[ "$PROXY_STATUS" = "0x1" ]'

  L2_PROXY_CODE=$(cast code --rpc-url "$L2_RPC" "${L2_COUNTER_PROXY:-0x0000000000000000000000000000000000000001}" 2>/dev/null || echo "0x")
fi

assert "STEP3: L2 proxy has code" \
  '[ -n "$L2_COUNTER_PROXY" ] && [ "$L2_PROXY_CODE" != "0x" ] && [ -n "$L2_PROXY_CODE" ]'
echo "L2 Counter proxy confirmed at: $L2_COUNTER_PROXY"

print_elapsed "STEP 3"
echo ""

# ══════════════════════════════════════════
#  STEP 4: Deploy ReturnDataLogger on L2
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 4: Deploy ReturnDataLogger on L2"
echo "========================================"
start_timer

L2_NONCE=$(cast nonce --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
echo "Test account L2 nonce: $L2_NONCE"

LOGGER_ADDRESS=""

# Check previous nonces for existing Logger deployment.
for CHECK_NONCE in $(seq "$L2_NONCE" -1 0 2>/dev/null | head -20); do
  ADDR=$(cast compute-address "$TEST_ADDR" --nonce "$CHECK_NONCE" 2>/dev/null \
    | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
  if [ -n "$ADDR" ]; then
    CODE=$(cast code --rpc-url "$L2_RPC" "$ADDR" 2>/dev/null || echo "0x")
    if [ "$CODE" != "0x" ] && [ -n "$CODE" ]; then
      # Check if it has lastReturnData() — selector 0x26a2b98c
      LOGGER_CHECK=$(cast call --rpc-url "$L2_RPC" "$ADDR" "lastReturnData()(bytes)" 2>/dev/null || echo "ERR")
      if [ "$LOGGER_CHECK" != "ERR" ]; then
        echo "Logger already deployed at: $ADDR (nonce=$CHECK_NONCE)"
        LOGGER_ADDRESS="$ADDR"
        break
      fi
    fi
  fi
done

if [ -z "$LOGGER_ADDRESS" ]; then
  echo "Deploying ReturnDataLogger on L2..."
  DEPLOY_RESULT=$(cast send \
    --rpc-url "$L2_RPC" \
    --private-key "$TEST_KEY" \
    --create "$LOGGER_BYTECODE" \
    --json 2>&1 || echo "{}")
  DEPLOY_STATUS=$(echo "$DEPLOY_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
  echo "Deploy tx status: $DEPLOY_STATUS"
  assert "STEP4: Logger deploy succeeded" '[ "$DEPLOY_STATUS" = "0x1" ]'

  LOGGER_ADDRESS=$(echo "$DEPLOY_RESULT" | grep -oP '"contractAddress"\s*:\s*"\K[^"]+' || echo "")
  if [ -z "$LOGGER_ADDRESS" ]; then
    LOGGER_ADDRESS=$(cast compute-address "$TEST_ADDR" --nonce "$L2_NONCE" 2>/dev/null \
      | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
  fi
  echo "Logger deployed at: $LOGGER_ADDRESS"
fi

LOGGER_CODE=$(cast code --rpc-url "$L2_RPC" "$LOGGER_ADDRESS" 2>/dev/null || echo "0x")
assert "STEP4: Logger has code on L2" '[ "$LOGGER_CODE" != "0x" ] && [ -n "$LOGGER_CODE" ]'
echo "Logger confirmed at: $LOGGER_ADDRESS"

print_elapsed "STEP 4"
echo ""

# ══════════════════════════════════════════
#  STEP 5 (KEY TEST): L2->L1 call with return data
# ══════════════════════════════════════════
#
# Logger.execute(L2CounterProxy, increment_selector) via L2 RPC proxy.
# The L2 proxy must:
#   1. Trace the tx and detect internal call to L2CounterProxy
#   2. Simulate L1 delivery, capture return data (uint256)
#   3. Build execution entries with return data in L2 RESULT entry
#   4. L2 tx: Logger -> proxy -> CCM -> returns data to Logger
#   5. Logger stores returnData on-chain

echo "========================================"
echo "  STEP 5 (KEY TEST): L2->L1 return data"
echo "========================================"
start_timer

echo "Calling Logger.execute(L2CounterProxy, increment())..."
echo "  Logger:         $LOGGER_ADDRESS"
echo "  L2CounterProxy: $L2_COUNTER_PROXY"
echo "  Via L2 proxy:   $L2_PROXY"
echo ""

# Wait for any pending submissions from prior steps.
wait_for_pending_zero 30 >/dev/null || true

CALL_RESULT=$(cast send \
  --rpc-url "$L2_PROXY" \
  --private-key "$TEST_KEY" \
  "$LOGGER_ADDRESS" \
  "execute(address,bytes)" "$L2_COUNTER_PROXY" "0xd09de08a" \
  --gas-limit 1000000 \
  --json 2>&1 || echo "{}")

CALL_HASH=$(echo "$CALL_RESULT" | grep -oP '"transactionHash"\s*:\s*"\K[^"]+' || echo "")
CALL_STATUS=$(echo "$CALL_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")

echo "Tx hash:   ${CALL_HASH:-<not found>}"
echo "Tx status: ${CALL_STATUS:-<not found>}"

if [ "$CALL_STATUS" != "0x1" ] && [ -n "$CALL_HASH" ]; then
  echo "Waiting for tx to be mined (up to 60s)..."
  for _i in $(seq 1 12); do
    sleep 5
    RECEIPT=$(cast receipt --rpc-url "$L2_RPC" "$CALL_HASH" --json 2>/dev/null || echo "")
    CALL_STATUS=$(echo "$RECEIPT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
    if [ -n "$CALL_STATUS" ]; then
      echo "Receipt status: $CALL_STATUS"
      break
    fi
  done
fi

assert "STEP5: Logger.execute tx succeeded (status=0x1)" '[ "$CALL_STATUS" = "0x1" ]' \
  "status=${CALL_STATUS:-unknown} hash=${CALL_HASH:-none}"

echo ""

# Wait for L1 trigger to execute and state to converge.
echo "Waiting for state convergence (up to 90s)..."
wait_for_pending_zero 60 >/dev/null || true

# Give L1 trigger time to land.
L2_BLK_NOW=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_NOW" 3 60 >/dev/null || true
wait_for_pending_zero 60 >/dev/null || true

print_elapsed "STEP 5"
echo ""

# ══════════════════════════════════════════
#  STEP 6: Verify return data on L2
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 6: Verify return data"
echo "========================================"
start_timer

# Check Logger.lastSuccess()
LAST_SUCCESS=$(cast call --rpc-url "$L2_RPC" "$LOGGER_ADDRESS" "lastSuccess()(bool)" 2>/dev/null || echo "?")
echo "Logger.lastSuccess(): $LAST_SUCCESS"
assert "STEP6: Logger.lastSuccess == true" '[ "$LAST_SUCCESS" = "true" ]'

# Check Logger.lastReturnData()
LAST_RETURN_DATA=$(cast call --rpc-url "$L2_RPC" "$LOGGER_ADDRESS" "lastReturnData()(bytes)" 2>/dev/null || echo "")
echo "Logger.lastReturnData(): $LAST_RETURN_DATA"

# Expected: abi.encode(uint256(N)) where N = L1_COUNTER_INITIAL + 1.
# For a fresh Counter, N = 1 = 0x0000...0001
EXPECTED_COUNTER=$((${L1_COUNTER_INITIAL:-0} + 1))
EXPECTED_HEX=$(printf "0x%064x" "$EXPECTED_COUNTER")
echo "Expected return data:    $EXPECTED_HEX"

assert "STEP6: Return data is NOT empty (issue #242 regression)" \
  '[ -n "$LAST_RETURN_DATA" ] && [ "$LAST_RETURN_DATA" != "0x" ]' \
  "lastReturnData=$LAST_RETURN_DATA"

assert "STEP6: Return data == abi.encode(uint256($EXPECTED_COUNTER))" \
  '[ "$LAST_RETURN_DATA" = "$EXPECTED_HEX" ]' \
  "got=$LAST_RETURN_DATA expected=$EXPECTED_HEX"

# Verify L1 Counter actually incremented.
L1_COUNTER_AFTER=$(cast call --rpc-url "$L1_RPC" "$L1_COUNTER_ADDRESS" "counter()(uint256)" 2>/dev/null || echo "?")
echo "L1 Counter.counter(): $L1_COUNTER_AFTER (expected: $EXPECTED_COUNTER)"
assert "STEP6: L1 Counter incremented" '[ "$L1_COUNTER_AFTER" = "$EXPECTED_COUNTER" ]' \
  "got=$L1_COUNTER_AFTER expected=$EXPECTED_COUNTER"

print_elapsed "STEP 6"
echo ""

# ══════════════════════════════════════════
#  STEP 7: Final health check
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 7: Final health check"
echo "========================================"
start_timer

echo "Waiting for state root convergence (up to 60s)..."
ROOTS=$(wait_for_convergence 60)
echo "State roots: $ROOTS"
assert "STEP7: State roots converge" '[ "$ROOTS" = "MATCH" ]'

HEALTH=$(get_health)
FINAL_MODE=$(echo "$HEALTH" | jq -r '.mode // "UNKNOWN"')
FINAL_HEALTHY=$(echo "$HEALTH" | jq -r '.healthy // false')
FINAL_PENDING=$(echo "$HEALTH" | jq -r '.pending_submissions // "?"')
FINAL_REWINDS=$(echo "$HEALTH" | jq -r '.consecutive_rewind_cycles // "?"')

echo "Builder mode:     $FINAL_MODE"
echo "Healthy:          $FINAL_HEALTHY"
echo "Pending:          $FINAL_PENDING"
echo "Rewind cycles:    $FINAL_REWINDS"

assert "STEP7: Builder still in Builder mode" '[ "$FINAL_MODE" = "Builder" ]'
assert "STEP7: Builder reports healthy" '[ "$FINAL_HEALTHY" = "true" ]'
assert "STEP7: No pending submissions" '[ "$FINAL_PENDING" = "0" ]'
assert "STEP7: No rewind cycles" '[ "$FINAL_REWINDS" = "0" ]'

print_elapsed "STEP 7"
echo ""

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════

if [ "$JSON_MODE" = "true" ]; then
  print_json_summary "l2-to-l1-return-data"
else
  echo "========================================"
  echo "  L2->L1 RETURN DATA TEST RESULTS"
  echo "========================================"
  echo ""
  echo "  L1 Counter:        $L1_COUNTER_ADDRESS"
  echo "  L2 Counter proxy:  $L2_COUNTER_PROXY"
  echo "  L2 Logger:         $LOGGER_ADDRESS"
  echo "  Return data:       ${LAST_RETURN_DATA:-<empty>}"
  echo "  Expected:          ${EXPECTED_HEX:-?}"
  echo "  L1 counter value:  ${L1_COUNTER_AFTER:-?}"
  echo ""
  echo "  Passed: $PASS_COUNT"
  echo "  Failed: $FAIL_COUNT"
  echo "  Total:  $TOTAL_COUNT"
  echo ""
  print_total_elapsed
  echo ""

  if [ "$FAIL_COUNT" -eq 0 ]; then
    echo -e "  ${GREEN}STATUS: ALL TESTS PASSED${RESET}"
    echo ""
    echo "========================================"
    exit 0
  else
    echo -e "  ${RED}STATUS: $FAIL_COUNT TEST(S) FAILED${RESET}"
    echo ""
    echo "========================================"
    exit 1
  fi
fi

if [ "$FAIL_COUNT" -eq 0 ]; then
  exit 0
else
  exit 1
fi
