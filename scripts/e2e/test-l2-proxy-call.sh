#!/usr/bin/env bash
# test-l2-proxy-call.sh — E2E test for L2 CrossChainProxy call via L2 proxy.
#
# Scenario: A user funds an L2 account via bridgeEther (L1→L2 deposit), then
# sends ETH to a CrossChainProxy address on L2 through the L2 RPC proxy.
# The L2 proxy must detect the proxy target, create execution entries, and
# forward the tx — resulting in a successful receipt (status=0x1).
#
# This test exercises the L2 proxy's symmetric counterpart to the L1 proxy:
# just as the L1 proxy detects calls to CrossChainProxy addresses on L1,
# the L2 proxy must detect calls to CrossChainProxy addresses on L2.
#
# Steps:
#   1. Wait for builder to be healthy.
#   2. Bridge 0.1 ETH from L1 to L2 for the test account.
#   3. Deploy a Counter contract on L2 as the interaction target.
#   4. Create a CrossChainProxy on L1 for the Counter (initiates cross-chain
#      call from L1 → L2 so the CCM creates the corresponding L2 proxy).
#   5. Wait for the L2 CrossChainProxy to appear (CCM deploys it on first call).
#   6. Send ETH to the L2 CrossChainProxy via the L2 RPC proxy (THE KEY TEST).
#   7. Assert tx receipt status = 0x1.
#
# Test account: dev key #8
#   Address:     0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f
#   Private key: 0x689af8efa8c651a91ad287602527f3af2fe9f6501a7ac4b061667b5a93e037fd
#
# Usage: ./scripts/e2e/test-l2-proxy-call.sh [--json]
#
# Defaults (from host):
#   L1_RPC=http://localhost:9555
#   L2_RPC=http://localhost:9545
#   L1_PROXY=http://localhost:9556
#   L2_PROXY=http://localhost:9548
#   HEALTH_URL=http://localhost:9560/health

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

# Dev account #8 — dedicated to this test. Distinct from all other services:
#   #0 deployer/builder, #1 tx-sender, #2 crosschain-health-check,
#   #3 bridge-health-check, #4 crosschain-tx-sender, #5 complex-tx-sender,
#   #6 double-deposit user2, #7 bridge T18 deployer.
TEST_KEY="0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97"
TEST_ADDR="0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f"

# Counter bytecode (trivial: uint256 public counter; function increment() external returns (uint256))
# Compiled with solc 0.8.33, evm-version paris (no PUSH0, works on both L1 and L2).
COUNTER_BYTECODE="0x6080604052348015600f57600080fd5b5061017f8061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100365760003560e01c806361bc221a1461003b578063d09de08a14610059575b600080fd5b610043610077565b60405161005091906100b7565b60405180910390f35b61006161007d565b60405161006e91906100b7565b60405180910390f35b60005481565b600080600081548092919061009190610101565b9190505550600054905090565b6000819050919050565b6100b18161009e565b82525050565b60006020820190506100cc60008301846100a8565b92915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b600061010c8261009e565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff820361013e5761013d6100d2565b5b60018201905091905056fea26469706673582212203dcec02a2fe7260919dd7cb86d1128a36e74ee651874f6f0a26f8e688fd7407764736f6c63430008210033"

# ── Colors (disabled when not a terminal) ──

if [ -t 1 ]; then
  CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'
  YELLOW='\033[1;33m'; BOLD='\033[1m'; DIM='\033[2m'; RESET='\033[0m'
else
  CYAN=''; GREEN=''; RED=''; YELLOW=''; BOLD=''; DIM=''; RESET=''
fi

# ── Load rollup.env ──

echo ""
echo -e "${CYAN}========================================"
echo -e "  L2 CROSSCHAIN PROXY CALL TEST"
echo -e "========================================${RESET}"
echo ""
echo "Loading rollup.env..."

eval "$($DOCKER_COMPOSE_CMD exec -T builder cat /shared/rollup.env 2>/dev/null)"
if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
  echo -e "${RED}ERROR: Could not load rollup.env — is the builder running?${RESET}"
  exit 1
fi

