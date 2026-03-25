#!/usr/bin/env bash
# Deploy and test PingPongReturn contracts — regression test for issue #242.
#
# Like deploy-ping-pong.sh but the contracts return uint256 from ping/pong/start,
# and the test verifies that return data propagates across the L2→L1 boundary.
#
# Deploys:
#   - PingPongReturnL2 on L2  (start/pong return uint256, store lastReturnValue)
#   - PingPongReturnL1 on L1  (ping returns uint256(pongCount))
#   - CrossChainProxies on both sides
#
# After deployment, runs start(1) and verifies:
#   - L1 PingPongReturnL1.pongCount == 1
#   - L1 PingPongReturnL1.done == true
#   - L2 PingPongReturnL2.pingCount == 1
#   - L2 PingPongReturnL2.lastReturnValue == 1  (KEY: return data from L1)
#
# Account used: dev#11 (0x71bE63f3384f5fb98995898A86B02Fb2426c5788)
# WARNING: Uses well-known Anvil dev key. LOCAL DEVELOPMENT ONLY.
set -euo pipefail
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1

# ── Constants ─────────────────────────────────────────────────────────────────

DEFAULT_PK="0x701b615bbdfb9de65240bc28bd21bbc0d996645a3dd57e7b12bc2bdf6f192c82"
DEFAULT_ADDR="0x71bE63f3384f5fb98995898A86B02Fb2426c5788"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CONTRACTS_DIR="${REPO_ROOT}/contracts/test-depth2"

source "$SCRIPT_DIR/lib-health-check.sh"

# ── Defaults ──────────────────────────────────────────────────────────────────

L1_RPC="${L1_RPC:-http://localhost:9555}"
L2_RPC="${L2_RPC:-http://localhost:9545}"
L2_PROXY="${L2_PROXY:-http://localhost:9548}"
PK="${PK:-$DEFAULT_PK}"
ROLLUPS_ADDRESS="${ROLLUPS_ADDRESS:-}"
CROSS_CHAIN_MANAGER_ADDRESS="${CROSS_CHAIN_MANAGER_ADDRESS:-}"
ROLLUP_ID="${ROLLUP_ID:-1}"

# ── Parse CLI args ────────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --l1-rpc)        L1_RPC="$2";                        shift 2;;
        --l2-rpc)        L2_RPC="$2";                        shift 2;;
        --l2-proxy)      L2_PROXY="$2";                      shift 2;;
        --pk)            PK="$2";                            shift 2;;
        --rollups)       ROLLUPS_ADDRESS="$2";               shift 2;;
        --manager-l2)    CROSS_CHAIN_MANAGER_ADDRESS="$2";   shift 2;;
        --rollup-id)     ROLLUP_ID="$2";                     shift 2;;
        --json)          JSON_MODE=true;                     shift;;
        *) echo "Unknown argument: $1"; exit 1;;
    esac
done

# ── Load rollup.env ───────────────────────────────────────────────────────────

if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
    echo "Loading rollup.env..."
    ROLLUP_ENV_FILE=""
    if [ -f "/shared/rollup.env" ]; then
        ROLLUP_ENV_FILE="/shared/rollup.env"
    elif [ -n "${SHARED_DIR:-}" ] && [ -f "${SHARED_DIR}/rollup.env" ]; then
        ROLLUP_ENV_FILE="${SHARED_DIR}/rollup.env"
    else
        ROLLUP_ENV_FILE="/tmp/_rollup_env_$$"
        sudo docker exec testnet-eez-builder-1 cat /shared/rollup.env > "$ROLLUP_ENV_FILE" 2>/dev/null || true
    fi
    if [ -n "$ROLLUP_ENV_FILE" ] && [ -f "$ROLLUP_ENV_FILE" ]; then
        eval "$(cat "$ROLLUP_ENV_FILE")"
    fi
fi

if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
    echo "ERROR: ROLLUPS_ADDRESS not set. Start devnet first or pass --rollups."
    exit 1
fi
if [ -z "${CROSS_CHAIN_MANAGER_ADDRESS:-}" ]; then
    echo "ERROR: CROSS_CHAIN_MANAGER_ADDRESS not set."
    exit 1
fi

# ── Fund deployer on L1 (keys #10+ are not pre-funded by reth --dev) ──
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L1_BAL_CHECK=$(cast balance --rpc-url "$L1_RPC" "$DEFAULT_ADDR" 2>/dev/null || echo "0")
if [ "$L1_BAL_CHECK" = "0" ] || [ "$L1_BAL_CHECK" = "0x0" ]; then
    echo "Funding $DEFAULT_ADDR on L1 with 100 ETH (dev#9 funder)..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$DEFAULT_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
