#!/usr/bin/env bash
# ============================================================================
# L1 Block Inspector
#
# Decodes postBatch entries, cross-chain state deltas, L2 block data, proxy
# creation, trigger calls, and receipt events for one or more L1 blocks.
#
# Usage:
#   inspect-l1-block.sh <block|latest|from-to> [L1_RPC] [L2_RPC]
#
# Examples:
#   inspect-l1-block.sh 42
#   inspect-l1-block.sh latest
#   inspect-l1-block.sh 40-45
#   inspect-l1-block.sh 42 http://localhost:9555 http://localhost:9545
# ============================================================================
set -euo pipefail

BLOCK_ARG="${1:?Usage: inspect-l1-block.sh <block|latest|from-to> [l1_rpc] [l2_rpc]}"
L1_RPC="${2:-http://localhost:9555}"
L2_RPC="${3:-http://localhost:9545}"

# ── Dependencies ────────────────────────────────────────────────
for cmd in cast jq; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "ERROR: $cmd required but not found"; exit 1; }
done

# ── Colors (disabled for non-TTY) ──────────────────────────────
if [[ -t 1 ]]; then
    B="\033[1m" D="\033[2m" R="\033[0m"
    RED="\033[31m" GRN="\033[32m" YLW="\033[33m" BLU="\033[34m" CYN="\033[36m" MAG="\033[35m"
else
    B="" D="" R="" RED="" GRN="" YLW="" BLU="" CYN="" MAG=""
fi

# ── Known selectors (computed at runtime from canonical ABI signatures) ─
_sel() { cast sig "$1" 2>/dev/null || echo "0xDEAD"; }
SEL_POSTBATCH=$(_sel "postBatch(((uint256,bytes32,bytes32,int256)[],bytes32,(uint8,uint256,address,uint256,bytes,bool,address,uint256,uint256[]))[],uint256,bytes,bytes)")
SEL_CREATE_PROXY=$(_sel "createCrossChainProxy(address,uint256)")
SEL_EXEC_CC=$(_sel "executeCrossChainCall(address,bytes)")
SEL_EXEC_BEHALF=$(_sel "executeOnBehalf(address,bytes)")
SEL_AUTH_PROXY=$(_sel "authorizedProxies(address)")
SEL_CLAIM=$(_sel "claim()")
SEL_LOAD_TABLE=$(_sel "loadExecutionTable(((uint256,bytes32,bytes32,int256)[],bytes32,(uint8,uint256,address,uint256,bytes,bool,address,uint256,uint256[]))[])")
SEL_BRIDGE_ETHER=$(_sel "bridgeEther(uint256,address)")
SEL_EXEC_INCOMING=$(_sel "executeIncomingCrossChainCall(address,uint256,bytes,address,uint256,uint256[])")
SEL_SET_CONTEXT=$(_sel "setContext(uint256,bytes32)")
SEL_INCREMENT=$(_sel "increment()")
# Event topic0 hashes — computed at runtime to stay in sync with contract ABI
_ek() { cast keccak "$1" 2>/dev/null || echo ""; }
TOPIC_BATCH_POSTED=$(_ek "BatchPosted(((uint256,bytes32,bytes32,int256)[],bytes32,(uint8,uint256,address,uint256,bytes,bool,address,uint256,uint256[]))[],bytes32)")
TOPIC_EXEC_CONSUMED=$(_ek "ExecutionConsumed(bytes32,(uint8,uint256,address,uint256,bytes,bool,address,uint256,uint256[]))")
TOPIC_PROXY_CREATED=$(_ek "CrossChainProxyCreated(address,address,uint256)")
TOPIC_STATE_UPDATED=$(_ek "StateUpdated(uint256,bytes32)")
TOPIC_L2_EXEC=$(_ek "L2ExecutionPerformed(uint256,bytes32,bytes32)")
TOPIC_CC_CALL_EXEC=$(_ek "CrossChainCallExecuted(bytes32,address,address,bytes,uint256)")
TOPIC_L2TX_EXEC=$(_ek "L2TXExecuted(bytes32,uint256,bytes)")

# ── Load rollup.env for known addresses ────────────────────────
ROLLUPS_ADDRESS="" BRIDGE_ADDRESS="" ROLLUP_ID=""
_load_env() {
    local env_file=""
    if [[ -f "/shared/rollup.env" ]]; then
        env_file="/shared/rollup.env"
    elif sudo docker exec testnet-eez-builder-1 cat /shared/rollup.env > /tmp/_rollup_env_$$ 2>/dev/null; then
        env_file="/tmp/_rollup_env_$$"
    fi
    if [[ -n "$env_file" ]]; then
        # shellcheck disable=SC1090
        source "$env_file" 2>/dev/null || true
    fi
}
_load_env

