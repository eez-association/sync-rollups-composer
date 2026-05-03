#!/usr/bin/env bash
# Test L2→L1 ERC20 bridgeTokens — regression test for issue #46.
#
# Round-trips a fresh ERC20 through the canonical Bridge:
#   1. Deploy MockERC20 on L1.
#   2. EOA → Bridge_L1.bridgeTokens (L1→L2). Asserts wrapped is minted on L2.
#   3. EOA → Bridge_L2.bridgeTokens (L2→L1). Asserts L1 release succeeds.
#
# Step 3 is the regression case: pre-fix it reverts at the L2 user tx with
# CallExecutionFailed (0x6b3b6576) because the composer's Step-1 enrichment
# in `direction.rs::enrich_calls_before_retrace` calls Bridge_L1.receiveTokens
# from the L2 sender's literal address (instead of via its L1 proxy), is
# rejected by `onlyBridgeProxy` with UnauthorizedCaller (0x5c427cd9), and the
# allowlist (`is_protocol_error`) treats that as a real delivery failure
# instead of falling through to the full bundle simulation.
#
# Uses dev account #19 (0xC4..., generated; not in deploy.sh fund list — funded
# at runtime from FUNDER_KEY = dev#9). Avoids collisions with other E2E tests.
#
# Prerequisites:
# - Docker environment running with dev overlay
# - Builder healthy, deploy completed (Bridge_L1 + Bridge_L2 alive at canonical
#   CREATE2 addresses)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

# ── Args ──
JSON_MODE="false"
[ "${1:-}" = "--json" ] && JSON_MODE="true"

# ── Config ──
ROLLUP_ID=1
BRIDGE_L1=0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9
BRIDGE_L2=0xbC44fC0a4874eeFFf5bc2a5A1046589688ac6828

# Dedicated test key (off-mnemonic; no overlap with existing E2E tests #2-#18).
# Funded at runtime from FUNDER_KEY (dev#9).
TEST_KEY=0xbf0452dbb94181c1169b98f062cd03fd96230982cb2b3f7b21fc3fa4d88e31db
TEST_ADDR=0xaD5C129dcfc0389993EFBba6304D755E54BD4b28

FUNDER_KEY=0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6
FUNDER_ADDR=0xa0Ee7A142d267C1f36714E4a8F75612F20a79720

CONTRACTS_DIR="$(cd "$SCRIPT_DIR/../../contracts/test-multi-call" && pwd)"

CO_L1="--legacy --gas-price 1000000000"
CO_L2="--legacy --gas-price 2000000000"

# Amount to round-trip
AMOUNT=500000000000000000   # 0.5 TT (1e18 = 1.0)

# ── Setup ──
[ "$JSON_MODE" = "false" ] && echo "=== test-bridge-tokens-l2-to-l1 (issue #46) ==="
start_timer

# Fund TEST_ADDR on L1 (if not already)
L1_BAL=$(get_balance "$L1_RPC" "$TEST_ADDR")
if [ "$L1_BAL" -lt 1000000000000000000 ]; then
  cast send $CO_L1 --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
    "$TEST_ADDR" --value 5ether --gas-limit 21000 > /dev/null 2>&1
fi

# Fund TEST_ADDR on L2 via bridgeEther (going through L1→L2 composer so it lands
# on L2 as native ETH for gas).
L2_BAL=$(get_balance "$L2_RPC" "$TEST_ADDR")
if [ "$L2_BAL" -lt 100000000000000000 ]; then
  cast send $CO_L1 --rpc-url "$L1_PROXY" --private-key "$FUNDER_KEY" \
    "$BRIDGE_L1" "bridgeEther(uint256,address)" "$ROLLUP_ID" "$TEST_ADDR" \
    --value 1ether --gas-limit 1500000 --timeout 90 > /dev/null 2>&1
  for _ in $(seq 1 30); do
    L2_BAL=$(get_balance "$L2_RPC" "$TEST_ADDR")
    [ "$L2_BAL" -ge 100000000000000000 ] && break
    sleep 2
  done
fi
assert "TEST_ADDR funded on L1+L2" \
  '[ "$(get_balance "$L1_RPC" "$TEST_ADDR")" -ge 1000000000000000000 ] && [ "$(get_balance "$L2_RPC" "$TEST_ADDR")" -ge 100000000000000000 ]' \
  "L1=$(get_balance "$L1_RPC" "$TEST_ADDR") L2=$(get_balance "$L2_RPC" "$TEST_ADDR")"

# ── Deploy MockERC20 on L1 ──
TOKEN_L1=$(forge create $CO_L1 --rpc-url "$L1_RPC" --private-key "$TEST_KEY" --broadcast \
  --root "$CONTRACTS_DIR" src/MockERC20.sol:MockERC20 \
  --constructor-args "TestToken46" "TT46" 18 2>&1 | grep "Deployed to:" | awk '{print $3}')
assert "TOKEN_L1 deployed" '[ -n "$TOKEN_L1" ] && [ "$TOKEN_L1" != "0x" ]' "got '$TOKEN_L1'"

