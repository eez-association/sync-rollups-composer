#!/usr/bin/env bash
# Launch a Kurtosis-based Ethereum L1 devnet with real PoS finality.
# Outputs the L1 RPC/WS URLs that deployments/kurtosis-1337/docker-compose.yml uses.
#
# Prerequisites:
#   - Kurtosis CLI installed (https://docs.kurtosis.com/install/)
#   - Docker running
#   - kurtosis engine started (`kurtosis engine start`)
#
# Usage:
#   ./deployments/kurtosis-1337/start.sh              # Start fresh
#   ./deployments/kurtosis-1337/start.sh --restart    # Destroy existing enclave and restart
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENCLAVE_NAME="based-rollup-l1"
NETWORK_PARAMS="${SCRIPT_DIR}/network_params.yaml"
ENV_FILE="${SCRIPT_DIR}/.env.kurtosis"

# ── Helper ──────────────────────────────────────────────────────────
info()  { echo -e "\033[1;34m[kurtosis]\033[0m $*"; }
error() { echo -e "\033[1;31m[kurtosis]\033[0m $*" >&2; }

# ── Pre-flight checks ──────────────────────────────────────────────
if ! command -v kurtosis &>/dev/null; then
    error "kurtosis CLI not found. Install: https://docs.kurtosis.com/install/"
    exit 1
fi

if ! command -v jq &>/dev/null; then
    error "jq not found. Install: sudo apt-get install jq"
    exit 1
fi

# Check engine is running
if ! kurtosis engine status 2>/dev/null | grep -q "running\|Running"; then
    info "Starting Kurtosis engine..."
    kurtosis engine start
fi

# ── Handle --restart flag ───────────────────────────────────────────
if [[ "${1:-}" == "--restart" ]]; then
    info "Destroying existing enclave '${ENCLAVE_NAME}'..."
    kurtosis enclave rm -f "$ENCLAVE_NAME" 2>/dev/null || true
fi

# ── Check if enclave already exists ─────────────────────────────────
if kurtosis enclave inspect "$ENCLAVE_NAME" &>/dev/null; then
    info "Enclave '${ENCLAVE_NAME}' already running."
    info "Use --restart to destroy and recreate, or use the existing endpoints."
else
    info "Starting Kurtosis Ethereum devnet (enclave: ${ENCLAVE_NAME})..."
    info "Config: ${NETWORK_PARAMS}"

    kurtosis run \
        --enclave "$ENCLAVE_NAME" \
        github.com/ethpandaops/ethereum-package \
        --args-file "$NETWORK_PARAMS"

    info "Enclave '${ENCLAVE_NAME}' started."
fi

# ── Extract RPC/WS endpoints ───────────────────────────────────────
info "Discovering L1 endpoints..."

# Find the EL service name (ethereum-package names it "el-1-reth-lighthouse")
EL_SERVICE=$(kurtosis enclave inspect "$ENCLAVE_NAME" 2>/dev/null \
    | grep -oE 'el-[0-9]+-[a-z][-a-z]*' | head -1)

if [ -z "$EL_SERVICE" ]; then
    error "Could not find EL service in enclave. Listing services:"
    kurtosis enclave inspect "$ENCLAVE_NAME"
    exit 1
fi

info "Found EL service: ${EL_SERVICE}"

# Use `kurtosis port print` — clean, reliable output: "127.0.0.1:PORT"
RPC_HOSTPORT=$(kurtosis port print "$ENCLAVE_NAME" "$EL_SERVICE" rpc 2>/dev/null || true)
WS_HOSTPORT=$(kurtosis port print "$ENCLAVE_NAME" "$EL_SERVICE" ws 2>/dev/null || true)

if [ -z "$RPC_HOSTPORT" ]; then
    error "Could not determine RPC port for ${EL_SERVICE}."
    kurtosis enclave inspect "$ENCLAVE_NAME"
    exit 1
fi

RPC_URL="http://${RPC_HOSTPORT}"
WS_URL="ws://${WS_HOSTPORT:-${RPC_HOSTPORT}}"

# ── Wait for L1 readiness ──────────────────────────────────────────
info "Waiting for L1 RPC at ${RPC_URL}..."
WAIT_COUNT=0
MAX_WAIT=120
until curl -sf -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "$RPC_URL" >/dev/null 2>&1; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ "$WAIT_COUNT" -ge "$MAX_WAIT" ]; then
        error "Timed out waiting for L1 RPC after ${MAX_WAIT}s"
        exit 1
    fi
    sleep 1
done

