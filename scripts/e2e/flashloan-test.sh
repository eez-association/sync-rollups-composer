#!/usr/bin/env bash
# Flash Loan E2E Test — sends executor.execute() via L1 proxy and verifies results.
# All contracts must be pre-deployed (deploy.sh with DEPLOY_FLASH_LOAN=true).
# Usage: flashloan-test.sh [L1_BLOCK_TO_WAIT_FOR]
set -euo pipefail

L1_RPC="${L1_RPC:-http://localhost:9555}"
L2_RPC="${L2_RPC:-http://localhost:9545}"
FULLNODE1_RPC="${FULLNODE1_RPC:-http://localhost:9546}"
L1_PROXY="${L1_PROXY:-http://localhost:9556}"
HEALTH_URL="${HEALTH_URL:-http://localhost:9560/health}"
# dev#14 key — test caller (dedicated to flashloan-test.sh, not pre-funded by reth --dev)
CALLER_KEY="0xc526ee95bf44d8fc405a158bb884d9d1238d99f0612e9f33d006bb0789009aaa"
CALLER_ADDR="0xdF3e18d64BC6A983f673Ab319CCaE4f1a57C7097"

# Resolve rollup.env: Docker volume path, explicit SHARED_DIR, or extract from container.
if [ -f "/shared/rollup.env" ]; then
    SHARED_DIR="${SHARED_DIR:-/shared}"
elif [ -n "${SHARED_DIR:-}" ] && [ -f "${SHARED_DIR}/rollup.env" ]; then
    : # SHARED_DIR already set and valid
else
    # Running on host — extract from Docker container
    _TMPDIR=$(mktemp -d)
    sudo docker exec testnet-eez-builder-1 cat /shared/rollup.env > "$_TMPDIR/rollup.env" 2>/dev/null || {
        echo "ERROR: Cannot find rollup.env (not in Docker, not in SHARED_DIR, container not running)"
        exit 1
    }
    SHARED_DIR="$_TMPDIR"
    trap "rm -rf $_TMPDIR" EXIT
fi

# Load addresses from rollup.env
while IFS='=' read -r key value; do
    case "$key" in
        FLASH_TOKEN_ADDRESS|FLASH_POOL_ADDRESS|FLASH_EXECUTOR_L2_ADDRESS|\
        FLASH_NFT_ADDRESS|FLASH_EXECUTOR_L2_PROXY_ADDRESS|FLASH_EXECUTOR_L1_ADDRESS)
            export "$key=$value"
            ;;
    esac
done < "${SHARED_DIR}/rollup.env"

: "${FLASH_EXECUTOR_L1_ADDRESS:?FLASH_EXECUTOR_L1_ADDRESS not set in rollup.env}"
: "${FLASH_TOKEN_ADDRESS:?FLASH_TOKEN_ADDRESS not set in rollup.env}"
: "${FLASH_POOL_ADDRESS:?FLASH_POOL_ADDRESS not set in rollup.env}"

ZERO="0x0000000000000000000000000000000000000000"
if [ "$FLASH_EXECUTOR_L1_ADDRESS" = "$ZERO" ]; then
    echo "ERROR: Flash loan contracts were not deployed (FLASH_EXECUTOR_L1_ADDRESS is zero)."
    echo "       Re-deploy with DEPLOY_FLASH_LOAN=true."
    exit 1
fi

echo "=========================================="
echo "  Flash Loan E2E Test"
echo "=========================================="
echo "ExecutorL1:  $FLASH_EXECUTOR_L1_ADDRESS"
echo "Token:       $FLASH_TOKEN_ADDRESS"
echo "Pool:        $FLASH_POOL_ADDRESS"
echo "L1 Proxy:    $L1_PROXY"
echo "Caller:      $CALLER_ADDR"
echo "=========================================="

# ── Fund caller on L1 (dev#14 is not pre-funded by reth --dev) ──
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
CALLER_L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$CALLER_ADDR" 2>/dev/null || echo "0")
if [ "$CALLER_L1_BAL" = "0" ] || [ "$CALLER_L1_BAL" = "0x0" ]; then
    echo "Funding $CALLER_ADDR on L1 with 100 ETH (dev#9 funder)..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$CALLER_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
fi

