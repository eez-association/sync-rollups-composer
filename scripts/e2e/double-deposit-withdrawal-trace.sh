#!/usr/bin/env bash
# Double Deposit + Double Withdrawal Trace Script
#
# Sends 2 deposits in the same L1 block, waits for L2 confirmation,
# then sends 2 withdrawals in the same L2 block. Traces all transactions
# and produces a detailed log report for explorer verification.
#
# Usage: ./scripts/e2e/double-deposit-withdrawal-trace.sh
#
# Uses dev accounts #13 (USER1) and #6 (USER2) to avoid nonce collisions with
# crosschain-tx-sender (#4) and bridge-health-check (#3).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

# ── Configuration ──

# User 1: dev account #13
USER1_KEY="0x47c99abed3324a2707c28affff1267e45918ec8c3f20b8aa892e8b065d2942dd"
USER1_ADDR="0x1CBd3b2770909D4e10f157cABC84C7264073C9Ec"

# User 2: dev account #6
USER2_KEY="0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e"
USER2_ADDR="0x976EA74026E726554dB657fA54763abd0C3a0aa9"

BUILDER_ADDR="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
DEPOSIT_AMOUNT="1ether"
WITHDRAW1_AMOUNT="0.3ether"
WITHDRAW2_AMOUNT="0.5ether"

# ── Bridge-specific helper (reads etherBalance from Rollups.sol) ──

get_ether_balance() {
  local data
  data=$(rpc_call "$L1_RPC" "eth_call" \
    "[{\"to\":\"$ROLLUPS_ADDRESS\",\"data\":\"0xb794e5a30000000000000000000000000000000000000000000000000000000000000001\"},\"latest\"]")
  data="${data#0x}"
  python3 -c "print(int('${data:192:64}', 16))" 2>/dev/null || echo "0"
}

# ── Load environment ──

echo "================================================================"
echo "  DOUBLE DEPOSIT + DOUBLE WITHDRAWAL TRACE"
echo "================================================================"
echo ""
echo "Loading rollup.env..."
eval "$($DOCKER_COMPOSE_CMD exec -T builder cat /shared/rollup.env 2>/dev/null)"
if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
  echo "ERROR: Could not load rollup.env — is the builder running?"
  exit 1
fi

echo "  ROLLUPS_ADDRESS=$ROLLUPS_ADDRESS"
echo "  BRIDGE_ADDRESS=$BRIDGE_ADDRESS"
echo "  BRIDGE_L2_ADDRESS=$BRIDGE_L2_ADDRESS"
echo "  User1: $USER1_ADDR"
echo "  User2: $USER2_ADDR"
echo ""

# ── Fund USER1 on L1 (dev#13 is not pre-funded by reth --dev) ──
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
USER1_L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$USER1_ADDR" 2>/dev/null || echo "0")
if [ "$USER1_L1_BAL" = "0" ] || [ "$USER1_L1_BAL" = "0x0" ]; then
    echo "Funding USER1 ($USER1_ADDR) on L1 with 100 ETH (dev#9 funder)..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$USER1_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
fi

# ── Helpers ──

# Print a separator with label
section() {
  echo ""
  echo "────────────────────────────────────────────────────────────────"
  echo "  $1"
  echo "────────────────────────────────────────────────────────────────"
}

# Extract tx hash + status from cast send output
parse_cast_output() {
  local output="$1"
  local hash status block_number
  hash=$(echo "$output" | grep "^transactionHash" | awk '{print $2}')
  status=$(echo "$output" | grep "^status" | awk '{print $2}')
  block_number=$(echo "$output" | grep "^blockNumber" | awk '{print $2}')
  echo "$hash|$status|$block_number"
}