# Verify chain ID is 1337
CHAIN_ID_HEX=$(curl -sf -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
    "$RPC_URL" | jq -r '.result')
CHAIN_ID=$((CHAIN_ID_HEX))
info "L1 chain ID: ${CHAIN_ID}"

if [ "$CHAIN_ID" -ne 1337 ]; then
    error "WARNING: Chain ID is ${CHAIN_ID}, expected 1337."
    error "The network_params.yaml network_id may not have taken effect."
    error "Existing scripts hardcode chain ID 1337 — this may cause issues."
fi

# Check a pre-funded account has balance
BALANCE_HEX=$(curl -sf -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_getBalance","params":["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","latest"],"id":1}' \
    "$RPC_URL" | jq -r '.result')
info "Deployer account balance: ${BALANCE_HEX}"

# ── Build container-accessible URLs ────────────────────────────────
# Kurtosis L1 runs in Docker on the host. Compose containers can't reach
# 127.0.0.1 on the host — they need host.docker.internal (resolved via
# extra_hosts in the compose file). Replace 127.0.0.1 with that hostname.
CONTAINER_RPC_URL="${RPC_URL//127.0.0.1/host.docker.internal}"
CONTAINER_WS_URL="${WS_URL:-${RPC_URL/http/ws}}"
CONTAINER_WS_URL="${CONTAINER_WS_URL//127.0.0.1/host.docker.internal}"

# ── Preserve operator-provided BUILDER_PRIVATE_KEY across runs ──────
# start.sh overwrites this file every invocation. If the operator has
# already set BUILDER_PRIVATE_KEY (required by compose), carry it over.
EXISTING_BUILDER_PK=""
if [ -f "$ENV_FILE" ]; then
    EXISTING_BUILDER_PK=$(grep -E '^BUILDER_PRIVATE_KEY=' "$ENV_FILE" 2>/dev/null || true)
fi

# ── Write env file for docker-compose ──────────────────────────────
cat > "$ENV_FILE" <<EOF
# Auto-generated by deployments/kurtosis-1337/start.sh — do not edit
# Source this file or use with docker-compose --env-file
# Container URLs use host.docker.internal so compose services can reach the host
KURTOSIS_L1_RPC_URL=${CONTAINER_RPC_URL}
KURTOSIS_L1_WS_URL=${CONTAINER_WS_URL}
# Host URLs for scripts running directly on the host (verify-finality.sh, etc.)
KURTOSIS_L1_RPC_URL_HOST=${RPC_URL}
KURTOSIS_L1_WS_URL_HOST=${WS_URL:-${RPC_URL/http/ws}}
KURTOSIS_ENCLAVE=${ENCLAVE_NAME}
KURTOSIS_EL_SERVICE=${EL_SERVICE}
EOF

# Re-append the operator-provided builder key (or a placeholder that will
# make compose fail loudly so the operator remembers to set it).
if [ -n "$EXISTING_BUILDER_PK" ]; then
    echo "$EXISTING_BUILDER_PK" >> "$ENV_FILE"
    info "  Preserved BUILDER_PRIVATE_KEY from previous $ENV_FILE"
else
    cat >> "$ENV_FILE" <<'EOF'
# BUILDER_PRIVATE_KEY is REQUIRED. Generate with:
#   cast wallet new --json | jq -r '.[0].private_key'
# Then replace the placeholder below. Compose will fail fast if left as-is.
# MUST NOT be dev#0 (0xac0974…) — see docs/issue-29.
BUILDER_PRIVATE_KEY=0x_YOUR_BUILDER_PRIVATE_KEY_HERE
EOF
    info "  WARNING: BUILDER_PRIVATE_KEY placeholder written to $ENV_FILE — replace before 'docker compose up'"
fi

info ""
info "════════════════════════════════════════════════════════════════"
info "  Kurtosis L1 Devnet Ready"
info "════════════════════════════════════════════════════════════════"
info "  RPC URL:    ${RPC_URL}"
info "  WS URL:     ${WS_URL:-${RPC_URL/http/ws}}"
info "  Chain ID:   ${CHAIN_ID}"
info "  Enclave:    ${ENCLAVE_NAME}"
info "  Env file:   ${ENV_FILE}"
info ""
info "  Next steps:"
info "    1. cd $(dirname "$SCRIPT_DIR")"
info "    2. docker compose -f deployments/kurtosis-1337/docker-compose.yml --env-file deployments/kurtosis-1337/.env.kurtosis up -d"
info ""
info "  To verify finality:"
info "    bash deployments/kurtosis-1337/verify-finality.sh"
info "════════════════════════════════════════════════════════════════"