# ── Utility functions ──────────────────────────────────────────

# Read 32-byte word from hex data (no 0x prefix) at byte offset
_word() { local d="$1" off="$2"; echo "${d:$((off * 2)):64}"; }

# Hex → decimal (unsigned)
_dec() { printf "%d" "0x${1}" 2>/dev/null || echo "0"; }

# Extract address (last 20 bytes) from a 32-byte word
_addr() { echo "0x${1:24:40}"; }

# Hex string → decimal (handles 0x prefix)
_h2d() { printf "%d" "${1}" 2>/dev/null || echo "0"; }

# Truncate hash for display
_short() { local h="$1"; echo "${h:0:10}…${h: -6}"; }

# Action type number → name
_atype() {
    case "$1" in
        0) echo "CALL" ;; 1) echo "RESULT" ;; 2) echo "L2TX" ;;
        3) echo "REVERT" ;; 4) echo "REVERT_CONTINUE" ;; *) echo "?($1)" ;;
    esac
}

# Selector → function name
_fname() {
    case "$1" in
        "$SEL_POSTBATCH")    echo "postBatch" ;;
        "$SEL_CREATE_PROXY") echo "createCrossChainProxy" ;;
        "$SEL_EXEC_CC")      echo "executeCrossChainCall" ;;
        "$SEL_EXEC_BEHALF")  echo "executeOnBehalf" ;;
        "$SEL_AUTH_PROXY")   echo "authorizedProxies" ;;
        "$SEL_CLAIM")        echo "claim" ;;
        "$SEL_LOAD_TABLE")    echo "loadExecutionTable" ;;
        "$SEL_BRIDGE_ETHER")  echo "bridgeEther" ;;
        "$SEL_EXEC_INCOMING") echo "executeIncomingCrossChainCall" ;;
        "$SEL_SET_CONTEXT")   echo "setContext" ;;
        "$SEL_INCREMENT")     echo "increment" ;;
        *) echo "unknown($1)" ;;
    esac
}

# Label known addresses
_label() {
    local a="${1,,}"
    [[ -n "$ROLLUPS_ADDRESS" && "$a" == "${ROLLUPS_ADDRESS,,}" ]] && echo " (Rollups)" && return
    [[ -n "$BRIDGE_ADDRESS"  && "$a" == "${BRIDGE_ADDRESS,,}" ]]  && echo " (Bridge)"  && return
    case "$a" in
        0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266) echo " (dev#0 builder)" ;;
        0x70997970c51812dc3a010c7d01b50e0d17dc79c8) echo " (dev#1 tx-sender)" ;;
        0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc) echo " (dev#2 crosschain)" ;;
        0x90f79bf6eb2c4f870365e785982e1f101e93b906) echo " (dev#3 bridge)" ;;
        0x4200000000000000000000000000000000000003) echo " (CCM_L2)" ;;
        *) echo "" ;;
    esac
}

# Format signed int256 ether delta (word = 64 hex chars, no prefix)
_ether_delta() {
    local w="$1"
    if [[ "$w" == "0000000000000000000000000000000000000000000000000000000000000000" ]]; then
        echo "0"; return
    fi
    # Decode as signed int256
    local signed
    signed=$(cast decode-abi "f()(int256)" "0x$w" 2>/dev/null || echo "?")
    if [[ "$signed" == "?" ]]; then echo "0x${w:0:16}…"; return; fi
    if [[ "$signed" == -* ]]; then
        local abs="${signed#-}"
        local eth; eth=$(cast from-wei "$abs" 2>/dev/null || echo "$abs wei")
        echo "-${eth} ETH"
    elif [[ "$signed" == "0" ]]; then
        echo "0"
    else
        local eth; eth=$(cast from-wei "$signed" 2>/dev/null || echo "$signed wei")
        echo "+${eth} ETH"
    fi
}

# Format timestamp
_ts() { date -d "@$1" -u "+%Y-%m-%d %H:%M:%S UTC" 2>/dev/null || echo "$1"; }

