#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════════
# scripts/refactor/capture_baseline.sh
#
# Captures the canonical pre-refactor baseline used by PLAN step 5.7
# (replay gate). For each canonical scenario in the protocol E2E suite,
# spins up an isolated anvil pair, deploys the protocol infra + the
# scenario's Deploy* contracts, runs ComputeExpected and ExecuteNetwork
# read-only, and writes the canonical expected hashes + user tx fields
# to tests/baseline/<scenario>.json.
#
# ── Why isolated anvil and not the devnet ──
#
# The devnet-eez composer/builder has timing-sensitive race conditions
# (pre_state_root mismatch under contention) that the refactor is meant
# to fix. Capturing a baseline against a racy system would record
# unstable bytes. Instead, the baseline captures the PROTOCOL-DEFINED
# canonical hashes (computed by ComputeExpected.sol from the action
# structures) — these are deterministic across runs and across
# implementations, because they come from the protocol Solidity itself.
#
# The replay gate (PLAN step 5.7) then runs this repo's composer in
# network mode against devnet-eez and verifies that the produced
# postBatch entries CONTAIN the baseline's expected hashes (subset
# match — same semantics as the protocol's VerifyL1Batch /
# VerifyL2Blocks / VerifyL2Calls). The replay gate is allowed to
# retry on transient mismatches; the baseline is not.
#
# ── Usage ──
#
#   # Capture all canonical scenarios:
#   bash scripts/refactor/capture_baseline.sh
#
#   # Capture a single scenario (debugging):
#   bash scripts/refactor/capture_baseline.sh --scenario counter
#
# ── Prerequisites ──
#
#   1. anvil, forge, cast, jq must be on PATH.
#   2. The contracts/sync-rollups-protocol submodule must be initialized.
#   3. Ports 18545 and 18546 must be free.
# ═══════════════════════════════════════════════════════════════════════

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PROTO_DIR="$REPO_ROOT/contracts/sync-rollups-protocol"
BASELINE_DIR="$REPO_ROOT/tests/baseline"
LIB="$REPO_ROOT/scripts/refactor/baseline_lib.sh"

# shellcheck source=baseline_lib.sh
source "$LIB"

# Source the protocol's E2EBase.sh — gives us deploy_contracts,
# _export_outputs, extract, ensure_create2_factory, etc. We don't
# reimplement them. Disable `-e` afterwards because E2EBase.sh sets
# `set -euo pipefail` and we need our own per-scenario error handling
# (continue on a failed scenario instead of aborting the whole capture).
source "$PROTO_DIR/script/e2e/shared/E2EBase.sh"
set +e

# ── Anvil ports (isolated from devnet-eez / testnet-eez / local fullnode) ──
ANVIL_L1_PORT=18545
ANVIL_L2_PORT=18546
export L1_RPC="http://localhost:$ANVIL_L1_PORT"
export L2_RPC="http://localhost:$ANVIL_L2_PORT"
export L2_ROLLUP_ID=1
# Anvil default key #0 (always funded with 10000 ETH)
export PK="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

# Set after anvil is up + DeployInfra runs
export ROLLUPS=""
export MANAGER_L2=""
export RPC=""

# Will be set during the run for the meta file
SCENARIOS_TOTAL=0
SCENARIOS_CAPTURED=0
SCENARIOS_FAILED=0

# ── Anvil PIDs (cleaned up on exit) ──
ANVIL_L1_PID=""
ANVIL_L2_PID=""

cleanup() {
    if [[ -n "$ANVIL_L1_PID" ]]; then
        kill "$ANVIL_L1_PID" 2>/dev/null || true
    fi
    if [[ -n "$ANVIL_L2_PID" ]]; then
        kill "$ANVIL_L2_PID" 2>/dev/null || true
    fi
    # Best-effort: free the ports if anything else is hanging on them
    fuser -k "${ANVIL_L1_PORT}/tcp" 2>/dev/null || true
    fuser -k "${ANVIL_L2_PORT}/tcp" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── Canonical scenario list (PLAN step 0.8 initial set) ──
ALL_SCENARIOS=(
    counter
    counterL2
    bridge
    multi-call-twice
    multi-call-two-diff
    flash-loan
    nestedCounter
    nestedCounterL2
    revertContinue
    revertContinueL2
)

# ── CLI parsing ──
SCENARIO_FILTER=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --scenario) SCENARIO_FILTER="$2"; shift 2 ;;
        --pk)       PK="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,50p' "$0" | grep '^#' | sed 's/^# \?//'
            exit 0 ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

