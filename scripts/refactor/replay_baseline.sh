#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════════
# scripts/refactor/replay_baseline.sh
#
# Replay baseline gate (PLAN step 5.7). Runs the protocol E2E suite
# against a live devnet-eez and verifies that ALL scenarios pass.
#
# This is the final gate before merging refactor branches to main.
# The protocol E2E suite verifies byte-level correctness: each
# scenario's ComputeExpected contract generates deterministic expected
# hashes, and the Verify* contracts check that the system produces
# matching entries.
#
# ── Usage ──
#
#   # Requires a running devnet-eez (core services only, no tx-senders):
#   cargo build --release
#   sudo docker compose -f deployments/devnet-eez/docker-compose.yml \
#        -f deployments/devnet-eez/docker-compose.dev.yml \
#        up -d l1 deploy builder fullnode1 fullnode2 deploy-l2
#
#   # Run the gate:
#   bash scripts/refactor/replay_baseline.sh
#
#   # Or with a running devnet (auto-detects health endpoint):
#   bash scripts/refactor/replay_baseline.sh --auto
#
# ── Exit codes ──
#   0 = all scenarios pass
#   1 = one or more scenarios failed
# ═══════════════════════════════════════════════════════════════════════

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PROTO_DIR="$REPO_ROOT/contracts/sync-rollups-protocol"
BASELINE_DIR="$REPO_ROOT/tests/baseline"

# ── Configuration (devnet-eez ports) ──
L1_RPC="http://localhost:11556"   # L1 composer RPC
L2_RPC="http://localhost:11548"   # L2 composer RPC
HEALTH_URL="http://localhost:11560/health"
ROLLUPS="0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"
MANAGER_L2="0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"
# Funder keys
L1_FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L2_FUNDER_KEY="0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6"

export FOUNDRY_DISABLE_NIGHTLY_WARNING=1

# ── Auto mode: start fresh devnet if --auto ──
AUTO=false
for arg in "$@"; do
    [[ "$arg" == "--auto" ]] && AUTO=true
done

if $AUTO; then
    echo "=== Auto mode: starting fresh devnet ==="
    cd "$REPO_ROOT"
    cargo build --release 2>&1 | tail -1
    sudo docker compose -f deployments/devnet-eez/docker-compose.yml \
         -f deployments/devnet-eez/docker-compose.dev.yml down -v 2>&1 | tail -2
    sudo docker compose -f deployments/devnet-eez/docker-compose.yml \
         -f deployments/devnet-eez/docker-compose.dev.yml \
         up -d l1 deploy builder fullnode1 fullnode2 deploy-l2 2>&1 | tail -2
fi

# ── Wait for builder ──
echo "Waiting for builder to be healthy..."
for i in $(seq 1 60); do
    health=$(curl -s --max-time 2 "$HEALTH_URL" 2>/dev/null || echo "")
    if echo "$health" | grep -q '"mode":"Builder"'; then
        echo "Builder healthy: $(echo "$health" | jq -c '{mode, l2_head}')"
        break
    fi
    [[ $i -eq 60 ]] && { echo "FATAL: builder not healthy after 5 minutes"; exit 1; }
    sleep 5
done

# ── Generate fresh test key ──
cd "$PROTO_DIR"
PK=$(cast wallet new --json | jq -r '.[0].private_key')
ADDR=$(cast wallet address --private-key "$PK")
echo "Test key: $ADDR"

# Fund on both chains
cast send "$ADDR" --value 100ether --rpc-url "$L1_RPC" \
    --private-key "$L1_FUNDER_KEY" > /dev/null 2>&1
cast send "$ADDR" --value 5ether --rpc-url "$L2_RPC" \
    --private-key "$L2_FUNDER_KEY" > /dev/null 2>&1 || \
cast send "$ADDR" --value 5ether --rpc-url "http://localhost:11545" \
    --private-key "$L2_FUNDER_KEY" > /dev/null 2>&1
sleep 15

echo "L1: $(cast balance "$ADDR" --rpc-url http://localhost:11555 --ether) ETH"
echo "L2: $(cast balance "$ADDR" --rpc-url http://localhost:11545 --ether) ETH"

# ── Prepare network (CREATE2 factories, bridge ETH) ──
echo ""
echo "=== Preparing network ==="
bash script/e2e/shared/prepare-network.sh \
    --l1-rpc "$L1_RPC" --l2-rpc "$L2_RPC" \
    --pk "$PK" --rollups "$ROLLUPS" 2>&1 | tail -3

# ── Run all scenarios ──
echo ""
echo "========================================"
echo "  REPLAY BASELINE GATE (step 5.7)"
echo "========================================"
echo ""

PASS=0
FAIL=0
SKIP=0
RESULTS=""

for d in script/e2e/*/; do
    scenario=$(basename "$d")
    [[ "$scenario" == "shared" ]] && continue
    sol="$d/E2E.s.sol"
    [[ ! -f "$sol" ]] && continue

    printf "  %-30s " "$scenario"

    OUTPUT=$(timeout 300 bash script/e2e/shared/run-network.sh "$sol" \
        --l1-rpc "$L1_RPC" --l2-rpc "$L2_RPC" --pk "$PK" \
        --rollups "$ROLLUPS" --manager-l2 "$MANAGER_L2" 2>&1)

    if echo "$OUTPUT" | grep -q '====== Done ======'; then
        echo "PASS"
        PASS=$((PASS + 1))
        RESULTS="${RESULTS}\n  PASS  $scenario"
    else
        echo "FAIL"
        FAIL=$((FAIL + 1))
        RESULTS="${RESULTS}\n  FAIL  $scenario"
        # Log first failure line for debugging
        REASON=$(echo "$OUTPUT" | grep -E 'FAIL|ERROR' | head -1)
        [[ -n "$REASON" ]] && RESULTS="${RESULTS}  -- ${REASON:0:80}"
    fi
done

# ── Summary ──
echo ""
echo "========================================"
echo "  REPLAY BASELINE RESULTS"
echo "========================================"
echo -e "$RESULTS"
echo ""
echo "  Passed:  $PASS"
echo "  Failed:  $FAIL"
echo "  Total:   $((PASS + FAIL))"
echo ""

if [[ $FAIL -eq 0 ]]; then
    echo "  STATUS: ALL SCENARIOS PASS ✓"
    echo "  The refactor is byte-equivalent to the protocol spec."
    echo "========================================"
    exit 0
else
    echo "  STATUS: $FAIL SCENARIO(S) FAILED ✗"
    echo "  The refactor has behavioral differences — DO NOT MERGE."
    echo "========================================"
    exit 1
fi