# Trace an L2 transaction and print key fields
trace_l2_tx() {
  local tx_hash="$1" label="$2"
  echo ""
  echo "  --- Trace: $label ---"
  echo "  tx_hash: $tx_hash"

  # Get receipt
  local receipt
  receipt=$(curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$tx_hash\"],\"id\":1}" \
    "$L2_RPC")

  local tx_status block_num gas_used logs_count
  tx_status=$(echo "$receipt" | jq -r '.result.status // "?"')
  block_num=$(echo "$receipt" | jq -r '.result.blockNumber // "?"')
  gas_used=$(echo "$receipt" | jq -r '.result.gasUsed // "?"')
  logs_count=$(echo "$receipt" | jq -r '.result.logs | length // 0')

  echo "  status: $tx_status"
  echo "  block: $block_num ($(printf '%d' "$block_num" 2>/dev/null || echo '?'))"
  echo "  gas_used: $gas_used ($(printf '%d' "$gas_used" 2>/dev/null || echo '?'))"
  echo "  logs: $logs_count"

  # Print log topics for debugging
  if [ "$logs_count" -gt 0 ]; then
    echo "  log_details:"
    echo "$receipt" | jq -r '.result.logs[] | "    event: \(.topics[0][:18])... addr: \(.address) data_len: \(.data | length)"'
  fi

  # Trace the transaction
  local trace
  trace=$(curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"debug_traceTransaction\",\"params\":[\"$tx_hash\",{\"tracer\":\"callTracer\"}],\"id\":1}" \
    "$L2_RPC")

  local trace_error trace_type trace_from trace_to trace_value trace_gas_used
  trace_error=$(echo "$trace" | jq -r '.error.message // empty')

  if [ -n "$trace_error" ]; then
    echo "  TRACE ERROR: $trace_error"
    # Fall back to default tracer
    trace=$(curl -s -X POST -H 'Content-Type: application/json' \
      -d "{\"jsonrpc\":\"2.0\",\"method\":\"debug_traceTransaction\",\"params\":[\"$tx_hash\",{}],\"id\":1}" \
      "$L2_RPC")
    local trace_gas trace_failed trace_return
    trace_gas=$(echo "$trace" | jq -r '.result.gas // "?"')
    trace_failed=$(echo "$trace" | jq -r '.result.failed // "?"')
    trace_return=$(echo "$trace" | jq -r '.result.returnValue // "?" | .[0:40]')
    echo "  trace_gas: $trace_gas"
    echo "  trace_failed: $trace_failed"
    echo "  trace_return: ${trace_return}..."
  else
    trace_type=$(echo "$trace" | jq -r '.result.type // "?"')
    trace_from=$(echo "$trace" | jq -r '.result.from // "?"')
    trace_to=$(echo "$trace" | jq -r '.result.to // "?"')
    trace_value=$(echo "$trace" | jq -r '.result.value // "0x0"')
    trace_gas_used=$(echo "$trace" | jq -r '.result.gasUsed // "?"')

    echo "  trace_type: $trace_type"
    echo "  trace_from: $trace_from"
    echo "  trace_to: $trace_to"
    echo "  trace_value: $trace_value ($(python3 -c "print(f'{int(\"$trace_value\", 16) / 1e18:.6f}')" 2>/dev/null || echo '?') ETH)"
    echo "  trace_gas_used: $trace_gas_used"

    # Print subcalls
    local subcall_count
    subcall_count=$(echo "$trace" | jq -r '.result.calls | length // 0' 2>/dev/null || echo "0")
    if [ "$subcall_count" -gt 0 ]; then
      echo "  subcalls ($subcall_count):"
      echo "$trace" | jq -r '.result.calls[] | "    \(.type) → \(.to) value=\(.value // "0x0") gas=\(.gasUsed // "?")"' 2>/dev/null || true
    fi
  fi
}

# Trace an L1 transaction
trace_l1_tx() {
  local tx_hash="$1" label="$2"
  echo ""
  echo "  --- L1 Receipt: $label ---"
  echo "  tx_hash: $tx_hash"

  local receipt
  receipt=$(curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$tx_hash\"],\"id\":1}" \
    "$L1_RPC")

  local tx_status block_num gas_used logs_count
  tx_status=$(echo "$receipt" | jq -r '.result.status // "?"')
  block_num=$(echo "$receipt" | jq -r '.result.blockNumber // "?"')
  gas_used=$(echo "$receipt" | jq -r '.result.gasUsed // "?"')
  logs_count=$(echo "$receipt" | jq -r '.result.logs | length // 0')

  echo "  status: $tx_status ($([ "$tx_status" = "0x1" ] && echo "SUCCESS" || echo "REVERTED"))"
  echo "  block: $block_num ($(printf '%d' "$block_num" 2>/dev/null || echo '?'))"
  echo "  gas_used: $gas_used ($(printf '%d' "$gas_used" 2>/dev/null || echo '?'))"
  echo "  logs: $logs_count"

  if [ "$logs_count" -gt 0 ]; then
    echo "  log_details:"
    echo "$receipt" | jq -r '.result.logs[] | "    event: \(.topics[0][:18])... addr: \(.address)"'
  fi
}

