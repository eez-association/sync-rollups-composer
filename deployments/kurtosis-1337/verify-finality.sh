#!/usr/bin/env bash
# Verify that the Kurtosis L1 has real finality — head, safe, and finalized
# block numbers should differ, unlike reth --dev which finalizes every block.
#
# Usage:
#   ./kurtosis/verify-finality.sh                   # auto-detect from .env.kurtosis
#   ./kurtosis/verify-finality.sh http://127.0.0.1:XXXXX  # explicit RPC URL
#   ./kurtosis/verify-finality.sh --watch            # poll every 12s
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="${SCRIPT_DIR}/.env.kurtosis"

# ── Parse args ──────────────────────────────────────────────────────
WATCH=false
RPC_URL=""

for arg in "$@"; do
    case "$arg" in
        --watch) WATCH=true ;;
        http://*|https://*) RPC_URL="$arg" ;;
    esac
done

# Auto-detect RPC URL from .env.kurtosis
if [ -z "$RPC_URL" ]; then
    if [ -f "$ENV_FILE" ]; then
        # Prefer the host URL (127.0.0.1) for scripts running on the host
        RPC_URL=$(grep '^KURTOSIS_L1_RPC_URL_HOST=' "$ENV_FILE" | cut -d= -f2-)
        if [ -z "$RPC_URL" ]; then
            RPC_URL=$(grep '^KURTOSIS_L1_RPC_URL=' "$ENV_FILE" | cut -d= -f2-)
        fi
    fi
fi

if [ -z "$RPC_URL" ]; then
    echo "ERROR: No RPC URL. Run kurtosis/start.sh first or pass URL as argument."
    exit 1
fi

# ── Helper ──────────────────────────────────────────────────────────
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[1;34m'
DIM='\033[2m'
RESET='\033[0m'

get_block_number() {
    local tag="$1"
    local hex
    hex=$(curl -sf -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBlockByNumber\",\"params\":[\"${tag}\",false],\"id\":1}" \
        "$RPC_URL" | jq -r '.result.number // empty')
    if [ -n "$hex" ]; then
        printf "%d" "$hex"
    else
        echo "N/A"
    fi
}

check_finality() {
    local head safe finalized

    head=$(get_block_number "latest")
    safe=$(get_block_number "safe")
    finalized=$(get_block_number "finalized")

    local ts
    ts=$(date '+%H:%M:%S')

    echo -e "${BLUE}[${ts}]${RESET} L1 Finality Status  ${DIM}(${RPC_URL})${RESET}"
    echo -e "  Head:      ${GREEN}${head}${RESET}"
    echo -e "  Safe:      ${YELLOW}${safe}${RESET}"
    echo -e "  Finalized: ${RED}${finalized}${RESET}"

    # Check for gaps
    if [ "$head" = "N/A" ] || [ "$safe" = "N/A" ] || [ "$finalized" = "N/A" ]; then
        echo -e "  Status:    ${RED}UNAVAILABLE${RESET} — some block tags returned N/A"
        echo -e "             The chain may still be starting up. Finality takes ~4 epochs (~25 min)."
        return 1
    fi

    local head_safe_gap=$((head - safe))
    local safe_fin_gap=$((safe - finalized))
    local head_fin_gap=$((head - finalized))

    echo -e "  Gaps:      head-safe=${head_safe_gap}  safe-finalized=${safe_fin_gap}  head-finalized=${head_fin_gap}"

    if [ "$head_fin_gap" -gt 0 ]; then
        echo -e "  Status:    ${GREEN}REAL FINALITY${RESET} — head/safe/finalized differ!"
        echo -e "             This is what we need for testing rewind-to-finalized behavior."
        return 0
    else
        echo -e "  Status:    ${YELLOW}NO GAP YET${RESET} — head == finalized (chain may be too young)"
        echo -e "             Wait for ~4 epochs (~25 min with 32 slots/epoch, 12s slots)."
        return 1
    fi
}

# ── Run ─────────────────────────────────────────────────────────────
echo "Checking L1 finality at ${RPC_URL}..."
echo ""

if [ "$WATCH" = true ]; then
    echo "Watching finality every 12 seconds (Ctrl+C to stop)..."
    echo ""
    while true; do
        check_finality || true
        echo ""
        sleep 12
    done
else
    if check_finality; then
        echo ""
        echo "Use --watch to monitor continuously."
        exit 0
    else
        echo ""
        echo "Tip: Use --watch to poll until finality kicks in."
        exit 1
    fi
fi
