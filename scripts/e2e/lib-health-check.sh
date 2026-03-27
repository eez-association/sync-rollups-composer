#!/usr/bin/env bash
# lib-health-check.sh — Shared helpers for E2E health check scripts.
#
# Source this file from health check scripts:
#   SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   source "$SCRIPT_DIR/lib-health-check.sh"

# ── Default Endpoints ──

L1_RPC="${L1_RPC:-http://localhost:9555}"
L2_RPC="${L2_RPC:-http://localhost:9545}"
FULLNODE1_RPC="${FULLNODE1_RPC:-http://localhost:9546}"
FULLNODE2_RPC="${FULLNODE2_RPC:-http://localhost:9547}"
L1_PROXY="${L1_PROXY:-http://localhost:9556}"
L2_PROXY="${L2_PROXY:-http://localhost:9548}"
HEALTH_URL="${HEALTH_URL:-http://localhost:9560/health}"

# Auto-detect compose path from HEALTH_URL port (devnet=11560, testnet=9560).
# This ensures log greps target the same network as the RPC endpoints.
if [ -z "${DOCKER_COMPOSE_CMD:-}" ]; then
  _REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  if echo "$HEALTH_URL" | grep -q "11560"; then
    DOCKER_COMPOSE_CMD="sudo docker compose -f ${_REPO_ROOT}/deployments/devnet-eez/docker-compose.yml -f ${_REPO_ROOT}/deployments/devnet-eez/docker-compose.dev.yml"
  elif [ -f "${_REPO_ROOT}/deployments/testnet-eez/docker-compose.yml" ]; then
    DOCKER_COMPOSE_CMD="sudo docker compose -f ${_REPO_ROOT}/deployments/testnet-eez/docker-compose.yml -f ${_REPO_ROOT}/deployments/testnet-eez/docker-compose.dev.yml"
  else
    DOCKER_COMPOSE_CMD="sudo docker compose -f docker-compose.yml -f docker-compose.dev.yml"
  fi
fi

# ── Counters ──

PASS_COUNT=0
FAIL_COUNT=0
TOTAL_COUNT=0

# ── JSON output mode ──

JSON_MODE="${JSON_MODE:-false}"
JSON_RESULTS=()

# ── Timing ──

_LIB_GLOBAL_START=$(date +%s)

start_timer() {
  _LIB_TIMER_START=$(date +%s)
}

print_elapsed() {
  local label="$1"
  local now elapsed
  now=$(date +%s)
  elapsed=$((now - _LIB_TIMER_START))
  echo "  [$label completed in ${elapsed}s]"
}

print_total_elapsed() {
  local now elapsed
  now=$(date +%s)
  elapsed=$((now - _LIB_GLOBAL_START))
  echo "  [Total elapsed: ${elapsed}s]"
}

# ── RPC Helpers ──

rpc_call() {
  local url="$1" method="$2" params="$3"
  curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}" \
    "$url" | jq -r '.result // empty'
}

get_balance() {
  local url="$1" addr="$2"
  local hex
  hex=$(rpc_call "$url" "eth_getBalance" "[\"$addr\",\"latest\"]")
  python3 -c "print(int('${hex}', 16))" 2>/dev/null || echo "0"
}

get_block_number() {
  local url="$1"
  rpc_call "$url" "eth_blockNumber" "[]"
}

get_state_root() {
  local url="$1" block="$2"
  curl -s -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBlockByNumber\",\"params\":[\"$block\",false],\"id\":1}" \
    "$url" | jq -r '.result.stateRoot // "NOT_FOUND"'
}

wei_to_eth() {
  python3 -c "print(f'{int(\"$1\") / 1e18:.6f}')" 2>/dev/null || echo "?"
}

# ── State Root Convergence ──