# ── Event name decoder ─────────────────────────────────────────
_event_name() {
    local json="$1" idx="$2" topic0="$3"
    case "$topic0" in
        "$TOPIC_BATCH_POSTED")
            echo "BatchPosted" ;;
        "$TOPIC_EXEC_CONSUMED")
            local ah; ah=$(echo "$json" | jq -r ".logs[$idx].topics[1] // \"\"" 2>/dev/null)
            echo "ExecutionConsumed(${ah:0:10}…)" ;;
        "$TOPIC_PROXY_CREATED")
            local pa oa rh
            pa=$(echo "$json" | jq -r ".logs[$idx].topics[1] // \"\"" 2>/dev/null)
            oa=$(echo "$json" | jq -r ".logs[$idx].topics[2] // \"\"" 2>/dev/null)
            rh=$(echo "$json" | jq -r ".logs[$idx].topics[3] // \"\"" 2>/dev/null)
            echo "ProxyCreated(proxy=0x${pa:26}, orig=0x${oa:26}, rid=$(_h2d "$rh"))" ;;
        "$TOPIC_STATE_UPDATED")
            local rid; rid=$(echo "$json" | jq -r ".logs[$idx].topics[1] // \"\"" 2>/dev/null)
            echo "StateUpdated(rollup=$(_h2d "$rid"))" ;;
        "$TOPIC_L2_EXEC")
            local rid; rid=$(echo "$json" | jq -r ".logs[$idx].topics[1] // \"\"" 2>/dev/null)
            echo "L2ExecutionPerformed(rollup=$(_h2d "$rid"))" ;;
        "$TOPIC_CC_CALL_EXEC")
            local ah; ah=$(echo "$json" | jq -r ".logs[$idx].topics[1] // \"\"" 2>/dev/null)
            echo "CrossChainCallExecuted(${ah:0:10}…)" ;;
        "$TOPIC_L2TX_EXEC")
            local ah; ah=$(echo "$json" | jq -r ".logs[$idx].topics[1] // \"\"" 2>/dev/null)
            echo "L2TXExecuted(${ah:0:10}…)" ;;
        *)
            echo "event(${topic0:0:10}…)" ;;
    esac
}

# ── Show events from a receipt ─────────────────────────────────
_show_tx_events() {
    local json="$1"
    local cnt; cnt=$(echo "$json" | jq '.logs | length' 2>/dev/null || echo "0")
    if (( cnt > 0 )); then
        local _hdr; _hdr=$(printf "─ Events (%d logs) ─" "$cnt")
        printf "  ${CYN}╭%s╮${R}\n" "$_hdr"
        for (( i=0; i < cnt; i++ )); do
            local t0; t0=$(echo "$json" | jq -r ".logs[$i].topics[0] // \"\"")
            printf "  ${CYN}│${R} [%d] %s\n" "$i" "$(_event_name "$json" "$i" "$t0")"
        done
        local _bot; _bot=$(printf '─%.0s' $(seq 1 "${#_hdr}"))
        printf "  ${CYN}╰%s╯${R}\n" "$_bot"
    fi
}