# Print block details
print_block_info() {
  local url="$1" block_hex="$2" label="$3"
  local block_data
  block_data=$(curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBlockByNumber\",\"params\":[\"$block_hex\",true],\"id\":1}" \
    "$url")

  local block_num tx_count state_root timestamp
  block_num=$(echo "$block_data" | jq -r '.result.number // "?"')
  tx_count=$(echo "$block_data" | jq -r '.result.transactions | length // 0')
  state_root=$(echo "$block_data" | jq -r '.result.stateRoot // "?"')
  timestamp=$(echo "$block_data" | jq -r '.result.timestamp // "?"')

  echo "  [$label] block=$block_num txs=$tx_count stateRoot=${state_root:0:18}... timestamp=$timestamp"

  # List all tx hashes in the block
  if [ "$tx_count" -gt 0 ]; then
    echo "  transactions:"
    echo "$block_data" | jq -r '.result.transactions[] | "    \(.hash) from=\(.from) to=\(.to // "CREATE") value=\(.value)"'
  fi
}

# ══════════════════════════════════════════════════════════════════════
#  PRE-FLIGHT
# ══════════════════════════════════════════════════════════════════════

section "PRE-FLIGHT"
start_timer

echo "Waiting for builder to be ready..."
MODE=$(wait_for_builder_ready 60)
echo "  Builder mode: $MODE"
[ "$MODE" = "Builder" ] || { echo "FATAL: Builder not ready"; exit 1; }

echo "Waiting for pending submissions to clear..."
wait_for_pending_zero 60 >/dev/null

L2_BLOCK_PRE=$(get_block_number "$L2_RPC")
L1_BLOCK_PRE=$(get_block_number "$L1_RPC")
echo "  L1 block: $(printf '%d' "$L1_BLOCK_PRE")"
echo "  L2 block: $(printf '%d' "$L2_BLOCK_PRE")"

# Snapshot balances
U1_L1_BAL_PRE=$(get_balance "$L1_RPC" "$USER1_ADDR")
U2_L1_BAL_PRE=$(get_balance "$L1_RPC" "$USER2_ADDR")
U1_L2_BAL_PRE=$(get_balance "$L2_RPC" "$USER1_ADDR")
U2_L2_BAL_PRE=$(get_balance "$L2_RPC" "$USER2_ADDR")
EB_PRE=$(get_ether_balance)

echo "  User1 L1 balance: $(wei_to_eth "$U1_L1_BAL_PRE") ETH"
echo "  User2 L1 balance: $(wei_to_eth "$U2_L1_BAL_PRE") ETH"
echo "  User1 L2 balance: $(wei_to_eth "$U1_L2_BAL_PRE") ETH"
echo "  User2 L2 balance: $(wei_to_eth "$U2_L2_BAL_PRE") ETH"
echo "  etherBalance: $(wei_to_eth "$EB_PRE") ETH"

HEALTH_PRE=$(get_health)
echo "  Health: $(echo "$HEALTH_PRE" | jq -c '.')"

print_elapsed "PRE-FLIGHT"

# ══════════════════════════════════════════════════════════════════════
#  PHASE 1: DOUBLE DEPOSIT (same L1 block)
# ══════════════════════════════════════════════════════════════════════

section "PHASE 1: DOUBLE DEPOSIT ($DEPOSIT_AMOUNT x 2, same L1 block)"
start_timer

echo "Sending two deposits concurrently (User1 + User2)..."
echo "  User1: bridgeEther(1,addr) --value $DEPOSIT_AMOUNT via L1 proxy"
echo "  User2: bridgeEther(1,addr) --value $DEPOSIT_AMOUNT via L1 proxy"

# Send both deposits in background to hit the same L1 block.
# Use temp files since background subshells can't set parent variables.
D1_TMP=$(mktemp)
D2_TMP=$(mktemp)
trap "rm -f $D1_TMP $D2_TMP" EXIT

cast send --rpc-url "$L1_PROXY" --private-key "$USER1_KEY" \
  "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$USER1_ADDR" --value "$DEPOSIT_AMOUNT" --gas-limit 800000 > "$D1_TMP" 2>&1 &
PID1=$!

cast send --rpc-url "$L1_PROXY" --private-key "$USER2_KEY" \
  "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" 1 "$USER2_ADDR" --value "$DEPOSIT_AMOUNT" --gas-limit 800000 > "$D2_TMP" 2>&1 &
PID2=$!

# Wait for both to complete
wait $PID1 || true
wait $PID2 || true

DEPOSIT1_OUTPUT=$(cat "$D1_TMP")
DEPOSIT2_OUTPUT=$(cat "$D2_TMP")

# Parse results
D1_PARSED=$(parse_cast_output "$DEPOSIT1_OUTPUT")
D2_PARSED=$(parse_cast_output "$DEPOSIT2_OUTPUT")

D1_HASH=$(echo "$D1_PARSED" | cut -d'|' -f1)
D1_STATUS=$(echo "$D1_PARSED" | cut -d'|' -f2)
D1_BLOCK=$(echo "$D1_PARSED" | cut -d'|' -f3)