# Check state roots across builder and fullnodes at builder_block - 5 (floor 0x1).
check_state_roots() {
  local builder_block check_block check_dec r1 r2 r3
  builder_block=$(get_block_number "$L2_RPC")
  check_dec=$(( $(printf '%d' "$builder_block") - 5 ))
  if [ "$check_dec" -lt 1 ]; then
    check_dec=1
  fi
  check_block=$(printf '0x%x' "$check_dec")
  r1=$(get_state_root "$L2_RPC" "$check_block")
  r2=$(get_state_root "$FULLNODE1_RPC" "$check_block")
  r3=$(get_state_root "$FULLNODE2_RPC" "$check_block")
  if [ "$r1" = "$r2" ] && [ "$r2" = "$r3" ] && [ "$r1" != "NOT_FOUND" ]; then
    echo "MATCH"
  else
    echo "MISMATCH (builder=$r1 fn1=$r2 fn2=$r3 block=$check_block)"
  fi
}

# Poll until state roots converge or timeout (default 120s).
wait_for_convergence() {
  local timeout="${1:-120}" elapsed=0
  while [ "$elapsed" -lt "$timeout" ]; do
    local result
    result=$(check_state_roots)
    if [ "$result" = "MATCH" ]; then
      echo "MATCH"
      return 0
    fi
    sleep 5
    elapsed=$((elapsed + 5))
  done
  check_state_roots
}

# ── Health Endpoint ──

get_health() {
  curl -s "$HEALTH_URL" 2>/dev/null
}

check_health_summary() {
  local health pending rewinds
  health=$(get_health)
  pending=$(echo "$health" | jq -r '.pending_submissions // "?"')
  rewinds=$(echo "$health" | jq -r '.consecutive_rewind_cycles // "?"')
  echo "pending=$pending rewinds=$rewinds"
}

get_rewind_cycles() {
  get_health | jq -r '.consecutive_rewind_cycles // 0' 2>/dev/null || echo "0"
}

# Update PEAK_REWIND_CYCLES if current value is higher.
sample_rewind_cycles() {
  local current
  current=$(get_rewind_cycles)
  if [ "$current" -gt "${PEAK_REWIND_CYCLES:-0}" ] 2>/dev/null; then
    PEAK_REWIND_CYCLES="$current"
  fi
}

# Poll health for N seconds, sampling rewind cycles.
monitor_rewinds_for() {
  local seconds="$1" elapsed=0
  while [ "$elapsed" -lt "$seconds" ]; do
    sample_rewind_cycles
    sleep 5
    elapsed=$((elapsed + 5))
  done
}

# Poll until pending submissions reach 0 or timeout (default 120s).
wait_for_pending_zero() {
  local timeout="${1:-120}" elapsed=0
  while [ "$elapsed" -lt "$timeout" ]; do
    local health pending
    health=$(get_health)
    pending=$(echo "$health" | jq -r '.pending_submissions // "?"')
    if [ "$pending" = "0" ]; then
      echo "0"
      return 0
    fi
    sleep 5
    elapsed=$((elapsed + 5))
  done
  get_health | jq -r '.pending_submissions // "?"'
}

# ── Polling Helpers ──

# Poll until block number advances N blocks past baseline, or timeout.
# Usage: wait_for_block_advance $url $baseline_hex $n $timeout
wait_for_block_advance() {
  local url="$1" baseline_hex="$2" n="$3" timeout="${4:-120}"
  local baseline_dec target elapsed=0 current_hex current_dec
  baseline_dec=$(printf '%d' "$baseline_hex")
  target=$((baseline_dec + n))
  while [ "$elapsed" -lt "$timeout" ]; do
    current_hex=$(get_block_number "$url")
    current_dec=$(printf '%d' "$current_hex")
    if [ "$current_dec" -ge "$target" ]; then
      echo "$current_hex"
      return 0
    fi
    sleep 3
    elapsed=$((elapsed + 3))
  done
  # Timeout — return current block
  get_block_number "$url"
  return 1
}

# Poll health until mode=Builder or timeout.
wait_for_builder_ready() {
  local timeout="${1:-60}" elapsed=0
  while [ "$elapsed" -lt "$timeout" ]; do
    local health mode
    health=$(get_health 2>/dev/null || true)
    mode=$(echo "$health" | jq -r '.mode // "UNKNOWN"' 2>/dev/null || echo "UNKNOWN")
    if [ "$mode" = "Builder" ]; then
      echo "$mode"
      return 0
    fi
    sleep 3
    elapsed=$((elapsed + 3))
  done
  echo "TIMEOUT"
  return 1
}

