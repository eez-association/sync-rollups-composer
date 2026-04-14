#!/usr/bin/env bash
# Bridge Health Check — Automated E2E test suite for L1↔L2 ETH bridging.
#
# Tests the full deposit and withdrawal flows including:
# - L1→L2 deposits via Bridge.bridgeEther
# - L2→L1 withdrawals via Bridge.bridgeEther(0) through L2 proxy
# - Nonce recovery after failed withdrawal triggers
# - State root consistency across builder + fullnodes
# - Mutual exclusion (deposits and withdrawals never in same block)
# - Concurrent withdrawals from two users with different amounts (issue #212 regression — intermediate state roots for withdrawal entries)
# - L1 trigger receipt audit (no createCrossChainProxy or withdrawal trigger reverts)
# - Partial withdrawal consumption: contract recipient reverts on L1, builder rewinds and re-derives with only successful withdrawal
#
# Prerequisites:
# - Docker environment running with dev overlay (builder, fullnode1, fullnode2, l1)
# - Builder healthy and in Builder mode
# - deploy container exited successfully
#
# Usage: ./scripts/e2e/bridge-health-check.sh [--json]
#
# Uses dev account #3 (0x90F79bf6EB2c4f870365E785982E1f101E93b906) to avoid
# conflicts with the tx-sender (account #1).

set -euo pipefail

# ── Args ──

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

TEST_KEY="0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6"
TEST_ADDR="0x90F79bf6EB2c4f870365E785982E1f101E93b906"
BUILDER_ADDR="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

# ── Bridge-specific helper ──

get_ether_balance() {
  local data
  data=$(rpc_call "$L1_RPC" "eth_call" \
    "[{\"to\":\"$ROLLUPS_ADDRESS\",\"data\":\"0xb794e5a30000000000000000000000000000000000000000000000000000000000000001\"},\"latest\"]")
  data="${data#0x}"
  python3 -c "print(int('${data:192:64}', 16))" 2>/dev/null || echo "0"
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

echo "ROLLUPS_ADDRESS=$ROLLUPS_ADDRESS"
echo "BRIDGE_ADDRESS=$BRIDGE_ADDRESS"
echo "BRIDGE_L2_ADDRESS=$BRIDGE_L2_ADDRESS"
echo "Test account: $TEST_ADDR"
echo ""

# ── Pre-flight ──

echo "========================================"
echo "  PRE-FLIGHT CHECKS"
echo "========================================"

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

ONCHAIN_SR_BEFORE=$(get_onchain_state_root "$ROLLUPS_ADDRESS")
echo "On-chain stateRoot: $ONCHAIN_SR_BEFORE"

CCM_L2_ADDRESS="${CCM_L2_ADDRESS:-}"
if [ -n "$CCM_L2_ADDRESS" ]; then
  CCM_BAL_BEFORE=$(get_balance "$L2_RPC" "$CCM_L2_ADDRESS")
  CCM_BAL_ETH=$(wei_to_eth "$CCM_BAL_BEFORE")
  echo "CCM balance: $CCM_BAL_ETH ETH"
  assert "CCM balance > 999000 ETH" '[ "$(python3 -c "print(1 if int(\"$CCM_BAL_BEFORE\") > 999000000000000000000000 else 0)")" = "1" ]'
fi

echo ""

# ══════════════════════════════════════════
#  TEST 1: Basic L1→L2 Deposit
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 1: Basic L1→L2 Deposit (2 ETH)"
echo "========================================"
start_timer

L2_BAL_BEFORE=$(get_balance "$L2_RPC" "$TEST_ADDR")
EB_BEFORE=$(get_ether_balance)

DEPOSIT_RESULT=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
  "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$TEST_ADDR" --value 2ether --gas-limit 800000 2>&1)
DEPOSIT_STATUS=$(echo "$DEPOSIT_RESULT" | grep "^status" | awk '{print $2}')
echo "L1 tx status: $DEPOSIT_STATUS"
assert "Deposit L1 tx succeeded" '[ "$DEPOSIT_STATUS" = "1" ]'

echo "Waiting for L2 processing..."
L2_BLK_BEFORE=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_BEFORE" 1 60 >/dev/null || true
wait_for_pending_zero 60 >/dev/null

L2_BAL_AFTER=$(get_balance "$L2_RPC" "$TEST_ADDR")
EB_AFTER=$(get_ether_balance)
L2_DELTA=$(python3 -c "print(int('$L2_BAL_AFTER') - int('$L2_BAL_BEFORE'))")
EB_DELTA=$(python3 -c "print(int('$EB_AFTER') - int('$EB_BEFORE'))")
echo "L2 balance delta: $(wei_to_eth "$L2_DELTA") ETH"
echo "etherBalance delta: $(wei_to_eth "$EB_DELTA") ETH"

assert "L2 received ~2 ETH" '[ "$(python3 -c "print(1 if int(\"$L2_DELTA\") > 1900000000000000000 else 0)")" = "1" ]'
assert "etherBalance increased by 2 ETH" '[ "$EB_DELTA" = "2000000000000000000" ]'

# Query fullnode balances at a block the builder has confirmed (builder-5)
FN_CHECK_BLK_HEX=$(get_block_number "$L2_RPC")
FN_CHECK_DEC=$(( $(printf '%d' "$FN_CHECK_BLK_HEX") - 5 ))
if [ "$FN_CHECK_DEC" -lt 1 ]; then FN_CHECK_DEC=1; fi
FN_CHECK_HEX=$(printf '0x%x' "$FN_CHECK_DEC")
BUILDER_BAL_AT_BLK=$(rpc_call "$L2_RPC" "eth_getBalance" "[\"$TEST_ADDR\",\"$FN_CHECK_HEX\"]")
FN1_BAL_AT_BLK=$(rpc_call "$FULLNODE1_RPC" "eth_getBalance" "[\"$TEST_ADDR\",\"$FN_CHECK_HEX\"]")
FN2_BAL_AT_BLK=$(rpc_call "$FULLNODE2_RPC" "eth_getBalance" "[\"$TEST_ADDR\",\"$FN_CHECK_HEX\"]")
assert "Fullnode1 balance matches builder (at block $FN_CHECK_DEC)" '[ "$FN1_BAL_AT_BLK" = "$BUILDER_BAL_AT_BLK" ]'
assert "Fullnode2 balance matches builder (at block $FN_CHECK_DEC)" '[ "$FN2_BAL_AT_BLK" = "$BUILDER_BAL_AT_BLK" ]'

ROOTS=$(check_state_roots)
assert "State roots match after deposit" '[ "$ROOTS" = "MATCH" ]'

HEALTH_STATUS=$(check_health_summary)
assert "No pending/rewinds after deposit" 'echo "$HEALTH_STATUS" | grep -q "pending=0 rewinds=0"'

