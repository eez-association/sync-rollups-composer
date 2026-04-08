#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════════
# scripts/refactor/baseline_lib.sh
#
# Shared helpers for capture_baseline.sh. Sourced, not executed.
#
# Refactor PLAN step 0.8 — captures the protocol E2E test outputs as
# the canonical baseline for the refactor's byte-equivalence gate
# (step 5.7).
# ═══════════════════════════════════════════════════════════════════════

set -uo pipefail

# Suppress foundry nightly warning across the script
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1

# ────────────────────────────────────────────────
# bl_extract <output> <key>
# Extracts `KEY=VALUE` from forge script stdout.
# Returns the value or empty string if not found.
# ────────────────────────────────────────────────
bl_extract() {
    local output="$1"
    local key="$2"
    echo "$output" | grep -oE "${key}=[^[:space:]]+" | head -1 | sed "s/^${key}=//"
}

# ────────────────────────────────────────────────
# bl_strip_quotes <value>
# Removes surrounding quotes if present (forge sometimes wraps in "").
# ────────────────────────────────────────────────
bl_strip_quotes() {
    sed 's/^"//; s/"$//' <<< "$1"
}

# ────────────────────────────────────────────────
# bl_capture_tx_input <tx_hash> <rpc>
# Returns the raw input (calldata hex) of a tx.
# Empty string if the tx is not found.
# ────────────────────────────────────────────────
bl_capture_tx_input() {
    local tx_hash="$1"
    local rpc="$2"
    if [[ -z "$tx_hash" || "$tx_hash" == "null" ]]; then
        echo ""
        return 0
    fi
    cast tx --rpc-url "$rpc" "$tx_hash" --json 2>/dev/null \
        | jq -r '.input // ""'
}

# ────────────────────────────────────────────────
# bl_capture_tx_block <tx_hash> <rpc>
# Returns the block number that includes the tx (decimal).
# ────────────────────────────────────────────────
bl_capture_tx_block() {
    local tx_hash="$1"
    local rpc="$2"
    if [[ -z "$tx_hash" || "$tx_hash" == "null" ]]; then
        echo ""
        return 0
    fi
    cast tx --rpc-url "$rpc" "$tx_hash" --json 2>/dev/null \
        | jq -r '.blockNumber // ""' \
        | sed 's/^0x//' \
        | { read -r hex; [[ -n "$hex" ]] && printf "%d" "0x$hex" || echo ""; }
}

# ────────────────────────────────────────────────
# bl_capture_logs <l1_block> <address> <event_sig> <rpc>
# Returns the JSON array of logs matching the address + event signature
# in the given block.
# ────────────────────────────────────────────────
bl_capture_logs() {
    local block="$1"
    local addr="$2"
    local event="$3"
    local rpc="$4"
    if [[ -z "$block" ]]; then
        echo "[]"
        return 0
    fi
    cast logs \
        --rpc-url "$rpc" \
        --from-block "$block" \
        --to-block "$block" \
        --address "$addr" \
        "$event" \
        --json 2>/dev/null \
        || echo "[]"
}

# ────────────────────────────────────────────────
# bl_record_meta <output_path>
# Writes tests/baseline/_meta.json with submodule SHA, timestamp,
# and the git commit of the refactor branch. The replay gate (5.7)
# refuses to compare against a baseline whose submodule SHA differs
# from the current checkout.
# ────────────────────────────────────────────────
bl_record_meta() {
    local out="$1"
    local repo_sha
    local submodule_sha
    local generated_at
    repo_sha=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo "unknown")
    submodule_sha=$(git -C "$REPO_ROOT/contracts/sync-rollups-protocol" \
        rev-parse HEAD 2>/dev/null || echo "unknown")
    generated_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    cat > "$out" <<EOF
{
  "schema_version": 1,
  "generated_at": "$generated_at",
  "repo_sha": "$repo_sha",
  "submodule_sha": "$submodule_sha",
  "l1_rpc": "$L1_RPC",
  "l2_rpc": "$L2_RPC",
  "rollups_address": "$ROLLUPS",
  "manager_l2_address": "$MANAGER_L2",
  "l2_rollup_id": $L2_ROLLUP_ID,
  "scenarios_total": $SCENARIOS_TOTAL,
  "scenarios_captured": $SCENARIOS_CAPTURED,
  "scenarios_failed": $SCENARIOS_FAILED
}
EOF
}

