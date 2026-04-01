#!/usr/bin/env bash
# test-sibling-scopes.sh — E2E test for sibling scopes [0] and [1]
#
# Scenario: CallTwoProxies(SCX) calls CounterA_proxy.increment() then
# CounterB_proxy.increment() sequentially. Two L2→L1 cross-chain calls
# at depth=1, producing sibling scopes [0] and [1] on L1.
#
# Steps:
#   1. Wait for builder healthy
#   2. Fund test account on L1 + L2
#   3. Deploy CounterA + CounterB on L1
#   4. Create L2 proxies for both Counters
#   5. Deploy CallTwoProxies(SCX) on L2
#   6. Call SCX.callBoth() via L2 composer RPC
#   7. Wait for L1 batch + trigger
#   8. Verify: CounterA=1 and CounterB=1 on L1
#
# Test account: dev key #10
#
# Usage: L1_RPC=http://localhost:11555 L2_RPC=http://localhost:11545 \
#        L2_PROXY=http://localhost:11548 HEALTH_URL=http://localhost:11560/health \
#        bash scripts/e2e/test-sibling-scopes.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

TEST_KEY="0xf214f2b2cd398c806f84e317254e0f0b801d0643303237d97a22a48e01628897"
TEST_ADDR="0xBcd4042DE499D14e55001CcbB24a551F3b954096"

COUNTER_BYTECODE="0x6080806040523460135760bc908160188239f35b5f80fdfe60808060405260043610156011575f80fd5b5f3560e01c90816361bc221a14606f575063d09de08a14602f575f80fd5b34606b575f366003190112606b575f545f198114605757600160209101805f55604051908152f35b634e487b7160e01b5f52601160045260245ffd5b5f80fd5b34606b575f366003190112606b576020905f548152f3fea26469706673582212203e55fa77c6759db076a4bed9ac4ffc7277fe1d5bb18130aac7ed523c51165beb64736f6c634300081c0033"

CALL_TWO_PROXIES_BYTECODE="0x608034608557601f61029238819003918201601f19168301916001600160401b038311848410176089578084926040948552833981010312608557604b6020604583609d565b9201609d565b5f80546001600160a01b039384166001600160a01b031991821617909155600180549290931691161790556040516101e190816100b18239f35b5f80fd5b634e487b7160e01b5f52604160045260245ffd5b51906001600160a01b038216820360855756fe6080806040526004361015610012575f80fd5b5f3560e01c9081634719d9ef1461009057508063a6e8a859146100685763eac3e7991461003d575f80fd5b34610064575f366003190112610064575f546040516001600160a01b039091168152602090f35b5f80fd5b34610064575f366003190112610064576001546040516001600160a01b039091168152602090f35b34610064575f366003190112610064575f805463684ef04560e11b8352602091839160049183916001600160a01b03165af1801561012c57610137575b5060015460405163684ef04560e11b815290602090829060049082905f906001600160a01b03165af1801561012c5761010257005b6101239060203d602011610125575b61011b8183610166565b81019061019c565b005b503d610111565b6040513d5f823e3d90fd5b6020813d60201161015e575b8161015060209383610166565b8101031261006457516100cd565b3d9150610143565b90601f8019910116810190811067ffffffffffffffff82111761018857604052565b634e487b7160e01b5f52604160045260245ffd5b9081602091031261006457519056fea2646970667358221220fb6db18201bd9e76617c10459bf1cfb0c616567e614e1472aa711b25192b6b9964736f6c634300081c0033"

COUNTER_SELECTOR="0x61bc221a"
CALL_BOTH_SELECTOR="0x4719d9ef"

# ── Colors ──
if [ -t 1 ]; then
  CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'
  BOLD='\033[1m'; RESET='\033[0m'
else
  CYAN=''; GREEN=''; RED=''; BOLD=''; RESET=''
fi

# ── Load rollup.env ──
echo ""
echo -e "${CYAN}========================================"
echo -e "  SIBLING SCOPES [0],[1] E2E TEST"
echo -e "========================================${RESET}"
echo ""

eval "$($DOCKER_COMPOSE_CMD exec -T builder cat /shared/rollup.env 2>/dev/null)"
CCM_L2_ADDRESS="${CROSS_CHAIN_MANAGER_ADDRESS}"
echo "CCM L2: $CCM_L2_ADDRESS | Test: $TEST_ADDR"

# ── Step 1: Wait for builder ──
echo -e "\n${BOLD}Step 1: Waiting for builder...${RESET}"
for i in $(seq 1 60); do
  HEALTH=$(curl -s "$HEALTH_URL" 2>/dev/null || echo "{}")
  if [ "$(echo "$HEALTH" | jq -r '.healthy // false')" = "true" ]; then
    echo "  Builder healthy"; break
  fi
  [ "$i" = "60" ] && { echo -e "${RED}ERROR: not healthy${RESET}"; exit 1; }
  sleep 1
done