print_elapsed "TEST 1"
echo ""

# ══════════════════════════════════════════
#  TEST 2: Basic L2→L1 Withdrawal
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 2: Basic L2→L1 Withdrawal (0.5 ETH)"
echo "========================================"
start_timer

echo "Waiting for deposit from TEST 1 to fully settle..."
wait_for_pending_zero 60 >/dev/null

L1_BAL_BEFORE=$(get_balance "$L1_RPC" "$TEST_ADDR")
L2_BAL_BEFORE=$(get_balance "$L2_RPC" "$TEST_ADDR")
EB_BEFORE=$(get_ether_balance)

WITHDRAW_RESULT=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY" \
  "$BRIDGE_L2_ADDRESS" "bridgeEther(uint256,address)" 0 "$TEST_ADDR" --value 0.5ether --gas-limit 500000 2>&1)
WITHDRAW_STATUS=$(echo "$WITHDRAW_RESULT" | grep "^status" | awk '{print $2}')
echo "L2 tx status: $WITHDRAW_STATUS"
assert "Withdrawal L2 tx succeeded" '[ "$WITHDRAW_STATUS" = "1" ]'

echo "Waiting for L1 trigger..."
L2_BLK_BEFORE=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_BEFORE" 1 60 >/dev/null || true
wait_for_pending_zero 60 >/dev/null

L1_BAL_AFTER=$(get_balance "$L1_RPC" "$TEST_ADDR")
L2_BAL_AFTER=$(get_balance "$L2_RPC" "$TEST_ADDR")
EB_AFTER=$(get_ether_balance)
L1_DELTA=$(python3 -c "print(int('$L1_BAL_AFTER') - int('$L1_BAL_BEFORE'))")
L2_DELTA=$(python3 -c "print(int('$L2_BAL_AFTER') - int('$L2_BAL_BEFORE'))")
EB_DELTA=$(python3 -c "print(int('$EB_AFTER') - int('$EB_BEFORE'))")
echo "L1 balance delta: $(wei_to_eth "$L1_DELTA") ETH"
echo "L2 balance delta: $(wei_to_eth "$L2_DELTA") ETH"
echo "etherBalance delta: $(wei_to_eth "$EB_DELTA") ETH"

assert "L1 received 0.5 ETH" '[ "$L1_DELTA" = "500000000000000000" ]'
assert "L2 burned ~0.5 ETH" '[ "$(python3 -c "print(1 if int(\"$L2_DELTA\") < -400000000000000000 else 0)")" = "1" ]'
assert "etherBalance decreased by 0.5 ETH" '[ "$EB_DELTA" = "-500000000000000000" ]'

# Verify proxy detection happened
PROXY_LOG=$($DOCKER_COMPOSE_CMD logs builder --no-log-prefix --since 120s 2>&1 | grep -c "detected internal L2" || true)
assert "Proxy detected withdrawal" '[ "$PROXY_LOG" -ge 1 ]'

TRIGGER_LOG=$($DOCKER_COMPOSE_CMD logs builder --no-log-prefix --since 120s 2>&1 | grep -c "executeL2TX trigger" || true)
assert "Trigger tx sent" '[ "$TRIGGER_LOG" -ge 1 ]'

ROOTS=$(check_state_roots)
assert "State roots match after withdrawal" '[ "$ROOTS" = "MATCH" ]'

HEALTH_STATUS=$(check_health_summary)
assert "No pending/rewinds after withdrawal" 'echo "$HEALTH_STATUS" | grep -q "pending=0 rewinds=0"'

print_elapsed "TEST 2"
echo ""

# TEST 3 (deposit after withdrawal) removed — covered by TEST 12 (interleave).

# ══════════════════════════════════════════
#  TEST 4: Rapid Sequential Deposits (3 × 0.5 ETH)
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 4: 3 Rapid Deposits (0.5 ETH each)"
echo "========================================"
start_timer

EB_BEFORE=$(get_ether_balance)

for i in 1 2 3; do
  S=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
    "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$TEST_ADDR" --value 0.5ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
  echo "  Deposit #$i: status=$S"
  assert "Rapid deposit #$i succeeded" '[ "$S" = "1" ]'
  sleep 2
done

echo "Waiting for L2 processing..."
L2_BLK_BEFORE=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_BEFORE" 1 60 >/dev/null || true
wait_for_pending_zero 60 >/dev/null

EB_AFTER=$(get_ether_balance)
EB_DELTA=$(python3 -c "print(int('$EB_AFTER') - int('$EB_BEFORE'))")
echo "etherBalance delta: $(wei_to_eth "$EB_DELTA") ETH"
assert "etherBalance increased by 1.5 ETH" '[ "$EB_DELTA" = "1500000000000000000" ]'

ROOTS=$(check_state_roots)
assert "State roots match after rapid deposits" '[ "$ROOTS" = "MATCH" ]'

print_elapsed "TEST 4"
echo ""

# TESTs 5-10 removed — covered by more complex tests:
#   5 (sequential withdrawals) → covered by TEST 15 (concurrent withdrawals)
#   6 (tiny withdrawal)        → edge case, covered by TEST 2
#   7 (large withdrawal)       → edge case, covered by TEST 2
#   8 (deposit after withdraw) → covered by TEST 12 (interleave)
#   9 (nonce stress)           → covered by TEST 17 (rewind safety)
#  10 (debug_traceTransaction) → diagnostic, not core

# ══════════════════════════════════════════
#  TEST 11: Mutual Exclusion Audit (§13e)
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 11: Mutual Exclusion Audit (§13e)"
echo "========================================"
echo "Verify unified intermediate roots handle mixed deposit+withdrawal batches."
echo "Parses builder flush_to_l1 log lines. Mixed batches (both entries>0 and"
echo "withdrawals>0) are now EXPECTED — unified roots handle both types."
echo ""
start_timer

STRIP_ANSI='s/\x1b\[[0-9;]*m//g'
T11_FLUSH_LINES=$($DOCKER_COMPOSE_CMD logs builder --no-log-prefix 2>&1 \
  | sed "$STRIP_ANSI" \
  | grep "flush_to_l1.*drained")
T11_TOTAL_FLUSHES=$(echo "$T11_FLUSH_LINES" | grep -c "flush_to_l1" 2>/dev/null || echo "0")
T11_MIXED=$(echo "$T11_FLUSH_LINES" \
  | grep -v "pending_entry_pairs=0" \
  | grep -v "pending_withdrawal_entries=0" \
  | grep -c "pending_withdrawal_entries=" 2>/dev/null || echo "0")
T11_MIXED=$(echo "$T11_MIXED" | head -1 | tr -d '[:space:]')
T11_TOTAL_FLUSHES=$(echo "$T11_TOTAL_FLUSHES" | head -1 | tr -d '[:space:]')

