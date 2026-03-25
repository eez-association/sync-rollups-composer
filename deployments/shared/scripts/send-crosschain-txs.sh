#!/usr/bin/env bash
# Synchronous composability demo — deploy Counter on L2, deploy a CrossChainProxy
# on L1, then send normal L1 transactions to the proxy via the L1 RPC proxy.
#
# The L1 RPC proxy (running on the builder) intercepts eth_sendRawTransaction,
# traces the tx, detects the cross-chain call, populates the L2 execution table,
# then forwards the tx to L1. No custom RPC methods needed from the user's side.
#
# Flow each iteration:
#   1. Send increment() to CrossChainProxy on L1 via L1 RPC proxy
#   2. L1 proxy traces tx, detects executeCrossChainCall → calls initiateCrossChainCall on L2
#   3. L2 execution table loaded, cross-chain call executed on L2 (Counter.increment())
#   4. L1 tx forwarded and confirmed
#   5. Query counter value on L2, compare state roots
#
# Usage: send-crosschain-txs.sh [L2_RPC_URL] [L1_RPC_URL]
#
# WARNING: Uses well-known anvil test keys. LOCAL DEVELOPMENT ONLY.
set -euo pipefail

# Graceful shutdown on SIGTERM/SIGINT (sent by Docker on compose down)
trap 'echo "[crosschain] Shutting down..."; exit 0' SIGTERM SIGINT

L2_RPC="${1:-http://builder:8545}"
L1_RPC="${2:-http://l1:8545}"
# L1 proxy runs on builder — traces L1 txs for cross-chain calls
L1_PROXY="${3:-http://builder:9556}"
# Well-known anvil key #4 — dedicated to crosschain demo (avoids nonce conflicts
# with tx-sender which uses key #1, and deploy-crosschain which uses key #0)
SENDER_KEY="0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a"
SENDER_ADDR="0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"

# Colors (disabled if not a terminal)
if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
    CYAN='\033[0;36m'; BOLD='\033[1m'; DIM='\033[2m'; RESET='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; CYAN=''; BOLD=''; DIM=''; RESET=''
fi

log()   { echo -e "${DIM}$(date +%H:%M:%S)${RESET} [crosschain] $*"; }
ok()    { log "${GREEN}✓${RESET} $*"; }
warn()  { log "${YELLOW}⚠${RESET} $*"; }
err()   { log "${RED}✗${RESET} $*"; }
info()  { log "${CYAN}→${RESET} $*"; }
header(){ echo -e "\n${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"; }
short_hash() { echo "${1:0:10}…${1: -6}"; }

# ─── Wait for node to be ready ────────────────────────────────────────────────
info "Waiting for L2 node at ${CYAN}$L2_RPC${RESET}..."
WAIT=0
MAX_WAIT=120
until cast block-number --rpc-url "$L2_RPC" >/dev/null 2>&1; do
    WAIT=$((WAIT + 1))
    if [ "$WAIT" -ge "$MAX_WAIT" ]; then
        warn "L2 not ready after ${MAX_WAIT}s, proceeding anyway"
        break
    fi
    sleep 1
done
ok "L2 node is ready (${WAIT}s)"

# Wait for sync
info "Waiting for L2 node to sync..."
WAIT=0
while true; do
    SYNCED=$(cast rpc --rpc-url "$L2_RPC" syncrollups_isSynced 2>/dev/null || echo "false")
    SYNCED=$(echo "$SYNCED" | tr -d '"' | tr -d ' ')
    if [ "$SYNCED" = "true" ]; then
        ok "Node is synced (${WAIT}s)"
        break
    fi
    WAIT=$((WAIT + 1))
    if [ "$WAIT" -ge 120 ]; then
        warn "Node not synced after 120s, proceeding anyway"
        break
    fi
    sleep 1
done

# Wait for L1 proxy to be ready
info "Waiting for L1 RPC proxy at ${CYAN}$L1_PROXY${RESET}..."
WAIT=0
until cast block-number --rpc-url "$L1_PROXY" >/dev/null 2>&1; do
    WAIT=$((WAIT + 1))
    if [ "$WAIT" -ge 60 ]; then
        warn "L1 proxy not ready after 60s, proceeding anyway"
        break
    fi
    sleep 1