# Mint AMOUNT*2 (so we have enough for the L1→L2 trip plus headroom)
MINT=$((AMOUNT * 2))
cast send $CO_L1 --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
  "$TOKEN_L1" "mint(address,uint256)" "$TEST_ADDR" "$MINT" --gas-limit 100000 > /dev/null 2>&1

# ── L1→L2 bridge ──
cast send $CO_L1 --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
  "$TOKEN_L1" "approve(address,uint256)" "$BRIDGE_L1" "$AMOUNT" --gas-limit 100000 > /dev/null 2>&1

L1_TO_L2_HASH=$(cast send $CO_L1 --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
  "$BRIDGE_L1" "bridgeTokens(address,uint256,uint256,address)" \
  "$TOKEN_L1" "$AMOUNT" "$ROLLUP_ID" "$TEST_ADDR" \
  --gas-limit 1500000 --timeout 90 --json 2>/dev/null | jq -r '.transactionHash // empty')
L1_TO_L2_STATUS=$(cast receipt --rpc-url "$L1_RPC" "$L1_TO_L2_HASH" --json 2>/dev/null | jq -r '.status // "0x0"')
assert "L1→L2 bridgeTokens succeeded" '[ "$L1_TO_L2_STATUS" = "0x1" ]' "tx=$L1_TO_L2_HASH status=$L1_TO_L2_STATUS"

# Wait for L2 wrapped delivery
WRAPPED_L2=""
for _ in $(seq 1 30); do
  WRAPPED_L2=$(cast call --rpc-url "$L2_RPC" "$BRIDGE_L2" \
    "getWrappedToken(address,uint256)(address)" "$TOKEN_L1" 0 2>/dev/null || true)
  if [ -n "$WRAPPED_L2" ] && [ "$WRAPPED_L2" != "0x0000000000000000000000000000000000000000" ]; then
    L2_BAL=$(cast call --rpc-url "$L2_RPC" "$WRAPPED_L2" "balanceOf(address)(uint256)" "$TEST_ADDR" 2>/dev/null | awk '{print $1}')
    [ "${L2_BAL:-0}" -ge "$AMOUNT" ] && break
  fi
  sleep 2
done
assert "L2 wrapped TT delivered to TEST_ADDR" \
  '[ "${L2_BAL:-0}" -ge "$AMOUNT" ]' "wrapped=$WRAPPED_L2 balance=${L2_BAL:-0}"

# ── L2→L1 bridge (the regression case) ──
cast send $CO_L2 --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
  "$WRAPPED_L2" "approve(address,uint256)" "$BRIDGE_L2" "$AMOUNT" --gas-limit 100000 > /dev/null 2>&1

L1_BAL_BEFORE=$(cast call --rpc-url "$L1_RPC" "$TOKEN_L1" "balanceOf(address)(uint256)" "$TEST_ADDR" 2>/dev/null | awk '{print $1}')

L2_TO_L1_HASH=$(cast send $CO_L2 --rpc-url "$L2_PROXY" --private-key "$TEST_KEY" \
  "$BRIDGE_L2" "bridgeTokens(address,uint256,uint256,address)" \
  "$WRAPPED_L2" "$AMOUNT" 0 "$TEST_ADDR" \
  --gas-limit 2000000 --timeout 120 --json 2>/dev/null | jq -r '.transactionHash // empty')
L2_TO_L1_STATUS=$(cast receipt --rpc-url "$L2_RPC" "$L2_TO_L1_HASH" --json 2>/dev/null | jq -r '.status // "0x0"')
assert "L2→L1 bridgeTokens user tx succeeded (regression: issue #46)" \
  '[ "$L2_TO_L1_STATUS" = "0x1" ]' "tx=$L2_TO_L1_HASH status=$L2_TO_L1_STATUS"

# Wait for L1 release: TEST_ADDR's TOKEN_L1 balance must increase by AMOUNT.
TARGET=$((L1_BAL_BEFORE + AMOUNT))
L1_BAL_AFTER="$L1_BAL_BEFORE"
for _ in $(seq 1 30); do
  L1_BAL_AFTER=$(cast call --rpc-url "$L1_RPC" "$TOKEN_L1" "balanceOf(address)(uint256)" "$TEST_ADDR" 2>/dev/null | awk '{print $1}')
  [ "${L1_BAL_AFTER:-0}" -ge "$TARGET" ] && break
  sleep 2
done
assert "L1 native tokens released to TEST_ADDR" \
  '[ "${L1_BAL_AFTER:-0}" -ge "$TARGET" ]' "before=$L1_BAL_BEFORE after=${L1_BAL_AFTER:-0} expected≥$TARGET"

# ── Summary ──
print_elapsed "test-bridge-tokens-l2-to-l1"
print_total_elapsed

if [ "$JSON_MODE" = "true" ]; then
  print_json_summary "test-bridge-tokens-l2-to-l1"
else
  echo ""
  echo "=== Summary: $PASS_COUNT/$TOTAL_COUNT passed ==="
fi

[ "$FAIL_COUNT" -eq 0 ]