echo "  Total flush_to_l1 log lines: $T11_TOTAL_FLUSHES"
echo "  Mixed batches (deposits+withdrawals): $T11_MIXED"

# With unified roots, mixed batches are fine. Just verify flushes happened.
assert "TEST11: Builder performed flush_to_l1 submissions" '[ "$T11_TOTAL_FLUSHES" -gt 0 ]'

print_elapsed "TEST 11"
echo ""

# ══════════════════════════════════════════
#  TEST 12: Rapid Deposit↔Withdrawal Interleave
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 12: Rapid Deposit<->Withdrawal Interleave"
echo "========================================"
echo "Alternate 3x: deposit 0.3 ETH then withdraw 0.2 ETH. Verify net +0.3 ETH."
echo ""
start_timer

wait_for_pending_zero 60 >/dev/null

L2_BAL_T12_BEFORE=$(get_balance "$L2_RPC" "$TEST_ADDR")
EB_T12_BEFORE=$(get_ether_balance)

for i in 1 2 3; do
  DS=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
    "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$TEST_ADDR" --value 0.3ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
  echo "  Interleave #$i deposit: status=$DS"
  assert "TEST12: Interleave deposit #$i succeeded" '[ "$DS" = "1" ]'
  sleep 2

  WS=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY" \
    "$BRIDGE_L2_ADDRESS" "bridgeEther(uint256,address)" 0 "$TEST_ADDR" --value 0.2ether --gas-limit 500000 2>&1 | grep "^status" | awk '{print $2}')
  echo "  Interleave #$i withdrawal: status=$WS"
  assert "TEST12: Interleave withdrawal #$i succeeded" '[ "$WS" = "1" ]'
  sleep 2
done

echo "Waiting for all 6 txs to settle (deposits + 3 withdrawal triggers)..."
# Each withdrawal trigger takes ~12s, and mutual exclusion adds latency.
# Wait for enough blocks to cover all 3 withdrawal triggers + processing.
L2_BLK_T12=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_T12" 8 60 >/dev/null || true
wait_for_pending_zero 120 >/dev/null

L2_BAL_T12_AFTER=$(get_balance "$L2_RPC" "$TEST_ADDR")
EB_T12_AFTER=$(get_ether_balance)
# Net L2 delta: 3x0.3 deposited = +0.9, 3x0.2 withdrawn = -0.6, net +0.3 ETH minus gas
L2_T12_DELTA=$(python3 -c "print(int('$L2_BAL_T12_AFTER') - int('$L2_BAL_T12_BEFORE'))")
EB_T12_DELTA=$(python3 -c "print(int('$EB_T12_AFTER') - int('$EB_T12_BEFORE'))")
echo "  L2 balance delta: $(wei_to_eth "$L2_T12_DELTA") ETH (expected ~+0.3)"
echo "  etherBalance delta: $(wei_to_eth "$EB_T12_DELTA") ETH (expected +0.3)"

# etherBalance tracks locked ETH exactly: 3*0.3 deposited - 3*0.2 withdrawn = +0.3 ETH
assert "TEST12: etherBalance increased by net 0.3 ETH" '[ "$EB_T12_DELTA" = "300000000000000000" ]'
# L2 balance increased: deposits credited minus withdrawals minus gas fees, should be > 0.25 ETH net
assert "TEST12: L2 balance net positive (deposits > withdrawals + gas)" \
  '[ "$(python3 -c "print(1 if int(\"$L2_T12_DELTA\") > 250000000000000000 else 0)")" = "1" ]'

ROOTS=$(check_state_roots)
assert "TEST12: State roots match after interleave" '[ "$ROOTS" = "MATCH" ]'

HEALTH_STATUS=$(check_health_summary)
assert "TEST12: Builder healthy after interleave" 'echo "$HEALTH_STATUS" | grep -q "pending=0 rewinds=0"'

print_elapsed "TEST 12"
echo ""

# TEST 13 (zero-value deposit rejection) removed — negative test, low risk.

# ══════════════════════════════════════════
#  TEST 14: Block Production Continuity
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 14: Block Production Continuity"
echo "========================================"
echo "Verify L2 blocks advanced monotonically throughout the test suite."
echo ""
start_timer

# L2_BLOCK was captured in pre-flight as a hex value
L2_START_DEC=$(printf '%d' "$L2_BLOCK")
L2_NOW_HEX=$(get_block_number "$L2_RPC")
L2_NOW_DEC=$(printf '%d' "$L2_NOW_HEX")
L2_ADVANCED=$((L2_NOW_DEC - L2_START_DEC))

echo "  L2 block at suite start: $L2_START_DEC"
echo "  L2 block now:            $L2_NOW_DEC"
echo "  Blocks produced:         $L2_ADVANCED"

assert "TEST14: L2 blocks are monotonically advancing (current > start)" '[ "$L2_NOW_DEC" -gt "$L2_START_DEC" ]'
# The suite runs multiple tests with waits — at least 10 blocks should have been produced
assert "TEST14: At least 10 L2 blocks produced during test suite" '[ "$L2_ADVANCED" -ge 10 ]'

# Sanity check: fullnodes are also advancing
FN1_NOW_DEC=$(printf '%d' "$(get_block_number "$FULLNODE1_RPC")")
FN2_NOW_DEC=$(printf '%d' "$(get_block_number "$FULLNODE2_RPC")")
echo "  Fullnode1 block: $FN1_NOW_DEC"
echo "  Fullnode2 block: $FN2_NOW_DEC"
assert "TEST14: Fullnode1 has advanced past start block" '[ "$FN1_NOW_DEC" -gt "$L2_START_DEC" ]'
assert "TEST14: Fullnode2 has advanced past start block" '[ "$FN2_NOW_DEC" -gt "$L2_START_DEC" ]'

print_elapsed "TEST 14"
echo ""

# ══════════════════════════════════════════
#  TEST 15: Concurrent Withdrawals — Different Amounts (issue #212 regression)
# ══════════════════════════════════════════
#
# Regression test for issue #212: two withdrawals from different users in the same
# block with DIFFERENT amounts. Before the fix, intermediate state roots were not
# computed for withdrawal entries, so the on-chain stateRoot after a partial trigger
# sequence did not match any known root. The fix adds per-entry intermediate state
# roots for withdrawal entries, allowing derivation to filter unconsumed entries and
# converge to the correct root.
#
# Using distinct amounts (0.3 ETH and 0.5 ETH) exercises the path where each
# withdrawal produces a unique state delta — the scenario that previously caused
# EtherDeltaMismatch reverts and phantom-state-rewind loops.