fi

echo ""
echo "=========================================="
echo "  PingPongReturn Deploy + Test (#242)"
echo "=========================================="
echo "L1 RPC:      $L1_RPC"
echo "L2 RPC:      $L2_RPC"
echo "L2 Proxy:    $L2_PROXY"
echo "Rollups:     $ROLLUPS_ADDRESS"
echo "CCM L2:      $CROSS_CHAIN_MANAGER_ADDRESS"
echo "Rollup ID:   $ROLLUP_ID"
echo "Deployer:    $DEFAULT_ADDR"
echo ""

# ── Step 0: Compile ──────────────────────────────────────────────────────────

echo "====== Step 0: Compile PingPongReturn contracts ======"
cd "$CONTRACTS_DIR"
forge build
echo "Compilation successful."
echo ""

# ── Check deployer balance ───────────────────────────────────────────────────

L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$DEFAULT_ADDR" 2>/dev/null || echo "0")
L2_BAL=$(cast balance --rpc-url "$L2_RPC" "$DEFAULT_ADDR" 2>/dev/null || echo "0")
echo "Deployer balances:"
echo "  L1: $L1_BAL wei"
echo "  L2: $L2_BAL wei"

if [ "$L1_BAL" = "0" ] || [ "$L1_BAL" = "0x0" ]; then
    echo "ERROR: Deployer has no ETH on L1."
    exit 1
