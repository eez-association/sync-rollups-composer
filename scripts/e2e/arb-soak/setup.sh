#!/usr/bin/env bash
# setup.sh — Deploy the arb-soak stack (adapted from PR #33's deploy_arb_eez.sh).
#
# Deploys on a running devnet or testnet:
#   - L1 MockERC20 WETH + USDC (mints liquidity to the funder)
#   - L1 SimpleAMM seeded 100 WETH / 300k USDC
#   - Bridges wrap to discover L2 token addresses
#   - L2 SimpleAMM seeded with bridged liquidity
#   - L2Executor + its cross-chain proxy on L1
#   - Two CrossChainArb contracts (one per bot operator), each funded 1 WETH
#   - Writes /tmp/arb_config.json and /tmp/arb_config_2.json
#
# Endpoints are taken from lib-health-check.sh. Funder is dev#9 per CLAUDE.md.
# Bot operator keys are unique to this harness; they are NOT in the CLAUDE.md
# shared dev#N allocation, so this script is safe to run concurrently with
# the existing E2E tests (bridge-health-check, crosschain-health-check, etc.).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../lib-health-check.sh
source "$SCRIPT_DIR/../lib-health-check.sh"

CONTRACTS_DIR="$(cd "$SCRIPT_DIR/../../../contracts/test-multi-call" && pwd)"

# Auto-load contract addresses from the running builder's /shared/rollup.env
# (same pattern as bridge-health-check.sh:51-58). Falls back to testnet
# defaults if neither the shared volume nor the builder container is reachable.
if [ -z "${BRIDGE_L2_ADDRESS:-}" ]; then
    if [ -f "/shared/rollup.env" ]; then
        eval "$(cat /shared/rollup.env)"
    elif [ -n "${SHARED_DIR:-}" ] && [ -f "${SHARED_DIR}/rollup.env" ]; then
        eval "$(cat "${SHARED_DIR}/rollup.env")"
    else
        # Try both devnet and testnet builder containers.
        for CTR in devnet-eez-builder-1 testnet-eez-builder-1; do
            env=$(sudo docker exec "$CTR" cat /shared/rollup.env 2>/dev/null || true)
            if [ -n "$env" ]; then eval "$env"; break; fi
        done
    fi
fi
ROLLUPS="${ROLLUPS_ADDRESS:-0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512}"
BRIDGE="${BRIDGE_ADDRESS:-0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9}"
BRIDGE_L2="${BRIDGE_L2_ADDRESS:-0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0}"
ROLLUP_ID="${ROLLUP_ID:-1}"