# ── postBatch entry decoder ────────────────────────────────────
# Parses the raw ABI-encoded calldata to extract entries, state
# deltas, action hashes, action types, and the L2 block callData.
decode_postbatch() {
    local input="$1" tx_hash="$2"

    # Strip 0x prefix + 4-byte selector
    local data="${input#0x}"; data="${data:8}"

    # Top-level head: (offset_entries, blobCount, offset_callData, offset_proof)
    local off_entries blob_count off_cd off_proof
    off_entries=$(_dec "$(_word "$data" 0)")
    blob_count=$(_dec "$(_word "$data" 32)")
    off_cd=$(_dec "$(_word "$data" 64)")
    off_proof=$(_dec "$(_word "$data" 96)")

    # Entry count
    local entry_count; entry_count=$(_dec "$(_word "$data" "$off_entries")")

    # Proof length
    local proof_len; proof_len=$(_dec "$(_word "$data" "$off_proof")")

    # callData (L2 block data)
    local cd_len; cd_len=$(_dec "$(_word "$data" "$off_cd")")
    local cd_hex=""
    if (( cd_len > 0 )); then
        cd_hex="${data:$(( (off_cd + 32) * 2 )):$(( cd_len * 2 ))}"
    fi

    # Decode L2 block numbers from callData
    # Flat abi.encode(uint256[], bytes[]) — no tuple wrapper.
    local -a l2_blocks=()
    if [[ -n "$cd_hex" ]] && (( ${#cd_hex} > 0 )); then
        local decoded
        decoded=$(cast decode-abi "f()(uint256[],bytes[])" "0x${cd_hex}" 2>/dev/null || echo "")
        if [[ -n "$decoded" ]]; then
            local line1; line1=$(echo "$decoded" | head -1)
            line1="${line1#\[}"; line1="${line1%\]}"
            if [[ -n "$line1" ]]; then
                IFS=',' read -ra nums <<< "$line1"
                for n in "${nums[@]}"; do
                    n=$(echo "$n" | tr -d ' ')
                    [[ -n "$n" ]] && l2_blocks+=("$n")
                done
            fi
        fi
    fi

    # ── Display header ──
    printf "  ${CYN}╭─ postBatch ──────────────────────────────────────────────────╮${R}\n"
    printf "  ${CYN}│${R} Entries: ${B}%d${R}   Blobs: %d   Proof: %d bytes\n" \
        "$entry_count" "$blob_count" "$proof_len"
    if (( ${#l2_blocks[@]} > 0 )); then
        printf "  ${CYN}│${R} L2 Blocks: ${GRN}%s${R}\n" "${l2_blocks[*]}"
    fi
    printf "  ${CYN}│${R}\n"

    # ── Decode each entry from raw ABI ──
    # entries array: at off_entries, first word = length, then N offsets, then data
    local entries_base=$(( off_entries + 32 ))  # byte position of offset array

    for (( idx=0; idx < entry_count; idx++ )); do
        # Read offset to entry[idx] (relative to entries_base)
        local entry_rel_off; entry_rel_off=$(_dec "$(_word "$data" $(( entries_base + idx * 32 )))")
        local entry_abs=$(( entries_base + entry_rel_off ))

        # Entry head: (offset_stateDeltas, actionHash, offset_nextAction)
        local sd_rel_off; sd_rel_off=$(_dec "$(_word "$data" "$entry_abs")")
        local action_hash; action_hash=$(_word "$data" $(( entry_abs + 32 )))
        local na_rel_off; na_rel_off=$(_dec "$(_word "$data" $(( entry_abs + 64 )))")

        # Classify
        local entry_type
        if [[ "$action_hash" == "0000000000000000000000000000000000000000000000000000000000000000" ]]; then
            entry_type="${GRN}IMMEDIATE${R}"
        else
            entry_type="${YLW}DEFERRED${R}"
        fi

        printf "  ${CYN}│${R} ${B}Entry %d${R} [%b]\n" "$idx" "$entry_type"
        if [[ "$action_hash" != "0000000000000000000000000000000000000000000000000000000000000000" ]]; then
            printf "  ${CYN}│${R}   actionHash: ${D}%s${R}\n" "$(_short "0x$action_hash")"
        fi

        # ── State deltas ──
        local sd_abs=$(( entry_abs + sd_rel_off ))
        local delta_count; delta_count=$(_dec "$(_word "$data" "$sd_abs")")

        for (( d=0; d < delta_count; d++ )); do
            local d_start=$(( sd_abs + 32 + d * 128 ))
            local rid; rid=$(_dec "$(_word "$data" "$d_start")")
            local cur_state; cur_state=$(_word "$data" $(( d_start + 32 )))
            local new_state; new_state=$(_word "$data" $(( d_start + 64 )))
            local ether_d; ether_d=$(_word "$data" $(( d_start + 96 )))

            local rid_label=""
            (( rid == 0 )) && rid_label=" (L1)"
            (( rid > 0 ))  && rid_label=" (L2)"

            printf "  ${CYN}│${R}   Delta #%d: rollup=%d%s\n" "$d" "$rid" "$rid_label"
            printf "  ${CYN}│${R}     state: ${D}%s${R} → ${D}%s${R}\n" \
                "$(_short "0x$cur_state")" "$(_short "0x$new_state")"
            local ed; ed=$(_ether_delta "$ether_d")
            if [[ "$ed" != "0" ]]; then
                printf "  ${CYN}│${R}     ether: ${MAG}%s${R}\n" "$ed"
            fi
        done

        # ── Next action ──
        local na_abs=$(( entry_abs + na_rel_off ))
        # Action struct head: (actionType, rollupId, destination, value,
        #   offset_data, failed, sourceAddress, sourceRollup, offset_scope)
        local atype; atype=$(_dec "$(_word "$data" "$na_abs")")
        local a_rid; a_rid=$(_dec "$(_word "$data" $(( na_abs + 32 )))")
        local a_dest; a_dest=$(_addr "$(_word "$data" $(( na_abs + 64 )))")
        local a_val; a_val=$(_dec "$(_word "$data" $(( na_abs + 96 )))")
        local a_failed; a_failed=$(_dec "$(_word "$data" $(( na_abs + 160 )))")
        local a_src; a_src=$(_addr "$(_word "$data" $(( na_abs + 192 )))")
        local a_src_rid; a_src_rid=$(_dec "$(_word "$data" $(( na_abs + 224 )))")

        # Scope
        local scope_rel; scope_rel=$(_dec "$(_word "$data" $(( na_abs + 256 )))")
        local scope_abs=$(( na_abs + scope_rel ))
        local scope_len; scope_len=$(_dec "$(_word "$data" "$scope_abs")")
        local scope_str=""
        if (( scope_len > 0 )); then
            scope_str="["
            for (( s=0; s < scope_len; s++ )); do
                local sv; sv=$(_dec "$(_word "$data" $(( scope_abs + 32 + s * 32 )))")
                (( s > 0 )) && scope_str+=","
                scope_str+="$sv"
            done
            scope_str+="]"
        fi

        # Action data (bytes)
        local data_rel; data_rel=$(_dec "$(_word "$data" $(( na_abs + 128 )))")
        local data_abs=$(( na_abs + data_rel ))
        local data_len; data_len=$(_dec "$(_word "$data" "$data_abs")")
        local action_data_hex=""
        if (( data_len > 0 )); then
            action_data_hex="${data:$(( (data_abs + 32) * 2 )):$(( data_len * 2 ))}"
        fi

        local atype_name; atype_name=$(_atype "$atype")
        local dest_label; dest_label=$(_label "$a_dest")
        printf "  ${CYN}│${R}   Action: ${B}%s${R} → %s%s (rollup %d)\n" \
            "$atype_name" "$a_dest" "$dest_label" "$a_rid"

        if (( a_val > 0 )); then
            local val_eth; val_eth=$(cast from-wei "$a_val" 2>/dev/null || echo "$a_val wei")
            printf "  ${CYN}│${R}     value: %s ETH\n" "$val_eth"
        fi

        if [[ "$atype_name" == "CALL" || "$atype_name" == "L2TX" ]]; then
            local src_label; src_label=$(_label "$a_src")
            printf "  ${CYN}│${R}     from: %s%s (rollup %d)\n" "$a_src" "$src_label" "$a_src_rid"
        fi

        if [[ -n "$scope_str" ]]; then
            printf "  ${CYN}│${R}     scope: %s\n" "$scope_str"
        fi

        if (( a_failed == 1 )); then
            printf "  ${CYN}│${R}     ${RED}failed: true${R}\n"
        fi

        if (( data_len > 0 )); then
            local sel4=""
            if (( data_len >= 4 )); then
                sel4="0x${action_data_hex:0:8}"
                local fn; fn=$(_fname "$sel4")
                printf "  ${CYN}│${R}     data: %d bytes (selector: %s = %s)\n" "$data_len" "$sel4" "$fn"
            else
                printf "  ${CYN}│${R}     data: %d bytes\n" "$data_len"
            fi
        elif [[ "$atype_name" == "RESULT" ]]; then
            printf "  ${CYN}│${R}     data: ${D}(empty — void return)${R}\n"
        fi

        printf "  ${CYN}│${R}\n"
    done

    # ── Receipt events ──
    local receipt_json
    receipt_json=$(cast receipt "$tx_hash" --json --rpc-url "$L1_RPC" 2>/dev/null || echo "{}")
    local log_count
    log_count=$(echo "$receipt_json" | jq '.logs | length' 2>/dev/null || echo "0")

    if (( log_count > 0 )); then
        printf "  ${CYN}│${R} ${D}Events (%d logs):${R}\n" "$log_count"
        for (( li=0; li < log_count; li++ )); do
            local topic0
            topic0=$(echo "$receipt_json" | jq -r ".logs[$li].topics[0] // \"\"" 2>/dev/null)
            local evt_name
            evt_name=$(_event_name "$receipt_json" "$li" "$topic0")
            printf "  ${CYN}│${R}   [%d] %s\n" "$li" "$evt_name"
        done
    fi

    printf "  ${CYN}╰──────────────────────────────────────────────────────────────╯${R}\n"

    # ── L2 block details ──
    if (( ${#l2_blocks[@]} > 0 )); then
        printf "\n"
        printf "  ${BLU}═══ L2 Block Details ═══${R}\n"
        for bn in "${l2_blocks[@]}"; do
            show_l2_block "$bn"
        done
    fi
}

# ── createCrossChainProxy decoder ──────────────────────────────
decode_create_proxy() {
    local input="$1"
    local decoded
    decoded=$(cast calldata-decode "createCrossChainProxy(address,uint256)" "$input" 2>/dev/null || echo "")
    if [[ -n "$decoded" ]]; then
        local orig_addr; orig_addr=$(echo "$decoded" | sed -n '1p')
        local orig_rid; orig_rid=$(echo "$decoded" | sed -n '2p')
        local label; label=$(_label "$orig_addr")
        printf "  ${CYN}╭─ createCrossChainProxy ──╮${R}\n"
        printf "  ${CYN}│${R} Original: %s%s\n" "$orig_addr" "$label"
        printf "  ${CYN}│${R} RollupId: %s\n" "$orig_rid"
        printf "  ${CYN}╰──────────────────────────╯${R}\n"
    fi
}

# ── L2 block display ──────────────────────────────────────────
show_l2_block() {
    local bn="$1"
    local block_json
    block_json=$(cast block "$bn" --json --rpc-url "$L2_RPC" 2>/dev/null || echo "")
    if [[ -z "$block_json" || "$block_json" == "null" ]]; then
        printf "  ${D}L2 Block #%s: not found on L2 RPC${R}\n" "$bn"
        return
    fi

    local hash state_root timestamp gas_used tx_count
    hash=$(echo "$block_json" | jq -r '.hash // "?"')
    state_root=$(echo "$block_json" | jq -r '.stateRoot // "?"')
    timestamp=$(echo "$block_json" | jq -r '.timestamp // "0"')
    gas_used=$(echo "$block_json" | jq -r '.gasUsed // "0"')

    # Transactions
    local txs_raw
    txs_raw=$(echo "$block_json" | jq -r '.transactions // [] | if type == "array" then . else [] end | .[]' 2>/dev/null)
    tx_count=$(echo "$block_json" | jq '.transactions | length' 2>/dev/null || echo "0")

    local ts_dec; ts_dec=$(_h2d "$timestamp")
    local ts_fmt; ts_fmt=$(_ts "$ts_dec")
    local gas_dec; gas_dec=$(_h2d "$gas_used")

    printf "\n  ${B}L2 Block #%s${R}\n" "$bn"
    printf "    Hash:       ${D}%s${R}\n" "$hash"
    printf "    StateRoot:  ${D}%s${R}\n" "$state_root"
    printf "    Timestamp:  %s (%s)\n" "$ts_dec" "$ts_fmt"
    printf "    Gas Used:   %'d\n" "$gas_dec"
    printf "    Txs:        %s\n" "$tx_count"

    if (( tx_count > 0 )); then
        local ti=0
        while IFS= read -r tx_hash; do
            [[ -z "$tx_hash" ]] && continue
            local tx_json
            tx_json=$(cast tx "$tx_hash" --json --rpc-url "$L2_RPC" 2>/dev/null || echo "{}")
            local from to value_hex input_data
            from=$(echo "$tx_json" | jq -r '.from // "?"')
            to=$(echo "$tx_json" | jq -r '.to // "null"')
            value_hex=$(echo "$tx_json" | jq -r '.value // "0x0"')
            input_data=$(echo "$tx_json" | jq -r '.input // "0x"')

            local sel=""
            local fn="(no data)"
            if [[ ${#input_data} -ge 10 ]]; then
                sel="${input_data:0:10}"
                fn=$(_fname "$sel")
            fi

            local to_label=""
            [[ "$to" != "null" ]] && to_label=$(_label "$to")

            # Get receipt status
            local status_str=""
            local rcpt_json
            rcpt_json=$(cast receipt "$tx_hash" --json --rpc-url "$L2_RPC" 2>/dev/null || echo "{}")
            local status; status=$(echo "$rcpt_json" | jq -r '.status // "?"')
            if [[ "$status" == "0x1" || "$status" == "1" ]]; then
                status_str="${GRN}✓${R}"
            elif [[ "$status" == "0x0" || "$status" == "0" ]]; then
                status_str="${RED}✗${R}"
            fi

            local val_dec; val_dec=$(_h2d "$value_hex")
            local val_str=""
            if (( val_dec > 0 )); then
                val_str=" value=$(cast from-wei "$val_dec" 2>/dev/null || echo "$val_dec")ETH"
            fi

            local to_display
            if [[ "$to" == "null" || -z "$to" ]]; then
                to_display="CREATE"
            else
                to_display="$(_short "$to")"
            fi
            printf "      [%d] %b %s → %s%s  %s%s\n" \
                "$ti" "$status_str" "$(_short "$from")" \
                "$to_display" "$to_label" "$fn" "$val_str"
            ti=$((ti + 1))
        done <<< "$txs_raw"
    fi
}

# ── Single L1 block inspector ─────────────────────────────────
inspect_block() {
    local block_num="$1"
    local block_json
    block_json=$(cast block "$block_num" --json --rpc-url "$L1_RPC" 2>/dev/null || echo "")
    if [[ -z "$block_json" || "$block_json" == "null" ]]; then
        printf "${RED}Block %s not found on %s${R}\n" "$block_num" "$L1_RPC"
        return 1
    fi

    local hash parent ts gas_used gas_limit base_fee
    hash=$(echo "$block_json" | jq -r '.hash // "?"')
    parent=$(echo "$block_json" | jq -r '.parentHash // "?"')
    ts=$(echo "$block_json" | jq -r '.timestamp // "0"')
    gas_used=$(echo "$block_json" | jq -r '.gasUsed // "0"')
    gas_limit=$(echo "$block_json" | jq -r '.gasLimit // "0"')
    base_fee=$(echo "$block_json" | jq -r '.baseFeePerGas // "0"')

    # Convert hex values to decimal
    local ts_dec; ts_dec=$(_h2d "$ts")
    gas_used=$(_h2d "$gas_used")
    gas_limit=$(_h2d "$gas_limit")
    base_fee=$(_h2d "$base_fee")

    # Transaction hashes
    local txs_raw
    txs_raw=$(echo "$block_json" | jq -r '.transactions[]? // empty' 2>/dev/null || echo "")
    local tx_count
    tx_count=$(echo "$block_json" | jq '.transactions | length' 2>/dev/null || echo "0")

    local gas_pct="0"
    if (( gas_limit > 0 )); then
        gas_pct=$(( gas_used * 100 / gas_limit ))
    fi

    printf "\n${BLU}╔══════════════════════════════════════════════════════════════════╗${R}\n"
    printf "${BLU}║${R}  ${B}L1 Block #%s${R}%*s${BLU}║${R}\n" "$block_num" $((54 - ${#block_num})) ""
    printf "${BLU}╚══════════════════════════════════════════════════════════════════╝${R}\n"

    printf "  Hash:       ${D}%s${R}\n" "$hash"
    printf "  Parent:     ${D}%s${R}\n" "$parent"
    printf "  Timestamp:  %s  (%s)\n" "$ts_dec" "$(_ts "$ts_dec")"
    printf "  Gas:        %'d / %'d (%s%%)\n" "$gas_used" "$gas_limit" "$gas_pct"
    printf "  BaseFee:    %s wei\n" "$base_fee"
    printf "  Txs:        ${B}%s${R}\n" "$tx_count"

    if (( tx_count == 0 )); then
        printf "\n  ${D}(empty block — no transactions)${R}\n"
        return 0
    fi

    printf "\n"

    # ── Process each transaction ──
    local ti=0
    while IFS= read -r tx_hash; do
        [[ -z "$tx_hash" ]] && continue

        local tx_json rcpt_json
        tx_json=$(cast tx "$tx_hash" --json --rpc-url "$L1_RPC" 2>/dev/null || echo "{}")
        rcpt_json=$(cast receipt "$tx_hash" --json --rpc-url "$L1_RPC" 2>/dev/null || echo "{}")

        local from to value_hex input_data status gas_tx
        from=$(echo "$tx_json" | jq -r '.from // "?"')
        to=$(echo "$tx_json" | jq -r '.to // "null"')
        value_hex=$(echo "$tx_json" | jq -r '.value // "0x0"')
        input_data=$(echo "$tx_json" | jq -r '.input // "0x"')
        status=$(echo "$rcpt_json" | jq -r '.status // "?"')
        gas_tx=$(_h2d "$(echo "$rcpt_json" | jq -r '.gasUsed // "0"')")

        local sel="0x"
        [[ ${#input_data} -ge 10 ]] && sel="${input_data:0:10}"
        local fn; fn=$(_fname "$sel")

        local status_str
        if [[ "$status" == "0x1" || "$status" == "1" ]]; then
            status_str="${GRN}✓ SUCCESS${R}"
        elif [[ "$status" == "0x0" || "$status" == "0" ]]; then
            status_str="${RED}✗ REVERTED${R}"
        else
            status_str="${YLW}? $status${R}"
        fi

        local from_label; from_label=$(_label "$from")
        local to_label=""
        [[ "$to" != "null" ]] && to_label=$(_label "$to")

        printf "  ${B}──── TX %d: %s [%b] ────${R}\n" "$ti" "$fn" "$status_str"
        printf "    Hash:  ${D}%s${R}\n" "$tx_hash"
        printf "    From:  %s%s\n" "$from" "$from_label"
        if [[ "$to" != "null" ]]; then
            printf "    To:    %s%s\n" "$to" "$to_label"
        else
            printf "    To:    ${D}(contract creation)${R}\n"
        fi

        local val_dec; val_dec=$(_h2d "$value_hex")
        if (( val_dec > 0 )); then
            local val_eth; val_eth=$(cast from-wei "$val_dec" 2>/dev/null || echo "$val_dec wei")
            printf "    Value: %s ETH\n" "$val_eth"
        fi
        printf "    Gas:   %'d\n" "$gas_tx"
        printf "\n"

        # ── Dispatch to specific decoders ──
        case "$sel" in
            "$SEL_POSTBATCH")
                decode_postbatch "$input_data" "$tx_hash"
                ;;
            "$SEL_CREATE_PROXY")
                decode_create_proxy "$input_data"
                ;;
            "$SEL_EXEC_CC"|"$SEL_EXEC_BEHALF")
                _show_tx_events "$rcpt_json"
                ;;
            *)
                # For unknown selectors on proxy addresses, likely a trigger
                if [[ "$to" != "null" && "$to" != "${ROLLUPS_ADDRESS:-}" ]]; then
                    if [[ "$status" == "0x0" || "$status" == "0" ]]; then
                        printf "  ${RED}⚠ Possibly failed trigger. Debug with:${R}\n"
                        printf "    cast run %s --rpc-url %s\n" "$tx_hash" "$L1_RPC"
                    fi
                    _show_tx_events "$rcpt_json"
                fi
                ;;
        esac

        printf "\n"
        ti=$((ti + 1))
    done <<< "$txs_raw"

    # ── On-chain rollup state at this block ──
    if [[ -n "$ROLLUPS_ADDRESS" && -n "$ROLLUP_ID" ]]; then
        printf "  ${BLU}═══ Rollup State (rollup %s at L1 block %s) ═══${R}\n" "$ROLLUP_ID" "$block_num"
        local state_raw
        state_raw=$(cast call --rpc-url "$L1_RPC" --block "$block_num" "$ROLLUPS_ADDRESS" \
            "rollups(uint256)((address,bytes32,bytes32,uint256))" "$ROLLUP_ID" 2>/dev/null || echo "")
        if [[ -n "$state_raw" ]]; then
            # Parse tuple: (owner, verificationKey, stateRoot, etherBalance)
            local owner vk sr bal
            owner=$(echo "$state_raw" | grep -oP '0x[0-9a-fA-F]{40}' | head -1 || echo "?")
            sr=$(echo "$state_raw" | grep -oP '0x[0-9a-fA-F]{64}' | tail -1 || echo "?")
            # Extract ether balance — last numeric value
            bal=$(echo "$state_raw" | grep -oP '\d{10,}' | tail -1 || echo "0")
            local bal_eth; bal_eth=$(cast from-wei "$bal" 2>/dev/null || echo "?")
            printf "    Owner:      %s%s\n" "$owner" "$(_label "$owner")"
            printf "    State Root: ${D}%s${R}\n" "$sr"
            printf "    Deposited:  %s ETH (%s wei)\n" "$bal_eth" "$bal"
        else
            printf "    ${D}(could not query rollup state)${R}\n"
        fi
    fi
}

# ── Main ───────────────────────────────────────────────────────

printf "${BLU}╔══════════════════════════════════════════════════════════════════╗${R}\n"
printf "${BLU}║${R}  ${B}L1 Block Inspector${R}                                              ${BLU}║${R}\n"
printf "${BLU}║${R}  L1: %-42s                  ${BLU}║${R}\n" "$L1_RPC"
printf "${BLU}║${R}  L2: %-42s                  ${BLU}║${R}\n" "$L2_RPC"
if [[ -n "$ROLLUPS_ADDRESS" ]]; then
    printf "${BLU}║${R}  Rollups: %-38s             ${BLU}║${R}\n" "$ROLLUPS_ADDRESS"
fi
printf "${BLU}╚══════════════════════════════════════════════════════════════════╝${R}\n"

# Resolve block range
if [[ "$BLOCK_ARG" == "latest" ]]; then
    BLOCK_ARG=$(cast block-number --rpc-url "$L1_RPC" 2>/dev/null)
    printf "\n${D}Latest L1 block: %s${R}\n" "$BLOCK_ARG"
fi

if [[ "$BLOCK_ARG" == *-* ]]; then
    FROM="${BLOCK_ARG%%-*}"
    TO="${BLOCK_ARG##*-}"
    for (( b=FROM; b<=TO; b++ )); do
        inspect_block "$b"
    done
else
    inspect_block "$BLOCK_ARG"
fi

# ── L2 head for context ──
printf "\n${D}─────────────────────────────────────────────────────${R}\n"
L2_HEAD=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "?")
printf "L2 head: %s\n" "$L2_HEAD"

# Builder health
HEALTH=$(curl -s --max-time 3 http://localhost:9560/health 2>/dev/null || echo "(unreachable)")
printf "Builder: %s\n" "$HEALTH"
printf "${D}─────────────────────────────────────────────────────${R}\n"