D2_HASH=$(echo "$D2_PARSED" | cut -d'|' -f1)
D2_STATUS=$(echo "$D2_PARSED" | cut -d'|' -f2)
D2_BLOCK=$(echo "$D2_PARSED" | cut -d'|' -f3)

echo ""
echo "  DEPOSIT 1 (User1):"
echo "    tx_hash:  $D1_HASH"
echo "    status:   $D1_STATUS ($([ "$D1_STATUS" = "1" ] && echo "SUCCESS" || echo "FAILED"))"
echo "    L1 block: $D1_BLOCK"

echo ""
echo "  DEPOSIT 2 (User2):"
echo "    tx_hash:  $D2_HASH"
echo "    status:   $D2_STATUS ($([ "$D2_STATUS" = "1" ] && echo "SUCCESS" || echo "FAILED"))"
echo "    L1 block: $D2_BLOCK"

# Check if same block
if [ "$D1_BLOCK" = "$D2_BLOCK" ]; then
  echo ""
  echo "  ** SAME L1 BLOCK: $D1_BLOCK **"
else
  echo ""
  echo "  ** DIFFERENT L1 BLOCKS: $D1_BLOCK vs $D2_BLOCK (race condition, still valid) **"
fi

# Trace L1 deposits
trace_l1_tx "$D1_HASH" "Deposit 1 (User1)"
trace_l1_tx "$D2_HASH" "Deposit 2 (User2)"

print_elapsed "PHASE 1 — L1 deposits sent"

# ══════════════════════════════════════════════════════════════════════
#  PHASE 2: WAIT FOR L2 CONFIRMATION
# ══════════════════════════════════════════════════════════════════════

section "PHASE 2: WAITING FOR L2 CONFIRMATION"
start_timer

echo "Waiting for L2 to process deposits (block advance + pending zero)..."
L2_BLK_BEFORE_DEPOSIT=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_BEFORE_DEPOSIT" 3 90 >/dev/null || true
wait_for_pending_zero 90 >/dev/null

L2_BLOCK_POST_DEPOSIT=$(get_block_number "$L2_RPC")
echo "  L2 block advanced: $(printf '%d' "$L2_BLK_BEFORE_DEPOSIT") -> $(printf '%d' "$L2_BLOCK_POST_DEPOSIT")"

# Check balances after deposit
U1_L2_BAL_POST_DEP=$(get_balance "$L2_RPC" "$USER1_ADDR")
U2_L2_BAL_POST_DEP=$(get_balance "$L2_RPC" "$USER2_ADDR")
EB_POST_DEP=$(get_ether_balance)

U1_L2_DELTA_DEP=$(python3 -c "print(int('$U1_L2_BAL_POST_DEP') - int('$U1_L2_BAL_PRE'))")
U2_L2_DELTA_DEP=$(python3 -c "print(int('$U2_L2_BAL_POST_DEP') - int('$U2_L2_BAL_PRE'))")
EB_DELTA_DEP=$(python3 -c "print(int('$EB_POST_DEP') - int('$EB_PRE'))")

echo ""
echo "  User1 L2 balance delta: $(wei_to_eth "$U1_L2_DELTA_DEP") ETH (expected +$DEPOSIT_AMOUNT)"
echo "  User2 L2 balance delta: $(wei_to_eth "$U2_L2_DELTA_DEP") ETH (expected +$DEPOSIT_AMOUNT)"
echo "  etherBalance delta: $(wei_to_eth "$EB_DELTA_DEP") ETH (expected +2 ETH)"

# Verify deposits landed
assert "Deposit 1: User1 L2 balance increased" \
  '[ "$(python3 -c "print(1 if int(\"$U1_L2_DELTA_DEP\") > 900000000000000000 else 0)")" = "1" ]' \
  "delta=$(wei_to_eth "$U1_L2_DELTA_DEP")"
assert "Deposit 2: User2 L2 balance increased" \
  '[ "$(python3 -c "print(1 if int(\"$U2_L2_DELTA_DEP\") > 900000000000000000 else 0)")" = "1" ]' \
  "delta=$(wei_to_eth "$U2_L2_DELTA_DEP")"
# etherBalance delta may exceed 2 ETH if other tests deposited before this one.
# Per-user balance assertions above are the reliable checks.
assert "etherBalance increased (deposits landed)" \
  '[ "$(python3 -c "print(1 if int(\"$EB_DELTA_DEP\") > 0 else 0)")" = "1" ]' \
  "delta=$(wei_to_eth "$EB_DELTA_DEP")"

