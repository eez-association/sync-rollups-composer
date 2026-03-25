#!/usr/bin/env bash
# Deploy configurable-depth PingPong test contracts for issue #236.
#
# Deploys:
#   - PingPongL2 on L2  (start(maxRounds) and pong(round, maxRounds))
#   - PingPongL1 on L1  (ping(round, maxRounds) — terminal when round==maxRounds)
#   - CrossChainProxy for PingPongL2 on L1 (via Rollups.createCrossChainProxy)
#   - CrossChainProxy for PingPongL1 on L2 (via CCM.createCrossChainProxy)
#
# The L2-side proxy for PingPongL1 is created by CCM automatically on first cross-chain
# call, but we compute its address here so both contracts can be set up correctly.
#
# Setup flow:
#   1. Deploy PingPongL2 on L2  (no constructor args — uses setup() pattern)
#   2. Deploy PingPongL1 on L1  (no constructor args — uses setup() pattern)
#   3. Create L1-side proxy for PingPongL2 via Rollups.createCrossChainProxy
#   4. Compute L2-side proxy address for PingPongL1 via CCM.computeCrossChainProxyAddress
#   5. Call PingPongL2.setup(l2ProxyForL1, pingPongL1)
#   6. Call PingPongL1.setup(l1ProxyForL2)
#   7. Verify setup via cast call
#
# Usage:
#   # From host (reads rollup.env from running Docker devnet):
#   bash scripts/e2e/deploy-ping-pong.sh
#
#   # With explicit RPCs and addresses:
#   bash scripts/e2e/deploy-ping-pong.sh \
#     --l1-rpc http://localhost:9555 \
#     --l2-rpc http://localhost:9545 \
#     --rollups 0x... \
#     --manager-l2 0x... \
#     --rollup-id 1
#
#   # Dry run (compile only, no deployment):
#   bash scripts/e2e/deploy-ping-pong.sh --dry-run
#
# Account used: dev#10 (0xBcd4042DE499D14e55001CcbB24a551F3b954096)
#   Avoids conflicts with: #0 deployer/builder, #1 tx-sender, #2 crosschain-health,
#   #3 bridge-health, #4 crosschain-tx-sender, #5 flash-loan/complex-tx-sender,
#   #6 double-deposit-trace, #7 bridge-T18, #8 crosschain-health Counter deployer,
#   #9 L1 funder (used for self-funding by keys #10+).
#
# WARNING: Uses well-known Anvil dev key. LOCAL DEVELOPMENT ONLY.
set -euo pipefail
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1

# ── Constants ─────────────────────────────────────────────────────────────────

# Dev account #10 — dedicated to deploy-ping-pong (never conflicts with Docker services)
DEFAULT_PK="0xf214f2b2cd398c806f84e317254e0f0b801d0643303237d97a22a48e01628897"
DEFAULT_ADDR="0xBcd4042DE499D14e55001CcbB24a551F3b954096"
ZERO="0x0000000000000000000000000000000000000000"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CONTRACTS_DIR="${REPO_ROOT}/contracts/test-depth2"

# ── Defaults (overridden by rollup.env or CLI flags) ──────────────────────────

L1_RPC="${L1_RPC:-http://localhost:9555}"
L2_RPC="${L2_RPC:-http://localhost:9545}"
PK="${PK:-$DEFAULT_PK}"
ROLLUPS_ADDRESS="${ROLLUPS_ADDRESS:-}"
CROSS_CHAIN_MANAGER_ADDRESS="${CROSS_CHAIN_MANAGER_ADDRESS:-}"
ROLLUP_ID="${ROLLUP_ID:-1}"
DRY_RUN=false