done
ok "L1 RPC proxy is ready (${WAIT}s)"

# ─── Read cross-chain config ────────────────────────────────────────────────
SHARED_DIR="${SHARED_DIR:-/shared}"
ROLLUPS_ADDRESS=""
ROLLUP_ID="1"
if [ -f "$SHARED_DIR/rollup.env" ]; then
    ROLLUPS_ADDRESS=$(grep "^ROLLUPS_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || true)
    ROLLUP_ID=$(grep "^ROLLUP_ID=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "1")
fi
info "Rollups contract: ${CYAN}${ROLLUPS_ADDRESS:-not set}${RESET}"
info "Rollup ID: ${BOLD}$ROLLUP_ID${RESET}"

# ─── Seed L1 state root if zero ────────────────────────────────────────────
# The Rollups contract is initialized with stateRoot=0x00..00 at deploy time
# (the genesis state root isn't known until the node starts). We need to seed
# it with the actual L2 state root so that postBatch's delta matching works.
if [ -n "$ROLLUPS_ADDRESS" ]; then
    DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
    ONCHAIN_ROOT=$(cast call --rpc-url "$L1_RPC" \
        "$ROLLUPS_ADDRESS" \
        "rollups(uint256)(address,bytes32,bytes32,uint256)" \
        "$ROLLUP_ID" 2>/dev/null | sed -n '3p' || echo "")

    if [ "$ONCHAIN_ROOT" = "0x0000000000000000000000000000000000000000000000000000000000000000" ]; then
        L2_ROOT=$(cast rpc --rpc-url "$L2_RPC" syncrollups_getStateRoot 2>/dev/null | tr -d '"' || echo "")
        if [ -n "$L2_ROOT" ] && [ "$L2_ROOT" != "error" ]; then
            info "Seeding L1 state root with L2 genesis root: $(short_hash "$L2_ROOT")"
            # Key #0 is shared with the builder (Inbox.submitBatch), so we may
            # hit "replacement transaction underpriced". Retry with high gas price
            # to outbid any pending builder tx on the same nonce.
            SEED_OK=false
            for attempt in 1 2 3; do
                if cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
                    "$ROLLUPS_ADDRESS" \
                    "setStateByOwner(uint256,bytes32)" \
                    "$ROLLUP_ID" "$L2_ROOT" \
                    --gas-price 100000000000 \
                    >/dev/null 2>&1; then
                    SEED_OK=true
                    break
                fi
                warn "Seed attempt $attempt failed, retrying in 5s..."
                sleep 5
            done
            $SEED_OK && ok "L1 state root seeded successfully" \
                || err "Failed to seed L1 state root after 3 attempts"
        fi
    else
        ok "L1 state root already set: $(short_hash "$ONCHAIN_ROOT")"
    fi
fi

# ─── Wait for sender to have ETH on L2 ────────────────────────────────────
# The sender account starts with 0 ETH on L2 (pre-funded on L1 only).
# The tx-sender script sends small ETH amounts to this address each cycle.
info "Waiting for sender ${DIM}$SENDER_ADDR${RESET} to have ETH on L2..."
WAIT=0
while true; do
    BAL=$(cast balance --rpc-url "$L2_RPC" "$SENDER_ADDR" 2>/dev/null || echo "0")
    if [ "$BAL" != "0" ] && [ -n "$BAL" ]; then
        BAL_ETH=$(cast from-wei "$BAL" 2>/dev/null || echo "$BAL wei")
        ok "Sender balance: ${BOLD}$BAL_ETH ETH${RESET} (${WAIT}s)"
        break
    fi
    WAIT=$((WAIT + 1))
    if [ "$WAIT" -ge 300 ]; then
        warn "Sender has no ETH after 5 min, proceeding anyway (deploy will likely fail)"
        break
    fi
    sleep 1
done

# ─── Deploy Counter on L2 ───────────────────────────────────────────────────
header
info "Deploying Counter contract on L2..."

# Deploy from source (test/mocks/CounterContracts.sol:Counter in sync-rollups).
# This ensures the deployed bytecode matches what verify-contracts.sh verifies on Blockscout.
# Counter has: uint256 public counter; function increment() external returns (uint256);
CONTRACTS_DIR="${CONTRACTS_DIR:-/app/contracts}"
SYNC_DIR="$CONTRACTS_DIR/sync-rollups"

# Check current nonce — if > 0 a previous deploy may have succeeded
CURRENT_NONCE=$(cast nonce --rpc-url "$L2_RPC" "$SENDER_ADDR" 2>/dev/null || echo "0")
info "Current nonce: $CURRENT_NONCE"

COUNTER_ADDRESS=""

if [ "$CURRENT_NONCE" != "0" ]; then
    # Nonce > 0 means a tx was already mined. Compute the CREATE address at nonce 0.
    PREDICTED=$(cast compute-address "$SENDER_ADDR" --nonce 0 2>/dev/null | grep -oP '0x[0-9a-fA-F]{40}' || echo "")
    if [ -n "$PREDICTED" ]; then
        CODE=$(cast code --rpc-url "$L2_RPC" "$PREDICTED" 2>/dev/null || echo "0x")
        if [ "$CODE" != "0x" ] && [ -n "$CODE" ]; then
            ok "Counter already deployed at: ${CYAN}$PREDICTED${RESET} (nonce already advanced)"
            COUNTER_ADDRESS="$PREDICTED"
        else
            warn "Nonce $CURRENT_NONCE but no contract at $PREDICTED, deploying fresh..."
        fi
    fi
fi

if [ -z "$COUNTER_ADDRESS" ]; then
    # Try deploy — retry up to 3 times
    for attempt in 1 2 3; do
        info "Deploy attempt $attempt/3..."
        DEPLOY_OUTPUT=$(forge create \
            --broadcast \
            --root "$SYNC_DIR" \
            --rpc-url "$L2_RPC" \
            --private-key "$SENDER_KEY" \
            test/mocks/CounterContracts.sol:Counter 2>&1 || echo "")

        COUNTER_ADDRESS=$(echo "$DEPLOY_OUTPUT" | grep -oP 'Deployed to: \K0x[0-9a-fA-F]+' || echo "")

        if [ -n "$COUNTER_ADDRESS" ]; then
            ok "Counter deployed at: ${CYAN}$COUNTER_ADDRESS${RESET}"
            break
        fi

        err "Deploy failed: ${DEPLOY_OUTPUT:0:200}"
        sleep 5
    done
fi

if [ -z "$COUNTER_ADDRESS" ]; then
    warn "Could not deploy Counter after 3 attempts, will only track state roots"
fi

# Wait for the deploy tx to be included in a block
sleep 15

# Write counter address to shared dir so other services can use it
if [ -n "$COUNTER_ADDRESS" ] && [ -d "$SHARED_DIR" ]; then
    echo "COUNTER_ADDRESS=$COUNTER_ADDRESS" > "$SHARED_DIR/counter.env.tmp"
    mv "$SHARED_DIR/counter.env.tmp" "$SHARED_DIR/counter.env"
    ok "Wrote counter address to $SHARED_DIR/counter.env"
fi

# Get L1 chain ID for CrossChainProxy address computation
L1_CHAIN_ID=$(cast chain-id --rpc-url "$L1_RPC" 2>/dev/null || echo "1337")
info "L1 chain ID: $L1_CHAIN_ID"

# ─── Deploy CrossChainProxy on L1 ────────────────────────────────────────────
# Creates a proxy on L1 that represents the Counter contract on L2.
# When called, the proxy forwards to Rollups.executeCrossChainCall().
# The L1 RPC proxy detects this in the trace and pre-populates the L2 execution table.
PROXY_ADDRESS=""
if [ -n "$COUNTER_ADDRESS" ] && [ -n "$ROLLUPS_ADDRESS" ]; then
    info "Creating CrossChainProxy on L1 for Counter (${CYAN}$COUNTER_ADDRESS${RESET}, rollup $ROLLUP_ID)..."
    # createCrossChainProxy(address originalAddress, uint256 originalRollupId)
    # returns (address proxy)
    PROXY_RESULT=$(cast send --rpc-url "$L1_RPC" \
        --private-key "$SENDER_KEY" \
        "$ROLLUPS_ADDRESS" \
        "createCrossChainProxy(address,uint256)(address)" \
        "$COUNTER_ADDRESS" "$ROLLUP_ID" \
        --json 2>&1 || echo "{}")

    # Check if the tx succeeded
    TX_STATUS=$(echo "$PROXY_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
    if [ "$TX_STATUS" = "0x1" ]; then
        # Read the proxy address via eth_call (view function)
        PROXY_ADDRESS=$(cast call --rpc-url "$L1_RPC" \
            "$ROLLUPS_ADDRESS" \
            "computeCrossChainProxyAddress(address,uint256)(address)" \
            "$COUNTER_ADDRESS" "$ROLLUP_ID" 2>/dev/null || echo "")
        if [ -n "$PROXY_ADDRESS" ]; then
            ok "CrossChainProxy deployed on L1 at: ${CYAN}$PROXY_ADDRESS${RESET}"
        else
            warn "Proxy tx succeeded but couldn't read proxy address"
        fi
    else
        # Proxy might already exist (idempotent CREATE2). Try reading the address anyway.
        PROXY_ADDRESS=$(cast call --rpc-url "$L1_RPC" \
            "$ROLLUPS_ADDRESS" \
            "computeCrossChainProxyAddress(address,uint256)(address)" \
            "$COUNTER_ADDRESS" "$ROLLUP_ID" 2>/dev/null || echo "")
        if [ -n "$PROXY_ADDRESS" ]; then
            PROXY_CODE=$(cast code --rpc-url "$L1_RPC" "$PROXY_ADDRESS" 2>/dev/null || echo "0x")
            if [ "$PROXY_CODE" != "0x" ] && [ -n "$PROXY_CODE" ]; then
                ok "CrossChainProxy already deployed on L1 at: ${CYAN}$PROXY_ADDRESS${RESET}"
            else
                err "CrossChainProxy deployment failed: ${PROXY_RESULT:0:200}"
                PROXY_ADDRESS=""
            fi
        else
            err "CrossChainProxy deployment failed: ${PROXY_RESULT:0:200}"
        fi
    fi
fi

# ─── Main loop ───────────────────────────────────────────────────────────────
ITERATION=0
SUCCESS_COUNT=0
FAIL_COUNT=0

while true; do
    ITERATION=$((ITERATION + 1))
    header
    log "${BOLD}Iteration #$ITERATION${RESET}  ${DIM}(${GREEN}$SUCCESS_COUNT ok${RESET}${DIM} / ${RED}$FAIL_COUNT fail${RESET}${DIM})${RESET}"

    # Get L2 block number
    L2_BLOCK=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "?")
    info "L2 block: ${BOLD}$L2_BLOCK${RESET}"

    # Get L2 state root via syncrollups RPC
    L2_STATE_ROOT=$(cast rpc --rpc-url "$L2_RPC" syncrollups_getStateRoot 2>/dev/null | tr -d '"' || echo "error")
    info "L2 state root: $(short_hash "${L2_STATE_ROOT:-error}")"

    # Get L1 state root for this rollup (from Rollups contract)
    if [ -n "$ROLLUPS_ADDRESS" ]; then
        L1_STATE_ROOT=$(cast call --rpc-url "$L1_RPC" \
            "$ROLLUPS_ADDRESS" \
            "rollups(uint256)(address,bytes32,bytes32,uint256)" \
            "$ROLLUP_ID" 2>/dev/null | sed -n '3p' || echo "")

        if [ -n "$L1_STATE_ROOT" ]; then
            info "L1 state root: $(short_hash "$L1_STATE_ROOT")"
            if [ "$L2_STATE_ROOT" = "$L1_STATE_ROOT" ]; then
                ok "State roots ${GREEN}MATCH${RESET}"
            elif [ "$L1_STATE_ROOT" = "0x0000000000000000000000000000000000000000000000000000000000000000" ]; then
                warn "L1 state root not yet set (waiting for postBatch)"
            else
                warn "State roots ${YELLOW}DIFFER${RESET} (batch pending or in-flight)"
            fi
        else
            err "L1 state root: could not read"
        fi
    fi

    # Read counter value before
    if [ -n "$COUNTER_ADDRESS" ]; then
        COUNT_BEFORE=$(cast call --rpc-url "$L2_RPC" \
            "$COUNTER_ADDRESS" \
            "counter()(uint256)" 2>/dev/null || echo "?")
        info "Counter before: ${BOLD}$COUNT_BEFORE${RESET}"
    fi

    # ─── Send cross-chain increment() via L1 proxy ─────────────────────────
    if [ -n "$PROXY_ADDRESS" ]; then
        info "Sending ${BOLD}increment()${RESET} → CrossChainProxy ${DIM}$PROXY_ADDRESS${RESET} via L1 proxy"
        SEND_RESULT=$(cast send \
            --rpc-url "$L1_PROXY" \
            --private-key "$SENDER_KEY" \
            "$PROXY_ADDRESS" \
            "increment()" \
            --gas-limit 500000 \
            --json 2>&1 || echo "{}")

        TX_HASH=$(echo "$SEND_RESULT" | grep -oP '"transactionHash"\s*:\s*"\K[^"]+' || echo "")
        TX_STATUS=$(echo "$SEND_RESULT" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")

        if [ -n "$TX_HASH" ]; then
            SHORT_TX=$(short_hash "$TX_HASH")
            if [ "$TX_STATUS" = "0x1" ]; then
                ok "L1 tx ${GREEN}confirmed${RESET}: $SHORT_TX"
                log "  ${DIM}Cross-chain call executed via transparent L1 proxy path${RESET}"
            else
                err "L1 tx ${RED}reverted${RESET}: $SHORT_TX"
                log "  ${DIM}Execution table may not have been ready${RESET}"
                FAIL_COUNT=$((FAIL_COUNT + 1))
            fi
        else
            err "Cross-chain call failed: ${SEND_RESULT:0:150}"
            FAIL_COUNT=$((FAIL_COUNT + 1))
        fi
    else
        warn "No CrossChainProxy deployed — skipping cross-chain call"
    fi

    # Wait a block for execution
    log "${DIM}Waiting 13s for block inclusion...${RESET}"
    sleep 13

    # Read counter value after
    if [ -n "$COUNTER_ADDRESS" ]; then
        COUNT_AFTER=$(cast call --rpc-url "$L2_RPC" \
            "$COUNTER_ADDRESS" \
            "counter()(uint256)" 2>/dev/null || echo "?")
        if [ "$COUNT_BEFORE" != "?" ] && [ "$COUNT_AFTER" != "?" ]; then
            if [ "$COUNT_AFTER" -gt "$COUNT_BEFORE" ] 2>/dev/null; then
                ok "Counter: ${BOLD}$COUNT_BEFORE → $COUNT_AFTER${RESET} ${GREEN}(+$((COUNT_AFTER - COUNT_BEFORE)))${RESET}"
                SUCCESS_COUNT=$((SUCCESS_COUNT + 1))
            else
                warn "Counter: ${BOLD}$COUNT_BEFORE → $COUNT_AFTER${RESET} (unchanged, may take another block)"
            fi
        else
            info "Counter after: ${BOLD}$COUNT_AFTER${RESET}"
        fi
    fi

    # Compute action hash via syncrollups_computeActionHash
    AH=$(cast rpc --rpc-url "$L2_RPC" syncrollups_computeActionHash \
        "{\"actionType\":\"L2TX\",\"rollupId\":\"0x$(printf '%x' "$ROLLUP_ID")\",\"destination\":\"0x0000000000000000000000000000000000000000\",\"value\":\"0x0\",\"data\":\"0x01\",\"failed\":false,\"sourceAddress\":\"0x0000000000000000000000000000000000000000\",\"sourceRollup\":\"0x0\",\"scope\":[]}" \
        2>/dev/null | tr -d '"' || echo "error")
    info "Action hash: $(short_hash "${AH:-error}")"

    # Wait for next block cycle
    log "${DIM}Sleeping 12s (next L2 block)...${RESET}"
    sleep 12
done