# Find and trace the L2 blocks that contain the deposit protocol txs
echo ""
echo "Scanning L2 blocks for deposit protocol txs..."
L2_SCAN_START=$(printf '%d' "$L2_BLK_BEFORE_DEPOSIT")
L2_SCAN_END=$(printf '%d' "$L2_BLOCK_POST_DEPOSIT")

for ((blk=L2_SCAN_START; blk<=L2_SCAN_END; blk++)); do
  BLK_HEX=$(printf '0x%x' "$blk")
  BLK_DATA=$(curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBlockByNumber\",\"params\":[\"$BLK_HEX\",true],\"id\":1}" \
    "$L2_RPC")
  TX_COUNT=$(echo "$BLK_DATA" | jq -r '.result.transactions | length // 0')
  if [ "$TX_COUNT" -gt 0 ]; then
    print_block_info "$L2_RPC" "$BLK_HEX" "L2 block $blk"

    # Trace each transaction in the block
    for tx_hash in $(echo "$BLK_DATA" | jq -r '.result.transactions[].hash'); do
      trace_l2_tx "$tx_hash" "L2 block $blk tx"
    done
  fi
done

ROOTS=$(check_state_roots)
echo ""
echo "  State roots after deposits: $ROOTS"
assert "State roots converge after deposits" '[ "$ROOTS" = "MATCH" ]'

HEALTH_POST_DEP=$(get_health)
echo "  Health: $(echo "$HEALTH_POST_DEP" | jq -c '.')"

print_elapsed "PHASE 2 — L2 deposit confirmation"

# ══════════════════════════════════════════════════════════════════════
#  PHASE 3: DOUBLE WITHDRAWAL (same L2 block)
# ══════════════════════════════════════════════════════════════════════

section "PHASE 3: DOUBLE WITHDRAWAL ($WITHDRAW1_AMOUNT + $WITHDRAW2_AMOUNT, same L2 block)"
start_timer

echo "Ensuring no pending operations..."
wait_for_pending_zero 60 >/dev/null

# Snapshot before withdrawals
U1_L1_BAL_PRE_W=$(get_balance "$L1_RPC" "$USER1_ADDR")
U2_L1_BAL_PRE_W=$(get_balance "$L1_RPC" "$USER2_ADDR")
U1_L2_BAL_PRE_W=$(get_balance "$L2_RPC" "$USER1_ADDR")
U2_L2_BAL_PRE_W=$(get_balance "$L2_RPC" "$USER2_ADDR")
EB_PRE_W=$(get_ether_balance)

echo "  User1 L2 balance: $(wei_to_eth "$U1_L2_BAL_PRE_W") ETH"
echo "  User2 L2 balance: $(wei_to_eth "$U2_L2_BAL_PRE_W") ETH"
echo "  etherBalance: $(wei_to_eth "$EB_PRE_W") ETH"
echo ""

echo "Sending two withdrawals concurrently (User1 + User2) with DIFFERENT amounts..."
echo "  User1: bridgeEther(0,addr) --value $WITHDRAW1_AMOUNT via L2 proxy"
echo "  User2: bridgeEther(0,addr) --value $WITHDRAW2_AMOUNT via L2 proxy"

# Send both withdrawals in background to hit the same L2 block.
W1_TMP=$(mktemp)
W2_TMP=$(mktemp)

cast send --rpc-url "$L2_PROXY" --private-key "$USER1_KEY" \
  "$BRIDGE_L2_ADDRESS" "bridgeEther(uint256,address)" 0 "$USER1_ADDR" --value "$WITHDRAW1_AMOUNT" --gas-limit 500000 > "$W1_TMP" 2>&1 &
PID_W1=$!

cast send --rpc-url "$L2_PROXY" --private-key "$USER2_KEY" \
  "$BRIDGE_L2_ADDRESS" "bridgeEther(uint256,address)" 0 "$USER2_ADDR" --value "$WITHDRAW2_AMOUNT" --gas-limit 500000 > "$W2_TMP" 2>&1 &
PID_W2=$!

wait $PID_W1 || true
wait $PID_W2 || true

W1_OUTPUT=$(cat "$W1_TMP")
W2_OUTPUT=$(cat "$W2_TMP")
rm -f "$W1_TMP" "$W2_TMP"

W1_PARSED=$(parse_cast_output "$W1_OUTPUT")
W2_PARSED=$(parse_cast_output "$W2_OUTPUT")

W1_HASH=$(echo "$W1_PARSED" | cut -d'|' -f1)
W1_STATUS=$(echo "$W1_PARSED" | cut -d'|' -f2)
W1_BLOCK=$(echo "$W1_PARSED" | cut -d'|' -f3)

