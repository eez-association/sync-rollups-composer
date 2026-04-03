#!/usr/bin/env bash
# test-deep-scope.sh — E2E test for deep scope [0,0] (L2→L1 with nested wrapping)
#
# Scenario: NestedCaller(SCA) → CounterAndProxy(SCB) → CounterL1_proxy → L1 Counter
# The L2 trace has depth=2 (SCA→SCB→proxy), so L1 entries must have scope=[0,0].
#
# Steps:
#   1. Wait for builder healthy
#   2. Fund test account on L1
#   3. Deploy Counter on L1
#   4. Create L2 proxy for Counter (via L1 composer RPC)
#   5. Deploy CounterAndProxy(SCB) on L2 pointing to Counter proxy
#   6. Deploy NestedCaller(SCA) on L2 pointing to SCB
#   7. Call SCA.callNested() via L2 composer RPC
#   8. Wait for L1 batch + trigger
#   9. Verify: Counter on L1 incremented to 1
#
# Test account: dev key #19 (0x1234... — unique to this test)
# Using dev#10 key (0xBcd4042DE499D14e55001CcbB24a551F3b954096)
#
# Usage: ./scripts/e2e/test-deep-scope.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

# Dev account #10 — dedicated to deploy-ping-pong, reuse for deep scope test
TEST_KEY="0xf214f2b2cd398c806f84e317254e0f0b801d0643303237d97a22a48e01628897"
TEST_ADDR="0xBcd4042DE499D14e55001CcbB24a551F3b954096"

# Contract bytecodes from sync-rollups-protocol/out/CounterContracts.sol/
COUNTER_BYTECODE="0x6080806040523460135760bc908160188239f35b5f80fdfe60808060405260043610156011575f80fd5b5f3560e01c90816361bc221a14606f575063d09de08a14602f575f80fd5b34606b575f366003190112606b575f545f198114605757600160209101805f55604051908152f35b634e487b7160e01b5f52601160045260245ffd5b5f80fd5b34606b575f366003190112606b576020905f548152f3fea26469706673582212203e55fa77c6759db076a4bed9ac4ffc7277fe1d5bb18130aac7ed523c51165beb64736f6c634300081c0033"

# CounterAndProxy — constructor(Counter _target) → address arg padded to 32 bytes
COUNTER_AND_PROXY_BYTECODE="0x608034606f57601f61023f38819003918201601f19168301916001600160401b03831184841017607357808492602094604052833981010312606f57516001600160a01b03811690819003606f575f80546001600160a01b0319169190911790556040516101b790816100888239f35b5f80fd5b634e487b7160e01b5f52604160045260245ffdfe6080806040526004361015610012575f80fd5b5f3560e01c908163110a9ade14610167575080632bf216471461009057806361bc221a146100735763d4b8399214610048575f80fd5b3461006f575f36600319011261006f575f546040516001600160a01b039091168152602090f35b5f80fd5b3461006f575f36600319011261006f576020600254604051908152f35b3461006f575f36600319011261006f575f805460405163684ef04560e11b81529160209183916004918391906001600160a01b03165af190811561015c575f91610100575b506001556002545f1981146100ec57600101600255005b634e487b7160e01b5f52601160045260245ffd5b905060203d602011610155575b601f8101601f1916820167ffffffffffffffff8111838210176101415760209183916040528101031261006f5751816100d5565b634e487b7160e01b5f52604160045260245ffd5b503d61010d565b6040513d5f823e3d90fd5b3461006f575f36600319011261006f576020906001548152f3fea2646970667358221220dabb1ab186546547683dd819672492b2064a136f129b2ace373b8d6aba23e34e64736f6c634300081c0033"

