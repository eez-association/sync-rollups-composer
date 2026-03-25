#!/usr/bin/env bash
# Send complex test transactions to the L2 builder.
# Includes: ERC20 deployment, token transfers, contract interactions,
# high-gas transactions, and multi-recipient batches.
# Usage: send-complex-txs.sh [L2_RPC_URL]
set -euo pipefail

# Graceful shutdown on SIGTERM/SIGINT (sent by Docker on compose down)
trap 'echo "Shutting down complex-tx-sender..."; exit 0' SIGTERM SIGINT

L2_RPC="${1:-http://builder:8545}"

# Dev account #5 — separate from tx-sender (account #1) to avoid nonce conflicts.
# Funded at block 1 via BOOTSTRAP_ACCOUNTS.
SENDER="${COMPLEX_TX_SENDER:-0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc}"
SENDER_KEY="${COMPLEX_TX_SENDER_KEY:-0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba}"

# Recipients
RECIPIENT_A="0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
RECIPIENT_B="0x90F79bf6EB2c4f870365E785982E1f101E93b906"
RECIPIENT_C="0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"

echo "Waiting for L2 builder at ${L2_RPC}..."
WAIT_COUNT=0
MAX_WAIT=120
until cast block-number --rpc-url "$L2_RPC" >/dev/null 2>&1; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ "$WAIT_COUNT" -ge "$MAX_WAIT" ]; then
        echo "ERROR: Timed out waiting for L2 builder after ${MAX_WAIT}s"
        exit 1
    fi
    sleep 2
done
echo "L2 builder is ready."

BLOCK_TIME=12
ROUND=0

# Simple counter contract bytecode — increment() and get() functions.
# Much simpler and more reliable than an ERC20 for testing.
# Solidity equivalent:
#   contract Counter {
#     uint256 public count;
#     function increment() external { count += 1; }
#     function get() external view returns (uint256) { return count; }
#   }
# We don't actually need a real ERC20 — contract deployments + interactions
# are what matter for tx variety testing.

# Simple storage contract: set(uint256) and get() view
STORAGE_BYTECODE="0x6080604052348015600e575f5ffd5b506101298061001c5f395ff3fe6080604052348015600e575f5ffd5b50600436106030575f3560e01c806360fe47b11460345780636d4ce63c14604c575b5f5ffd5b604a60048036038101906046919060a9565b6066565b005b6052606f565b604051605d919060dc565b60405180910390f35b805f8190555050565b5f5f54905090565b5f5ffd5b5f819050919050565b608b81607b565b81146094575f5ffd5b50565b5f8135905060a3816084565b92915050565b5f6020828403121560bb5760ba6077565b5b5f60c6848285016097565b91505092915050565b60d681607b565b82525050565b5f60208201905060ed5f83018460cf565b9291505056fea26469706673582212202463c26b628bd8b12956c4538c72f2a7d39e8be867b3949af8a21dd96ae1ea0f64736f6c63430008210033"

STORAGE_ADDRESS=""

deploy_storage() {
    echo "=== Deploying simple storage contract ==="
    STORAGE_ADDRESS=$(cast send \
        --rpc-url "$L2_RPC" \
        --private-key "$SENDER_KEY" \
        --create "$STORAGE_BYTECODE" \
        2>&1 | grep "contractAddress" | awk '{print $2}') || true

    if [ -n "$STORAGE_ADDRESS" ] && [ "$STORAGE_ADDRESS" != "null" ]; then
        echo "Storage contract deployed at: $STORAGE_ADDRESS"
    else
        echo "Storage deployment failed, will retry next round"
    fi
}

send_eth_transfers() {
    local count="${1:-3}"
    echo "--- Sending $count ETH transfers ---"
    for i in $(seq 1 "$count"); do
        local idx=$(( (i - 1) % 3 ))
        local recipients=("$RECIPIENT_A" "$RECIPIENT_B" "$RECIPIENT_C")
        local recipient="${recipients[$idx]}"
        local amount="0.00$(( (RANDOM % 9) + 1 ))ether"
        cast send \
            --rpc-url "$L2_RPC" \
            --private-key "$SENDER_KEY" \
            --value "$amount" \
            "$recipient" \
            2>&1 | grep -E "transactionHash|status" || echo "  transfer $i failed"
    done
}

