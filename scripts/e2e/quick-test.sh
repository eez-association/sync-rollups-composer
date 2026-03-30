#!/usr/bin/env bash
# quick-test.sh — Run a single protocol E2E test
#
# Usage: ./scripts/e2e/quick-test.sh <test-name> [--no-reset]
#   --no-reset  Skip docker down/up — reuse running devnet (faster iteration)
# Monitor: tail -f /tmp/quick-test-<name>.log

cd "$(git rev-parse --show-toplevel)"

TEST="${1:?Usage: quick-test.sh <test-name> [--no-reset]}"
NO_RESET=false
[[ "${2:-}" == "--no-reset" ]] && NO_RESET=true

SOL="contracts/sync-rollups-protocol/script/e2e/$TEST/E2E.s.sol"
[ -f "$SOL" ] || { echo "ERROR: $SOL not found"; exit 1; }

LOG="/tmp/quick-test-${TEST}.log"
DC="sudo docker compose -f deployments/devnet-eez/docker-compose.yml -f deployments/devnet-eez/docker-compose.dev.yml"
PK="0xf214f2b2cd398c806f84e317254e0f0b801d0643303237d97a22a48e01628897"

{
echo "=== quick-test: $TEST ($(date -Iseconds)) ==="

if $NO_RESET; then
    echo "[1/5] Reusing running devnet (--no-reset)"
else
    echo "[1/5] Fresh deploy..."
    $DC down -v --timeout 30 > /dev/null 2>&1
    sleep 3
    $DC up -d > /dev/null 2>&1 || true
fi

echo "[1/5] Waiting for builder..."
for i in $(seq 1 120); do
    MODE=$(curl -sf http://localhost:11560/health 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('mode',''))" 2>/dev/null || true)
    [ "$MODE" = "Builder" ] && echo "[1/5] Builder ready (${i}s)" && break
    sleep 1
done

# Stop tx senders IMMEDIATELY to prevent deposit txs from contaminating test batches.
$DC stop crosschain-tx-sender tx-sender > /dev/null 2>&1 || true
if ! $NO_RESET; then
    # Wait for Docker deploy services to complete — they deploy Bridge and flash loan
    # contracts via CREATE2 at deterministic addresses. Running the test before they
    # finish causes a race condition (both try to CREATE2-deploy at the same address).
    for ctr in devnet-eez-deploy-1 devnet-eez-deploy-l2-1 devnet-eez-deploy-reverse-flash-loan-1; do
        sudo docker wait "$ctr" > /dev/null 2>&1 || true
    done
fi
# Only extract the two variables we need — avoid polluting the environment
# with 40KB+ bytecode strings from rollup.env that can break forge subprocesses.
ROLLUPS_ADDRESS=$($DC exec -T builder sh -c 'grep "^ROLLUPS_ADDRESS=" /shared/rollup.env | cut -d= -f2' 2>/dev/null)
CROSS_CHAIN_MANAGER_ADDRESS=$($DC exec -T builder sh -c 'grep "^CROSS_CHAIN_MANAGER_ADDRESS=" /shared/rollup.env | cut -d= -f2' 2>/dev/null)
echo "[1/5] ROLLUPS=$ROLLUPS_ADDRESS"

echo "[2/5] Prepare network..."
cd contracts/sync-rollups-protocol
# Clear forge broadcast/cache to prevent stale nonces from previous test runs.
rm -rf broadcast cache/E2E.s.sol 2>/dev/null
bash script/e2e/shared/prepare-network.sh \
    --l1-rpc http://localhost:11556 --l2-rpc http://localhost:11548 \
    --pk "$PK" --rollups "$ROLLUPS_ADDRESS" 2>&1 | tail -3

# Wait for L2 chain to advance past the CREATE2 factory deployment block.
# On reth --dev, blocks are only produced when txs arrive, so the factory
# tx might be in a pending block when forge forks for simulation.
echo "[2/5] Waiting for CREATE2 factory..."
for i in $(seq 1 30); do
    code=$(cast code 0x4e59b44847b379578588920cA78FbF26c0B4956C --rpc-url http://localhost:11548 2>/dev/null)
    [ "$code" != "0x" ] && [ "${#code}" -gt 2 ] && echo "[2/5] CREATE2 factory ready" && break
    sleep 1
done

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