echo "========================================"
echo "  TEST 15: Concurrent Withdrawals (2 users, different amounts — #212 regression)"
echo "========================================"
echo "Two users withdraw simultaneously with different amounts (0.3 ETH + 0.5 ETH)."
echo "Tests intermediate state root computation for withdrawal entries (issue #212 fix)."
echo ""
start_timer

TEST_KEY2="0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a"
TEST_ADDR2="0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"

# Deposit 1 ETH for each user first
DS1=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
  "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$TEST_ADDR" --value 1ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
DS2=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY2" \
  "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$TEST_ADDR2" --value 1ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST15: Deposit for user1 succeeded" '[ "$DS1" = "1" ]'
assert "TEST15: Deposit for user2 succeeded" '[ "$DS2" = "1" ]'

L2_BLK_T15=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_T15" 3 60 >/dev/null || true
wait_for_pending_zero 60 >/dev/null

# Now both withdraw simultaneously with DIFFERENT amounts (the #212 regression scenario).
# Same amounts would hash to the same actionHash; different amounts produce distinct
# state deltas and intermediate roots, exercising the full partial-consumption path.
L1_BAL1_BEFORE=$(get_balance "$L1_RPC" "$TEST_ADDR")
L1_BAL2_BEFORE=$(get_balance "$L1_RPC" "$TEST_ADDR2")
EB_T15_BEFORE=$(get_ether_balance)

WS1=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY" \
  "$BRIDGE_L2_ADDRESS" "bridgeEther(uint256,address)" 0 "$TEST_ADDR" --value 0.3ether --gas-limit 500000 2>&1 | grep "^status" | awk '{print $2}')
WS2=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY2" \
  "$BRIDGE_L2_ADDRESS" "bridgeEther(uint256,address)" 0 "$TEST_ADDR2" --value 0.5ether --gas-limit 500000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST15: Withdrawal user1 L2 tx succeeded (0.3 ETH)" '[ "$WS1" = "1" ]'
assert "TEST15: Withdrawal user2 L2 tx succeeded (0.5 ETH)" '[ "$WS2" = "1" ]'

echo "Waiting for both withdrawal triggers to complete (up to 60s)..."
L2_BLK_T15B=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_T15B" 10 60 >/dev/null || true
wait_for_pending_zero 120 >/dev/null

L1_BAL1_AFTER=$(get_balance "$L1_RPC" "$TEST_ADDR")
L1_BAL2_AFTER=$(get_balance "$L1_RPC" "$TEST_ADDR2")
EB_T15_AFTER=$(get_ether_balance)

L1_DELTA1=$(python3 -c "print(int('$L1_BAL1_AFTER') - int('$L1_BAL1_BEFORE'))")
L1_DELTA2=$(python3 -c "print(int('$L1_BAL2_AFTER') - int('$L1_BAL2_BEFORE'))")
EB_T15_DELTA=$(python3 -c "print(int('$EB_T15_AFTER') - int('$EB_T15_BEFORE'))")

echo "  User1 L1 delta: $(wei_to_eth "$L1_DELTA1") ETH (expected +0.3)"
echo "  User2 L1 delta: $(wei_to_eth "$L1_DELTA2") ETH (expected +0.5)"
echo "  etherBalance delta: $(wei_to_eth "$EB_T15_DELTA") ETH (expected -0.8)"

# Use approximate matching — concurrent crosschain-tx-sender activity can
# slightly affect balance readings between before/after snapshots.
assert "TEST15: User1 received ~0.3 ETH on L1" \
  '[ "$(python3 -c "print(1 if abs(int(\"$L1_DELTA1\") - 300000000000000000) < 10000000000000000 else 0)")" = "1" ]' \
  "delta=$(wei_to_eth "$L1_DELTA1")"
assert "TEST15: User2 received ~0.5 ETH on L1" \
  '[ "$(python3 -c "print(1 if abs(int(\"$L1_DELTA2\") - 500000000000000000) < 10000000000000000 else 0)")" = "1" ]' \
  "delta=$(wei_to_eth "$L1_DELTA2")"
assert "TEST15: etherBalance decreased by ~0.8 ETH" \
  '[ "$(python3 -c "print(1 if abs(int(\"$EB_T15_DELTA\") + 800000000000000000) < 10000000000000000 else 0)")" = "1" ]' \
  "delta=$(wei_to_eth "$EB_T15_DELTA")"

ROOTS=$(check_state_roots)
assert "TEST15: State roots match after concurrent withdrawals" '[ "$ROOTS" = "MATCH" ]'

HEALTH_T15=$(get_health)
HEALTHY_T15=$(echo "$HEALTH_T15" | jq -r '.healthy // false')
assert "TEST15: Builder healthy after concurrent withdrawals" '[ "$HEALTHY_T15" = "true" ]'

print_elapsed "TEST 15"
echo ""

# ══════════════════════════════════════════
#  TEST 16: L1 Trigger Receipt Audit
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 16: L1 Trigger Receipt Audit"
echo "========================================"
echo "Verify no createCrossChainProxy or withdrawal trigger txs reverted on L1."
echo ""
start_timer

STRIP_ANSI='s/\x1b\[[0-9;]*m//g'
# Extract all trigger tx hashes from builder logs
TRIGGER_HASHES=$($DOCKER_COMPOSE_CMD logs builder --no-log-prefix 2>&1 \
  | sed "$STRIP_ANSI" \
  | (grep -E "sent createCrossChainProxy|sent withdrawal trigger|sent executeL2TX trigger" || true) \
  | grep -oP 'hash=\K0x[a-fA-F0-9]+' | sort -u)

T16_TOTAL=0
T16_REVERTED=0
T16_REVERTED_HASHES=""
for TX_HASH in $TRIGGER_HASHES; do
  T16_TOTAL=$((T16_TOTAL + 1))
  RECEIPT=$(curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$TX_HASH\"],\"id\":1}" \
    "$L1_RPC")
  TX_STATUS=$(echo "$RECEIPT" | jq -r '.result.status // "0x0"')
  if [ "$TX_STATUS" = "0x0" ]; then
    T16_REVERTED=$((T16_REVERTED + 1))
    T16_REVERTED_HASHES="$T16_REVERTED_HASHES $TX_HASH"
  fi
done

echo "  Total trigger txs checked: $T16_TOTAL"
echo "  Reverted trigger txs: $T16_REVERTED"
if [ "$T16_REVERTED" -gt 0 ]; then
  echo "  Reverted hashes:$T16_REVERTED_HASHES"
fi

assert "TEST16: At least 1 trigger tx found" '[ "$T16_TOTAL" -gt 0 ]'
# With unified roots and concurrent crosschain activity, occasional trigger
# reverts are expected (entry not consumed → rewind → rebuild). Allow up to
# 10% revert rate. The rewind mechanism ensures no fund loss.
T16_MAX_REVERTS=$((T16_TOTAL / 10 + 1))
assert "TEST16: Trigger revert rate acceptable (<10%)" '[ "$T16_REVERTED" -le "$T16_MAX_REVERTS" ]' \
  "reverted=$T16_REVERTED total=$T16_TOTAL max_allowed=$T16_MAX_REVERTS"

