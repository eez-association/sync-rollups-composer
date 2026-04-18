#!/usr/bin/env bash
# run.sh — End-to-end soak test orchestrator.
#
# Deploys the arb stack (setup.sh), runs 2 arb bots + 1 trader concurrently
# for SOAK_DURATION seconds, monitors health + bot logs, and exits 0/1 based
# on the pass/fail verdict in /tmp/soak-verdict.json.
#
# Environment:
#   SOAK_DURATION  seconds to run after setup completes (default 600 = 10 min)
#   SOAK_SKIP_SETUP=1  reuse existing /tmp/arb_config{,_2}.json (skip setup.sh)
#   L1_RPC, L1_PROXY, L2_RPC, HEALTH_URL — per lib-health-check.sh conventions.
#
# Usage:
#   ./scripts/e2e/arb-soak/run.sh                             # testnet (default ports)
#   HEALTH_URL=http://localhost:11560/health \
#     L1_RPC=http://localhost:11555 \
#     L1_PROXY=http://localhost:11556 \
#     L2_RPC=http://localhost:11545 \
#     SOAK_DURATION=1800 \
#     ./scripts/e2e/arb-soak/run.sh                           # devnet, 30 min

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../lib-health-check.sh
source "$SCRIPT_DIR/../lib-health-check.sh"

SOAK_DURATION="${SOAK_DURATION:-600}"
VERDICT_FILE="${VERDICT_FILE:-/tmp/soak-verdict.json}"
SKIP_SETUP="${SOAK_SKIP_SETUP:-0}"
VENV_DIR="${VENV_DIR:-/tmp/soak-venv}"

# Bootstrap a dedicated venv with web3/eth-account so we don't touch the system
# Python (PEP 668). Reused across runs.
if [ ! -x "$VENV_DIR/bin/python3" ]; then
    python3 -m venv "$VENV_DIR"
    "$VENV_DIR/bin/pip" install --quiet --upgrade pip
    "$VENV_DIR/bin/pip" install --quiet 'web3>=6' 'eth-account>=0.10'
fi
PY="$VENV_DIR/bin/python3"

blue()  { printf '\033[0;34m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
red()   { printf '\033[0;31m%s\033[0m\n' "$*" >&2; }

cleanup() {
    set +e
    for p in /tmp/arb_bot.pid /tmp/arb_bot2.pid /tmp/trader.pid; do
        if [ -f "$p" ]; then
            kill "$(cat "$p")" 2>/dev/null
        fi
    done
    # Give them a few seconds to flush final JSON lines
    sleep 2
}
trap cleanup EXIT

if [ "$SKIP_SETUP" = "0" ]; then
    blue "=== [1/4] Setup ==="
    bash "$SCRIPT_DIR/setup.sh"
else
    blue "=== [1/4] Setup (SKIPPED — reusing /tmp/arb_config{,_2}.json) ==="
fi

[ -f /tmp/arb_config.json ]   || { red "missing /tmp/arb_config.json"; exit 1; }
[ -f /tmp/arb_config_2.json ] || { red "missing /tmp/arb_config_2.json"; exit 1; }

# Truncate previous bot log files so monitor.py counts only this run
: > /tmp/arb_bot.log
: > /tmp/arb_bot2.log
: > /tmp/trader.log

blue "=== [2/4] Launch bots + trader (duration=${SOAK_DURATION}s) ==="
"$PY" "$SCRIPT_DIR/bot.py" /tmp/arb_config.json   --duration "$SOAK_DURATION" \
    > /tmp/arb_bot.stdout  2> /tmp/arb_bot.stderr  &
BOT1_PID=$!
"$PY" "$SCRIPT_DIR/bot.py" /tmp/arb_config_2.json --duration "$SOAK_DURATION" \
    > /tmp/arb_bot2.stdout 2> /tmp/arb_bot2.stderr &
BOT2_PID=$!
"$PY" "$SCRIPT_DIR/trader.py" --duration "$SOAK_DURATION" \
    > /tmp/trader.stdout   2> /tmp/trader.stderr  &
TRADER_PID=$!
green "  bot1=pid:$BOT1_PID  bot2=pid:$BOT2_PID  trader=pid:$TRADER_PID"

blue "=== [3/4] Monitor (duration=${SOAK_DURATION}s) ==="
MONITOR_EXTRA=()
if [ -n "${DOCKER_COMPOSE_CMD:-}" ]; then
    MONITOR_EXTRA+=(--docker-compose-cmd "$DOCKER_COMPOSE_CMD")
fi
set +e
"$PY" "$SCRIPT_DIR/monitor.py" \
    --health-url "$HEALTH_URL" \
    --duration "$SOAK_DURATION" \
    --out "$VERDICT_FILE" \
    "${MONITOR_EXTRA[@]}"
VERDICT_EXIT=$?
set -e

wait "$BOT1_PID"   2>/dev/null || true
wait "$BOT2_PID"   2>/dev/null || true
wait "$TRADER_PID" 2>/dev/null || true

blue "=== [4/4] Verdict ==="
if [ "$VERDICT_EXIT" = "0" ]; then
    green "PASS — see $VERDICT_FILE"
else
    red "FAIL — see $VERDICT_FILE"
fi
cat "$VERDICT_FILE" | python3 -m json.tool | head -30
exit "$VERDICT_EXIT"