# NestedCaller — constructor(CounterAndProxy _target) → address arg padded to 32 bytes
NESTED_CALLER_BYTECODE="0x608034606f57601f6101ed38819003918201601f19168301916001600160401b03831184841017607357808492602094604052833981010312606f57516001600160a01b03811690819003606f575f80546001600160a01b03191691909117905560405161016590816100888239f35b5f80fd5b634e487b7160e01b5f52604160045260245ffdfe6080806040526004361015610012575f80fd5b5f905f3560e01c90816361bc221a1461011557508063b8a5ffe5146100685763d4b839921461003f575f80fd5b34610065578060031936011261006557546040516001600160a01b039091168152602090f35b80fd5b5034610111575f366003190112610111575f546001600160a01b0316803b15610111575f8091600460405180948193632bf2164760e01b83525af18015610106576100d7575b506001545f1981146100c35760010160015580f35b634e487b7160e01b82526011600452602482fd5b905067ffffffffffffffff81116100f2576040525f5f6100ae565b634e487b7160e01b5f52604160045260245ffd5b6040513d5f823e3d90fd5b5f80fd5b34610111575f366003190112610111576020906001548152f3fea264697066735822122089a6c3914f2aa98ea92d3aff3e050558266cdfb6982a0e810f604f2a7c866bb464736f6c634300081c0033"

# Function selectors
INCREMENT_SELECTOR="0xd09de08a"
COUNTER_SELECTOR="0x61bc221a"
CALL_NESTED_SELECTOR="0xb8a5ffe5"

# ── Colors ──

if [ -t 1 ]; then
  CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'
  YELLOW='\033[1;33m'; BOLD='\033[1m'; DIM='\033[2m'; RESET='\033[0m'
else
  CYAN=''; GREEN=''; RED=''; YELLOW=''; BOLD=''; DIM=''; RESET=''
fi

# ── Load rollup.env ──

echo ""
echo -e "${CYAN}========================================"
echo -e "  DEEP SCOPE [0,0] E2E TEST"
echo -e "========================================${RESET}"
echo ""
echo "Loading rollup.env..."

eval "$($DOCKER_COMPOSE_CMD exec -T builder cat /shared/rollup.env 2>/dev/null)"
if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
  echo -e "${RED}ERROR: Could not load rollup.env — is the builder running?${RESET}"
  exit 1
fi

# CCM on L2 is deployed by the builder at block 1 (same address as CROSS_CHAIN_MANAGER_ADDRESS)
CCM_L2_ADDRESS="${CROSS_CHAIN_MANAGER_ADDRESS}"
echo "Rollups address: $ROLLUPS_ADDRESS"
echo "CCM L2 address: $CCM_L2_ADDRESS"
echo "Bridge address: ${BRIDGE_ADDRESS:-}"
echo "Test account: $TEST_ADDR"

# ── Step 1: Wait for builder ──

echo -e "${BOLD}Step 1: Waiting for builder to be healthy...${RESET}"
for i in $(seq 1 60); do
  HEALTH=$(curl -s "$HEALTH_URL" 2>/dev/null || echo "{}")
  IS_HEALTHY=$(echo "$HEALTH" | jq -r '.healthy // false')
  if [ "$IS_HEALTHY" = "true" ]; then
    echo "  Builder healthy: $HEALTH"
    break
  fi
  if [ "$i" = "60" ]; then
    echo -e "${RED}ERROR: Builder not healthy after 60s${RESET}"
    exit 1
  fi
  sleep 1
done

# ── Step 2: Fund test account on L1 + L2 ──

echo ""
echo -e "${BOLD}Step 2: Funding test account...${RESET}"

L1_BALANCE=$(cast balance "$TEST_ADDR" --rpc-url "$L1_RPC" 2>/dev/null || echo "0")
if [ "$(echo "$L1_BALANCE" | tr -d '[:space:]')" = "0" ]; then
  echo "  Funding on L1 from dev#9..."
  cast send --private-key "0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6" \
    "$TEST_ADDR" --value 10ether --rpc-url "$L1_RPC" >/dev/null 2>&1
  echo "  Funded 10 ETH on L1"