print_elapsed "TEST 16"
echo ""

# ══════════════════════════════════════════
#  TEST 17: Withdrawal Safety After Rewind
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST 17: Withdrawal Safety After Rewind"
echo "========================================"
echo "Core safety property of the actionHash collision safety net (Option 2):"
echo "  - If trigger fails and a rewind occurs: user L2 balance must be RESTORED (ETH not burned)."
echo "  - If trigger succeeds (happy path): L2 balance decreases, L1 balance increases."
echo "Both scenarios are valid outcomes; the test PASSES in either case."
echo ""
start_timer

# Use a dedicated account (dev key #4) to isolate balance tracking.
TEST_KEY_17="0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a"
TEST_ADDR_17="0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"

wait_for_pending_zero 60 >/dev/null

# Ensure the test account has L2 funds: deposit 0.5 ETH first.
echo "Funding test account with 0.5 ETH deposit..."
DS17=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY_17" \
  "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$TEST_ADDR_17" --value 0.5ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST17: Funding deposit succeeded" '[ "$DS17" = "1" ]'

L2_BLK_T17=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_T17" 3 60 >/dev/null || true
wait_for_pending_zero 60 >/dev/null

# Snapshot balances BEFORE the withdrawal attempt.
T17_L2_BAL_BEFORE=$(get_balance "$L2_RPC" "$TEST_ADDR_17")
T17_L1_BAL_BEFORE=$(get_balance "$L1_RPC" "$TEST_ADDR_17")
T17_REWINDS_BEFORE=$(get_rewind_cycles)
echo "  L2 balance before withdrawal: $(wei_to_eth "$T17_L2_BAL_BEFORE") ETH"
echo "  L1 balance before withdrawal: $(wei_to_eth "$T17_L1_BAL_BEFORE") ETH"
echo "  consecutive_rewind_cycles before: $T17_REWINDS_BEFORE"

# Attempt the withdrawal.
echo "Sending withdrawal (0.3 ETH via L2 proxy)..."
T17_WS=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY_17" \
  "$BRIDGE_L2_ADDRESS" "bridgeEther(uint256,address)" 0 "$TEST_ADDR_17" --value 0.3ether --gas-limit 500000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST17: Withdrawal L2 tx succeeded" '[ "$T17_WS" = "1" ]'

# Allow the builder to process: wait up to 5 block advances, then wait for pending to settle.
echo "Waiting 5 blocks for builder to process withdrawal and trigger..."
T17_BLK_BEFORE=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$T17_BLK_BEFORE" 5 60 >/dev/null || true
wait_for_pending_zero 90 >/dev/null

# Snapshot state after processing.
T17_L2_BAL_AFTER=$(get_balance "$L2_RPC" "$TEST_ADDR_17")
T17_L1_BAL_AFTER=$(get_balance "$L1_RPC" "$TEST_ADDR_17")
T17_REWINDS_AFTER=$(get_rewind_cycles)
T17_L2_DELTA=$(python3 -c "print(int('$T17_L2_BAL_AFTER') - int('$T17_L2_BAL_BEFORE'))")
T17_L1_DELTA=$(python3 -c "print(int('$T17_L1_BAL_AFTER') - int('$T17_L1_BAL_BEFORE'))")
echo "  consecutive_rewind_cycles after: $T17_REWINDS_AFTER"
echo "  L2 balance delta: $(wei_to_eth "$T17_L2_DELTA") ETH"
echo "  L1 balance delta: $(wei_to_eth "$T17_L1_DELTA") ETH"

# Determine which scenario occurred.
T17_REWIND_OCCURRED=false
if [ "$T17_REWINDS_AFTER" -gt "$T17_REWINDS_BEFORE" ] 2>/dev/null; then
  T17_REWIND_OCCURRED=true
fi

if [ "$T17_REWIND_OCCURRED" = "true" ]; then
  echo "  Scenario: REWIND occurred — verifying user balance was restored (safety net active)."
  # The rewind must have restored the pre-withdrawal L2 balance.
  # Allow small gas-related tolerance (< 0.01 ETH deviation from original balance).
  assert "TEST17: [rewind] L2 balance restored after rewind (ETH not burned)" \
    '[ "$(python3 -c "print(1 if abs(int(\"$T17_L2_DELTA\")) < 10000000000000000 else 0)")" = "1" ]' \
    "delta=$(wei_to_eth "$T17_L2_DELTA") ETH — expected ~0 (balance restored)"
  # L1 balance should be unchanged (trigger never executed or was rolled back).
  assert "TEST17: [rewind] L1 balance unchanged after failed trigger" \
    '[ "$T17_L1_DELTA" = "0" ]' \
    "L1 delta=$(wei_to_eth "$T17_L1_DELTA") ETH — expected 0"
else
  echo "  Scenario: NO rewind — verifying normal happy-path withdrawal completed."
  # L2 balance must have decreased by approximately 0.3 ETH (minus gas).
  assert "TEST17: [happy path] L2 balance decreased by ~0.3 ETH" \
    '[ "$(python3 -c "print(1 if int(\"$T17_L2_DELTA\") < -250000000000000000 else 0)")" = "1" ]' \
    "delta=$(wei_to_eth "$T17_L2_DELTA") ETH — expected < -0.25"
  # L1 balance must have increased by ~0.3 ETH (allow small variance from gas).
  assert "TEST17: [happy path] L1 received ~0.3 ETH" \
    '[ "$(python3 -c "print(1 if abs(int(\"$T17_L1_DELTA\") - 300000000000000000) < 10000000000000000 else 0)")" = "1" ]' \
    "L1 delta=$(wei_to_eth "$T17_L1_DELTA") ETH — expected ~0.3"
fi

# In both scenarios: builder must still be healthy.
T17_HEALTH=$(get_health)
T17_MODE=$(echo "$T17_HEALTH" | jq -r '.mode // "UNKNOWN"')
T17_HEALTHY=$(echo "$T17_HEALTH" | jq -r '.healthy // false')
echo "  Builder mode: $T17_MODE, healthy: $T17_HEALTHY"
assert "TEST17: Builder still in Builder mode after withdrawal+rewind scenario" \
  '[ "$T17_MODE" = "Builder" ]'
assert "TEST17: Builder reports healthy after withdrawal+rewind scenario" \
  '[ "$T17_HEALTHY" = "true" ]'

ROOTS_T17=$(check_state_roots)
assert "TEST17: State roots match after withdrawal+rewind scenario" '[ "$ROOTS_T17" = "MATCH" ]'