send_storage_txs() {
    if [ -z "$STORAGE_ADDRESS" ] || [ "$STORAGE_ADDRESS" = "null" ]; then
        return
    fi
    echo "--- Sending storage contract interactions ---"
    for i in $(seq 1 3); do
        local val=$((ROUND * 100 + i))
        cast send \
            --rpc-url "$L2_RPC" \
            --private-key "$SENDER_KEY" \
            "$STORAGE_ADDRESS" \
            "set(uint256)" "$val" \
            2>&1 | grep -E "transactionHash|status" || echo "  storage set $val failed"
    done
    # Verify via call
    local stored
    stored=$(cast call --rpc-url "$L2_RPC" "$STORAGE_ADDRESS" "get()(uint256)" 2>/dev/null) || true
    echo "  Storage value: $stored"
}

send_self_transfer() {
    echo "--- Sending self-transfer (edge case) ---"
    cast send \
        --rpc-url "$L2_RPC" \
        --private-key "$SENDER_KEY" \
        --value 0ether \
        "$SENDER" \
        2>&1 | grep -E "transactionHash|status" || echo "  self-transfer failed"
}

send_high_gas_tx() {
    echo "--- Sending high-gas transaction ---"
    # Send tx with explicit gas limit near block limit
    cast send \
        --rpc-url "$L2_RPC" \
        --private-key "$SENDER_KEY" \
        --value 0.0001ether \
        --gas-limit 500000 \
        "$RECIPIENT_A" \
        2>&1 | grep -E "transactionHash|status" || echo "  high-gas tx failed"
}

send_multiple_small_txs() {
    local count="${1:-10}"
    echo "--- Sending $count small rapid transactions ---"
    for i in $(seq 1 "$count"); do
        # Fire-and-forget style (don't wait for receipt)
        cast send \
            --rpc-url "$L2_RPC" \
            --private-key "$SENDER_KEY" \
            --value 0.0001ether \
            --async \
            "$RECIPIENT_B" \
            2>&1 | grep -E "0x" || echo "  small tx $i failed"
    done
}

# === Main Loop ===
echo "Starting complex transaction sender..."

# Deploy contracts on first round
deploy_storage

while true; do
    ROUND=$((ROUND + 1))
    echo ""
    echo "========================================"
    echo "Round $ROUND ($(date))"
    echo "========================================"

    case $((ROUND % 5)) in
        1)
            # Round type 1: Basic ETH transfers (3 txs)
            send_eth_transfers 3
            ;;
        2)
            # Round type 2: Contract interactions + transfers
            send_storage_txs
            send_eth_transfers 2
            ;;
        3)
            # Round type 3: Many small transactions (try to fill a block)
            send_multiple_small_txs 8
            ;;
        4)
            # Round type 4: Edge cases
            send_self_transfer
            send_high_gas_tx
            send_eth_transfers 1
            ;;
        0)
            # Round type 5: Mixed workload
            send_eth_transfers 2
            send_storage_txs
            send_self_transfer
            ;;
    esac

    # Report block info and verify L2Context
    BLOCK=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "?")
    echo ""
    echo "Current L2 block: $BLOCK"

    # Verify L2Context system call is working (read latest context)
    L2_CONTEXT="0x4200000000000000000000000000000000000001"
    L1_BLOCK_NUM=$(cast call --rpc-url "$L2_RPC" "$L2_CONTEXT" "latest()(uint256,bytes32,uint256,uint256)" 2>/dev/null | head -1) || true
    if [ -n "$L1_BLOCK_NUM" ]; then
        echo "L2Context latest L1 block: $L1_BLOCK_NUM"
    fi

    echo "Waiting ${BLOCK_TIME}s for next block..."
    sleep "$BLOCK_TIME"
done