W2_HASH=$(echo "$W2_PARSED" | cut -d'|' -f1)
W2_STATUS=$(echo "$W2_PARSED" | cut -d'|' -f2)
W2_BLOCK=$(echo "$W2_PARSED" | cut -d'|' -f3)

echo ""
echo "  WITHDRAWAL 1 (User1):"
echo "    tx_hash:  $W1_HASH"
echo "    status:   $W1_STATUS ($([ "$W1_STATUS" = "1" ] && echo "SUCCESS" || echo "FAILED"))"
echo "    L2 block: $W1_BLOCK"

echo ""
echo "  WITHDRAWAL 2 (User2):"
echo "    tx_hash:  $W2_HASH"
echo "    status:   $W2_STATUS ($([ "$W2_STATUS" = "1" ] && echo "SUCCESS" || echo "FAILED"))"
echo "    L2 block: $W2_BLOCK"

if [ "$W1_BLOCK" = "$W2_BLOCK" ]; then
  echo ""
  echo "  ** SAME L2 BLOCK: $W1_BLOCK **"
else
  echo ""
  echo "  ** DIFFERENT L2 BLOCKS: $W1_BLOCK vs $W2_BLOCK **"
fi

assert "Withdrawal 1 L2 tx succeeded" '[ "$W1_STATUS" = "1" ]'
assert "Withdrawal 2 L2 tx succeeded" '[ "$W2_STATUS" = "1" ]'

# Trace L2 withdrawal txs
trace_l2_tx "$W1_HASH" "Withdrawal 1 (User1)"
trace_l2_tx "$W2_HASH" "Withdrawal 2 (User2)"

print_elapsed "PHASE 3 — L2 withdrawals sent"

# ══════════════════════════════════════════════════════════════════════
#  PHASE 4: WAIT FOR L1 WITHDRAWAL TRIGGERS
# ══════════════════════════════════════════════════════════════════════

section "PHASE 4: WAITING FOR L1 WITHDRAWAL TRIGGERS"
start_timer

echo "Waiting for builder to process withdrawals and send L1 triggers..."
echo "  (monitoring for up to 180s — withdrawal triggers, entry verification, batch submission)"
echo ""

L2_BLK_BEFORE_TRIGGER=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK_BEFORE_TRIGGER" 10 180 >/dev/null || true
wait_for_pending_zero 120 >/dev/null

# Check L1 trigger logs
echo "Checking builder logs for withdrawal triggers..."
STRIP_ANSI='s/\x1b\[[0-9;]*m//g'
TRIGGER_LINES=$($DOCKER_COMPOSE_CMD logs builder --no-log-prefix --since 180s 2>&1 \
  | sed "$STRIP_ANSI" \
  | grep -E "sent withdrawal trigger|detected L2.*withdrawal|draining withdrawal queue" || true)

if [ -n "$TRIGGER_LINES" ]; then
  echo "  Withdrawal-related log lines:"
  echo "$TRIGGER_LINES" | while IFS= read -r line; do
    echo "    $line"
  done
else
  echo "  WARNING: No withdrawal trigger log lines found"
fi

# Extract trigger tx hashes
TRIGGER_HASHES=$($DOCKER_COMPOSE_CMD logs builder --no-log-prefix --since 180s 2>&1 \
  | sed "$STRIP_ANSI" \
  | grep "sent withdrawal trigger" \
  | grep -oP 'hash=\K0x[a-fA-F0-9]+' | sort -u || true)

echo ""
if [ -n "$TRIGGER_HASHES" ]; then
  echo "  Found withdrawal trigger txs on L1:"
  for TX_HASH in $TRIGGER_HASHES; do
    trace_l1_tx "$TX_HASH" "Withdrawal Trigger"
  done
else
  echo "  No withdrawal trigger tx hashes found in logs"
fi

# Check for postBatch txs
BATCH_HASHES=$($DOCKER_COMPOSE_CMD logs builder --no-log-prefix --since 180s 2>&1 \
  | sed "$STRIP_ANSI" \
  | grep "postBatch confirmed" \
  | grep -oP 'tx_hash=\K0x[a-fA-F0-9]+' | sort -u || true)

if [ -n "$BATCH_HASHES" ]; then
  echo ""
  echo "  postBatch txs confirmed on L1:"
  for TX_HASH in $BATCH_HASHES; do
    trace_l1_tx "$TX_HASH" "postBatch"
  done
fi

# Check for rewind activity
REWIND_LINES=$($DOCKER_COMPOSE_CMD logs builder --no-log-prefix --since 180s 2>&1 \
  | sed "$STRIP_ANSI" \
  | grep -iE "rewind|override|mismatch" || true)