CCM_L2_ADDRESS="${CROSS_CHAIN_MANAGER_ADDRESS:-}"
if [ -z "$CCM_L2_ADDRESS" ]; then
  echo -e "${RED}ERROR: CROSS_CHAIN_MANAGER_ADDRESS not set in rollup.env${RESET}"
  exit 1
fi

ROLLUP_ID="${ROLLUP_ID:-1}"
L2_CHAIN_ID=42069

echo "ROLLUPS_ADDRESS=$ROLLUPS_ADDRESS"
echo "CCM_L2_ADDRESS=$CCM_L2_ADDRESS (CrossChainManagerL2)"
echo "BRIDGE_L1_ADDRESS=${BRIDGE_L1_ADDRESS:-${BRIDGE_ADDRESS:-}}"
echo "ROLLUP_ID=$ROLLUP_ID"
echo "Test account: $TEST_ADDR"
echo ""

BRIDGE_ADDR="${BRIDGE_L1_ADDRESS:-${BRIDGE_ADDRESS:-}}"
if [ -z "$BRIDGE_ADDR" ]; then
  echo -e "${RED}ERROR: BRIDGE_L1_ADDRESS not set in rollup.env${RESET}"
  exit 1
fi

L1_CHAIN_ID=$(cast chain-id --rpc-url "$L1_RPC" 2>/dev/null || echo "1337")
echo "L1 chain ID: $L1_CHAIN_ID"
echo ""

# ── PRE-FLIGHT ──

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

print_elapsed "PRE-FLIGHT"
echo ""

# ══════════════════════════════════════════
#  STEP 1: Bridge deposit (L1 → L2)
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 1: Bridge 0.1 ETH from L1 to L2"
echo "========================================"
start_timer

L2_BAL_BEFORE=$(get_balance "$L2_RPC" "$TEST_ADDR")
echo "Test account L2 balance before: $(python3 -c "print(f'{int(\"$L2_BAL_BEFORE\")/1e18:.6f}')" 2>/dev/null || echo "$L2_BAL_BEFORE wei") ETH"

# Only bridge if balance is insufficient to cover gas + value for later steps.
MIN_BALANCE=50000000000000000  # 0.05 ETH — enough to cover gas and the 0.01 ETH proxy call
if python3 -c "exit(0 if int('$L2_BAL_BEFORE') >= $MIN_BALANCE else 1)" 2>/dev/null; then
  echo "Test account already has sufficient L2 balance — skipping deposit."
else
  echo "Sending bridgeEther (0.1 ETH) via L1 proxy at $L1_PROXY..."
  DEPOSIT_RESULT=$(cast send \
    --rpc-url "$L1_PROXY" \
    --private-key "$TEST_KEY" \
    "$BRIDGE_ADDR" \
    "bridgeEther(uint256,address)" \
    "$ROLLUP_ID" "$TEST_ADDR" \
    --value 0.1ether \
    --gas-limit 800000 \
    2>&1 || true)
  DEPOSIT_STATUS=$(echo "$DEPOSIT_RESULT" | grep "^status" | awk '{print $2}' || echo "")
  echo "L1 bridge tx status: $DEPOSIT_STATUS"
  assert "STEP1: bridgeEther L1 tx succeeded" '[ "$DEPOSIT_STATUS" = "1" ]'

  # Wait for the deposit to be included in an L2 block.
  echo "Waiting for deposit to appear on L2 (up to 60s)..."
  L2_BLK_BEFORE=$(get_block_number "$L2_RPC")
  wait_for_block_advance "$L2_RPC" "$L2_BLK_BEFORE" 2 60 >/dev/null || true
  wait_for_pending_zero 60 >/dev/null || true

  L2_BAL_AFTER=$(get_balance "$L2_RPC" "$TEST_ADDR")
  echo "Test account L2 balance after: $(python3 -c "print(f'{int(\"$L2_BAL_AFTER\")/1e18:.6f}')" 2>/dev/null || echo "$L2_BAL_AFTER wei") ETH"
  assert "STEP1: Test account received ETH on L2" \
    'python3 -c "exit(0 if int(\"$L2_BAL_AFTER\") > int(\"$L2_BAL_BEFORE\") else 1)" 2>/dev/null'
