#!/usr/bin/env bash
# Start the based-rollup node.
# Waits for /shared/rollup.env (written by deploy.sh), then:
# 1. Initializes the database from genesis.json (if not already done)
# 2. Starts the node with config from rollup.env
set -euo pipefail

SHARED_DIR="${SHARED_DIR:-/shared}"
DATADIR="${DATADIR:-/data}"
GENESIS="${GENESIS:-/etc/based-rollup/genesis.json}"

echo "Waiting for rollup config at ${SHARED_DIR}/rollup.env..."
WAIT_COUNT=0
MAX_WAIT=120  # 2 minutes
until [ -f "${SHARED_DIR}/rollup.env" ]; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ "$WAIT_COUNT" -ge "$MAX_WAIT" ]; then
        echo "ERROR: Timed out waiting for rollup.env after ${MAX_WAIT}s"
        exit 1
    fi
    sleep 1
done
echo "Config found."

# Load the config from rollup.env, but don't overwrite vars already set
# (e.g., BUILDER_MODE=false set by docker-compose for fullnodes)
while IFS= read -r line; do
    # Skip comments and empty lines
    [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
    # Extract key (everything before the first '=')
    key="${line%%=*}"
    value="${line#*=}"
    # Strip all whitespace (spaces and tabs) from key
    key="${key//[$' \t']/}"
    [ -z "$key" ] && continue
    # Only set if not already in environment (or empty — treat empty as unset
    # so docker-compose can safely pass ${VAR:-} without blocking rollup.env)
    if [ -z "${!key+x}" ] || [ -z "${!key}" ]; then
        export "$key=$value"
    fi
done < "${SHARED_DIR}/rollup.env"

# Validate required environment variables
: "${DEPLOYMENT_L1_BLOCK:?ERROR: DEPLOYMENT_L1_BLOCK not set}"
: "${L1_RPC_URL:?ERROR: L1_RPC_URL not set}"
: "${ROLLUPS_ADDRESS:?ERROR: ROLLUPS_ADDRESS not set}"

# Validate genesis file exists before attempting init
if [ ! -f "${GENESIS}" ]; then
    echo "ERROR: Genesis file not found at ${GENESIS}"
    exit 1
fi

# Initialize the database if needed
if [ ! -d "${DATADIR}/db" ]; then
    echo "Initializing database from ${GENESIS}..."
    based-rollup init \
        --datadir "$DATADIR" \
        --chain "$GENESIS"
    echo "Database initialized."
fi

# Default BUILDER_MODE to false if not set (must be before any usage with set -u)
BUILDER_MODE="${BUILDER_MODE:-false}"

# Fullnodes don't need the builder key
if [ "${BUILDER_MODE}" = "false" ]; then
    unset BUILDER_PRIVATE_KEY
fi

echo "Starting based-rollup node..."
echo "  BUILDER_MODE=${BUILDER_MODE}"
echo "  ROLLUPS_ADDRESS=${ROLLUPS_ADDRESS:-not set}"
echo "  L1_RPC_URL=${L1_RPC_URL:-not set}"
echo "  CROSS_CHAIN_MANAGER_ADDRESS=${CROSS_CHAIN_MANAGER_ADDRESS:-not set}"
echo "  ROLLUP_ID=${ROLLUP_ID:-not set}"

EXTRA_ARGS=()

# Enable WebSocket on builder nodes
if [ "${BUILDER_MODE}" = "true" ]; then
    EXTRA_ARGS=(--ws --ws.addr 0.0.0.0 --ws.port 8546 --ws.api debug,trace,txpool,eth,net,web3)
fi

exec based-rollup node \
    --datadir "$DATADIR" \
    --chain "$GENESIS" \
    --http \
    --http.addr 0.0.0.0 \
    --http.port 8545 \
    --http.api debug,trace,txpool,eth,net,web3 \
    --http.corsdomain "*" \
    --authrpc.addr 127.0.0.1 \
    --authrpc.port 8551 \
    --disable-discovery \
    --no-persist-peers \
    --log.stdout.format terminal \
    --builder.extradata "" \
    --engine.persistence-threshold 128 \
    "${EXTRA_ARGS[@]}"