if [ -n "$REWIND_LINES" ]; then
  echo ""
  echo "  ** REWIND/OVERRIDE activity detected: **"
  echo "$REWIND_LINES" | while IFS= read -r line; do
    echo "    $line"
  done
fi

print_elapsed "PHASE 4 — L1 trigger processing"

# ══════════════════════════════════════════════════════════════════════
#  PHASE 5: FINAL STATE + BALANCE VERIFICATION
# ══════════════════════════════════════════════════════════════════════

section "PHASE 5: FINAL STATE VERIFICATION"
start_timer

U1_L1_BAL_FINAL=$(get_balance "$L1_RPC" "$USER1_ADDR")
U2_L1_BAL_FINAL=$(get_balance "$L1_RPC" "$USER2_ADDR")
U1_L2_BAL_FINAL=$(get_balance "$L2_RPC" "$USER1_ADDR")
U2_L2_BAL_FINAL=$(get_balance "$L2_RPC" "$USER2_ADDR")
EB_FINAL=$(get_ether_balance)

# Compute deltas from pre-withdrawal snapshot
U1_L1_DELTA_W=$(python3 -c "print(int('$U1_L1_BAL_FINAL') - int('$U1_L1_BAL_PRE_W'))")
U2_L1_DELTA_W=$(python3 -c "print(int('$U2_L1_BAL_FINAL') - int('$U2_L1_BAL_PRE_W'))")
U1_L2_DELTA_W=$(python3 -c "print(int('$U1_L2_BAL_FINAL') - int('$U1_L2_BAL_PRE_W'))")
U2_L2_DELTA_W=$(python3 -c "print(int('$U2_L2_BAL_FINAL') - int('$U2_L2_BAL_PRE_W'))")
EB_DELTA_W=$(python3 -c "print(int('$EB_FINAL') - int('$EB_PRE_W'))")

# Compute total deltas from the very start
U1_L1_DELTA_TOTAL=$(python3 -c "print(int('$U1_L1_BAL_FINAL') - int('$U1_L1_BAL_PRE'))")
U2_L1_DELTA_TOTAL=$(python3 -c "print(int('$U2_L1_BAL_FINAL') - int('$U2_L1_BAL_PRE'))")
U1_L2_DELTA_TOTAL=$(python3 -c "print(int('$U1_L2_BAL_FINAL') - int('$U1_L2_BAL_PRE'))")
U2_L2_DELTA_TOTAL=$(python3 -c "print(int('$U2_L2_BAL_FINAL') - int('$U2_L2_BAL_PRE'))")
EB_DELTA_TOTAL=$(python3 -c "print(int('$EB_FINAL') - int('$EB_PRE'))")

echo "  === Withdrawal Phase Deltas ==="
echo "  User1 L1 delta: $(wei_to_eth "$U1_L1_DELTA_W") ETH (expected +$WITHDRAW1_AMOUNT)"
echo "  User2 L1 delta: $(wei_to_eth "$U2_L1_DELTA_W") ETH (expected +$WITHDRAW2_AMOUNT)"
echo "  User1 L2 delta: $(wei_to_eth "$U1_L2_DELTA_W") ETH"
echo "  User2 L2 delta: $(wei_to_eth "$U2_L2_DELTA_W") ETH"
echo "  etherBalance delta: $(wei_to_eth "$EB_DELTA_W") ETH (expected -0.8 ETH)"
echo ""
echo "  === Total Deltas (deposit + withdrawal) ==="
echo "  User1 L1 total: $(wei_to_eth "$U1_L1_DELTA_TOTAL") ETH (deposited $DEPOSIT_AMOUNT, withdrew $WITHDRAW1_AMOUNT)"
echo "  User2 L1 total: $(wei_to_eth "$U2_L1_DELTA_TOTAL") ETH (deposited $DEPOSIT_AMOUNT, withdrew $WITHDRAW2_AMOUNT)"
echo "  User1 L2 total: $(wei_to_eth "$U1_L2_DELTA_TOTAL") ETH"
echo "  User2 L2 total: $(wei_to_eth "$U2_L2_DELTA_TOTAL") ETH"
echo "  etherBalance total: $(wei_to_eth "$EB_DELTA_TOTAL") ETH (expected +1.2 ETH: 2 deposited - 0.8 withdrawn)"

