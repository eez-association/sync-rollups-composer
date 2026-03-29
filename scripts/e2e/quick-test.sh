#!/usr/bin/env bash
# quick-test.sh — Fresh deploy + run a single protocol E2E test
#
# Usage: ./scripts/e2e/quick-test.sh <test-name>
# Monitor: tail -f /tmp/quick-test-<name>.log

cd "$(git rev-parse --show-toplevel)"

TEST="${1:?Usage: quick-test.sh <test-name>}"
SOL="contracts/sync-rollups-protocol/script/e2e/$TEST/E2E.s.sol"
[ -f "$SOL" ] || { echo "ERROR: $SOL not found"; exit 1; }

LOG="/tmp/quick-test-${TEST}.log"
DC="sudo docker compose -f deployments/devnet-eez/docker-compose.yml -f deployments/devnet-eez/docker-compose.dev.yml"
PK="0xf214f2b2cd398c806f84e317254e0f0b801d0643303237d97a22a48e01628897"

{
echo "=== quick-test: $TEST ($(date -Iseconds)) ==="

echo "[1/5] Fresh deploy..."
$DC down -v > /dev/null 2>&1
$DC up -d > /dev/null 2>&1 || true

echo "[1/5] Waiting for builder..."
for i in $(seq 1 120); do
    MODE=$(curl -sf http://localhost:11560/health 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('mode',''))" 2>/dev/null || true)
    [ "$MODE" = "Builder" ] && echo "[1/5] Builder ready (${i}s)" && break
    sleep 1
done

$DC stop crosschain-tx-sender > /dev/null 2>&1 || true
eval "$($DC exec -T builder cat /shared/rollup.env 2>/dev/null)"
echo "[1/5] ROLLUPS=$ROLLUPS_ADDRESS"

echo "[2/5] Prepare network..."
cd contracts/sync-rollups-protocol
bash script/e2e/shared/prepare-network.sh \
    --l1-rpc http://localhost:11556 --l2-rpc http://localhost:11548 \
    --pk "$PK" --rollups "$ROLLUPS_ADDRESS" 2>&1 | tail -3

echo "[3/5] Running: $TEST"
set +e
bash script/e2e/shared/run-network.sh "script/e2e/$TEST/E2E.s.sol" \
    --l1-rpc http://localhost:11556 --l2-rpc http://localhost:11548 \
    --pk "$PK" --rollups "$ROLLUPS_ADDRESS" \
    --manager-l2 "$CROSS_CHAIN_MANAGER_ADDRESS" 2>&1
RC=$?
set -e

echo ""
echo "[4/5] Post-test health:"
cd "$(git rev-parse --show-toplevel)"
curl -s http://localhost:11560/health 2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'  {d[\"mode\"]} l2={d[\"l2_head\"]} rewinds={d[\"consecutive_rewind_cycles\"]} pending={d[\"pending_submissions\"]}')" 2>/dev/null || true

echo ""
if [ "$RC" -eq 0 ]; then
    echo "[5/5] >>> $TEST: PASS"
else
    echo "[5/5] >>> $TEST: FAIL (exit=$RC)"
fi
echo "=== Done ($(date -Iseconds)) ==="
} 2>&1 | tee "$LOG"

exit $RC