fi
if [ "$L2_BAL" = "0" ] || [ "$L2_BAL" = "0x0" ]; then
    echo "Auto-funding deployer on L2 via bridge..."
    L1_PROXY_URL="${L1_PROXY:-http://localhost:9556}"
    BRIDGE_ADDRESS="${BRIDGE_L1_ADDRESS:-${BRIDGE_ADDRESS:-}}"
    if [ -z "$BRIDGE_ADDRESS" ]; then
        echo "ERROR: BRIDGE_ADDRESS not found."
        exit 1
    fi
    cast send --rpc-url "$L1_PROXY_URL" --private-key "$PK" \
        "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" "$ROLLUP_ID" "$DEFAULT_ADDR" \
        --value 1ether --gas-limit 800000 > /dev/null 2>&1
    echo "  Deposit sent. Waiting 30s for L2 processing..."
    sleep 30
    L2_BAL=$(cast balance "$DEFAULT_ADDR" --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
    if [ "$L2_BAL" = "0" ] || [ "$L2_BAL" = "0x0" ]; then
        echo "ERROR: L2 balance still 0 after deposit."
        exit 1
    fi
fi
echo ""

# ── Step 1: Deploy PingPongReturnL2 on L2 ────────────────────────────────────

echo "====== Step 1: Deploy PingPongReturnL2 on L2 ======"
PP_L2_OUTPUT=$(forge create \
    --rpc-url "$L2_RPC" \
    --private-key "$PK" \
    --broadcast \
    src/PingPongReturnL2.sol:PingPongReturnL2 2>&1)
echo "$PP_L2_OUTPUT" | tail -3
PP_L2=$(echo "$PP_L2_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
if [ -z "$PP_L2" ]; then
    echo "ERROR: PingPongReturnL2 deployment failed"
    echo "$PP_L2_OUTPUT"
    exit 1
fi
echo "PingPongReturnL2 deployed at: $PP_L2"
echo ""

# ── Step 2: Deploy PingPongReturnL1 on L1 ────────────────────────────────────

echo "====== Step 2: Deploy PingPongReturnL1 on L1 ======"
PP_L1_OUTPUT=$(forge create \
    --rpc-url "$L1_RPC" \
    --private-key "$PK" \
    --broadcast \
    src/PingPongReturnL1.sol:PingPongReturnL1 2>&1)
echo "$PP_L1_OUTPUT" | tail -3
PP_L1=$(echo "$PP_L1_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
if [ -z "$PP_L1" ]; then
    echo "ERROR: PingPongReturnL1 deployment failed"
    echo "$PP_L1_OUTPUT"
    exit 1
fi
echo "PingPongReturnL1 deployed at: $PP_L1"
echo ""

# ── Step 3: Create L1 proxy for PingPongReturnL2 ─────────────────────────────

echo "====== Step 3: Create L1-side proxy for PingPongReturnL2 ======"
L1_PROXY_FOR_L2=$(cast call --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" \
    "computeCrossChainProxyAddress(address,uint256)(address)" \
    "$PP_L2" "$ROLLUP_ID" 2>&1)
echo "Expected L1 proxy: $L1_PROXY_FOR_L2"

cast send --rpc-url "$L1_RPC" --private-key "$PK" \
    "$ROLLUPS_ADDRESS" "createCrossChainProxy(address,uint256)" \
    "$PP_L2" "$ROLLUP_ID" --gas-limit 500000 > /dev/null

PROXY_CODE=$(cast code --rpc-url "$L1_RPC" "$L1_PROXY_FOR_L2" 2>/dev/null || echo "0x")
if [ "$PROXY_CODE" = "0x" ] || [ -z "$PROXY_CODE" ]; then
    echo "ERROR: L1 proxy not deployed at $L1_PROXY_FOR_L2"
    exit 1
fi
echo "L1 proxy verified at: $L1_PROXY_FOR_L2"
echo ""

# ── Step 4: Create L2 proxy for PingPongReturnL1 ─────────────────────────────

echo "====== Step 4: Create L2-side proxy for PingPongReturnL1 ======"
L2_PROXY_FOR_L1=$(cast call --rpc-url "$L2_RPC" \
    "$CROSS_CHAIN_MANAGER_ADDRESS" \
    "computeCrossChainProxyAddress(address,uint256)(address)" \
    "$PP_L1" "0" 2>&1)
echo "Expected L2 proxy: $L2_PROXY_FOR_L1"

cast send --rpc-url "$L2_RPC" --private-key "$PK" \
    "$CROSS_CHAIN_MANAGER_ADDRESS" "createCrossChainProxy(address,uint256)(address)" \
    "$PP_L1" "0" --gas-limit 500000 > /dev/null

L2_PROXY_CODE=$(cast code --rpc-url "$L2_RPC" "$L2_PROXY_FOR_L1" 2>/dev/null || echo "0x")
if [ "$L2_PROXY_CODE" = "0x" ] || [ -z "$L2_PROXY_CODE" ]; then
    echo "ERROR: L2 proxy not deployed at $L2_PROXY_FOR_L1"
    exit 1
fi
echo "L2 proxy verified at: $L2_PROXY_FOR_L1"
echo ""

# ── Step 5: Setup PingPongReturnL2 ───────────────────────────────────────────

echo "====== Step 5: Setup PingPongReturnL2 ======"
cast send --rpc-url "$L2_RPC" --private-key "$PK" \
    "$PP_L2" "setup(address,address)" "$L2_PROXY_FOR_L1" "$PP_L1" > /dev/null

STORED_PROXY=$(cast call --rpc-url "$L2_RPC" "$PP_L2" "pingPongL1Proxy()(address)" 2>/dev/null)
STORED_L1=$(cast call --rpc-url "$L2_RPC" "$PP_L2" "pingPongL1()(address)" 2>/dev/null)
echo "  pingPongL1Proxy: $STORED_PROXY"
echo "  pingPongL1:      $STORED_L1"
if [ "${STORED_PROXY,,}" != "${L2_PROXY_FOR_L1,,}" ]; then
    echo "ERROR: proxy mismatch"; exit 1
fi
echo ""

# ── Step 6: Setup PingPongReturnL1 ───────────────────────────────────────────

echo "====== Step 6: Setup PingPongReturnL1 ======"
cast send --rpc-url "$L1_RPC" --private-key "$PK" \
    "$PP_L1" "setup(address)" "$L1_PROXY_FOR_L2" > /dev/null

STORED_L2_PROXY=$(cast call --rpc-url "$L1_RPC" "$PP_L1" "pingPongL2Proxy()(address)" 2>/dev/null)
echo "  pingPongL2Proxy: $STORED_L2_PROXY"
if [ "${STORED_L2_PROXY,,}" != "${L1_PROXY_FOR_L2,,}" ]; then
    echo "ERROR: proxy mismatch"; exit 1
fi
echo ""

# ══════════════════════════════════════════
#  TEST: start(1) — single-hop L2→L1 with return data
# ══════════════════════════════════════════

echo "=========================================="
echo "  TEST: start(1) — L2->L1 with return data"
echo "=========================================="
start_timer

echo "Calling PingPongReturnL2.start(1) via L2 proxy..."
RESULT=$(cast send \
    --rpc-url "$L2_PROXY" \
    --private-key "$PK" \
    "$PP_L2" "start(uint256)" 1 \
    --gas-limit 1000000 \
    --json 2>&1 || echo "{}")

TX_HASH=$(echo "$RESULT" | grep -oP '"transactionHash"\s*:\s*"\K[^"]+' || echo "")
TX_STATUS=$(echo "$RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")

echo "Tx hash:   ${TX_HASH:-<not found>}"
echo "Tx status: ${TX_STATUS:-<not found>}"

if [ "$TX_STATUS" != "0x1" ] && [ -n "$TX_HASH" ]; then
    echo "Waiting for tx (up to 60s)..."
    for _i in $(seq 1 12); do
        sleep 5
        RECEIPT=$(cast receipt --rpc-url "$L2_RPC" "$TX_HASH" --json 2>/dev/null || echo "")
        TX_STATUS=$(echo "$RECEIPT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
        if [ -n "$TX_STATUS" ]; then break; fi
    done
fi

assert "start(1) L2 tx succeeded" '[ "$TX_STATUS" = "0x1" ]' \
    "status=$TX_STATUS hash=$TX_HASH"

# Wait for L1 trigger to execute and state to settle.
echo "Waiting for L1 trigger and convergence..."
wait_for_pending_zero 90 >/dev/null || true

L2_BLK=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK" 3 60 >/dev/null || true
wait_for_pending_zero 60 >/dev/null || true

print_elapsed "start(1) call"
echo ""

# ══════════════════════════════════════════
#  VERIFY: check state on L1 and L2
# ══════════════════════════════════════════

echo "=========================================="
echo "  VERIFY: State after start(1)"
echo "=========================================="
start_timer

# L1 checks
L1_PONG_COUNT=$(cast call --rpc-url "$L1_RPC" "$PP_L1" "pongCount()(uint256)" 2>/dev/null || echo "?")
L1_DONE=$(cast call --rpc-url "$L1_RPC" "$PP_L1" "done()(bool)" 2>/dev/null || echo "?")
echo "L1 PingPongReturnL1.pongCount: $L1_PONG_COUNT (expected: 1)"
echo "L1 PingPongReturnL1.done:      $L1_DONE (expected: true)"

assert "L1 pongCount == 1" '[ "$L1_PONG_COUNT" = "1" ]' "got=$L1_PONG_COUNT"
assert "L1 done == true" '[ "$L1_DONE" = "true" ]' "got=$L1_DONE"

# L2 checks
L2_PING_COUNT=$(cast call --rpc-url "$L2_RPC" "$PP_L2" "pingCount()(uint256)" 2>/dev/null || echo "?")
echo "L2 PingPongReturnL2.pingCount: $L2_PING_COUNT (expected: 1)"
assert "L2 pingCount == 1" '[ "$L2_PING_COUNT" = "1" ]' "got=$L2_PING_COUNT"

# KEY CHECK: return data from L1
L2_LAST_RETURN=$(cast call --rpc-url "$L2_RPC" "$PP_L2" "lastReturnValue()(uint256)" 2>/dev/null || echo "?")
echo ""
echo "L2 PingPongReturnL2.lastReturnValue: $L2_LAST_RETURN (expected: 1)"
echo "  ^^ This is the KEY test — L1's pongCount returned to L2 caller."
echo "  Before #242 fix: 0 (empty return data decoded as 0)."
echo "  After  #242 fix: 1 (pongCount propagated via L2 RESULT entry)."
echo ""

assert "L2 lastReturnValue == 1 (return data propagated from L1)" \
    '[ "$L2_LAST_RETURN" = "1" ]' \
    "got=$L2_LAST_RETURN expected=1"

# State convergence
echo "Checking state root convergence (up to 60s)..."
ROOTS=$(wait_for_convergence 60)
assert "State roots converge" '[ "$ROOTS" = "MATCH" ]'

# Health check
HEALTH=$(get_health)
FINAL_REWINDS=$(echo "$HEALTH" | jq -r '.consecutive_rewind_cycles // "?"')
echo "Rewind cycles: $FINAL_REWINDS"
assert "No rewind cycles" '[ "$FINAL_REWINDS" = "0" ]'

print_elapsed "VERIFY"
echo ""

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════

echo "=========================================="
echo "  PingPongReturn TEST RESULTS"
echo "=========================================="
echo ""
echo "  PingPongReturnL2 (L2): $PP_L2"
echo "  PingPongReturnL1 (L1): $PP_L1"
echo "  L1 proxy for L2:       $L1_PROXY_FOR_L2"
echo "  L2 proxy for L1:       $L2_PROXY_FOR_L1"
echo ""
echo "  L1 pongCount:          $L1_PONG_COUNT"
echo "  L1 done:               $L1_DONE"
echo "  L2 pingCount:          $L2_PING_COUNT"
echo "  L2 lastReturnValue:    $L2_LAST_RETURN  <-- KEY (issue #242)"
echo ""
echo "  Passed: $PASS_COUNT"
echo "  Failed: $FAIL_COUNT"
echo "  Total:  $TOTAL_COUNT"
echo ""
print_total_elapsed
echo ""

if [ "$FAIL_COUNT" -eq 0 ]; then
    echo "STATUS: ALL TESTS PASSED"
    exit 0
else
    echo "STATUS: $FAIL_COUNT TEST(S) FAILED"
    exit 1
fi