# Wait for target L1 block if specified
TARGET_BLOCK="${1:-}"
if [ -n "$TARGET_BLOCK" ]; then
    CURRENT=$(cast block-number --rpc-url "$L1_RPC" 2>/dev/null || echo "0")
    if [ "$CURRENT" -ge "$((TARGET_BLOCK + 2))" ] 2>/dev/null; then
        echo "ERROR: Target L1 block $TARGET_BLOCK already passed (current: $CURRENT). Results would not be deterministic."
        exit 1
    fi
    echo "Waiting for L1 block $TARGET_BLOCK (current: $CURRENT)..."
    while true; do
        CURRENT=$(cast block-number --rpc-url "$L1_RPC" 2>/dev/null || echo "0")
        if [ "$CURRENT" -ge "$TARGET_BLOCK" ] 2>/dev/null; then break; fi
        sleep 1
    done
    echo "L1 block $TARGET_BLOCK reached (current: $CURRENT)."
fi

# Capture state before
L2_BLK_BEFORE=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
POOL_BAL_BEFORE=$(cast call --rpc-url "$L1_RPC" "$FLASH_TOKEN_ADDRESS" \
    "balanceOf(address)(uint256)" "$FLASH_POOL_ADDRESS" 2>/dev/null || echo "unknown")
echo "L2 block before: $L2_BLK_BEFORE"
echo "Pool token balance before: $POOL_BAL_BEFORE"

# Send executor.execute() via L1 proxy
echo ""
echo "Sending executor.execute() via L1 proxy..."
SEND_OUTPUT=$(cast send --rpc-url "$L1_PROXY" --private-key "$CALLER_KEY" \
    "$FLASH_EXECUTOR_L1_ADDRESS" "execute()" --gas-limit 2000000 2>&1) || true
echo "Send output: $SEND_OUTPUT"
TX_HASH=$(echo "$SEND_OUTPUT" | grep -oE '0x[0-9a-fA-F]{64}' | head -1 || echo "")

# Wait for L2 to process (3 blocks)
echo ""
echo "Waiting for L2 to process (3 blocks from $L2_BLK_BEFORE)..."
WAIT=0
MAX_WAIT=90
while [ "$WAIT" -lt "$MAX_WAIT" ]; do
    CURRENT_L2=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null)
    if [ -z "$CURRENT_L2" ]; then
        echo "  WARNING: cannot reach L2 RPC, retrying..."
        sleep 3; WAIT=$((WAIT + 3)); continue
    fi
    if [ "$CURRENT_L2" -ge $((L2_BLK_BEFORE + 3)) ] 2>/dev/null; then
        echo "  L2 advanced to $CURRENT_L2"
        break
    fi
    sleep 3; WAIT=$((WAIT + 3))
done
if [ "$WAIT" -ge "$MAX_WAIT" ]; then
    echo "  WARNING: timed out waiting for L2 blocks (last seen: ${CURRENT_L2:-unknown})"
fi
sleep 3  # extra buffer for entry verification

# Results
L2_BLK_AFTER=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "?")
POOL_BAL_AFTER=$(cast call --rpc-url "$L1_RPC" "$FLASH_TOKEN_ADDRESS" \
    "balanceOf(address)(uint256)" "$FLASH_POOL_ADDRESS" 2>/dev/null || echo "unknown")
BUILDER_ROOT=$(cast block --rpc-url "$L2_RPC" latest --field stateRoot 2>/dev/null || echo "unknown")
FN1_ROOT=$(cast block --rpc-url "$FULLNODE1_RPC" latest --field stateRoot 2>/dev/null || echo "unknown")
HEALTH=$(curl -s "$HEALTH_URL" 2>/dev/null || echo "unavailable")

# L1 tx status
TX_STATUS=0
if [ -n "$TX_HASH" ]; then
    TX_STATUS=$(cast receipt --rpc-url "$L1_RPC" "$TX_HASH" status 2>/dev/null || echo "0")
fi

echo ""
echo "=========================================="
echo "  Results"
echo "=========================================="
echo "L2 blocks advanced: $((L2_BLK_AFTER - L2_BLK_BEFORE)) (${L2_BLK_BEFORE} → ${L2_BLK_AFTER})"
echo "Pool balance before: $POOL_BAL_BEFORE"
echo "Pool balance after:  $POOL_BAL_AFTER"
echo "Builder state root:  $BUILDER_ROOT"
echo "Fullnode1 state root:$FN1_ROOT"
echo "Builder health:      $HEALTH"
echo "L1 tx hash:          ${TX_HASH:-none}"
echo "L1 tx status:        ${TX_STATUS} (1=success)"
echo "=========================================="

if [ "${TX_STATUS}" = "1" ]; then
    echo "PASS: execute() tx succeeded."
    exit 0
else
    echo "FAIL: execute() tx did not succeed (status=${TX_STATUS})."
    exit 1
fi