# ══════════════════════════════════════════════
#  Pre-flight
# ══════════════════════════════════════════════
echo "════════════════════════════════════════════════════════════════"
echo "Refactor baseline capture (PLAN step 0.8)"
echo "════════════════════════════════════════════════════════════════"
echo "Mode:           isolated anvil (deterministic baseline)"
echo "L1 anvil port:  $ANVIL_L1_PORT"
echo "L2 anvil port:  $ANVIL_L2_PORT"
echo "Test PK:        anvil dev key #0"
echo ""

bl_check_tools
if ! command -v anvil &>/dev/null; then
    echo "ERROR: anvil not found on PATH" >&2
    exit 1
fi
if ! command -v fuser &>/dev/null; then
    echo "WARN: fuser not found — anvil port cleanup will be best-effort" >&2
fi

mkdir -p "$BASELINE_DIR"

# Build the protocol contracts so forge has the artifacts available.
echo "════════════════════════════════════════════════════════════════"
echo "forge build (sync-rollups-protocol)"
echo "════════════════════════════════════════════════════════════════"
( cd "$PROTO_DIR" && forge build 2>&1 | tail -5 ) || {
    echo "ERROR: forge build failed in $PROTO_DIR" >&2
    exit 1
}

# ══════════════════════════════════════════════
#  Step 1: start anvils
# ══════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "Starting anvils"
echo "════════════════════════════════════════════════════════════════"

start_anvil() {
    local port="$1"
    local pid_var="$2"
    local label="$3"
    # Kill anything already on the port (e.g. leftover from a previous run).
    fuser -k "${port}/tcp" 2>/dev/null || true
    sleep 0.2
    # Instant-mining (no --block-time) keeps timestamps deterministic
    # across runs. Fixed --gas-price + --base-fee 0 prevents EIP-1559
    # fee oracle drift between runs (which would cause cast mktx to
    # produce different signed bytes for L2 trigger scenarios).
    anvil --port "$port" --silent \
        --gas-price 1000000000 \
        --base-fee 0 \
        > "/tmp/anvil-${port}.log" 2>&1 &
    local pid=$!
    eval "$pid_var=$pid"
    # Poll until anvil responds
    for _ in $(seq 1 30); do
        if cast block-number --rpc-url "http://localhost:$port" &>/dev/null; then
            echo "$label anvil up on port $port (pid $pid)"
            return 0
        fi
        sleep 0.2
    done
    echo "ERROR: $label anvil failed to start on port $port" >&2
    return 1
}

start_anvil "$ANVIL_L1_PORT" ANVIL_L1_PID "L1"
start_anvil "$ANVIL_L2_PORT" ANVIL_L2_PID "L2"

# ══════════════════════════════════════════════
#  Step 2: deploy infra (Rollups + ManagerL2)
# ══════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "Deploying infra (Rollups L1 + CrossChainManagerL2)"
echo "════════════════════════════════════════════════════════════════"
SYSTEM_ADDRESS="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
DEPLOY_OUT=$(cd "$PROTO_DIR" && forge script script/e2e/shared/DeployInfra.s.sol:DeployRollupsL1 \
    --rpc-url "$L1_RPC" --broadcast --private-key "$PK" 2>&1)
ROLLUPS=$(echo "$DEPLOY_OUT" | grep -oE 'ROLLUPS=0x[0-9a-fA-F]+' | head -1 | sed 's/^ROLLUPS=//')
if [[ -z "$ROLLUPS" ]]; then
    echo "ERROR: failed to extract ROLLUPS address from DeployRollupsL1 output" >&2
    echo "$DEPLOY_OUT" | tail -30 >&2
    exit 1
fi
echo "ROLLUPS:    $ROLLUPS"

DEPLOY_L2_OUT=$(cd "$PROTO_DIR" && forge script script/e2e/shared/DeployInfra.s.sol:DeployManagerL2 \
    --rpc-url "$L2_RPC" --broadcast --private-key "$PK" \
    --sig "run(uint256,address)" "$L2_ROLLUP_ID" "$SYSTEM_ADDRESS" 2>&1)
MANAGER_L2=$(echo "$DEPLOY_L2_OUT" | grep -oE 'MANAGER_L2=0x[0-9a-fA-F]+' | head -1 | sed 's/^MANAGER_L2=//')
if [[ -z "$MANAGER_L2" ]]; then
    echo "ERROR: failed to extract MANAGER_L2 address from DeployManagerL2 output" >&2
    echo "$DEPLOY_L2_OUT" | tail -30 >&2
    exit 1
fi
echo "MANAGER_L2: $MANAGER_L2"