else
  echo "  L1 already funded: $L1_BALANCE"
fi

L2_BALANCE=$(cast balance "$TEST_ADDR" --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
if [ "$(echo "$L2_BALANCE" | tr -d '[:space:]')" = "0" ]; then
  echo "  Funding on L2 from dev#1..."
  # dev#1 has massive L2 balance from genesis
  cast send --private-key "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d" \
    "$TEST_ADDR" --value 1ether --rpc-url "$L2_RPC" >/dev/null 2>&1
  echo "  Funded 1 ETH on L2"
else
  echo "  L2 already funded: $L2_BALANCE"
fi

# ── Step 3: Deploy Counter on L1 ──

echo ""
echo -e "${BOLD}Step 3: Deploying Counter on L1...${RESET}"

COUNTER_L1=$(cast send --private-key "$TEST_KEY" --rpc-url "$L1_RPC" --create "$COUNTER_BYTECODE" --json 2>/dev/null | jq -r '.contractAddress')
echo "  Counter L1: $COUNTER_L1"

# Verify deployment
COUNTER_VAL=$(cast call "$COUNTER_L1" "$COUNTER_SELECTOR" --rpc-url "$L1_RPC" 2>/dev/null)
echo "  Counter initial value: $COUNTER_VAL"

# ── Step 4: Create L2 proxy for Counter via L1 composer RPC ──

echo ""
echo -e "${BOLD}Step 4: Creating L2 proxy for Counter...${RESET}"

# Compute the proxy address
PROXY_ADDR=$(cast call "$CCM_L2_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" "$COUNTER_L1" 0 --rpc-url "$L2_RPC" 2>/dev/null || echo "")
echo "  Predicted proxy address: $PROXY_ADDR"

# Check if proxy already has code; if not, create it
PROXY_CODE=$(cast code "$PROXY_ADDR" --rpc-url "$L2_RPC" 2>/dev/null || echo "0x")
if [ "$PROXY_CODE" = "0x" ] || [ -z "$PROXY_CODE" ]; then
  echo "  Creating proxy via CCM (may need multiple attempts)..."
  for attempt in 1 2 3; do
    cast send --private-key "$TEST_KEY" "$CCM_L2_ADDRESS" "createCrossChainProxy(address,uint256)" "$COUNTER_L1" 0 --rpc-url "$L2_RPC" >/dev/null 2>&1
    sleep 2
    PROXY_CODE=$(cast code "$PROXY_ADDR" --rpc-url "$L2_RPC" 2>/dev/null || echo "0x")
    if [ "$PROXY_CODE" != "0x" ] && [ -n "$PROXY_CODE" ]; then
      echo "  Proxy created successfully on attempt $attempt"
      break
    fi
    echo "  Attempt $attempt: proxy still has no code, retrying..."
  done
  if [ "$PROXY_CODE" = "0x" ] || [ -z "$PROXY_CODE" ]; then
    echo -e "${RED}ERROR: Failed to create proxy after 3 attempts${RESET}"
    exit 1
  fi
else
  echo "  Proxy already has code"
fi

echo "  Counter proxy on L2: $PROXY_ADDR"

# ── Step 5: Deploy CounterAndProxy (SCB) on L2 ──

echo ""
echo -e "${BOLD}Step 5: Deploying CounterAndProxy (SCB) on L2...${RESET}"

# Constructor arg: address of Counter proxy (padded to 32 bytes)
PROXY_PADDED=$(echo "$PROXY_ADDR" | sed 's/0x//' | tr '[:upper:]' '[:lower:]')
CONSTRUCTOR_ARG=$(printf '%064s' "$PROXY_PADDED" | tr ' ' '0')
SCB_DEPLOY_DATA="${COUNTER_AND_PROXY_BYTECODE}${CONSTRUCTOR_ARG}"

SCB_ADDR=$(cast send --private-key "$TEST_KEY" --rpc-url "$L2_RPC" --create "$SCB_DEPLOY_DATA" --json 2>/dev/null | jq -r '.contractAddress')
echo "  SCB (CounterAndProxy): $SCB_ADDR"

# ── Step 6: Deploy NestedCaller (SCA) on L2 ──

echo ""
echo -e "${BOLD}Step 6: Deploying NestedCaller (SCA) on L2...${RESET}"

SCB_PADDED=$(echo "$SCB_ADDR" | sed 's/0x//' | tr '[:upper:]' '[:lower:]')
SCA_CONSTRUCTOR_ARG=$(printf '%064s' "$SCB_PADDED" | tr ' ' '0')
SCA_DEPLOY_DATA="${NESTED_CALLER_BYTECODE}${SCA_CONSTRUCTOR_ARG}"

SCA_ADDR=$(cast send --private-key "$TEST_KEY" --rpc-url "$L2_RPC" --create "$SCA_DEPLOY_DATA" --json 2>/dev/null | jq -r '.contractAddress')
echo "  SCA (NestedCaller): $SCA_ADDR"

# ── Step 7: Call SCA.callNested() via L2 composer RPC ──

echo ""
echo -e "${BOLD}Step 7: Calling SCA.callNested() via L2 composer RPC...${RESET}"
echo "  SCA=$SCA_ADDR, SCB=$SCB_ADDR, Counter proxy=$PROXY_ADDR"
echo "  This should trigger: SCA → SCB → proxy → CCM (depth=2, scope=[0,0])"

# Skip gas estimation (reverts without entries loaded) — use fixed gas limit
TX_HASH=$(cast send --private-key "$TEST_KEY" "$SCA_ADDR" "$CALL_NESTED_SELECTOR" \
  --gas-limit 500000 --rpc-url "$L2_PROXY" --json 2>/dev/null | jq -r '.transactionHash')

echo "  L2 tx hash: $TX_HASH"

# Check tx receipt
TX_STATUS=$(cast receipt "$TX_HASH" --rpc-url "$L2_RPC" --json 2>/dev/null | jq -r '.status')
echo "  L2 tx status: $TX_STATUS"

if [ "$TX_STATUS" != "0x1" ]; then
  echo -e "${RED}FAIL: L2 tx reverted (status=$TX_STATUS)${RESET}"
  exit 1
fi

# ── Step 8: Wait for L1 batch + trigger execution ──

echo ""
echo -e "${BOLD}Step 8: Waiting for L1 Counter to be incremented...${RESET}"
echo "  (waiting up to 120s for batch submission + trigger execution)"

MAX_WAIT=120
INTERVAL=5
ELAPSED=0

while [ $ELAPSED -lt $MAX_WAIT ]; do
  COUNTER_VAL_RAW=$(cast call "$COUNTER_L1" "$COUNTER_SELECTOR" --rpc-url "$L1_RPC" 2>/dev/null || echo "0x0")
  COUNTER_VAL=$(printf "%d" "$COUNTER_VAL_RAW" 2>/dev/null || echo "0")

  if [ "$COUNTER_VAL" -gt 0 ] 2>/dev/null; then
    echo ""
    echo -e "${GREEN}========================================"
    echo -e "  DEEP SCOPE TEST PASSED!"
    echo -e "  Counter L1 value: $COUNTER_VAL"
    echo -e "  Scope [0,0] navigation worked correctly"
    echo -e "========================================${RESET}"
    exit 0
  fi

  echo "  Counter still 0, waiting... ($ELAPSED/${MAX_WAIT}s)"
  sleep $INTERVAL
  ELAPSED=$((ELAPSED + INTERVAL))
done

echo ""
echo -e "${RED}========================================"
echo -e "  DEEP SCOPE TEST FAILED"
echo -e "  Counter L1 still 0 after ${MAX_WAIT}s"
echo -e "  L1 batch/trigger may not have executed"
echo -e "========================================${RESET}"
exit 1