# ────────────────────────────────────────────────
# bl_write_scenario_json <scenario_name> <output_dir>
# Writes tests/baseline/<scenario>.json from the captured fields.
# Expects the following globals to be set by the caller:
#   EXPECTED_L1_HASHES, EXPECTED_L2_HASHES, EXPECTED_L2_CALL_HASHES,
#   TARGET, VALUE, CALLDATA
#
# All fields are protocol-defined and deterministic. The replay gate
# (PLAN step 5.7) re-runs each scenario against this repo's composer
# and verifies that the produced postBatch contains these expected
# hashes (subset match).
# ────────────────────────────────────────────────
bl_write_scenario_json() {
    local scenario="$1"
    local out_dir="$2"
    local out_file="$out_dir/${scenario}.json"

    # Helper: emit a JSON string field, escaping properly via jq.
    _str() { jq -Rn --arg v "${1:-}" '$v'; }

    # Helper: emit a raw JSON value (array/object/null) — passed through.
    _raw() {
        local v="${1:-}"
        if [[ -z "$v" ]]; then
            echo "null"
        else
            echo "$v"
        fi
    }

    # Helper: convert Solidity bytes32 array literal "[0xabc,0xdef,...]"
    # to a proper JSON array '["0xabc","0xdef",...]'.
    # Empty input or "[]" → []. Single value → ["0xabc"].
    _hashes() {
        local v="${1:-}"
        if [[ -z "$v" || "$v" == "[]" ]]; then
            echo "[]"
            return
        fi
        # Strip surrounding brackets and whitespace, split on comma,
        # quote each element, re-wrap in brackets.
        local stripped="${v#[}"
        stripped="${stripped%]}"
        local IFS=','
        # shellcheck disable=SC2206
        local arr=($stripped)
        local out="["
        local first=1
        for elem in "${arr[@]}"; do
            elem="${elem## }"; elem="${elem%% }"
            [[ -z "$elem" ]] && continue
            if (( first )); then
                first=0
            else
                out+=","
            fi
            out+="\"$elem\""
        done
        out+="]"
        echo "$out"
    }

    cat > "$out_file" <<EOF
{
  "scenario": $(_str "$scenario"),
  "schema_version": 1,
  "expected": {
    "l1_hashes": $(_hashes "${EXPECTED_L1_HASHES:-}"),
    "l2_hashes": $(_hashes "${EXPECTED_L2_HASHES:-}"),
    "l2_call_hashes": $(_hashes "${EXPECTED_L2_CALL_HASHES:-}")
  },
  "user_tx": {
    "target": $(_str "${TARGET:-}"),
    "value": $(_str "${VALUE:-0}"),
    "calldata": $(_str "${CALLDATA:-}")
  }
}
EOF

    # Round-trip through jq to validate JSON syntax + canonicalize formatting.
    local tmp
    tmp=$(mktemp)
    if jq '.' "$out_file" > "$tmp" 2>/dev/null; then
        mv "$tmp" "$out_file"
    else
        echo "WARN: scenario $scenario produced invalid JSON; leaving as-is" >&2
        rm -f "$tmp"
    fi
}

# ────────────────────────────────────────────────
# bl_check_tools
# Verifies forge, cast, jq are available.
# ────────────────────────────────────────────────
bl_check_tools() {
    local missing=""
    for tool in forge cast jq; do
        if ! command -v "$tool" &>/dev/null; then
            missing="$missing $tool"
        fi
    done
    if [[ -n "$missing" ]]; then
        echo "ERROR: missing required tools:$missing" >&2
        exit 1
    fi
}