# ── Parse CLI args ────────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --l1-rpc)        L1_RPC="$2";                        shift 2;;
        --l2-rpc)        L2_RPC="$2";                        shift 2;;
        --pk)            PK="$2";                            shift 2;;
        --rollups)       ROLLUPS_ADDRESS="$2";               shift 2;;
        --manager-l2)    CROSS_CHAIN_MANAGER_ADDRESS="$2";   shift 2;;
        --rollup-id)     ROLLUP_ID="$2";                     shift 2;;
        --dry-run)       DRY_RUN=true;                       shift;;
        *) echo "Unknown argument: $1"; exit 1;;
    esac
done

# ── Load rollup.env (auto-detect: Docker volume, host SHARED_DIR, or container) ──

if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
    echo "Loading rollup.env..."
    ROLLUP_ENV_FILE=""
    if [ -f "/shared/rollup.env" ]; then
        ROLLUP_ENV_FILE="/shared/rollup.env"
    elif [ -n "${SHARED_DIR:-}" ] && [ -f "${SHARED_DIR}/rollup.env" ]; then
        ROLLUP_ENV_FILE="${SHARED_DIR}/rollup.env"
    else
        # Running on host — extract from running builder container
        ROLLUP_ENV_FILE="/tmp/_rollup_env_$$"
        sudo docker exec testnet-eez-builder-1 cat /shared/rollup.env > "$ROLLUP_ENV_FILE" 2>/dev/null || true
    fi
    if [ -n "$ROLLUP_ENV_FILE" ] && [ -f "$ROLLUP_ENV_FILE" ]; then
        eval "$(cat "$ROLLUP_ENV_FILE")"
    fi
fi

if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
    echo "ERROR: Could not load rollup.env — ROLLUPS_ADDRESS not set."
    echo "  Start the devnet first, or pass --rollups <addr>."
    exit 1
fi

if [ -z "${CROSS_CHAIN_MANAGER_ADDRESS:-}" ]; then
    echo "ERROR: CROSS_CHAIN_MANAGER_ADDRESS not set."
    echo "  Ensure rollup.env was loaded correctly, or pass --manager-l2 <addr>."
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
echo "  PingPong Configurable-Depth Deploy"
echo "=========================================="
echo "L1 RPC:      $L1_RPC"
echo "L2 RPC:      $L2_RPC"
echo "Rollups:     $ROLLUPS_ADDRESS"
echo "CCM L2:      $CROSS_CHAIN_MANAGER_ADDRESS"
echo "Rollup ID:   $ROLLUP_ID"
echo "Deployer:    $DEFAULT_ADDR"
if [ "$DRY_RUN" = "true" ]; then
    echo "Mode:        DRY RUN (compile only)"
fi
echo ""

# ── Step 0: Compile contracts ─────────────────────────────────────────────────

echo "====== Step 0: Compile PingPong contracts ======"
cd "$CONTRACTS_DIR"
forge build
echo "Compilation successful."
echo ""

if [ "$DRY_RUN" = "true" ]; then
    echo "Dry run complete — contracts compiled. Exiting without deployment."
    exit 0
fi

# ── Check deployer balance on L1 and L2 ──────────────────────────────────────

L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$DEFAULT_ADDR" 2>/dev/null || echo "0")
L2_BAL=$(cast balance --rpc-url "$L2_RPC" "$DEFAULT_ADDR" 2>/dev/null || echo "0")
echo "Deployer ($DEFAULT_ADDR) balances:"
echo "  L1: $L1_BAL wei"
echo "  L2: $L2_BAL wei"
echo ""

if [ "$L1_BAL" = "0" ] || [ "$L1_BAL" = "0x0" ]; then
    echo "ERROR: Deployer has no ETH on L1. Fund $DEFAULT_ADDR first."
    exit 1