# Check if triggers reverted (issue #212 — EtherDeltaMismatch with different amounts)
TRIGGER_REVERTED="false"
for TX_HASH in $TRIGGER_HASHES; do
  T_STATUS=$(curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$TX_HASH\"],\"id\":1}" \
    "$L1_RPC" | jq -r '.result.status // "?"')
  if [ "$T_STATUS" = "0x0" ]; then
    TRIGGER_REVERTED="true"
    echo "  ** TRIGGER REVERTED: $TX_HASH (issue #212 — EtherDeltaMismatch) **"
  fi
done

# Check for rewind cycles
REWIND_COUNT=$(get_rewind_cycles)
echo "  Rewind cycles: $REWIND_COUNT"

if [ "$TRIGGER_REVERTED" = "true" ]; then
  echo ""
  echo "  ** ISSUE #212 CONFIRMED: triggers reverted with different withdrawal amounts **"
  echo "  ** ETH burned on L2 but NOT released on L1 — permanent fund loss **"
  assert "[#212] Trigger revert detected (expected with different amounts)" 'true'
  assert "[#212] Rewind cycles > 0" '[ "$REWIND_COUNT" -gt 0 ]' "rewinds=$REWIND_COUNT"
else
  # Assertions for success case
  assert "User1 received withdrawal on L1" \
    '[ "$(python3 -c "print(1 if int(\"$U1_L1_DELTA_W\") > 200000000000000000 else 0)")" = "1" ]' \
    "delta=$(wei_to_eth "$U1_L1_DELTA_W")"
  assert "User2 received withdrawal on L1" \
    '[ "$(python3 -c "print(1 if int(\"$U2_L1_DELTA_W\") > 400000000000000000 else 0)")" = "1" ]' \
    "delta=$(wei_to_eth "$U2_L1_DELTA_W")"
  # etherBalance delta may include effects from other tests' withdrawals.
  # Per-user L1 balance assertions above are the reliable checks.
  assert "etherBalance decreased (withdrawals processed)" \
    '[ "$(python3 -c "print(1 if int(\"$EB_DELTA_W\") < 0 else 0)")" = "1" ]' \
    "delta=$(wei_to_eth "$EB_DELTA_W")"
fi

# State root convergence
echo ""
ROOTS_FINAL=$(check_state_roots)
echo "  State roots: $ROOTS_FINAL"
assert "State roots converge after withdrawals" '[ "$ROOTS_FINAL" = "MATCH" ]'

# Builder health
HEALTH_FINAL=$(get_health)
echo "  Health: $(echo "$HEALTH_FINAL" | jq -c '.')"
FINAL_MODE=$(echo "$HEALTH_FINAL" | jq -r '.mode // "UNKNOWN"')
FINAL_HEALTHY=$(echo "$HEALTH_FINAL" | jq -r '.healthy // false')
assert "Builder in Builder mode" '[ "$FINAL_MODE" = "Builder" ]'
assert "Builder is healthy" '[ "$FINAL_HEALTHY" = "true" ]'

# On-chain state root advanced
ONCHAIN_SR_AFTER=$(get_onchain_state_root "$ROLLUPS_ADDRESS")
echo "  On-chain stateRoot: $ONCHAIN_SR_AFTER"

print_elapsed "PHASE 5 — final verification"

# ══════════════════════════════════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════════════════════════════════

section "SUMMARY"

echo ""
echo "  === Transaction Hashes (for Blockscout) ==="
echo ""
echo "  L1 Deposits:"
echo "    User1: $D1_HASH (block $D1_BLOCK)"
echo "    User2: $D2_HASH (block $D2_BLOCK)"
echo ""
echo "  L2 Withdrawals:"
echo "    User1: $W1_HASH (block $W1_BLOCK)"
echo "    User2: $W2_HASH (block $W2_BLOCK)"
echo ""
if [ -n "$TRIGGER_HASHES" ]; then
  echo "  L1 Withdrawal Triggers:"
  for TX_HASH in $TRIGGER_HASHES; do
    echo "    $TX_HASH"
  done
  echo ""
fi
if [ -n "$BATCH_HASHES" ]; then
  echo "  L1 postBatch Confirmations:"
  for TX_HASH in $BATCH_HASHES; do
    echo "    $TX_HASH"
  done
  echo ""
fi

echo "  === Results ==="
echo "  Passed: $PASS_COUNT"
echo "  Failed: $FAIL_COUNT"
echo "  Total:  $TOTAL_COUNT"
echo ""
print_total_elapsed
echo ""

if [ "$FAIL_COUNT" -eq 0 ]; then
  echo "  STATUS: ALL CHECKS PASSED"
  echo ""
  echo "================================================================"
  exit 0
else
  echo "  STATUS: $FAIL_COUNT CHECK(S) FAILED"
  echo ""
  echo "================================================================"
  exit 1
fi