fi

print_elapsed "STEP 1"
echo ""

# ══════════════════════════════════════════
#  STEP 2: Deploy Counter on L2
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 2: Deploy Counter on L2"
echo "========================================"
start_timer

# Idempotent: if nonce > 0, the Counter may already be deployed at CREATE(addr, 0).
TEST_NONCE=$(cast nonce --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
echo "Test account nonce on L2: $TEST_NONCE"
COUNTER_ADDRESS=""

if [ "$TEST_NONCE" != "0" ]; then
  PREDICTED=$(cast compute-address "$TEST_ADDR" --nonce 0 2>/dev/null \
    | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
  if [ -n "$PREDICTED" ]; then
    CODE=$(cast code --rpc-url "$L2_RPC" "$PREDICTED" 2>/dev/null || echo "0x")
    if [ "$CODE" != "0x" ] && [ -n "$CODE" ]; then
      echo "Counter already deployed at: $PREDICTED"
      COUNTER_ADDRESS="$PREDICTED"
    fi
  fi
fi

if [ -z "$COUNTER_ADDRESS" ]; then
  echo "Deploying Counter on L2..."
  DEPLOY_RESULT=$(cast send \
    --rpc-url "$L2_RPC" \
    --private-key "$TEST_KEY" \
    --create "$COUNTER_BYTECODE" \
    --json 2>&1 || echo "{}")
  DEPLOY_STATUS=$(echo "$DEPLOY_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
  echo "Deploy tx status: $DEPLOY_STATUS"
  assert "STEP2: Counter deploy tx succeeded" '[ "$DEPLOY_STATUS" = "0x1" ]'

  COUNTER_ADDRESS=$(cast compute-address "$TEST_ADDR" --nonce 0 2>/dev/null \
    | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
  if [ -z "$COUNTER_ADDRESS" ]; then
    echo -e "${RED}ERROR: Could not compute Counter address after deploy${RESET}"
    exit 1
  fi
  echo "Counter deployed at: $COUNTER_ADDRESS"
fi

COUNTER_CODE=$(cast code --rpc-url "$L2_RPC" "$COUNTER_ADDRESS" 2>/dev/null || echo "0x")
assert "STEP2: Counter code exists on L2" '[ "$COUNTER_CODE" != "0x" ] && [ -n "$COUNTER_CODE" ]'
echo "Counter verified at: $COUNTER_ADDRESS"

print_elapsed "STEP 2"
echo ""

# ══════════════════════════════════════════
#  STEP 3: Create CrossChainProxy on L1
# ══════════════════════════════════════════
#
# createCrossChainProxy(originalAddress, originalRollupId) on Rollups.sol
# creates a proxy on L1 that forwards calls to executeCrossChainCall.
# The first time this proxy is invoked via the L1 RPC proxy, the CCM on L2
# deploys the corresponding L2 CrossChainProxy via CREATE2.

echo "========================================"
echo "  STEP 3: Create CrossChainProxy on L1"
echo "========================================"
start_timer

# Compute expected proxy address (idempotent CREATE2).
L1_PROXY_ADDR=$(cast call --rpc-url "$L1_RPC" \
  "$ROLLUPS_ADDRESS" \
  "computeCrossChainProxyAddress(address,uint256)(address)" \
  "$COUNTER_ADDRESS" "$ROLLUP_ID" 2>/dev/null || echo "")
echo "Expected L1 CrossChainProxy: $L1_PROXY_ADDR"

L1_PROXY_CODE=$(cast code --rpc-url "$L1_RPC" "${L1_PROXY_ADDR:-0x0000000000000000000000000000000000000001}" 2>/dev/null || echo "0x")
if [ "$L1_PROXY_CODE" != "0x" ] && [ -n "$L1_PROXY_CODE" ] && [ -n "$L1_PROXY_ADDR" ]; then
  echo "L1 CrossChainProxy already exists at: $L1_PROXY_ADDR"
else
  echo "Creating CrossChainProxy on L1 (Rollups.createCrossChainProxy)..."
  PROXY_CREATE_RESULT=$(cast send \
    --rpc-url "$L1_RPC" \
    --private-key "$TEST_KEY" \
    "$ROLLUPS_ADDRESS" \
    "createCrossChainProxy(address,uint256)(address)" \
    "$COUNTER_ADDRESS" "$ROLLUP_ID" \
    --json 2>&1 || echo "{}")
  PROXY_CREATE_STATUS=$(echo "$PROXY_CREATE_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
  echo "createCrossChainProxy tx status: $PROXY_CREATE_STATUS"
  assert "STEP3: createCrossChainProxy tx succeeded" '[ "$PROXY_CREATE_STATUS" = "0x1" ]'

  # Re-read the proxy address after creation.
  L1_PROXY_ADDR=$(cast call --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" \
    "computeCrossChainProxyAddress(address,uint256)(address)" \
    "$COUNTER_ADDRESS" "$ROLLUP_ID" 2>/dev/null || echo "")
  L1_PROXY_CODE=$(cast code --rpc-url "$L1_RPC" "${L1_PROXY_ADDR:-0x0000000000000000000000000000000000000001}" 2>/dev/null || echo "0x")
fi

assert "STEP3: L1 CrossChainProxy has code" \
  '[ -n "$L1_PROXY_ADDR" ] && [ "$L1_PROXY_CODE" != "0x" ] && [ -n "$L1_PROXY_CODE" ]'
echo "L1 CrossChainProxy confirmed at: $L1_PROXY_ADDR"

print_elapsed "STEP 3"
echo ""

# ══════════════════════════════════════════
#  STEP 4: Trigger a cross-chain call L1→L2
#          to make CCM deploy the L2 proxy
# ══════════════════════════════════════════
#
# The L2 CrossChainProxy is deployed by CCM the first time a cross-chain call
# from L1 targets it. We send increment() through the L1 RPC proxy so the
# L1 proxy detects the executeCrossChainCall trace and queues entries on L2.
# After the block is produced, the L2 proxy should exist.

echo "========================================"
echo "  STEP 4: Trigger L1->L2 call to deploy L2 proxy"
echo "========================================"
start_timer

# Compute expected L2 CrossChainProxy address.
# The L2 proxy represents the test account (TEST_ADDR) from L1 (originalRollupId=0
# means origin is L1/external).
L2_CCM_PROXY_ADDR=$(cast call --rpc-url "$L2_RPC" \
  "$CCM_L2_ADDRESS" \
  "computeCrossChainProxyAddress(address,uint256)(address)" \
  "$TEST_ADDR" 0 2>/dev/null || echo "")
echo "Expected L2 CrossChainProxy (for test account from L1): $L2_CCM_PROXY_ADDR"

L2_CCM_PROXY_CODE=$(cast code --rpc-url "$L2_RPC" "${L2_CCM_PROXY_ADDR:-0x0000000000000000000000000000000000000001}" 2>/dev/null || echo "0x")
if [ "$L2_CCM_PROXY_CODE" != "0x" ] && [ -n "$L2_CCM_PROXY_CODE" ] && [ -n "$L2_CCM_PROXY_ADDR" ]; then
  echo "L2 CrossChainProxy already exists — skipping trigger call."
else
  echo "Sending increment() to L1 CrossChainProxy via L1 RPC proxy..."
  echo "  This will cause CCM to deploy the L2 CrossChainProxy for $TEST_ADDR..."
  L1_CALL_RESULT=$(cast send \
    --rpc-url "$L1_PROXY" \
    --private-key "$TEST_KEY" \
    "$L1_PROXY_ADDR" \
    "increment()" \
    --gas-limit 500000 \
    2>&1 || true)
  L1_CALL_STATUS=$(echo "$L1_CALL_RESULT" | grep "^status" | awk '{print $2}' || echo "")
  echo "L1 cross-chain call tx status: $L1_CALL_STATUS"
  # A non-1 status here is not fatal — it may be a gas estimation issue or
  # the proxy may not yet be ready. The key check is whether the L2 proxy appears.
  if [ "$L1_CALL_STATUS" != "1" ]; then
    echo -e "${YELLOW}WARNING: L1 cross-chain call did not confirm with status=1 (status='$L1_CALL_STATUS')${RESET}"
    echo "  The L2 proxy may still be deployed if the tx was accepted. Continuing..."
  fi

  # Wait for the L2 block that processes the cross-chain call.
  echo "Waiting for L2 blocks to process the cross-chain call (up to 60s)..."
  L2_BLK_TRG=$(get_block_number "$L2_RPC")
  wait_for_block_advance "$L2_RPC" "$L2_BLK_TRG" 2 60 >/dev/null || true
  wait_for_pending_zero 60 >/dev/null || true

  # Re-check L2 proxy.
  L2_CCM_PROXY_CODE=$(cast code --rpc-url "$L2_RPC" "${L2_CCM_PROXY_ADDR:-0x0000000000000000000000000000000000000001}" 2>/dev/null || echo "0x")
fi

assert "STEP4: L2 CrossChainProxy has code" \
  '[ -n "$L2_CCM_PROXY_ADDR" ] && [ "$L2_CCM_PROXY_CODE" != "0x" ] && [ -n "$L2_CCM_PROXY_CODE" ]'

# Also verify the proxy is registered in CCM authorizedProxies.
# authorizedProxies(address) selector 0x360d95b6 — returns (address originalAddress, uint64 originalRollupId)
AUTH_RESULT=$(cast call --rpc-url "$L2_RPC" \
  "$CCM_L2_ADDRESS" \
  "authorizedProxies(address)(address,uint64)" \
  "$L2_CCM_PROXY_ADDR" 2>/dev/null || echo "")
echo "CCM authorizedProxies($L2_CCM_PROXY_ADDR): $AUTH_RESULT"
AUTH_ORIGINAL=$(echo "$AUTH_RESULT" | awk 'NR==1{print $1}' | tr -d '[:space:]')
assert "STEP4: L2 proxy is registered in CCM authorizedProxies" \
  '[ -n "$AUTH_ORIGINAL" ] && [ "$AUTH_ORIGINAL" != "0x0000000000000000000000000000000000000000" ]'

echo "L2 CrossChainProxy confirmed at: $L2_CCM_PROXY_ADDR"

print_elapsed "STEP 4"
echo ""

# ══════════════════════════════════════════
#  STEP 5 (KEY TEST): Call L2 CrossChainProxy via L2 RPC proxy
# ══════════════════════════════════════════
#
# Send 0.01 ETH from the test account to the L2 CrossChainProxy address,
# routed through the L2 RPC proxy (port 9548). The proxy must:
#   1. Intercept the tx.
#   2. Detect the target is a registered L2 CrossChainProxy.
#   3. Call syncrollups_initiateCrossChainCall to queue execution entries.
#   4. Forward the tx to L2 for inclusion.
#   5. The tx receipt must show status=0x1.
#
# Before the fix, this would revert with ExecutionNotFound() because no
# entries were queued. After the fix, the L2 proxy handles it symmetrically
# to the L1 proxy.

echo "========================================"
echo "  STEP 5 (KEY TEST): L2 proxy call"
echo "========================================"
start_timer

echo "Sending 0.01 ETH to L2 CrossChainProxy via L2 RPC proxy..."
echo "  L2 proxy URL: $L2_PROXY"
echo "  Target (L2 CrossChainProxy): $L2_CCM_PROXY_ADDR"
echo "  Sender: $TEST_ADDR"
echo ""

L2_PROXY_CALL_RESULT=$(cast send \
  --rpc-url "$L2_PROXY" \
  --private-key "$TEST_KEY" \
  "$L2_CCM_PROXY_ADDR" \
  --value 0.01ether \
  --gas-limit 500000 \
  --json 2>&1 || echo "{}")

L2_PROXY_CALL_HASH=$(echo "$L2_PROXY_CALL_RESULT" | grep -oP '"transactionHash"\s*:\s*"\K[^"]+' || echo "")
L2_PROXY_CALL_STATUS=$(echo "$L2_PROXY_CALL_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")

echo "Tx hash:   ${L2_PROXY_CALL_HASH:-<not found>}"
echo "Tx status: ${L2_PROXY_CALL_STATUS:-<not found>}"
echo ""

if [ "$L2_PROXY_CALL_STATUS" = "0x1" ]; then
  echo -e "${GREEN}SUCCESS: L2 proxy call confirmed with status=0x1${RESET}"
  echo "  The L2 proxy correctly detected the CrossChainProxy target,"
  echo "  queued execution entries, and the tx succeeded."
elif [ "$L2_PROXY_CALL_STATUS" = "0x0" ]; then
  echo -e "${RED}FAILURE: L2 proxy call reverted (status=0x0)${RESET}"
  echo "  Expected status=0x1. The fix may not be active."
  echo "  Raw result: ${L2_PROXY_CALL_RESULT:0:400}"
else
  echo -e "${YELLOW}WARNING: Unexpected status '${L2_PROXY_CALL_STATUS}' — tx may not have been included yet.${RESET}"
  if [ -n "$L2_PROXY_CALL_HASH" ]; then
    echo "Waiting for tx to be mined (up to 30s)..."
    L2_RECEIPT=""
    for _i in $(seq 1 6); do
      sleep 5
      L2_RECEIPT=$(cast receipt --rpc-url "$L2_RPC" "$L2_PROXY_CALL_HASH" --json 2>/dev/null || echo "")
      L2_PROXY_CALL_STATUS=$(echo "$L2_RECEIPT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
      if [ -n "$L2_PROXY_CALL_STATUS" ]; then
        echo "Receipt status: $L2_PROXY_CALL_STATUS"
        break
      fi
    done
  fi
fi

assert "STEP5: L2 proxy call tx succeeded (status=0x1)" '[ "$L2_PROXY_CALL_STATUS" = "0x1" ]' \
  "status=${L2_PROXY_CALL_STATUS:-unknown} hash=${L2_PROXY_CALL_HASH:-none}"

print_elapsed "STEP 5"
echo ""

# ══════════════════════════════════════════
#  STEP 6: Post-call health check
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 6: Post-call health check"
echo "========================================"
start_timer

echo "Waiting for state root convergence (up to 60s)..."
ROOTS=$(wait_for_convergence 60)
echo "State roots: $ROOTS"
assert "STEP6: State roots converge after L2 proxy call" '[ "$ROOTS" = "MATCH" ]'

HEALTH=$(get_health)
FINAL_MODE=$(echo "$HEALTH" | jq -r '.mode // "UNKNOWN"')
FINAL_HEALTHY=$(echo "$HEALTH" | jq -r '.healthy // false')
FINAL_PENDING=$(echo "$HEALTH" | jq -r '.pending_submissions // "?"')
FINAL_REWINDS=$(echo "$HEALTH" | jq -r '.consecutive_rewind_cycles // "?"')

echo "Builder mode:     $FINAL_MODE"
echo "Healthy:          $FINAL_HEALTHY"
echo "Pending:          $FINAL_PENDING"
echo "Rewind cycles:    $FINAL_REWINDS"

assert "STEP6: Builder still in Builder mode" '[ "$FINAL_MODE" = "Builder" ]'
assert "STEP6: Builder reports healthy" '[ "$FINAL_HEALTHY" = "true" ]'
assert "STEP6: No pending submissions" '[ "$FINAL_PENDING" = "0" ]'
assert "STEP6: No rewind cycles" '[ "$FINAL_REWINDS" = "0" ]'

print_elapsed "STEP 6"
echo ""

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════

if [ "$JSON_MODE" = "true" ]; then
  print_json_summary "l2-proxy-call"
else
  echo "========================================"
  echo "  L2 CROSSCHAIN PROXY CALL TEST RESULTS"
  echo "========================================"
  echo ""
  echo "  Test account:          $TEST_ADDR"
  echo "  Counter (L2):          $COUNTER_ADDRESS"
  echo "  L1 CrossChainProxy:    $L1_PROXY_ADDR"
  echo "  L2 CrossChainProxy:    $L2_CCM_PROXY_ADDR"
  echo "  Key test tx hash:      ${L2_PROXY_CALL_HASH:-<none>}"
  echo "  Key test tx status:    ${L2_PROXY_CALL_STATUS:-<none>}"
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