# ── Step 2: Fund ──
echo -e "\n${BOLD}Step 2: Funding...${RESET}"
L2_BAL=$(cast balance "$TEST_ADDR" --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
if [ "$(echo "$L2_BAL" | tr -d '[:space:]')" = "0" ]; then
  cast send --private-key "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d" \
    "$TEST_ADDR" --value 1ether --rpc-url "$L2_RPC" >/dev/null 2>&1
  echo "  Funded 1 ETH on L2"
else
  echo "  L2 already funded"
fi

# ── Step 3: Deploy CounterA + CounterB on L1 ──
echo -e "\n${BOLD}Step 3: Deploying CounterA + CounterB on L1...${RESET}"
COUNTER_A=$(cast send --private-key "$TEST_KEY" --rpc-url "$L1_RPC" --create "$COUNTER_BYTECODE" --json 2>/dev/null | jq -r '.contractAddress')
COUNTER_B=$(cast send --private-key "$TEST_KEY" --rpc-url "$L1_RPC" --create "$COUNTER_BYTECODE" --json 2>/dev/null | jq -r '.contractAddress')
echo "  CounterA: $COUNTER_A"
echo "  CounterB: $COUNTER_B"

# ── Step 4: Create L2 proxies ──
echo -e "\n${BOLD}Step 4: Creating L2 proxies...${RESET}"

create_proxy() {
  local l1_addr=$1
  local proxy_addr
  proxy_addr=$(cast call "$CCM_L2_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" "$l1_addr" 0 --rpc-url "$L2_RPC" 2>/dev/null || echo "")
  local code
  code=$(cast code "$proxy_addr" --rpc-url "$L2_RPC" 2>/dev/null || echo "0x")
  if [ "$code" = "0x" ] || [ -z "$code" ]; then
    for attempt in 1 2 3; do
      cast send --private-key "$TEST_KEY" "$CCM_L2_ADDRESS" "createCrossChainProxy(address,uint256)" "$l1_addr" 0 --rpc-url "$L2_RPC" >/dev/null 2>&1
      sleep 2
      code=$(cast code "$proxy_addr" --rpc-url "$L2_RPC" 2>/dev/null || echo "0x")
      [ "$code" != "0x" ] && [ -n "$code" ] && break
    done
  fi
  echo "$proxy_addr"
}

PROXY_A=$(create_proxy "$COUNTER_A")
PROXY_B=$(create_proxy "$COUNTER_B")
echo "  ProxyA: $PROXY_A"
echo "  ProxyB: $PROXY_B"

# ── Step 5: Deploy CallTwoProxies (SCX) on L2 ──
echo -e "\n${BOLD}Step 5: Deploying CallTwoProxies (SCX) on L2...${RESET}"
PROXY_A_PAD=$(printf '%064s' "$(echo "$PROXY_A" | sed 's/0x//' | tr '[:upper:]' '[:lower:]')" | tr ' ' '0')
PROXY_B_PAD=$(printf '%064s' "$(echo "$PROXY_B" | sed 's/0x//' | tr '[:upper:]' '[:lower:]')" | tr ' ' '0')
SCX_DEPLOY="${CALL_TWO_PROXIES_BYTECODE}${PROXY_A_PAD}${PROXY_B_PAD}"
SCX_ADDR=$(cast send --private-key "$TEST_KEY" --rpc-url "$L2_RPC" --create "$SCX_DEPLOY" --json 2>/dev/null | jq -r '.contractAddress')
echo "  SCX: $SCX_ADDR"

# ── Step 6: Call SCX.callBoth() via L2 composer RPC ──
echo -e "\n${BOLD}Step 6: Calling SCX.callBoth() via L2 composer RPC...${RESET}"
echo "  SCX=$SCX_ADDR → ProxyA=$PROXY_A + ProxyB=$PROXY_B"
echo "  Expect: 2 calls at depth=1 → scope=[0] and [1]"

TX_HASH=$(cast send --private-key "$TEST_KEY" "$SCX_ADDR" "$CALL_BOTH_SELECTOR" \
  --gas-limit 500000 --rpc-url "$L2_PROXY" --json 2>/dev/null | jq -r '.transactionHash')
echo "  L2 tx: $TX_HASH"

TX_STATUS=$(cast receipt "$TX_HASH" --rpc-url "$L2_RPC" --json 2>/dev/null | jq -r '.status')
echo "  Status: $TX_STATUS"
if [ "$TX_STATUS" != "0x1" ]; then
  echo -e "${RED}FAIL: L2 tx reverted${RESET}"; exit 1
fi

# ── Step 7: Wait for L1 batch + trigger ──
echo -e "\n${BOLD}Step 7: Waiting for Counters to increment on L1...${RESET}"

MAX_WAIT=120
INTERVAL=5
ELAPSED=0

while [ $ELAPSED -lt $MAX_WAIT ]; do
  VA_RAW=$(cast call "$COUNTER_A" "$COUNTER_SELECTOR" --rpc-url "$L1_RPC" 2>/dev/null || echo "0x0")
  VB_RAW=$(cast call "$COUNTER_B" "$COUNTER_SELECTOR" --rpc-url "$L1_RPC" 2>/dev/null || echo "0x0")
  VA=$(printf "%d" "$VA_RAW" 2>/dev/null || echo "0")
  VB=$(printf "%d" "$VB_RAW" 2>/dev/null || echo "0")

  if [ "$VA" -gt 0 ] 2>/dev/null && [ "$VB" -gt 0 ] 2>/dev/null; then
    echo ""
    echo -e "${GREEN}========================================"
    echo -e "  SIBLING SCOPES TEST PASSED!"
    echo -e "  CounterA = $VA, CounterB = $VB"
    echo -e "  Scope [0] and [1] navigation worked"
    echo -e "========================================${RESET}"
    exit 0
  fi

  echo "  CounterA=$VA CounterB=$VB ($ELAPSED/${MAX_WAIT}s)"
  sleep $INTERVAL
  ELAPSED=$((ELAPSED + INTERVAL))
done

echo -e "\n${RED}========================================"
echo -e "  SIBLING SCOPES TEST FAILED"
echo -e "  CounterA=$VA CounterB=$VB after ${MAX_WAIT}s"
echo -e "========================================${RESET}"
exit 1
