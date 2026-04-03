#!/bin/bash
set -euo pipefail

# ═══════════════════════════════════════════════════════════════════════
# Local CI runner — replicates the GHA Docker E2E sequence exactly.
#
# Uses testnet-eez compose + dev overlay + port override (12xxx).
# Independent of testnet-eez (9xxx) and devnet-eez (11xxx).
#
# Usage:
#   bash scripts/ci-local.sh              # full run
#   bash scripts/ci-local.sh --teardown   # cleanup only
# ═══════════════════════════════════════════════════════════════════════

PROJECT=ci-local

compose() {
    sudo docker compose \
        -f deployments/ci-local/docker-compose.yml \
        -f deployments/ci-local/docker-compose.dev.yml \
        "$@"
}

# Ports (12xxx)
L1_DIRECT=http://localhost:12555
L1_COMPOSER=http://localhost:12556
L2_DIRECT=http://localhost:12545
L2_COMPOSER=http://localhost:12548
HEALTH=http://localhost:12560

# Protocol E2E test key (dev#10)
PK=0xf214f2b2cd398c806f84e317254e0f0b801d0643303237d97a22a48e01628897

teardown() {
    echo "=== Tearing down ci-local ==="
    compose down -v 2>&1 | tail -3
}

if [[ "${1:-}" == "--teardown" ]]; then
    teardown
    exit 0
fi

trap teardown EXIT

echo "=== Step 1: Fresh deploy ==="
compose down -v 2>/dev/null || true
compose up -d l1 deploy builder fullnode1 fullnode2 deploy-l2 tx-sender

echo "=== Step 2: Wait for builder ==="
for i in $(seq 1 60); do
    health=$(curl -s --max-time 2 "$HEALTH/health" 2>/dev/null || echo "")
    if echo "$health" | grep -q '"mode":"Builder"'; then
        echo "Builder healthy: $health"
        break
    fi
    echo "  [$i/60] $health"
    sleep 5
done
if ! curl -s "$HEALTH/health" 2>/dev/null | grep -q '"mode":"Builder"'; then
    echo "FATAL: builder not healthy"
    compose logs builder --tail 30
    exit 1
fi

sleep 10  # Wait for initial blocks

echo "=== Step 3: Extract addresses ==="
ROLLUP_ENV=$(compose exec -T builder cat /shared/rollup.env)
ROLLUPS=$(echo "$ROLLUP_ENV" | grep '^ROLLUPS_ADDRESS=' | cut -d= -f2)
MANAGER_L2=$(echo "$ROLLUP_ENV" | grep '^CROSS_CHAIN_MANAGER_ADDRESS=' | cut -d= -f2)
BRIDGE_L1=$(echo "$ROLLUP_ENV" | grep '^BRIDGE_L1_ADDRESS=' | cut -d= -f2)
echo "Rollups=$ROLLUPS Manager=$MANAGER_L2 Bridge=$BRIDGE_L1"

echo "=== Step 4: Prepare network ==="
cd contracts/sync-rollups-protocol
bash script/e2e/shared/prepare-network.sh \
    --l1-rpc "$L1_COMPOSER" --l2-rpc "$L2_COMPOSER" --pk "$PK" --rollups "$ROLLUPS"

echo ""
echo "=== Step 5: Protocol E2E tests ==="
TESTS="counter counterL2 bridge helloWorld multi-call-twice multi-call-two-diff nestedCounter nestedCounterL2 deepScope siblingScopes multi-call-nested multi-call-nestedL2 flash-loan reentrantCrossChainCalls"
PASS=0
FAIL=0
for test in $TESTS; do
    result=$(bash script/e2e/shared/run-network.sh "script/e2e/$test/E2E.s.sol" \
        --l1-rpc "$L1_COMPOSER" --l2-rpc "$L2_COMPOSER" --pk "$PK" \
        --rollups "$ROLLUPS" --manager-l2 "$MANAGER_L2" 2>&1)
    if echo "$result" | grep -q "Done"; then
        echo "$test: PASS"
        PASS=$((PASS + 1))
    else
        echo "$test: FAIL"
        FAIL=$((FAIL + 1))
    fi
done
echo "Protocol E2E: $PASS passed, $FAIL failed"
cd ../..

echo ""
echo "=== Step 6: Wait for builder to settle ==="
for i in $(seq 1 30); do
    health=$(curl -s --max-time 2 "$HEALTH/health" 2>/dev/null || echo "")
    pending=$(echo "$health" | python3 -c "import sys,json; print(json.load(sys.stdin).get('pending_submissions',0))" 2>/dev/null || echo "?")
    rewinds=$(echo "$health" | python3 -c "import sys,json; print(json.load(sys.stdin).get('consecutive_rewind_cycles',0))" 2>/dev/null || echo "?")
    echo "  [$i/30] pending=$pending rewinds=$rewinds"
    if [ "$pending" = "0" ] && [ "$rewinds" = "0" ]; then
        echo "Builder settled."
        break
    fi
    sleep 2
done

echo ""
echo "=== Step 7: Bridge E2E test ==="
export L1_RPC="$L1_DIRECT"
export L2_RPC="$L2_DIRECT"
export L1_PROXY="$L1_COMPOSER"
export L2_PROXY="$L2_COMPOSER"
export BRIDGE_ADDRESS="$BRIDGE_L1"
export ROLLUPS_ADDRESS="$ROLLUPS"
bash scripts/e2e/bridge-health-check.sh

echo ""
echo "=== CI Local: DONE ==="