# ── On-chain State Root (Rollups.sol) ──

# Read stateRoot from Rollups.sol for rollupId=1.
# Usage: get_onchain_state_root $rollups_addr
get_onchain_state_root() {
  local rollups_addr="$1"
  # Rollups.rollups(1) selector = 0xb794e5a3
  # Return struct layout: word0=owner(address), word1=rollupId, word2=stateRoot, word3=etherBalance
  # stateRoot is at word 2 = byte offset 64..128 (hex chars 128..192)
  local data
  data=$(rpc_call "$L1_RPC" "eth_call" \
    "[{\"to\":\"$rollups_addr\",\"data\":\"0xb794e5a30000000000000000000000000000000000000000000000000000000000000001\"},\"latest\"]")
  if [ -z "$data" ] || [ "$data" = "null" ]; then
    echo "0x0"
    return
  fi
  data="${data#0x}"
  # stateRoot is at word 2 (chars 128..192)
  echo "0x${data:128:64}"
}

# ── Cast Send with Retry ──

# Retry cast send up to 3 times with 5s delay.
# Usage: retry_cast_send <args...>
# Returns the full output of the successful cast send.
retry_cast_send() {
  local attempts=3 delay=5 i result
  for ((i=1; i<=attempts; i++)); do
    result=$(cast send "$@" 2>&1) && { echo "$result"; return 0; }
    if [ "$i" -lt "$attempts" ]; then
      echo "  [retry_cast_send: attempt $i failed, retrying in ${delay}s...]" >&2
      sleep "$delay"
    fi
  done
  echo "$result"
  return 1
}

# ── Assertions ──

# Dump diagnostic info on failure.
on_failure_dump() {
  local test_name="$1"
  echo ""
  echo "  === FAILURE DUMP: $test_name ==="
  echo "  -- Health --"
  get_health 2>/dev/null | jq -c '.' 2>/dev/null || echo "  (health unavailable)"
  echo "  -- L2 block --"
  local blk
  blk=$(get_block_number "$L2_RPC" 2>/dev/null || echo "?")
  echo "  builder block: $(printf '%d' "$blk" 2>/dev/null || echo "$blk")"
  echo "  -- State roots --"
  check_state_roots
  echo "  -- Recent builder logs (last 30s) --"
  $DOCKER_COMPOSE_CMD logs builder --no-log-prefix --since 30s 2>&1 | tail -20 || true
  echo "  === END DUMP ==="
  echo ""
}

assert() {
  local test_name="$1" condition="$2" detail="${3:-}"
  TOTAL_COUNT=$((TOTAL_COUNT + 1))
  if eval "$condition"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    if [ "$JSON_MODE" = "true" ]; then
      JSON_RESULTS+=("{\"name\":$(echo "$test_name" | jq -Rs .),\"status\":\"PASS\"}")
    else
      echo "  PASS: $test_name"
    fi
  else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    if [ "$JSON_MODE" = "true" ]; then
      JSON_RESULTS+=("{\"name\":$(echo "$test_name" | jq -Rs .),\"status\":\"FAIL\",\"detail\":$(echo "${detail:-}" | jq -Rs .)}")
    else
      echo "  FAIL: $test_name ${detail:+($detail)}"
      on_failure_dump "$test_name"
    fi
  fi
}

# ── JSON Summary ──

print_json_summary() {
  local suite_name="$1"
  echo "{"
  echo "  \"suite\": \"$suite_name\","
  echo "  \"passed\": $PASS_COUNT,"
  echo "  \"failed\": $FAIL_COUNT,"
  echo "  \"total\": $TOTAL_COUNT,"
  echo "  \"results\": ["
  local i
  for ((i=0; i<${#JSON_RESULTS[@]}; i++)); do
    if [ "$i" -lt $((${#JSON_RESULTS[@]} - 1)) ]; then
      echo "    ${JSON_RESULTS[$i]},"
    else
      echo "    ${JSON_RESULTS[$i]}"
    fi
  done
  echo "  ]"
  echo "}"
}

# ── Parse --json flag ──

parse_lib_args() {
  for arg in "$@"; do
    if [ "$arg" = "--json" ]; then
      JSON_MODE="true"
    fi
  done
}