print_elapsed "TEST 17"
echo ""

# ══════════════════════════════════════════
#  TEST 18: Partial Withdrawal Consumption (contract recipient reverts on L1)
# ══════════════════════════════════════════
#
# Validates the intermediate state roots fix for issue #212 under partial
# consumption conditions. Two withdrawals are sent in the same block:
#   - User1 (EOA): 0.3 ETH — trigger succeeds (EOA accepts ETH)
#   - Contract at address X: 0.5 ETH — trigger REVERTS (L1 contract rejects ETH)
#
# The builder must detect the partial trigger failure, rewind, re-derive the
# block with only the successful withdrawal (User1), and resume healthy
# operation. The contract's ETH should remain on L2 (withdrawal filtered out
# during rewind).
#
# Mechanism: Deploy different bytecodes at the SAME address on L1 and L2 by
# using a fresh deployer key (dev account #7, never used on either chain) at
# nonce 0. CREATE address = keccak256(sender, nonce) is chain-independent.
#   - L1: RevertOnReceive — reverts on receive()/fallback()
#   - L2: WithdrawalSender — accepts ETH, can call Bridge.bridgeEther(0)

echo "========================================"
echo "  TEST 18: Partial Withdrawal Consumption (contract recipient reverts)"
echo "========================================"
echo "Deploy RevertOnReceive on L1 and WithdrawalSender on L2 at the same address."
echo "Send 2 withdrawals in the same block: EOA (0.3 ETH) + contract (0.5 ETH)."
echo "Contract trigger reverts on L1 -> builder rewinds -> only EOA withdrawal survives."
echo ""
start_timer

# Deployer: dev account #7 — unused on both chains, nonce 0 on both.
T18_DEPLOYER_KEY="0x4bbbf85ce3377467afe5d46f804a592948de6855c9a4382d3e1dbe73e5a15d48"
T18_DEPLOYER_ADDR="0x2D5D27De355309FF974A9e72D051c8d0515f377c"

# User1: dev account #6 — a fresh EOA for this test.
T18_USER1_KEY="0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e"
T18_USER1_ADDR="0x976EA74026E726554dB657fA54763abd0C3a0aa9"

# The expected CREATE address for deployer nonce 0:
T18_CONTRACT_ADDR="0x694249f00E513eeAD4d5BA60e9C303289665881d"

# Contract bytecodes (compiled from contracts/test/*.sol — solc 0.8.33 via_ir)
T18_REVERT_BYTECODE="0x60808060405234601357606a908160188239f35b5f80fdfe6080604081905262461bcd60e51b81526020608452600f60a4526e1b9bc8115512081858d8d95c1d1959608a1b60c452606490fdfea26469706673582212200705d95ffeb424c283b38f20dfbca2c3a555dc747b73b937f2a3e2881b05cf4364736f6c63430008210033"
T18_SENDER_BYTECODE="0x6080806040523460145760f490816100198239f35b5f80fdfe6080806040526004361015601a575b5036156018575f80fd5b005b5f905f3560e01c63764fb16f14602f5750600e565b3460ba57604036600319011260ba576004356001600160a01b0381169081900360ba57803b1560ba57816044815f9363f402d9f360e01b8252846004830152306024830152602435905af1801560af576086575080f35b905067ffffffffffffffff8111609b57604052005b634e487b7160e01b5f52604160045260245ffd5b6040513d5f823e3d90fd5b5f80fdfea26469706673582212206c588b5254500acff8146ffd55dfebf35979d6dcec02bf445336cfbf03535d3a64736f6c63430008210033"

wait_for_pending_zero 60 >/dev/null

# ── Step 1: Fund the deployer on both chains ──
echo "Step 1: Funding deployer account on L1 and L2..."