export ROLLUPS MANAGER_L2
export RPC="$L1_RPC"

# ── CREATE2 factory on both ──
ensure_create2() {
    local rpc="$1"
    local label="$2"
    local CREATE2_FACTORY="0x4e59b44847b379578588920cA78FbF26c0B4956C"
    local code
    code=$(cast code "$CREATE2_FACTORY" --rpc-url "$rpc" 2>/dev/null || echo "0x")
    if [[ "$code" != "0x" && ${#code} -gt 2 ]]; then
        echo "$label CREATE2 factory already deployed"
        return
    fi
    # Pre-fund the factory deployer (Arachnid's deterministic address) and submit the keyless tx
    cast send --rpc-url "$rpc" --private-key "$PK" --value 0.01ether \
        0x3fAB184622Dc19b6109349B94811493BF2a45362 >/dev/null 2>&1 || true
    local TX="0xf8a58085174876e800830186a08080b853604580600e600039806000f350fe7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf31ba02222222222222222222222222222222222222222222222222222222222222222a02222222222222222222222222222222222222222222222222222222222222222"
    cast publish "$TX" --rpc-url "$rpc" >/dev/null 2>&1 || true
    echo "$label CREATE2 factory deployed"
}
ensure_create2 "$L1_RPC" "L1"
ensure_create2 "$L2_RPC" "L2"

# ══════════════════════════════════════════════
#  Step 3: capture each scenario
# ══════════════════════════════════════════════

# Helper: deploy app contracts for the scenario by delegating to the
# protocol's `deploy_contracts` (sourced from E2EBase.sh). It auto-
# discovers Deploy* contracts in file order and exports their KEY=VALUE
# outputs as env vars via `_export_outputs`.
deploy_app() {
    local sol_file="$1"
    pushd "$PROTO_DIR" >/dev/null
    deploy_contracts "$sol_file" "$L1_RPC" "$L2_RPC" "$PK"
    local rc=$?
    popd >/dev/null
    return $rc
}

run_compute_expected() {
    local sol_file="$1"
    cd "$PROTO_DIR" && forge script "$sol_file:ComputeExpected" \
        --rpc-url "$L1_RPC" --sender "$(cast wallet address --private-key "$PK")" 2>&1
}

run_execute_network() {
    local sol_file="$1"
    local _trigger_contract="ExecuteNetwork"
    local _trigger_rpc="$L1_RPC"
    if grep -q 'contract ExecuteNetworkL2 ' "$sol_file"; then
        _trigger_contract="ExecuteNetworkL2"
        _trigger_rpc="$L2_RPC"
    fi
    cd "$PROTO_DIR" && forge script "$sol_file:$_trigger_contract" \
        --rpc-url "$_trigger_rpc" 2>&1
}

# Create the signed raw user tx that ComputeExpected may need (especially
# L2 trigger scenarios that hash an L2TX action containing the RLP tx).
# Mirrors the cast mktx step in run-network.sh / run-local.sh.
create_rlp_encoded_tx() {
    local sol_file="$1"
    local target="$2"
    local value="$3"
    local calldata="$4"
    local _trigger_rpc="$L1_RPC"
    local _is_l2=0
    if grep -q 'contract ExecuteNetworkL2 ' "$sol_file"; then
        _trigger_rpc="$L2_RPC"
        _is_l2=1
    fi
    local _sender
    _sender=$(cast wallet address --private-key "$PK")
    local _nonce
    _nonce=$(cast nonce "$_sender" --rpc-url "$_trigger_rpc")
    local _user_nonce="$_nonce"
    # L2 trigger uses nonce+1 because loadExecutionTable bumps the system
    # nonce first; L1 trigger uses the current nonce.
    if (( _is_l2 )); then
        _user_nonce=$((_nonce + 1))
    fi
    cast mktx "$target" "$calldata" \
        --value "${value}wei" \
        --gas-limit 2000000 \
        --nonce "$_user_nonce" \
        --private-key "$PK" \
        --rpc-url "$_trigger_rpc"
}

FAILED_LIST=()

for scenario in "${ALL_SCENARIOS[@]}"; do
    if [[ -n "$SCENARIO_FILTER" && "$scenario" != "$SCENARIO_FILTER" ]]; then
        continue
    fi
    SCENARIOS_TOTAL=$((SCENARIOS_TOTAL + 1))

    sol_file="$PROTO_DIR/script/e2e/$scenario/E2E.s.sol"
    if [[ ! -f "$sol_file" ]]; then
        echo ""
        echo "── SKIP $scenario (no E2E.s.sol)" >&2
        SCENARIOS_FAILED=$((SCENARIOS_FAILED + 1))
        FAILED_LIST+=("$scenario:missing-sol")
        continue
    fi

    echo ""
    echo "════════════════════════════════════════════════════════════════"
    echo "Scenario: $scenario"
    echo "════════════════════════════════════════════════════════════════"

    # Reset captured fields
    EXPECTED_L1_HASHES=""
    EXPECTED_L2_HASHES=""
    EXPECTED_L2_CALL_HASHES=""
    TARGET=""
    VALUE=""
    CALLDATA=""

    # Step A: deploy app contracts
    if ! deploy_app "$sol_file"; then
        SCENARIOS_FAILED=$((SCENARIOS_FAILED + 1))
        FAILED_LIST+=("$scenario:deploy-failed")
        continue
    fi

    # Step B: ExecuteNetwork (read-only — TARGET/VALUE/CALLDATA only)
    # We run this BEFORE ComputeExpected because L2 trigger scenarios
    # need the RLP_ENCODED_TX env var (see step C below) which is
    # derived from the user tx params, and ComputeExpected reads it.
    exec_out=$(run_execute_network "$sol_file")
    TARGET=$(bl_extract "$exec_out" "TARGET")
    VALUE=$(bl_extract "$exec_out" "VALUE")
    CALLDATA=$(bl_extract "$exec_out" "CALLDATA")
    if [[ -z "$TARGET" || -z "$CALLDATA" ]]; then
        echo "── ExecuteNetwork did not emit TARGET / CALLDATA"
        echo "$exec_out" | tail -20
        SCENARIOS_FAILED=$((SCENARIOS_FAILED + 1))
        FAILED_LIST+=("$scenario:execute-network-failed")
        continue
    fi

    # Step C: build the signed raw user tx and export it for ComputeExpected.
    if ! RLP_ENCODED_TX=$(create_rlp_encoded_tx "$sol_file" "$TARGET" "$VALUE" "$CALLDATA"); then
        echo "── cast mktx FAILED"
        SCENARIOS_FAILED=$((SCENARIOS_FAILED + 1))
        FAILED_LIST+=("$scenario:mktx-failed")
        continue
    fi
    export RLP_ENCODED_TX

    # Step D: ComputeExpected
    compute_out=$(run_compute_expected "$sol_file")
    if [[ $? -ne 0 ]]; then
        echo "── ComputeExpected FAILED"
        echo "$compute_out" | tail -20
        SCENARIOS_FAILED=$((SCENARIOS_FAILED + 1))
        FAILED_LIST+=("$scenario:compute-failed")
        continue
    fi
    EXPECTED_L1_HASHES=$(bl_extract "$compute_out" "EXPECTED_L1_HASHES")
    EXPECTED_L2_HASHES=$(bl_extract "$compute_out" "EXPECTED_L2_HASHES")
    EXPECTED_L2_CALL_HASHES=$(bl_extract "$compute_out" "EXPECTED_L2_CALL_HASHES")

    # Step D: write JSON
    bl_write_scenario_json "$scenario" "$BASELINE_DIR"
    SCENARIOS_CAPTURED=$((SCENARIOS_CAPTURED + 1))
    echo "── OK   wrote $BASELINE_DIR/${scenario}.json"
    echo "        L1 hashes:    ${EXPECTED_L1_HASHES:-(empty)}"
    echo "        L2 hashes:    ${EXPECTED_L2_HASHES:-(empty)}"
    echo "        L2 calls:     ${EXPECTED_L2_CALL_HASHES:-(empty)}"
    echo "        target/calldata captured"
done

# ══════════════════════════════════════════════
#  Step 4: write _meta.json
# ══════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "Writing _meta.json"
echo "════════════════════════════════════════════════════════════════"
bl_record_meta "$BASELINE_DIR/_meta.json"

# ══════════════════════════════════════════════
#  Summary
# ══════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "Summary"
echo "════════════════════════════════════════════════════════════════"
echo "Scenarios total:    $SCENARIOS_TOTAL"
echo "Scenarios captured: $SCENARIOS_CAPTURED"
echo "Scenarios failed:   $SCENARIOS_FAILED"
if [[ ${#FAILED_LIST[@]} -gt 0 ]]; then
    echo "Failed list:"
    for entry in "${FAILED_LIST[@]}"; do
        echo "  - $entry"
    done
fi
echo ""
echo "Baseline written to: $BASELINE_DIR"

if [[ $SCENARIOS_CAPTURED -eq 0 ]]; then
    echo "ERROR: zero scenarios captured" >&2
    exit 1
fi

exit 0