FUNDER_KEY="${FUNDER_KEY:-0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6}"  # dev#9
FUNDER="$(cast wallet address --private-key "$FUNDER_KEY")"
if [ "${FUNDER,,}" = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266" ]; then
    printf '\033[0;31mREFUSING to run with dev#0 (builder/deployer key) as funder — this halts L2.\033[0m\n' >&2
    exit 1
fi

# Bot operator keys — unique to this harness, not in CLAUDE.md's dev#N allocation.
BOT1_KEY="${BOT1_KEY:-0x4fff4c1f39910ff9722e4257a8ae58f92b93b5e31ecad4c74d0628b97ec793d3}"
BOT1="$(cast wallet address --private-key "$BOT1_KEY")"
BOT2_KEY="${BOT2_KEY:-0xeaa861a9a01391ed3d587d8a5a84ca56ee277629a8b02c22093a419bf240e65d}"
BOT2="$(cast wallet address --private-key "$BOT2_KEY")"

CO="--legacy --gas-price 1000000000"
CO_LONG="--legacy --gas-price 1000000000 --timeout 300"

LIQ_WETH="100000000000000000000"     # 100 WETH
LIQ_USDC="300000000000"              # 300k USDC (6 decimals)
ARB_CAPITAL="1000000000000000000"    # 1 WETH per bot
DEV_FUND_ETH="10000000000000000000"  # 10 ETH gas money per bot
MINT_WETH="400000000000000000000"    # 400 WETH minted to funder
MINT_USDC="900000000000"             # 900k USDC minted to funder

blue()  { printf '\033[0;34m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
red()   { printf '\033[0;31m%s\033[0m\n' "$*" >&2; }

fcreate() {
    local rpc="$1" key="$2" contract="$3"; shift 3
    forge create $CO --rpc-url "$rpc" --private-key "$key" --broadcast \
        --root "$CONTRACTS_DIR" "$contract" "$@" 2>&1 \
        | grep "Deployed to:" | awk '{print $3}'
}

send() {
    local rpc="$1" key="$2" to="$3" sig="$4"; shift 4
    cast send $CO --rpc-url "$rpc" --private-key "$key" "$to" "$sig" "$@" \
        --gas-limit 1500000 > /dev/null
}

blue "=== arb-soak setup ==="
blue "  L1_RPC    = $L1_RPC"
blue "  L1_PROXY  = $L1_PROXY"
blue "  L2_RPC    = $L2_RPC"
blue "  HEALTH    = $HEALTH_URL"

blue "=== Compile contracts ==="
(cd "$CONTRACTS_DIR" && forge build > /dev/null 2>&1) || { red "compile failed"; exit 1; }
green "  ok"

blue "=== Fund bot operators ==="
for op in "$BOT1" "$BOT2"; do
    cast send $CO --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        --value "$DEV_FUND_ETH" "$op" --gas-limit 50000 > /dev/null
    green "  $op funded 10 ETH on L1"
done

blue "=== Deploy L1 tokens ==="
WETH_L1=$(fcreate "$L1_RPC" "$FUNDER_KEY" "src/MockERC20.sol:MockERC20" \
    --constructor-args "Wrapped Ether" "WETH" 18)
USDC_L1=$(fcreate "$L1_RPC" "$FUNDER_KEY" "src/MockERC20.sol:MockERC20" \
    --constructor-args "USD Coin" "USDC" 6)
[ -n "$WETH_L1" ] && [ -n "$USDC_L1" ] || { red "token deploy failed"; exit 1; }
green "  WETH_L1: $WETH_L1"
green "  USDC_L1: $USDC_L1"

blue "=== Mint L1 tokens to funder ==="
send "$L1_RPC" "$FUNDER_KEY" "$WETH_L1" "mint(address,uint256)" "$FUNDER" "$MINT_WETH"
send "$L1_RPC" "$FUNDER_KEY" "$USDC_L1" "mint(address,uint256)" "$FUNDER" "$MINT_USDC"
green "  minted 400 WETH / 900k USDC"

blue "=== Deploy L1 SimpleAMM and seed liquidity ==="
AMM_L1=$(fcreate "$L1_RPC" "$FUNDER_KEY" "src/SimpleAMM.sol:SimpleAMM" \
    --constructor-args "$WETH_L1" "$USDC_L1")
[ -n "$AMM_L1" ] || { red "AMM_L1 deploy failed"; exit 1; }
send "$L1_RPC" "$FUNDER_KEY" "$WETH_L1" "approve(address,uint256)" "$AMM_L1" "$LIQ_WETH"
send "$L1_RPC" "$FUNDER_KEY" "$USDC_L1" "approve(address,uint256)" "$AMM_L1" "$LIQ_USDC"
send "$L1_RPC" "$FUNDER_KEY" "$AMM_L1" "addLiquidity(uint256,uint256)" "$LIQ_WETH" "$LIQ_USDC"
green "  AMM_L1: $AMM_L1  seeded 100 WETH / 300k USDC"

blue "=== Bridge WETH and USDC to L2 ==="
send "$L1_RPC" "$FUNDER_KEY" "$WETH_L1" "approve(address,uint256)" "$BRIDGE" "$LIQ_WETH"
send "$L1_RPC" "$FUNDER_KEY" "$USDC_L1" "approve(address,uint256)" "$BRIDGE" "$LIQ_USDC"
cast send $CO_LONG --rpc-url "$L1_PROXY" --private-key "$FUNDER_KEY" "$BRIDGE" \
    "bridgeTokens(address,uint256,uint256,address)" "$WETH_L1" "$LIQ_WETH" "$ROLLUP_ID" "$FUNDER" \
    --gas-limit 1500000 > /dev/null
cast send $CO_LONG --rpc-url "$L1_PROXY" --private-key "$FUNDER_KEY" "$BRIDGE" \
    "bridgeTokens(address,uint256,uint256,address)" "$USDC_L1" "$LIQ_USDC" "$ROLLUP_ID" "$FUNDER" \
    --gas-limit 1500000 > /dev/null
green "  bridge txs sent"

blue "=== Bridge ETH to funder on L2 (for gas to deploy L2 contracts) ==="
# Without this, `forge create` on L2 fails with "gas required exceeds allowance".
# dev#9 has no L2 ETH by default; bridge a small amount before L2 deploys.
L2_GAS_ETH="5000000000000000000"  # 5 ETH
FUNDER_L2_BAL=$(cast balance --rpc-url "$L2_RPC" "$FUNDER" 2>/dev/null || echo "0")
if [ "${FUNDER_L2_BAL:-0}" -lt "$L2_GAS_ETH" ] 2>/dev/null; then
    cast send $CO --rpc-url "$L1_PROXY" --private-key "$FUNDER_KEY" \
        --value "$L2_GAS_ETH" --gas-limit 1500000 \
        "$BRIDGE" "bridgeEther(uint256,address)" "$ROLLUP_ID" "$FUNDER" > /dev/null
    green "  bridged 5 ETH to dev#9 on L2"
else
    green "  funder already has $((FUNDER_L2_BAL / 1000000000000000000)) ETH on L2"
fi

blue "=== Wait for L2 wrapped-token delivery ==="
WETH_L2=""
USDC_L2=""
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    sleep 3
    WETH_L2=$(cast call --rpc-url "$L2_RPC" "$BRIDGE_L2" \
        "getWrappedToken(address,uint256)(address)" "$WETH_L1" 0 2>/dev/null || true)
    USDC_L2=$(cast call --rpc-url "$L2_RPC" "$BRIDGE_L2" \
        "getWrappedToken(address,uint256)(address)" "$USDC_L1" 0 2>/dev/null || true)
    if [ -n "$WETH_L2" ] && [ -n "$USDC_L2" ] \
       && [ "$WETH_L2" != "0x0000000000000000000000000000000000000000" ] \
       && [ "$USDC_L2" != "0x0000000000000000000000000000000000000000" ]; then
        wbal=$(cast call --rpc-url "$L2_RPC" "$WETH_L2" "balanceOf(address)(uint256)" "$FUNDER" 2>/dev/null | awk '{print $1}')
        ubal=$(cast call --rpc-url "$L2_RPC" "$USDC_L2" "balanceOf(address)(uint256)" "$FUNDER" 2>/dev/null | awk '{print $1}')
        if [ "${wbal:-0}" -ge "$LIQ_WETH" ] 2>/dev/null && [ "${ubal:-0}" -ge "$LIQ_USDC" ] 2>/dev/null; then
            break
        fi
    fi
    blue "  waiting for bridge delivery... ($i/15)"
done
[ -n "$WETH_L2" ] && [ "$WETH_L2" != "0x0000000000000000000000000000000000000000" ] || { red "WETH_L2 not delivered"; exit 1; }
[ -n "$USDC_L2" ] && [ "$USDC_L2" != "0x0000000000000000000000000000000000000000" ] || { red "USDC_L2 not delivered"; exit 1; }
green "  WETH_L2: $WETH_L2"
green "  USDC_L2: $USDC_L2"

blue "=== Deploy L2 SimpleAMM and seed liquidity ==="
AMM_L2=$(fcreate "$L2_RPC" "$FUNDER_KEY" "src/SimpleAMM.sol:SimpleAMM" \
    --constructor-args "$WETH_L2" "$USDC_L2")
[ -n "$AMM_L2" ] || { red "AMM_L2 deploy failed"; exit 1; }
send "$L2_RPC" "$FUNDER_KEY" "$WETH_L2" "approve(address,uint256)" "$AMM_L2" "$LIQ_WETH"
send "$L2_RPC" "$FUNDER_KEY" "$USDC_L2" "approve(address,uint256)" "$AMM_L2" "$LIQ_USDC"
send "$L2_RPC" "$FUNDER_KEY" "$AMM_L2" "addLiquidity(uint256,uint256)" "$LIQ_WETH" "$LIQ_USDC"
green "  AMM_L2: $AMM_L2"

blue "=== Deploy L2Executor + its cross-chain proxy ==="
L2_EXEC=$(fcreate "$L2_RPC" "$FUNDER_KEY" "src/L2Executor.sol:L2Executor" \
    --constructor-args "$AMM_L2" "$BRIDGE_L2" "$WETH_L2" "$USDC_L2")
[ -n "$L2_EXEC" ] || { red "L2Executor deploy failed"; exit 1; }
send "$L1_RPC" "$FUNDER_KEY" "$ROLLUPS" "createCrossChainProxy(address,uint256)" "$L2_EXEC" "$ROLLUP_ID"
L2_EXEC_PROXY=$(cast call --rpc-url "$L1_RPC" "$ROLLUPS" \
    "computeCrossChainProxyAddress(address,uint256)(address)" "$L2_EXEC" "$ROLLUP_ID" 2>/dev/null)
green "  L2_EXEC: $L2_EXEC"
green "  L2_EXEC_PROXY: $L2_EXEC_PROXY"

blue "=== Deploy CrossChainArb contracts ==="
ARB1=$(fcreate "$L1_RPC" "$BOT1_KEY" "src/CrossChainArb.sol:CrossChainArb" \
    --constructor-args "$WETH_L1" "$USDC_L1" "$AMM_L1" "$BRIDGE" "$ROLLUP_ID")
[ -n "$ARB1" ] || { red "ARB1 deploy failed"; exit 1; }
send "$L1_RPC" "$BOT1_KEY" "$ARB1" "setL2Executor(address,address)" "$L2_EXEC" "$L2_EXEC_PROXY"
green "  ARB1: $ARB1"
ARB2=$(fcreate "$L1_RPC" "$BOT2_KEY" "src/CrossChainArb.sol:CrossChainArb" \
    --constructor-args "$WETH_L1" "$USDC_L1" "$AMM_L1" "$BRIDGE" "$ROLLUP_ID")
[ -n "$ARB2" ] || { red "ARB2 deploy failed"; exit 1; }
send "$L1_RPC" "$BOT2_KEY" "$ARB2" "setL2Executor(address,address)" "$L2_EXEC" "$L2_EXEC_PROXY"
green "  ARB2: $ARB2"

blue "=== Fund arb contracts with WETH working capital ==="
send "$L1_RPC" "$FUNDER_KEY" "$WETH_L1" "mint(address,uint256)" "$ARB1" "$ARB_CAPITAL"
send "$L1_RPC" "$FUNDER_KEY" "$WETH_L1" "mint(address,uint256)" "$ARB2" "$ARB_CAPITAL"
green "  each arb funded with 1 WETH"

blue "=== Write configs ==="
write_cfg() {
    local path="$1" name="$2" key="$3" addr="$4" arb="$5" log="$6" hb="$7" pid="$8"
    cat > "$path" <<EOF
{
  "l1_rpc": "$L1_RPC",
  "l1_proxy": "$L1_PROXY",
  "l2_rpc": "$L2_RPC",
  "weth_l1": "$WETH_L1",
  "usdc_l1": "$USDC_L1",
  "amm_l1": "$AMM_L1",
  "weth_l2": "$WETH_L2",
  "usdc_l2": "$USDC_L2",
  "amm_l2": "$AMM_L2",
  "bridge": "$BRIDGE",
  "l2_executor": "$L2_EXEC",
  "l2_executor_proxy": "$L2_EXEC_PROXY",
  "bot_name": "$name",
  "test_key": "$key",
  "test_addr": "$addr",
  "arb_contract": "$arb",
  "log_file": "$log",
  "heartbeat_file": "$hb",
  "pid_file": "$pid"
}
EOF
}
write_cfg /tmp/arb_config.json   bot1 "$BOT1_KEY" "$BOT1" "$ARB1" /tmp/arb_bot.log  /tmp/arb_bot_heartbeat.log  /tmp/arb_bot.pid
write_cfg /tmp/arb_config_2.json bot2 "$BOT2_KEY" "$BOT2" "$ARB2" /tmp/arb_bot2.log /tmp/arb_bot2_heartbeat.log /tmp/arb_bot2.pid
green "  /tmp/arb_config.json"
green "  /tmp/arb_config_2.json"

green ""
green "=== Deploy complete ==="
echo "ARB1=$ARB1  ARB2=$ARB2"
echo "AMM_L1=$AMM_L1  AMM_L2=$AMM_L2"
echo "L2_EXEC=$L2_EXEC  proxy=$L2_EXEC_PROXY"