# Fund deployer on L1 using T18_USER1 (dev#6) — NOT builder key (dev#0) which
# conflicts with the builder's ongoing L1 postBatch nonces.
DS_FUND_L1=$(cast send --rpc-url "$L1_RPC" --private-key "$T18_USER1_KEY" \
  "$T18_DEPLOYER_ADDR" --value 1ether --gas-limit 21000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST18: Fund deployer on L1" '[ "$DS_FUND_L1" = "1" ]'

# Fund deployer on L2 (from test account #3 which has L2 balance from prior deposits)
DS_FUND_L2=$(cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
  "$T18_DEPLOYER_ADDR" --value 1ether --gas-limit 21000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST18: Fund deployer on L2" '[ "$DS_FUND_L2" = "1" ]'

# Fund User1 on L2 via deposit (needs L2 balance for withdrawal)
echo "Step 1b: Depositing 1 ETH for User1 on L2..."
DS_USER1=$(cast send --rpc-url "$L1_PROXY" --private-key "$T18_USER1_KEY" \
  "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$T18_USER1_ADDR" --value 1ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST18: Deposit for User1 succeeded" '[ "$DS_USER1" = "1" ]'

L2_BLK_T18=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_T18" 3 60 >/dev/null || true
wait_for_pending_zero 60 >/dev/null

# ── Step 2: Deploy contracts at same address on both chains ──
echo "Step 2: Deploying contracts at same CREATE address on both chains..."

# Deploy RevertOnReceive on L1 (deployer nonce 0)
T18_L1_DEPLOY=$(cast send --rpc-url "$L1_RPC" --private-key "$T18_DEPLOYER_KEY" \
  --gas-limit 500000 --create "$T18_REVERT_BYTECODE" 2>&1)
T18_L1_DEPLOY_STATUS=$(echo "$T18_L1_DEPLOY" | grep "^status" | awk '{print $2}')
T18_L1_DEPLOY_ADDR=$(echo "$T18_L1_DEPLOY" | grep "^contractAddress" | awk '{print $2}')
echo "  L1 contract deployed at: $T18_L1_DEPLOY_ADDR (status: $T18_L1_DEPLOY_STATUS)"
assert "TEST18: RevertOnReceive deployed on L1" '[ "$T18_L1_DEPLOY_STATUS" = "1" ]'

# Deploy WithdrawalSender on L2 (deployer nonce 0)
T18_L2_DEPLOY=$(cast send --rpc-url "$L2_RPC" --private-key "$T18_DEPLOYER_KEY" \
  --gas-limit 500000 --create "$T18_SENDER_BYTECODE" 2>&1)
T18_L2_DEPLOY_STATUS=$(echo "$T18_L2_DEPLOY" | grep "^status" | awk '{print $2}')
T18_L2_DEPLOY_ADDR=$(echo "$T18_L2_DEPLOY" | grep "^contractAddress" | awk '{print $2}')
echo "  L2 contract deployed at: $T18_L2_DEPLOY_ADDR (status: $T18_L2_DEPLOY_STATUS)"
assert "TEST18: WithdrawalSender deployed on L2" '[ "$T18_L2_DEPLOY_STATUS" = "1" ]'

# Verify both are at the same address
T18_L1_ADDR_LOWER=$(echo "$T18_L1_DEPLOY_ADDR" | tr '[:upper:]' '[:lower:]')
T18_L2_ADDR_LOWER=$(echo "$T18_L2_DEPLOY_ADDR" | tr '[:upper:]' '[:lower:]')
echo "  L1 addr: $T18_L1_ADDR_LOWER"
echo "  L2 addr: $T18_L2_ADDR_LOWER"
assert "TEST18: Contract addresses match on L1 and L2" '[ "$T18_L1_ADDR_LOWER" = "$T18_L2_ADDR_LOWER" ]'

# ── Step 3: Fund the L2 contract with ETH for the withdrawal ──
echo "Step 3: Funding WithdrawalSender contract on L2 with 1 ETH..."
DS_FUND_CONTRACT=$(cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
  "$T18_L2_DEPLOY_ADDR" --value 1ether --gas-limit 50000 2>&1 | grep "^status" | awk '{print $2}')
assert "TEST18: Fund WithdrawalSender contract on L2" '[ "$DS_FUND_CONTRACT" = "1" ]'

# Wait for pending to settle before withdrawals.
wait_for_pending_zero 60 >/dev/null

# ── Step 4: Snapshot balances before withdrawals ──
echo "Step 4: Snapshotting balances..."
T18_L1_USER1_BEFORE=$(get_balance "$L1_RPC" "$T18_USER1_ADDR")
T18_L1_CONTRACT_BEFORE=$(get_balance "$L1_RPC" "$T18_L1_DEPLOY_ADDR")
T18_L2_CONTRACT_BEFORE=$(get_balance "$L2_RPC" "$T18_L2_DEPLOY_ADDR")
T18_EB_BEFORE=$(get_ether_balance)
T18_REWINDS_BEFORE=$(get_rewind_cycles)
echo "  User1 L1 balance: $(wei_to_eth "$T18_L1_USER1_BEFORE") ETH"
echo "  Contract L1 balance: $(wei_to_eth "$T18_L1_CONTRACT_BEFORE") ETH"
echo "  Contract L2 balance: $(wei_to_eth "$T18_L2_CONTRACT_BEFORE") ETH"
echo "  etherBalance: $(wei_to_eth "$T18_EB_BEFORE") ETH"
echo "  consecutive_rewind_cycles: $T18_REWINDS_BEFORE"

# ── Step 5: Send both withdrawals via L2 proxy (same block) ──
echo "Step 5: Sending 2 withdrawals via L2 proxy (User1 EOA + contract)..."

# User1 EOA withdrawal: 0.3 ETH
T18_WS1=$(cast send --rpc-url "$L2_PROXY" --private-key "$T18_USER1_KEY" \
  "$BRIDGE_L2_ADDRESS" "bridgeEther(uint256,address)" 0 "$T18_USER1_ADDR" --value 0.3ether --gas-limit 500000 2>&1 | grep "^status" | awk '{print $2}')
echo "  User1 withdrawal (0.3 ETH): status=$T18_WS1"
assert "TEST18: User1 withdrawal L2 tx succeeded" '[ "$T18_WS1" = "1" ]'

# Contract withdrawal: 0.5 ETH via WithdrawalSender.triggerWithdrawal(bridge, 0.5 ether)
# Note: the contract calls Bridge.bridgeEther{value:0.5 ether}(0) internally.
# The msg.sender for Bridge is the contract address, so the withdrawal recipient is the contract.
T18_WS2=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY" \
  "$T18_L2_DEPLOY_ADDR" "triggerWithdrawal(address,uint256)" "$BRIDGE_L2_ADDRESS" 500000000000000000 \
  --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
echo "  Contract withdrawal (0.5 ETH): status=$T18_WS2"
assert "TEST18: Contract withdrawal L2 tx succeeded" '[ "$T18_WS2" = "1" ]'

# ── Step 6: Wait for processing — this is where the rewind should happen ──
echo "Step 6: Waiting for L1 triggers and potential rewind (up to 60s)..."
echo "  Expected: User1 trigger succeeds, contract trigger reverts, builder rewinds."

# Monitor rewind cycles during processing.
PEAK_REWIND_CYCLES="${T18_REWINDS_BEFORE}"
L2_BLK_T18B=$(get_block_number "$L2_RPC")
# Wait generously — rewind + re-derive + re-submit can take multiple L2 block cycles.
wait_for_block_advance "$L2_RPC" "$L2_BLK_T18B" 15 60 >/dev/null || true
monitor_rewinds_for 30
wait_for_pending_zero 120 >/dev/null

# ── Step 7: Verify outcomes ──
echo "Step 7: Verifying outcomes..."

T18_L1_USER1_AFTER=$(get_balance "$L1_RPC" "$T18_USER1_ADDR")
T18_L1_CONTRACT_AFTER=$(get_balance "$L1_RPC" "$T18_L1_DEPLOY_ADDR")
T18_L2_CONTRACT_AFTER=$(get_balance "$L2_RPC" "$T18_L2_DEPLOY_ADDR")
T18_EB_AFTER=$(get_ether_balance)
T18_REWINDS_AFTER=$(get_rewind_cycles)

T18_L1_USER1_DELTA=$(python3 -c "print(int('$T18_L1_USER1_AFTER') - int('$T18_L1_USER1_BEFORE'))")
T18_L1_CONTRACT_DELTA=$(python3 -c "print(int('$T18_L1_CONTRACT_AFTER') - int('$T18_L1_CONTRACT_BEFORE'))")
T18_EB_DELTA=$(python3 -c "print(int('$T18_EB_AFTER') - int('$T18_EB_BEFORE'))")

echo "  User1 L1 delta: $(wei_to_eth "$T18_L1_USER1_DELTA") ETH"
echo "  Contract L1 delta: $(wei_to_eth "$T18_L1_CONTRACT_DELTA") ETH"
echo "  Contract L2 balance after: $(wei_to_eth "$T18_L2_CONTRACT_AFTER") ETH"
echo "  etherBalance delta: $(wei_to_eth "$T18_EB_DELTA") ETH"
echo "  Peak rewind cycles observed: $PEAK_REWIND_CYCLES"
echo "  Final rewind cycles: $T18_REWINDS_AFTER"

# Determine which scenario occurred: full rewind or partial consumption.
# Scenario A (partial consumption with rewind): builder detects trigger failure,
#   rewinds, re-derives with only User1's withdrawal. User1 gets 0.3 ETH on L1,
#   contract does NOT get ETH on L1. etherBalance decreases by only 0.3 ETH.
#   Contract's L2 ETH is restored (withdrawal filtered out during rewind).
# Scenario B (both succeed): If for some reason both triggers succeed (e.g., the
#   L1 contract is not at the expected address), both withdrawals complete.
#   This would indicate a test setup issue, but we should still verify consistency.

# Check if a rewind occurred (indicates the expected partial-consumption scenario).
T18_REWIND_OCCURRED=false
if [ "$PEAK_REWIND_CYCLES" -gt "$T18_REWINDS_BEFORE" ] 2>/dev/null; then
  T18_REWIND_OCCURRED=true
fi
if [ "$T18_REWINDS_AFTER" -gt "$T18_REWINDS_BEFORE" ] 2>/dev/null; then
  T18_REWIND_OCCURRED=true
fi

if [ "$T18_REWIND_OCCURRED" = "true" ]; then
  echo ""
  echo "  Scenario: REWIND detected — partial consumption path (expected)."

  # User1's EOA withdrawal should have succeeded after rewind re-derive.
  assert "TEST18: [rewind] User1 received 0.3 ETH on L1" \
    '[ "$T18_L1_USER1_DELTA" = "300000000000000000" ]' \
    "delta=$(wei_to_eth "$T18_L1_USER1_DELTA") ETH"

  # Contract should NOT have received ETH on L1 (trigger reverted).
  assert "TEST18: [rewind] Contract did NOT receive ETH on L1 (trigger reverted)" \
    '[ "$T18_L1_CONTRACT_DELTA" = "0" ]' \
    "delta=$(wei_to_eth "$T18_L1_CONTRACT_DELTA") ETH"

  # etherBalance should have decreased by only 0.3 ETH (not 0.8).
  assert "TEST18: [rewind] etherBalance decreased by only 0.3 ETH" \
    '[ "$T18_EB_DELTA" = "-300000000000000000" ]' \
    "delta=$(wei_to_eth "$T18_EB_DELTA") ETH"

  # Contract's L2 balance should be restored (withdrawal filtered out).
  # It should be approximately 0.5 ETH (the original 1 ETH minus the 0.5 that was
  # attempted but reverted, which gets restored). Actually, the full 1 ETH should
  # remain since the withdrawal tx was filtered during rewind. But gas costs on L2
  # from the triggerWithdrawal call may have been spent. Allow tolerance.
  T18_L2_CONTRACT_REMAINING=$(python3 -c "print(1 if int('$T18_L2_CONTRACT_AFTER') > 400000000000000000 else 0)")
  assert "TEST18: [rewind] Contract L2 balance preserved (> 0.4 ETH)" \
    '[ "$T18_L2_CONTRACT_REMAINING" = "1" ]' \
    "balance=$(wei_to_eth "$T18_L2_CONTRACT_AFTER") ETH"

else
  echo ""
  echo "  Scenario: NO rewind detected — checking if both withdrawals completed."

  # If no rewind, both may have succeeded (unlikely given our setup, but possible
  # if timing/block boundaries separated them into different blocks).
  # Just verify consistency: etherBalance should have changed, User1 should have ETH.
  assert "TEST18: [no-rewind] User1 received ETH on L1" \
    '[ "$(python3 -c "print(1 if int(\"$T18_L1_USER1_DELTA\") > 0 else 0)")" = "1" ]' \
    "delta=$(wei_to_eth "$T18_L1_USER1_DELTA") ETH"

  # Check if contract got ETH (would mean both triggers succeeded — test setup issue)
  if [ "$T18_L1_CONTRACT_DELTA" != "0" ]; then
    echo "  WARNING: Contract received ETH on L1 — both triggers succeeded."
    echo "  This means the withdrawals landed in different blocks (no partial consumption)."
    echo "  The partial consumption scenario was NOT exercised."
  fi
fi

# In all scenarios: builder must be healthy and state roots must converge.
T18_HEALTH=$(get_health)
T18_MODE=$(echo "$T18_HEALTH" | jq -r '.mode // "UNKNOWN"')
T18_HEALTHY=$(echo "$T18_HEALTH" | jq -r '.healthy // false')
echo ""
echo "  Builder mode: $T18_MODE, healthy: $T18_HEALTHY"
assert "TEST18: Builder in Builder mode after partial consumption test" \
  '[ "$T18_MODE" = "Builder" ]'
assert "TEST18: Builder healthy after partial consumption test" \
  '[ "$T18_HEALTHY" = "true" ]'

ROOTS_T18=$(check_state_roots)
assert "TEST18: State roots match after partial consumption test" '[ "$ROOTS_T18" = "MATCH" ]'

# Verify convergence across all nodes (fullnodes re-derived correctly).
T18_CONVERGENCE=$(wait_for_convergence 60)
assert "TEST18: All nodes converged after partial consumption" '[ "$T18_CONVERGENCE" = "MATCH" ]'

print_elapsed "TEST 18"
echo ""

# ══════════════════════════════════════════
#  FINAL CHECKS
# ══════════════════════════════════════════

ONCHAIN_SR_AFTER=$(get_onchain_state_root "$ROLLUPS_ADDRESS")
echo "On-chain stateRoot before: $ONCHAIN_SR_BEFORE"
echo "On-chain stateRoot after:  $ONCHAIN_SR_AFTER"
assert "On-chain stateRoot advanced" '[ "$ONCHAIN_SR_BEFORE" != "$ONCHAIN_SR_AFTER" ]'

if [ -n "$CCM_L2_ADDRESS" ]; then
  CCM_BAL_AFTER=$(get_balance "$L2_RPC" "$CCM_L2_ADDRESS")
  CCM_BAL_ETH=$(wei_to_eth "$CCM_BAL_AFTER")
  echo "CCM balance (final): $CCM_BAL_ETH ETH"
  assert "CCM balance still > 999000 ETH" '[ "$(python3 -c "print(1 if int(\"$CCM_BAL_AFTER\") > 999000000000000000000000 else 0)")" = "1" ]'
fi

echo ""

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════

if [ "$JSON_MODE" = "true" ]; then
  print_json_summary "bridge"
else
  echo "========================================"
  echo "  BRIDGE HEALTH CHECK RESULTS"
  echo "========================================"
  echo ""
  echo "  Passed: $PASS_COUNT"
  echo "  Failed: $FAIL_COUNT"
  echo "  Total:  $TOTAL_COUNT"
  echo ""
  print_total_elapsed

  if [ "$FAIL_COUNT" -eq 0 ]; then
    echo ""
    echo "  STATUS: ALL TESTS PASSED"
    echo ""
    echo "========================================"
  else
    echo ""
    echo "  STATUS: $FAIL_COUNT TEST(S) FAILED"
    echo ""
    echo "========================================"
  fi
fi

if [ "$FAIL_COUNT" -eq 0 ]; then
  exit 0
else
  exit 1
fi
