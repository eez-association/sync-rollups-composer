#!/usr/bin/env bash
# Send test transactions to the L2 builder.
# Sends 3 ETH transfers per block (~12s) using Anvil's default account #1.
# Usage: send-test-txs.sh [L2_RPC_URL]
set -euo pipefail

# Graceful shutdown on SIGTERM/SIGINT (sent by Docker on compose down)
trap 'echo "Shutting down tx-sender..."; exit 0' SIGTERM SIGINT

L2_RPC="${1:-http://builder:8545}"

# Anvil default account #1 (funded in genesis, separate from builder's account #0)
SENDER="0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
SENDER_KEY="0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"

# Recipients (Anvil accounts #2-#4, distinct from sender and builder)
RECIPIENTS=(
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
)

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

while true; do
    for i in "${!RECIPIENTS[@]}"; do
        RECIPIENT="${RECIPIENTS[$i]}"
        echo "Sending 0.001 ETH to ${RECIPIENT}..."
        cast send \
            --rpc-url "$L2_RPC" \
            --private-key "$SENDER_KEY" \
            --value 0.001ether \
            "$RECIPIENT" \
            2>&1 || echo "  tx failed (may be expected if block not ready)"
    done

    echo "Sent 3 txs, waiting ${BLOCK_TIME}s for next block..."
    sleep "$BLOCK_TIME"
done
