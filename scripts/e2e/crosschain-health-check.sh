#!/usr/bin/env bash
# Cross-Chain Execution Health Check — Automated E2E test suite for L1→L2 cross-chain calls.
#
# Tests seven scenarios:
#   TEST 1: Single cross-chain call (full consumption — counter increments, state roots match)
#   TEST 2: Rewind detection and recovery monitoring (observes consecutive_rewind_cycles)
#   TEST 3: Rapid burst of 3 calls (eventual full consumption — counter += 3)
#   TEST 4: Additional single cross-chain call (continued operation after burst)
#   TEST 5: Duplicate calls without delay (section 4f prefix counting — both processed)
#   TEST 6: Fullnode trace parity post-stress (counter and block lag match across all nodes)
#   TEST 7: Rapid burst of 5 calls (tighter 1s delay — stresses §4f prefix counting, counter += 5)
#
# Flow:
#   1. User sends tx to CrossChainProxy on L1 via L1 proxy (port 9556)
#   2. L1 proxy detects the cross-chain call, queues entry via syncrollups_initiateCrossChainCall
#   3. Builder includes the entry in the next L2 block (loadExecutionTable + executeIncomingCrossChainCall)
#   4. Builder submits batch to L1 via postBatch with the consumed entry
#   5. L1 executeCrossChainCall confirms the entry
#   If L1 doesn't consume all entries, the builder detects a state root mismatch and rewinds.
#
# Prerequisites:
#   - Docker environment running with dev overlay (builder, fullnode1, fullnode2, l1)
#   - Builder healthy and in Builder mode
#   - crosschain-tx-sender has deployed Counter and CrossChainProxy (counter.env must exist)
#
# Usage: ./scripts/e2e/crosschain-health-check.sh [--json]

set -euo pipefail

# ── Source shared library ──

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

# ── Parse args ──

parse_lib_args "$@"

# ── Configuration ──

# Dev account #8 — dedicated to health-check deploys. Distinct from:
#   #0 deployer/builder, #1 tx-sender, #3 bridge-health-check,
#   #4 crosschain-tx-sender, #5 complex-tx-sender, #7 bridge T18 deployer.
# Using a dedicated key means no nonce conflicts with the crosschain-tx-sender
# that continuously increments its own counter on key #4.
# Dev account #2 — has ETH on L2 from tx-sender, not used by any other service.
TEST_KEY="0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a"
TEST_ADDR="0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
# No separate funder needed — TEST_ADDR (#2) already has ETH from tx-sender transfers.
BUILDER_ADDR="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

# Track peak rewind cycles observed across the whole run (for TEST 2 reporting)
PEAK_REWIND_CYCLES=0

# ── Crosschain-specific helpers ──

get_counter() {
  cast call --rpc-url "$L2_RPC" "$TEST_COUNTER_ADDRESS" "counter()(uint256)" 2>/dev/null || echo "?"
}

# Send increment() to the health-check's dedicated CrossChainProxy on L1 via the L1 RPC proxy.
# Prints the tx status (1 = success, 0 = revert, "" = error).
send_increment() {
  local result status
  result=$(cast send \
    --rpc-url "$L1_PROXY" \
    --private-key "$TEST_KEY" \
    "$TEST_PROXY_ADDRESS" \
    "increment()" \
    --gas-limit 500000 \
    2>&1 || true)
  status=$(echo "$result" | grep "^status" | awk '{print $2}')
  echo "$status"
}

# ── Load environment ──

echo "Loading rollup.env..."
if [ -f "/shared/rollup.env" ]; then
  eval "$(cat /shared/rollup.env)"
elif [ -n "${SHARED_DIR:-}" ] && [ -f "${SHARED_DIR}/rollup.env" ]; then
  eval "$(cat "${SHARED_DIR}/rollup.env")"
else
  eval "$(sudo docker exec testnet-eez-builder-1 cat /shared/rollup.env 2>/dev/null)" || true
fi
if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
  echo "ERROR: Could not load rollup.env — is the builder running?"
  exit 1
fi

# Verify crosschain-tx-sender is running by checking counter.env exists.
# We do NOT use its counter address for assertions — the tx-sender keeps
# incrementing that counter in parallel, which would break exact delta checks.
echo "Verifying crosschain-tx-sender is running (counter.env check)..."
COUNTER_ENV_RAW=$(sudo docker exec testnet-eez-builder-1 \
  cat /shared/counter.env 2>/dev/null || true)