fi
if [ "$L2_BAL" = "0" ] || [ "$L2_BAL" = "0x0" ]; then
    echo "Deployer has no ETH on L2 — auto-funding via L1 proxy deposit..."
    L1_PROXY="${L1_PROXY:-http://localhost:9556}"
    BRIDGE_ADDRESS="${BRIDGE_ADDRESS:-}"
    if [ -z "$BRIDGE_ADDRESS" ]; then
        BRIDGE_ADDRESS=$(grep '^BRIDGE_ADDRESS=' "$ROLLUP_ENV_FILE" 2>/dev/null | cut -d= -f2 || echo "")
    fi
    if [ -z "$BRIDGE_ADDRESS" ]; then
        echo "ERROR: BRIDGE_ADDRESS not found. Fund $DEFAULT_ADDR on L2 manually."
        exit 1
    fi
    echo "  Depositing 1 ETH via L1 proxy ($L1_PROXY) to Bridge ($BRIDGE_ADDRESS)..."
    DEPOSIT_STATUS=$(cast send --rpc-url "$L1_PROXY" --private-key "$PK" \
        "$BRIDGE_ADDRESS" "bridgeEther(uint256,address)" "$ROLLUP_ID" "$DEFAULT_ADDR" \
        --value 1ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
    if [ "$DEPOSIT_STATUS" != "1" ] && [ "$DEPOSIT_STATUS" != "0x1" ]; then
        echo "ERROR: L1 bridge deposit failed (status=$DEPOSIT_STATUS)."
        exit 1
    fi
    echo "  Deposit tx succeeded. Waiting 30s for L2 processing..."
    sleep 30
    L2_BAL=$(cast balance "$DEFAULT_ADDR" --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
    echo "  L2 balance after deposit: $L2_BAL wei"
    if [ "$L2_BAL" = "0" ] || [ "$L2_BAL" = "0x0" ]; then
        echo "ERROR: L2 balance still 0 after deposit. Deposit may need more time."
        exit 1
    fi
fi

# ── Step 1: Deploy PingPongL2 on L2 ──────────────────────────────────────────

echo "====== Step 1: Deploy PingPongL2 on L2 ======"
PING_PONG_L2_OUTPUT=$(forge create \
    --rpc-url "$L2_RPC" \
    --private-key "$PK" \
    --broadcast \
    src/PingPongL2.sol:PingPongL2 2>&1)
echo "$PING_PONG_L2_OUTPUT" | tail -3
PING_PONG_L2=$(echo "$PING_PONG_L2_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
if [ -z "$PING_PONG_L2" ]; then
    echo "ERROR: PingPongL2 deployment failed"
    echo "$PING_PONG_L2_OUTPUT"
    exit 1
fi
echo "PingPongL2 deployed at: $PING_PONG_L2"
echo ""

# ── Step 2: Deploy PingPongL1 on L1 ──────────────────────────────────────────

echo "====== Step 2: Deploy PingPongL1 on L1 ======"
PING_PONG_L1_OUTPUT=$(forge create \
    --rpc-url "$L1_RPC" \
    --private-key "$PK" \
    --broadcast \
    src/PingPongL1.sol:PingPongL1 2>&1)
echo "$PING_PONG_L1_OUTPUT" | tail -3
PING_PONG_L1=$(echo "$PING_PONG_L1_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
if [ -z "$PING_PONG_L1" ]; then
    echo "ERROR: PingPongL1 deployment failed"
    echo "$PING_PONG_L1_OUTPUT"
    exit 1
fi
echo "PingPongL1 deployed at: $PING_PONG_L1"
echo ""

# ── Step 3: Create L1-side CrossChainProxy for PingPongL2 ────────────────────
# This creates the proxy on L1 that represents PingPongL2 (on rollup $ROLLUP_ID).
# When PingPongL1.ping() wants to call back to L2, it calls this proxy.

echo "====== Step 3: Create L1-side proxy for PingPongL2 on L1 ======"
# First compute expected address (static call — no gas used)
L1_PROXY_FOR_L2=$(cast call \
    --rpc-url "$L1_RPC" \
    "$ROLLUPS_ADDRESS" \
    "computeCrossChainProxyAddress(address,uint256)(address)" \
    "$PING_PONG_L2" "$ROLLUP_ID" 2>&1)
echo "Expected L1 proxy for PingPongL2: $L1_PROXY_FOR_L2"

# Create the proxy (deploys CrossChainProxy via CREATE2)
cast send \
    --rpc-url "$L1_RPC" \
    --private-key "$PK" \
    "$ROLLUPS_ADDRESS" \
    "createCrossChainProxy(address,uint256)" \
    "$PING_PONG_L2" "$ROLLUP_ID" \
    --gas-limit 500000 > /dev/null
echo "L1 proxy created."

# Verify the proxy was deployed at the expected address
PROXY_CODE=$(cast code --rpc-url "$L1_RPC" "$L1_PROXY_FOR_L2" 2>/dev/null || echo "0x")
if [ "$PROXY_CODE" = "0x" ] || [ -z "$PROXY_CODE" ]; then
    echo "ERROR: L1 proxy not deployed at expected address $L1_PROXY_FOR_L2"
    exit 1
fi
echo "L1 proxy verified at: $L1_PROXY_FOR_L2"
echo ""

# ── Step 4: Compute L2-side proxy address for PingPongL1 ─────────────────────
# This proxy will be created by CCM automatically on the first cross-chain call
# from L1 to L2 involving PingPongL1. We compute the address now so PingPongL2
# can be set up with the correct proxy address.
# The CCM proxy uses rollupId=0 (L1 mainnet) for any address originating on L1.

echo "====== Step 4: Compute L2-side proxy address for PingPongL1 ======"
L2_PROXY_FOR_L1=$(cast call \
    --rpc-url "$L2_RPC" \
    "$CROSS_CHAIN_MANAGER_ADDRESS" \
    "computeCrossChainProxyAddress(address,uint256)(address)" \
    "$PING_PONG_L1" "0" 2>&1)
echo "L2 proxy for PingPongL1 (rollupId=0): $L2_PROXY_FOR_L1"
echo ""

# ── Step 4b: Create L2-side proxy for PingPongL1 ────────────────────────────
# Unlike L1 proxies (created via Rollups.createCrossChainProxy), L2 proxies are
# normally auto-created by CCM on the first incoming L1→L2 call. But for L2→L1
# patterns like PingPong, PingPongL2.start() calls the proxy BEFORE any incoming
# call has created it. We must create it explicitly.

echo "====== Step 4b: Create L2-side proxy for PingPongL1 ======"
cast send \
    --rpc-url "$L2_RPC" \
    --private-key "$PK" \
    "$CROSS_CHAIN_MANAGER_ADDRESS" \
    "createCrossChainProxy(address,uint256)(address)" \
    "$PING_PONG_L1" "0" \
    --gas-limit 500000 > /dev/null
L2_PROXY_CODE=$(cast code --rpc-url "$L2_RPC" "$L2_PROXY_FOR_L1" 2>/dev/null || echo "0x")
if [ "$L2_PROXY_CODE" = "0x" ] || [ -z "$L2_PROXY_CODE" ]; then
    echo "ERROR: L2 proxy not deployed at expected address $L2_PROXY_FOR_L1"
    exit 1
fi
echo "L2 proxy created and verified at: $L2_PROXY_FOR_L1"
echo ""

# ── Step 5: Setup PingPongL2 ──────────────────────────────────────────────────

echo "====== Step 5: Setup PingPongL2 (set proxy + L1 address) ======"
cast send \
    --rpc-url "$L2_RPC" \
    --private-key "$PK" \
    "$PING_PONG_L2" \
    "setup(address,address)" \
    "$L2_PROXY_FOR_L1" "$PING_PONG_L1" > /dev/null
echo "PingPongL2.setup() called."

# Verify
STORED_PROXY=$(cast call --rpc-url "$L2_RPC" "$PING_PONG_L2" "pingPongL1Proxy()(address)" 2>/dev/null)
STORED_L1=$(cast call --rpc-url "$L2_RPC" "$PING_PONG_L2" "pingPongL1()(address)" 2>/dev/null)
if [ "${STORED_PROXY,,}" != "${L2_PROXY_FOR_L1,,}" ]; then
    echo "ERROR: pingPongL1Proxy mismatch. Got $STORED_PROXY, expected $L2_PROXY_FOR_L1"
    exit 1
fi
if [ "${STORED_L1,,}" != "${PING_PONG_L1,,}" ]; then
    echo "ERROR: pingPongL1 mismatch. Got $STORED_L1, expected $PING_PONG_L1"
    exit 1
fi
echo "  pingPongL1Proxy: $STORED_PROXY (verified)"
echo "  pingPongL1:      $STORED_L1 (verified)"
echo ""

# ── Step 6: Setup PingPongL1 ──────────────────────────────────────────────────

echo "====== Step 6: Setup PingPongL1 (set L2 proxy address) ======"
cast send \
    --rpc-url "$L1_RPC" \
    --private-key "$PK" \
    "$PING_PONG_L1" \
    "setup(address)" \
    "$L1_PROXY_FOR_L2" > /dev/null
echo "PingPongL1.setup() called."

# Verify
STORED_L2_PROXY=$(cast call --rpc-url "$L1_RPC" "$PING_PONG_L1" "pingPongL2Proxy()(address)" 2>/dev/null)
if [ "${STORED_L2_PROXY,,}" != "${L1_PROXY_FOR_L2,,}" ]; then
    echo "ERROR: pingPongL2Proxy mismatch. Got $STORED_L2_PROXY, expected $L1_PROXY_FOR_L2"
    exit 1
fi
echo "  pingPongL2Proxy: $STORED_L2_PROXY (verified)"
echo ""

# ── Summary ───────────────────────────────────────────────────────────────────

echo "=========================================="
echo "  Deployment Complete"
echo "=========================================="
echo ""
echo "L2 Contracts:"
echo "  PingPongL2:          $PING_PONG_L2"
echo "  L2 proxy for L1:     $L2_PROXY_FOR_L1  (deployed explicitly in Step 4b)"
echo ""
echo "L1 Contracts:"
echo "  PingPongL1:          $PING_PONG_L1"
echo "  L1 proxy for L2:     $L1_PROXY_FOR_L2  (deployed by createCrossChainProxy)"
echo ""
echo "Verification commands:"
echo "  cast call $PING_PONG_L2 'pingPongL1Proxy()(address)' --rpc-url $L2_RPC"
echo "  cast call $PING_PONG_L2 'pingPongL1()(address)' --rpc-url $L2_RPC"
echo "  cast call $PING_PONG_L2 'pingCount()(uint256)' --rpc-url $L2_RPC"
echo "  cast call $PING_PONG_L1 'pingPongL2Proxy()(address)' --rpc-url $L1_RPC"
echo "  cast call $PING_PONG_L1 'pongCount()(uint256)' --rpc-url $L1_RPC"
echo "  cast call $PING_PONG_L1 'done()(bool)' --rpc-url $L1_RPC"
echo ""
echo "Trigger with configurable depth (via L2 proxy port 9548):"
echo "  # maxRounds=N: N L2→L1 calls + (N-1) L1→L2 returns = 2N-1 hops"
echo "  cast send --rpc-url http://localhost:9548 --private-key \$PK $PING_PONG_L2 'start(uint256)' N"
echo ""
echo "Examples:"
echo "  cast send --rpc-url http://localhost:9548 --private-key \$PK $PING_PONG_L2 'start(uint256)' 1   # depth-1: 1 hop"
echo "  cast send --rpc-url http://localhost:9548 --private-key \$PK $PING_PONG_L2 'start(uint256)' 2   # depth-2: 3 hops"
echo "  cast send --rpc-url http://localhost:9548 --private-key \$PK $PING_PONG_L2 'start(uint256)' 5   # depth-5: 9 hops (max)"
echo ""
echo "Expected post-trigger state for maxRounds=N:"
echo "  PingPongL2.pingCount  = N"
echo "  PingPongL1.pongCount  = N"
echo "  PingPongL1.done       = true"