if [ -z "$COUNTER_ENV_RAW" ]; then
  echo "ERROR: /shared/counter.env not found — has crosschain-tx-sender finished deploying?"
  echo "       Start the 'sync' profile: docker compose ... --profile sync up -d"
  exit 1
fi
eval "$COUNTER_ENV_RAW"
if [ -z "${COUNTER_ADDRESS:-}" ]; then
  echo "ERROR: COUNTER_ADDRESS not set in counter.env"
  exit 1
fi
echo "crosschain-tx-sender counter: $COUNTER_ADDRESS (not used for assertions)"

# ── Deploy dedicated Counter on L2 ──
# We deploy a fresh Counter contract so assertions about exact increments are
# not polluted by the crosschain-tx-sender running in parallel.

L1_CHAIN_ID=$(cast chain-id --rpc-url "$L1_RPC" 2>/dev/null || echo "1337")
ROLLUP_ID="${ROLLUP_ID:-1}"

echo ""
echo "Deploying dedicated Counter on L2 for health-check assertions..."

# TEST_ADDR (#2) is bootstrapped with 100 ETH at block 1. Verify it has funds.
HC_BAL=$(cast balance --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
MIN_BAL=1000000000000000000  # 1 ETH
HC_BAL_INT=$(python3 -c "print(int('$HC_BAL'))" 2>/dev/null || echo "0")
if [ "$HC_BAL_INT" -lt "$MIN_BAL" ] 2>/dev/null; then
  echo "WARNING: TEST_ADDR balance low ($(python3 -c "print(int('$HC_BAL')/1e18)" 2>/dev/null) ETH). Waiting for bootstrap..."
  for _i in $(seq 1 6); do
    sleep 5
    HC_BAL=$(cast balance --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
    HC_BAL_INT=$(python3 -c "print(int('$HC_BAL'))" 2>/dev/null || echo "0")
    [ "$HC_BAL_INT" -ge "$MIN_BAL" ] 2>/dev/null && break
  done
  if [ "$HC_BAL_INT" -lt "$MIN_BAL" ] 2>/dev/null; then
    echo "ERROR: TEST_ADDR has insufficient ETH. Check BOOTSTRAP_ACCOUNTS in docker-compose.yml"
    exit 1
  fi
fi
echo "TEST_ADDR L2 balance: $(python3 -c "print(int('$HC_BAL')/1e18)" 2>/dev/null || echo "$HC_BAL") ETH"

# Counter creation bytecode (from forge inspect CounterContracts.sol:Counter bytecode).
# Hardcoded to avoid needing forge at runtime. The Counter is a trivial contract:
# uint256 public counter; function increment() external returns (uint256);
COUNTER_BYTECODE="0x6080806040523460135760bc908160188239f35b5f80fdfe60808060405260043610156011575f80fd5b5f3560e01c90816361bc221a14606f575063d09de08a14602f575f80fd5b34606b575f366003190112606b575f545f198114605757600160209101805f55604051908152f35b634e487b7160e01b5f52601160045260245ffd5b5f80fd5b34606b575f366003190112606b576020905f548152f3fea26469706673582212207c89cba351d88de9fc08451e767c8a592dcb470c85ae6740262a1a5e01ebee0a64736f6c63430008210033"

# Check whether we already deployed at nonce 0 on this key (idempotent re-run).
HC_NONCE=$(cast nonce --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
TEST_COUNTER_ADDRESS=""
if [ "$HC_NONCE" != "0" ]; then
  PREDICTED=$(cast compute-address "$TEST_ADDR" --nonce 0 2>/dev/null | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
  if [ -n "$PREDICTED" ]; then
    CODE=$(cast code --rpc-url "$L2_RPC" "$PREDICTED" 2>/dev/null || echo "0x")
    if [ "$CODE" != "0x" ] && [ -n "$CODE" ]; then
      echo "Health-check Counter already deployed at: $PREDICTED"
      TEST_COUNTER_ADDRESS="$PREDICTED"
    fi
  fi
fi

if [ -z "$TEST_COUNTER_ADDRESS" ]; then
  # NOTE: cast send --create uses the deployed bytecode (no constructor args needed for Counter).
  DEPLOY_RESULT=$(cast send \
    --rpc-url "$L2_RPC" \
    --private-key "$TEST_KEY" \
    --create "$COUNTER_BYTECODE" \
    --json 2>&1 || echo "{}")

  DEPLOY_STATUS=$(echo "$DEPLOY_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
  if [ "$DEPLOY_STATUS" != "0x1" ]; then
    echo "ERROR: Counter deploy tx failed or did not succeed."
    echo "       Result: ${DEPLOY_RESULT:0:300}"
    exit 1
  fi

  # The deployed address is the CREATE address at nonce 0.
  TEST_COUNTER_ADDRESS=$(cast compute-address "$TEST_ADDR" --nonce 0 2>/dev/null \
    | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
  if [ -z "$TEST_COUNTER_ADDRESS" ]; then
    echo "ERROR: Could not compute deployed Counter address."
    exit 1
  fi
  echo "Health-check Counter deployed at: $TEST_COUNTER_ADDRESS"
fi

# Verify code landed on chain.
HC_CODE=$(cast code --rpc-url "$L2_RPC" "$TEST_COUNTER_ADDRESS" 2>/dev/null || echo "0x")
if [ "$HC_CODE" = "0x" ] || [ -z "$HC_CODE" ]; then
  echo "ERROR: Counter code not found at $TEST_COUNTER_ADDRESS after deploy."
  exit 1
fi

# ── Create CrossChainProxy on L1 for the dedicated Counter ──

echo "Creating CrossChainProxy on L1 for health-check Counter..."
PROXY_RESULT=$(cast send \
  --rpc-url "$L1_RPC" \
  --private-key "$TEST_KEY" \
  "$ROLLUPS_ADDRESS" \
  "createCrossChainProxy(address,uint256)(address)" \
  "$TEST_COUNTER_ADDRESS" "$ROLLUP_ID" \
  --json 2>&1 || echo "{}")

PROXY_TX_STATUS=$(echo "$PROXY_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")

# Compute the proxy address regardless of tx status (proxy may already exist from a prior run).
TEST_PROXY_ADDRESS=$(cast call --rpc-url "$L1_RPC" \
  "$ROLLUPS_ADDRESS" \
  "computeCrossChainProxyAddress(address,uint256)(address)" \
  "$TEST_COUNTER_ADDRESS" "$ROLLUP_ID" 2>/dev/null || echo "")

if [ -z "$TEST_PROXY_ADDRESS" ] || [ "$TEST_PROXY_ADDRESS" = "0x0000000000000000000000000000000000000000" ]; then
  echo "ERROR: Could not compute CrossChainProxy address for health-check Counter."
  exit 1
fi

# Verify proxy code on L1.
TEST_PROXY_CODE=$(cast code --rpc-url "$L1_RPC" "$TEST_PROXY_ADDRESS" 2>/dev/null || echo "0x")
if [ "$TEST_PROXY_CODE" = "0x" ] || [ -z "$TEST_PROXY_CODE" ]; then
  echo "ERROR: CrossChainProxy at $TEST_PROXY_ADDRESS has no code on L1."
  echo "       createCrossChainProxy tx status: $PROXY_TX_STATUS"
  echo "       Result: ${PROXY_RESULT:0:300}"
  exit 1
fi

echo ""
echo "ROLLUPS_ADDRESS=$ROLLUPS_ADDRESS"
echo "ROLLUP_ID=$ROLLUP_ID"
echo "L1_CHAIN_ID=$L1_CHAIN_ID"
echo "TEST_COUNTER_ADDRESS=$TEST_COUNTER_ADDRESS  (dedicated, no tx-sender interference)"
echo "TEST_PROXY_ADDRESS=$TEST_PROXY_ADDRESS"
echo "Test account: $TEST_ADDR"
echo ""

# ── Pre-flight ──

echo "========================================"
echo "  PRE-FLIGHT CHECKS"
echo "========================================"
start_timer

echo "Waiting for builder to be ready (up to 60s)..."
MODE=$(wait_for_builder_ready 60)
echo "Builder mode: $MODE"
assert "Builder is in Builder mode" '[ "$MODE" = "Builder" ]'

L1_BLOCK=$(get_block_number "$L1_RPC")
echo "L1 block: $(printf '%d' "$L1_BLOCK")"
assert "L1 producing blocks" '[ "$(printf "%d" "$L1_BLOCK")" -gt 0 ]'

L2_BLOCK=$(get_block_number "$L2_RPC")
echo "L2 block: $(printf '%d' "$L2_BLOCK")"
assert "L2 producing blocks" '[ "$(printf "%d" "$L2_BLOCK")" -gt 0 ]'

COUNTER_PREFLIGHT=$(get_counter)
echo "Counter value: $COUNTER_PREFLIGHT"
assert "Counter readable" '[ "$COUNTER_PREFLIGHT" != "?" ]'

# Confirm proxy code on L1 (already checked above, but include in assert)
assert "CrossChainProxy deployed on L1" '[ "$TEST_PROXY_CODE" != "0x" ] && [ -n "$TEST_PROXY_CODE" ]'

ONCHAIN_SR_BEFORE=$(get_onchain_state_root "$ROLLUPS_ADDRESS")
echo "On-chain stateRoot: $ONCHAIN_SR_BEFORE"

print_elapsed "PRE-FLIGHT"
echo ""

# ══════════════════════════════════════════
#  TEST 1: Single Cross-Chain Call (Full Consumption)
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 1: Single Cross-Chain Call"
echo "========================================"
echo "Send increment() once, expect counter+1 and state root convergence."
echo ""
start_timer

COUNT_BEFORE=$(get_counter)
echo "Counter before: $COUNT_BEFORE"

TX_STATUS=$(send_increment)
echo "L1 tx status: $TX_STATUS"
assert "TEST1: L1 tx for increment() succeeded" '[ "$TX_STATUS" = "1" ]'

echo "Waiting for pending submissions to clear (up to 60s)..."
PENDING_RESULT=$(wait_for_pending_zero 60)
echo "Pending submissions: $PENDING_RESULT"
assert "TEST1: No pending submissions after single call" '[ "$PENDING_RESULT" = "0" ]'

echo "Waiting for state root convergence (up to 60s)..."
ROOTS=$(wait_for_convergence 60)
echo "State roots: $ROOTS"
assert "TEST1: State roots match across all nodes" '[ "$ROOTS" = "MATCH" ]'

COUNT_AFTER=$(get_counter)
echo "Counter after: $COUNT_AFTER"

if [ "$COUNT_BEFORE" != "?" ] && [ "$COUNT_AFTER" != "?" ]; then
  EXPECTED=$(python3 -c "print(int('$COUNT_BEFORE') + 1)")
  assert "TEST1: Counter incremented by 1" '[ "$COUNT_AFTER" = "$EXPECTED" ]' \
    "expected=$EXPECTED actual=$COUNT_AFTER"
else
  assert "TEST1: Counter incremented by 1" 'false' "could not read counter"
fi

# Wait for fullnodes to catch up to the builder block before reading counter.
# State roots match (checked above), but fullnodes may be 1 block behind on tip.
BUILDER_BLK_T1=$(get_block_number "$L2_RPC")
echo "Waiting for fullnodes to reach builder block $(printf '%d' "$BUILDER_BLK_T1")..."
wait_for_block_advance "$FULLNODE1_RPC" "$BUILDER_BLK_T1" 0 30 >/dev/null 2>&1 || true
wait_for_block_advance "$FULLNODE2_RPC" "$BUILDER_BLK_T1" 0 30 >/dev/null 2>&1 || true

FN1_COUNTER=$(cast call --rpc-url "$FULLNODE1_RPC" "$TEST_COUNTER_ADDRESS" "counter()(uint256)" 2>/dev/null || echo "?")
FN2_COUNTER=$(cast call --rpc-url "$FULLNODE2_RPC" "$TEST_COUNTER_ADDRESS" "counter()(uint256)" 2>/dev/null || echo "?")
echo "Counter: builder=$COUNT_AFTER fn1=$FN1_COUNTER fn2=$FN2_COUNTER"
assert "TEST1: Fullnode1 counter matches builder" '[ "$FN1_COUNTER" = "$COUNT_AFTER" ]' \
  "builder=$COUNT_AFTER fn1=$FN1_COUNTER"
assert "TEST1: Fullnode2 counter matches builder" '[ "$FN2_COUNTER" = "$COUNT_AFTER" ]' \
  "builder=$COUNT_AFTER fn2=$FN2_COUNTER"

# Capture any rewinds that occurred during test 1
sample_rewind_cycles

print_elapsed "TEST 1"
echo ""

# ══════════════════════════════════════════
#  TEST 2: Rewind Detection and Recovery Monitoring
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 2: Rewind Detection and Recovery"
echo "========================================"
echo "Monitor the builder for rewind cycles over 2 block periods (~25s)."
echo "If a rewind occurred during TEST 1 or during monitoring, verify recovery."
echo "If no rewind occurs, that is also a PASS (system working correctly)."
echo ""
start_timer

# Sample rewind cycles before we start monitoring
REWINDS_BEFORE=$(get_rewind_cycles)
echo "Rewind cycles before monitoring: $REWINDS_BEFORE"

# Monitor health every 5s for 25s to catch any in-flight rewind
echo "Monitoring health for 25s..."
monitor_rewinds_for 25

echo "Peak rewind cycles observed so far: $PEAK_REWIND_CYCLES"

# After monitoring, check that the builder has recovered (rewinds back to 0)
REWINDS_AFTER=$(get_rewind_cycles)
echo "Rewind cycles now: $REWINDS_AFTER"

# The builder is healthy if it either never rewound or successfully recovered
HEALTH_NOW=$(get_health)
HEALTHY=$(echo "$HEALTH_NOW" | jq -r '.healthy // false')
MODE_NOW=$(echo "$HEALTH_NOW" | jq -r '.mode // "UNKNOWN"')

assert "TEST2: Builder still in Builder mode after monitoring" '[ "$MODE_NOW" = "Builder" ]'
assert "TEST2: Builder reports healthy status" '[ "$HEALTHY" = "true" ]'

# If rewinds were observed, confirm recovery (cycles back to 0 or builder is healthy)
if [ "$PEAK_REWIND_CYCLES" -gt 0 ] 2>/dev/null; then
  echo "  NOTE: Rewind detected (peak=$PEAK_REWIND_CYCLES). Verifying recovery..."
  assert "TEST2: Builder recovered from rewind (cycles reset to 0 or healthy)" \
    '[ "$REWINDS_AFTER" -eq 0 ] || [ "$HEALTHY" = "true" ]' \
    "rewinds_after=$REWINDS_AFTER healthy=$HEALTHY"
  # Check logs for rewind messages as confirmation
  REWIND_LOG=$(sudo docker compose -f docker-compose.yml -f docker-compose.dev.yml \
    logs builder --no-log-prefix --since 120s 2>&1 | grep -ic "rewind" || true)
  echo "  Rewind log entries (last 120s): $REWIND_LOG"
  assert "TEST2: Builder log confirms rewind cycle(s) occurred" '[ "$REWIND_LOG" -ge 1 ]' \
    "log_count=$REWIND_LOG"
else
  echo "  NOTE: No rewinds observed — system is handling entries correctly."
  # Still check logs to confirm no unexpected error during this window.
  # Filter out known benign errors (connection retries, WS reconnects, etc.)
  ERROR_LOG=$(sudo docker compose -f docker-compose.yml -f docker-compose.dev.yml \
    logs builder --no-log-prefix --since 120s 2>&1 \
    | grep -i "panic\|fatal" \
    | grep -v -i "connection\|reconnect\|timeout\|retry" \
    | wc -l || true)
  echo "  Fatal/panic log entries (last 120s): $ERROR_LOG"
  assert "TEST2: No panics/fatals in builder logs during monitoring window" '[ "$ERROR_LOG" -eq 0 ]' \
    "error_count=$ERROR_LOG"
fi

ROOTS=$(check_state_roots)
echo "State roots after monitoring: $ROOTS"
assert "TEST2: State roots match after rewind monitoring" '[ "$ROOTS" = "MATCH" ]'

print_elapsed "TEST 2"
echo ""

# ══════════════════════════════════════════
#  TEST 3: Rapid Burst (3 calls — partial and eventual consumption)
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 3: Rapid Burst (3 Cross-Chain Calls)"
echo "========================================"
echo "Send 3 increment() calls in quick succession."
echo "L1 may consume them across multiple batches."
echo "After enough time, counter must have increased by exactly 3."
echo ""
start_timer

COUNT_BEFORE=$(get_counter)
echo "Counter before burst: $COUNT_BEFORE"

for i in 1 2 3; do
  S=$(send_increment)
  echo "  Call #$i: L1 tx status=$S"
  assert "TEST3: Burst call #$i L1 tx succeeded" '[ "$S" = "1" ]'
  # Small pause between calls to avoid nonce collisions from the same key
  sleep 2
done

echo "Waiting for pending submissions to clear (up to 180s)..."
PENDING_RESULT=$(wait_for_pending_zero 180)
echo "Pending submissions: $PENDING_RESULT"
assert "TEST3: No pending submissions after burst settles" '[ "$PENDING_RESULT" = "0" ]'

echo "Waiting for state root convergence (up to 120s)..."
ROOTS=$(wait_for_convergence 120)
echo "State roots after burst: $ROOTS"
assert "TEST3: State roots match after burst" '[ "$ROOTS" = "MATCH" ]'

COUNT_AFTER=$(get_counter)
echo "Counter after burst: $COUNT_AFTER"

if [ "$COUNT_BEFORE" != "?" ] && [ "$COUNT_AFTER" != "?" ]; then
  EXPECTED=$(python3 -c "print(int('$COUNT_BEFORE') + 3)")
  assert "TEST3: Counter incremented by exactly 3" '[ "$COUNT_AFTER" = "$EXPECTED" ]' \
    "expected=$EXPECTED actual=$COUNT_AFTER"
else
  assert "TEST3: Counter incremented by exactly 3" 'false' "could not read counter"
fi

HEALTH_FINAL=$(get_health)
HEALTHY_FINAL=$(echo "$HEALTH_FINAL" | jq -r '.healthy // false')
assert "TEST3: Builder is healthy after burst" '[ "$HEALTHY_FINAL" = "true" ]'

# Sample any new rewind cycles from the burst period
sample_rewind_cycles

print_elapsed "TEST 3"
echo ""

# ══════════════════════════════════════════
#  TEST 4: Cross-Chain Call with ETH Value
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 4: Cross-Chain Call (additional single call)"
echo "========================================"
echo "Send increment() once more to confirm continued operation after burst."
echo ""
start_timer

COUNT_BEFORE=$(get_counter)
echo "Counter before: $COUNT_BEFORE"

TX_STATUS=$(send_increment)
echo "L1 tx status: $TX_STATUS"
assert "TEST4: L1 tx succeeded" '[ "$TX_STATUS" = "1" ]'

echo "Waiting for pending submissions to clear (up to 60s)..."
PENDING_RESULT=$(wait_for_pending_zero 60)
assert "TEST4: No pending submissions" '[ "$PENDING_RESULT" = "0" ]'

echo "Waiting for state root convergence (up to 60s)..."
ROOTS=$(wait_for_convergence 60)
assert "TEST4: State roots match" '[ "$ROOTS" = "MATCH" ]'

COUNT_AFTER=$(get_counter)
echo "Counter after: $COUNT_AFTER"

if [ "$COUNT_BEFORE" != "?" ] && [ "$COUNT_AFTER" != "?" ]; then
  EXPECTED=$(python3 -c "print(int('$COUNT_BEFORE') + 1)")
  assert "TEST4: Counter incremented by 1" '[ "$COUNT_AFTER" = "$EXPECTED" ]' \
    "expected=$EXPECTED actual=$COUNT_AFTER"
else
  assert "TEST4: Counter incremented by 1" 'false' "could not read counter"
fi

print_elapsed "TEST 4"
echo ""

# ══════════════════════════════════════════
#  TEST 5: Duplicate Calls (no delay)
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 5: Duplicate Calls (no delay)"
echo "========================================"
echo "Send 2x increment() with no delay between them."
echo "Both should be processed — section 4f prefix counting does not deduplicate."
echo ""
start_timer

COUNT_BEFORE=$(get_counter)
echo "Counter before: $COUNT_BEFORE"

S1=$(send_increment)
S2=$(send_increment)
echo "Call 1 status: $S1"
echo "Call 2 status: $S2"
assert "TEST5: Duplicate call #1 succeeded" '[ "$S1" = "1" ]'
assert "TEST5: Duplicate call #2 succeeded" '[ "$S2" = "1" ]'

echo "Waiting for pending submissions to clear (up to 180s)..."
PENDING_RESULT=$(wait_for_pending_zero 180)
assert "TEST5: No pending submissions" '[ "$PENDING_RESULT" = "0" ]'

echo "Waiting for state root convergence (up to 120s)..."
ROOTS=$(wait_for_convergence 120)
assert "TEST5: State roots match" '[ "$ROOTS" = "MATCH" ]'

COUNT_AFTER=$(get_counter)
echo "Counter after: $COUNT_AFTER"

if [ "$COUNT_BEFORE" != "?" ] && [ "$COUNT_AFTER" != "?" ]; then
  EXPECTED=$(python3 -c "print(int('$COUNT_BEFORE') + 2)")
  assert "TEST5: Counter incremented by 2" '[ "$COUNT_AFTER" = "$EXPECTED" ]' \
    "expected=$EXPECTED actual=$COUNT_AFTER"
else
  assert "TEST5: Counter incremented by 2" 'false' "could not read counter"
fi

print_elapsed "TEST 5"
echo ""

# ══════════════════════════════════════════
#  TEST 6: Fullnode Trace Parity (Post-Stress)
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 6: Fullnode Trace Parity (Post-Stress)"
echo "========================================"
echo "Verify fullnode counter values match builder after all stress tests."
echo ""
start_timer

# Wait for fullnodes to catch up
echo "Waiting for state root convergence (up to 90s)..."
ROOTS=$(wait_for_convergence 90)
assert "TEST6: State roots match" '[ "$ROOTS" = "MATCH" ]'

# Wait for fullnodes to reach builder's block before comparing counter values.
BUILDER_BLK_T6=$(get_block_number "$L2_RPC")
echo "Waiting for fullnodes to reach builder block $(printf '%d' "$BUILDER_BLK_T6")..."
wait_for_block_advance "$FULLNODE1_RPC" "$BUILDER_BLK_T6" 0 30 >/dev/null 2>&1 || true
wait_for_block_advance "$FULLNODE2_RPC" "$BUILDER_BLK_T6" 0 30 >/dev/null 2>&1 || true

BUILDER_COUNT=$(get_counter)
FN1_COUNT=$(cast call --rpc-url "$FULLNODE1_RPC" "$TEST_COUNTER_ADDRESS" "counter()(uint256)" 2>/dev/null || echo "?")
FN2_COUNT=$(cast call --rpc-url "$FULLNODE2_RPC" "$TEST_COUNTER_ADDRESS" "counter()(uint256)" 2>/dev/null || echo "?")
echo "Counter: builder=$BUILDER_COUNT fn1=$FN1_COUNT fn2=$FN2_COUNT"

assert "TEST6: Fullnode1 counter matches builder" '[ "$FN1_COUNT" = "$BUILDER_COUNT" ]' \
  "builder=$BUILDER_COUNT fn1=$FN1_COUNT"
assert "TEST6: Fullnode2 counter matches builder" '[ "$FN2_COUNT" = "$BUILDER_COUNT" ]' \
  "builder=$BUILDER_COUNT fn2=$FN2_COUNT"

# Also verify block numbers are close (fullnodes shouldn't be too far behind)
BUILDER_BLK=$(get_block_number "$L2_RPC")
FN1_BLK=$(get_block_number "$FULLNODE1_RPC")
FN2_BLK=$(get_block_number "$FULLNODE2_RPC")
BUILDER_DEC=$(printf '%d' "$BUILDER_BLK")
FN1_DEC=$(printf '%d' "$FN1_BLK")
FN2_DEC=$(printf '%d' "$FN2_BLK")
FN1_LAG=$((BUILDER_DEC - FN1_DEC))
FN2_LAG=$((BUILDER_DEC - FN2_DEC))
echo "Block lag: fn1=${FN1_LAG} fn2=${FN2_LAG}"
assert "TEST6: Fullnode1 lag < 10 blocks" '[ "$FN1_LAG" -lt 10 ]' "lag=$FN1_LAG"
assert "TEST6: Fullnode2 lag < 10 blocks" '[ "$FN2_LAG" -lt 10 ]' "lag=$FN2_LAG"

print_elapsed "TEST 6"
echo ""

# ══════════════════════════════════════════
#  TEST 7: Rapid Burst (5 Cross-Chain Calls)
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 7: Rapid Burst (5 Cross-Chain Calls)"
echo "========================================"
echo "Send 5 increment() calls with minimal delay."
echo "Stresses §4f prefix counting under higher load."
echo ""
start_timer

COUNT_BEFORE=$(get_counter)
echo "Counter before burst: $COUNT_BEFORE"

for i in 1 2 3 4 5; do
  S=$(send_increment)
  echo "  Call #$i: L1 tx status=$S"
  assert "TEST7: Burst call #$i L1 tx succeeded" '[ "$S" = "1" ]'
  sleep 1
done

echo "Waiting for pending submissions to clear (up to 240s)..."
PENDING_RESULT=$(wait_for_pending_zero 240)
echo "Pending submissions: $PENDING_RESULT"
assert "TEST7: No pending submissions after burst" '[ "$PENDING_RESULT" = "0" ]'

echo "Waiting for state root convergence (up to 180s)..."
ROOTS=$(wait_for_convergence 180)
echo "State roots after burst: $ROOTS"
assert "TEST7: State roots match after burst" '[ "$ROOTS" = "MATCH" ]'

COUNT_AFTER=$(get_counter)
echo "Counter after burst: $COUNT_AFTER"

if [ "$COUNT_BEFORE" != "?" ] && [ "$COUNT_AFTER" != "?" ]; then
  EXPECTED=$(python3 -c "print(int('$COUNT_BEFORE') + 5)")
  assert "TEST7: Counter incremented by exactly 5" '[ "$COUNT_AFTER" = "$EXPECTED" ]' \
    "expected=$EXPECTED actual=$COUNT_AFTER"
else
  assert "TEST7: Counter incremented by exactly 5" 'false' "could not read counter"
fi

HEALTH_POST=$(get_health)
HEALTHY_POST=$(echo "$HEALTH_POST" | jq -r '.healthy // false')
assert "TEST7: Builder is healthy after burst of 5" '[ "$HEALTHY_POST" = "true" ]'

sample_rewind_cycles

print_elapsed "TEST 7"
echo ""

# ══════════════════════════════════════════
#  FINAL STATE CHECK
# ══════════════════════════════════════════

echo "========================================"
echo "  FINAL STATE CHECK"
echo "========================================"
start_timer

FINAL_COUNTER=$(get_counter)
echo "Final counter value: $FINAL_COUNTER"

FINAL_HEALTH=$(get_health)
FINAL_MODE=$(echo "$FINAL_HEALTH" | jq -r '.mode // "UNKNOWN"')
FINAL_PENDING=$(echo "$FINAL_HEALTH" | jq -r '.pending_submissions // "?"')
FINAL_REWINDS=$(echo "$FINAL_HEALTH" | jq -r '.consecutive_rewind_cycles // "?"')
FINAL_HEALTHY=$(echo "$FINAL_HEALTH" | jq -r '.healthy // false')
echo "Builder mode: $FINAL_MODE"
echo "Pending submissions: $FINAL_PENDING"
echo "Consecutive rewind cycles: $FINAL_REWINDS"
echo "Healthy: $FINAL_HEALTHY"
echo "Peak rewind cycles observed during run: $PEAK_REWIND_CYCLES"

FINAL_ROOTS=$(check_state_roots)
echo "Final state roots: $FINAL_ROOTS"

ONCHAIN_SR_AFTER=$(get_onchain_state_root "$ROLLUPS_ADDRESS")
echo "On-chain stateRoot before: $ONCHAIN_SR_BEFORE"
echo "On-chain stateRoot after:  $ONCHAIN_SR_AFTER"

assert "FINAL: Builder in Builder mode" '[ "$FINAL_MODE" = "Builder" ]'
assert "FINAL: Builder is healthy" '[ "$FINAL_HEALTHY" = "true" ]'
assert "FINAL: State roots converged" '[ "$FINAL_ROOTS" = "MATCH" ]'
assert "FINAL: No pending submissions" '[ "$FINAL_PENDING" = "0" ]'
assert "FINAL: Rewind cycles cleared" '[ "$FINAL_REWINDS" = "0" ]'
assert "FINAL: On-chain stateRoot advanced" '[ "$ONCHAIN_SR_BEFORE" != "$ONCHAIN_SR_AFTER" ]'

print_elapsed "FINAL STATE CHECK"
echo ""

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════

if [ "$JSON_MODE" = "true" ]; then
  print_json_summary "crosschain"
else
  echo "========================================"
  echo "  CROSSCHAIN HEALTH CHECK RESULTS"
  echo "========================================"
  echo ""
  echo "  Passed: $PASS_COUNT"
  echo "  Failed: $FAIL_COUNT"
  echo "  Total:  $TOTAL_COUNT"
  echo ""
  echo "  Peak rewind cycles observed: $PEAK_REWIND_CYCLES"
  print_total_elapsed
  echo ""

  if [ "$FAIL_COUNT" -eq 0 ]; then
    echo "  STATUS: ALL TESTS PASSED"
    echo ""
    echo "========================================"
    exit 0
  else
    echo "  STATUS: $FAIL_COUNT TEST(S) FAILED"
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
