#!/usr/bin/env bash
# test-multi-call-cross-chain.sh — E2E regression test for issue #256.
#
# Validates that an L1 contract making MULTIPLE cross-chain calls to L2
# in a single execution works correctly. Three sub-tests:
#
#   TEST A: EOA → CallTwice → L2CounterProxy (x2, same proxy)
#     Expected: L2 Counter increments by 2
#
#   TEST B: EOA → CallTwoDifferent → L2CounterAProxy + L2CounterBProxy
#     Expected: Both L2 Counters increment by 1
#
#   TEST C: Full depth-2 chain:
#     L2 Logger → L1 Logger → CallTwice → L2CounterProxy (x2)
#     Expected: L2 Counter increments by 2, L1 Logger gets (1,2) return
#
# Before the fix: only the first cross-chain call in a single execution
# gets an entry built. The second call fails with ExecutionNotFound.
#
# Test account: dev key #17 (HD mnemonic index 17)
#   Address:     0xbDA5747bFD65F08deb54cb465eB87D40e51B197E
#   Private key: 0x689af8efa8c651a91ad287602527f3af2fe9f6501a7ac4b061667b5a93e037fd
#
# Usage: ./scripts/e2e/test-multi-call-cross-chain.sh [--json]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-health-check.sh"

parse_lib_args "$@"

# ── Configuration ──

TEST_KEY="0x689af8efa8c651a91ad287602527f3af2fe9f6501a7ac4b061667b5a93e037fd"
TEST_ADDR="0xbDA5747bFD65F08deb54cb465eB87D40e51B197E"

# Counter bytecode: uint256 public counter; function increment() external returns (uint256)
# Also stores msg.sender per increment. Compiled with solc 0.8.28.
COUNTER_BYTECODE="0x6080604052348015600f57600080fd5b5061017f8061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100365760003560e01c806361bc221a1461003b578063d09de08a14610059575b600080fd5b610043610077565b60405161005091906100b7565b60405180910390f35b61006161007d565b60405161006e91906100b7565b60405180910390f35b60005481565b600080600081548092919061009190610101565b9190505550600054905090565b6000819050919050565b6100b18161009e565b82525050565b60006020820190506100cc60008301846100a8565b92915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b600061010c8261009e565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff820361013e5761013d6100d2565b5b60018201905091905056fea26469706673582212203dcec02a2fe7260919dd7cb86d1128a36e74ee651874f6f0a26f8e688fd7407764736f6c63430008210033"

# CallTwice bytecode: callCounterTwice(address) calls counter.increment() twice
# Source: contracts/test-multi-call/src/CallTwice.sol, solc 0.8.28
CALLTWICE_BYTECODE="0x6080604052348015600e575f5ffd5b506105888061001c5f395ff3fe608060405234801561000f575f5ffd5b5060043610610029575f3560e01c8063ede8a03c1461002d575b5f5ffd5b6100476004803603810190610042919061034b565b61005e565b60405161005592919061038e565b60405180910390f35b5f5f5f5f8473ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527fd09de08a000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff83818316178352505050506040516101099190610407565b5f604051808303815f865af19150503d805f8114610142576040519150601f19603f3d011682016040523d82523d5f602084013e610147565b606091505b50915091508161018c576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161018390610477565b60405180910390fd5b808060200190518101906101a091906104bf565b93505f5f8673ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527fd09de08a000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff838183161783525050505060405161024b9190610407565b5f604051808303815f865af19150503d805f8114610284576040519150601f19603f3d011682016040523d82523d5f602084013e610289565b606091505b5091509150816102ce576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016102c590610534565b60405180910390fd5b808060200190518101906102e291906104bf565b945050505050915091565b5f5ffd5b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f61031a826102f1565b9050919050565b61032a81610310565b8114610334575f5ffd5b50565b5f8135905061034581610321565b92915050565b5f602082840312156103605761035f6102ed565b5b5f61036d84828501610337565b91505092915050565b5f819050919050565b61038881610376565b82525050565b5f6040820190506103a15f83018561037f565b6103ae602083018461037f565b9392505050565b5f81519050919050565b5f81905092915050565b8281835e5f83830152505050565b5f6103e1826103b5565b6103eb81856103bf565b93506103fb8185602086016103c9565b80840191505092915050565b5f61041282846103d7565b915081905092915050565b5f82825260208201905092915050565b7f66697273742063616c6c206661696c65640000000000000000000000000000005f82015250565b5f61046160118361041d565b915061046c8261042d565b602082019050919050565b5f6020820190508181035f83015261048e81610455565b9050919050565b61049e81610376565b81146104a8575f5ffd5b50565b5f815190506104b981610495565b92915050565b5f602082840312156104d4576104d36102ed565b5b5f6104e1848285016104ab565b91505092915050565b7f7365636f6e642063616c6c206661696c656400000000000000000000000000005f82015250565b5f61051e60128361041d565b9150610529826104ea565b602082019050919050565b5f6020820190508181035f83015261054b81610512565b905091905056fea26469706673582212207ac1a165afbe073da8f6ccfcfc755c4d24fa13e620e765fdaa170de54447d27864736f6c634300081c0033"

# CallTwoDifferent bytecode: callBothCounters(address,address) calls two different counters
# Source: contracts/test-multi-call/src/CallTwoDifferent.sol, solc 0.8.28
CALLTWODIFF_BYTECODE="0x6080604052348015600e575f5ffd5b5061059d8061001c5f395ff3fe608060405234801561000f575f5ffd5b5060043610610029575f3560e01c8063e526b42e1461002d575b5f5ffd5b6100476004803603810190610042919061034d565b61005e565b6040516100559291906103a3565b60405180910390f35b5f5f5f5f8573ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527fd09de08a000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff8381831617835250505050604051610109919061041c565b5f604051808303815f865af19150503d805f8114610142576040519150601f19603f3d011682016040523d82523d5f602084013e610147565b606091505b50915091508161018c576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016101839061048c565b60405180910390fd5b808060200190518101906101a091906104d4565b93505f5f8673ffffffffffffffffffffffffffffffffffffffff166040516024016040516020818303038152906040527fd09de08a000000000000000000000000000000000000000000000000000000007bffffffffffffffffffffffffffffffffffffffffffffffffffffffff19166020820180517bffffffffffffffffffffffffffffffffffffffffffffffffffffffff838183161783525050505060405161024b919061041c565b5f604051808303815f865af19150503d805f8114610284576040519150601f19603f3d011682016040523d82523d5f602084013e610289565b606091505b5091509150816102ce576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016102c590610549565b60405180910390fd5b808060200190518101906102e291906104d4565b9450505050509250929050565b5f5ffd5b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f61031c826102f3565b9050919050565b61032c81610312565b8114610336575f5ffd5b50565b5f8135905061034781610323565b92915050565b5f5f60408385031215610363576103626102ef565b5b5f61037085828601610339565b925050602061038185828601610339565b9150509250929050565b5f819050919050565b61039d8161038b565b82525050565b5f6040820190506103b65f830185610394565b6103c36020830184610394565b9392505050565b5f81519050919050565b5f81905092915050565b8281835e5f83830152505050565b5f6103f6826103ca565b61040081856103d4565b93506104108185602086016103de565b80840191505092915050565b5f61042782846103ec565b915081905092915050565b5f82825260208201905092915050565b7f66697273742063616c6c206661696c65640000000000000000000000000000005f82015250565b5f610476601183610432565b915061048182610442565b602082019050919050565b5f6020820190508181035f8301526104a38161046a565b9050919050565b6104b38161038b565b81146104bd575f5ffd5b50565b5f815190506104ce816104aa565b92915050565b5f602082840312156104e9576104e86102ef565b5b5f6104f6848285016104c0565b91505092915050565b7f7365636f6e642063616c6c206661696c656400000000000000000000000000005f82015250565b5f610533601283610432565b915061053e826104ff565b602082019050919050565b5f6020820190508181035f83015261056081610527565b905091905056fea26469706673582212201458278ad0e02c79698b34489c58b8bf8ab93247349f24d1dce91bc246eb512864736f6c634300081c0033"

# ReturnDataLogger bytecode (same as test-depth2-generic.sh)
LOGGER_BYTECODE="0x6080604052348015600f57600080fd5b506107c98061001f6000396000f3fe608060405234801561001057600080fd5b50600436106100415760003560e01c80631cff79cd1461004657806326a2b98c146100765780639415bc5914610094575b600080fd5b610060600480360381019061005b91906102c9565b6100b2565b60405161006d91906103b9565b60405180910390f35b61007e61015b565b60405161008b91906103b9565b60405180910390f35b61009c6101e9565b6040516100a991906103f6565b60405180910390f35b60606000808573ffffffffffffffffffffffffffffffffffffffff1685856040516100de929190610450565b6000604051808303816000865af19150503d806000811461011b576040519150601f19603f3d011682016040523d82523d6000602084013e610120565b606091505b509150915081600160006101000a81548160ff021916908315150217905550806000908161014e91906106c1565b5080925050509392505050565b60008054610168906104c7565b80601f0160208091040260200160405190810160405280929190818152602001828054610194906104c7565b80156101e15780601f106101b6576101008083540402835291602001916101e1565b820191906000526020600020905b8154815290600101906020018083116101c457829003601f168201915b505050505081565b600160009054906101000a900460ff1681565b600080fd5b600080fd5b600073ffffffffffffffffffffffffffffffffffffffff82169050919050565b600061023182610206565b9050919050565b61024181610226565b811461024c57600080fd5b50565b60008135905061025e81610238565b92915050565b600080fd5b600080fd5b600080fd5b60008083601f84011261028957610288610264565b5b8235905067ffffffffffffffff8111156102a6576102a5610269565b5b6020830191508360018202830111156102c2576102c161026e565b5b9250929050565b6000806000604084860312156102e2576102e16101fc565b5b60006102f08682870161024f565b935050602084013567ffffffffffffffff81111561031157610310610201565b5b61031d86828701610273565b92509250509250925092565b600081519050919050565b600082825260208201905092915050565b60005b83811015610363578082015181840152602081019050610348565b60008484015250505050565b6000601f19601f8301169050919050565b600061038b82610329565b6103958185610334565b93506103a5818560208601610345565b6103ae8161036f565b840191505092915050565b600060208201905081810360008301526103d38184610380565b905092915050565b60008115159050919050565b6103f0816103db565b82525050565b600060208201905061040b60008301846103e7565b92915050565b600081905092915050565b82818337600083830152505050565b60006104378385610411565b935061044483858461041c565b82840190509392505050565b600061045d82848661042b565b91508190509392505050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052604160045260246000fd5b7f4e487b7100000000000000000000000000000000000000000000000000000000600052602260045260246000fd5b600060028204905060018216806104df57607f821691505b6020821081036104f2576104f1610498565b5b50919050565b60008190508160005260206000209050919050565b60006020601f8301049050919050565b600082821b905092915050565b60006008830261055a7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff8261051d565b610564868361051d565b95508019841693508086168417925050509392505050565b6000819050919050565b6000819050919050565b60006105ab6105a66105a18461057c565b610586565b61057c565b9050919050565b6000819050919050565b6105c583610590565b6105d96105d1826105b2565b84845461052a565b825550505050565b600090565b6105ee6105e1565b6105f98184846105bc565b505050565b60005b828110156106215761061660008284016105e6565b600181019050610601565b505050565b601f821115610675578282111561067457610640816104f8565b6106498361050d565b6106528561050d565b602086101561066057600090505b80830161066f828403826105fe565b505050505b5b505050565b600082821c905092915050565b60006106986000198460080261067a565b1980831691505092915050565b60006106b18383610687565b9150826002028217905092915050565b6106ca82610329565b67ffffffffffffffff8111156106e3576106e2610469565b5b6106ed82546104c7565b6106f8828285610626565b600060209050601f83116001811461072b5760008415610719578287015190505b61072385826106a5565b86555061078b565b601f198416610739866104f8565b60005b828110156107615784890151825560018201915060208501945060208101905061073c565b8683101561077e578489015161077a601f891682610687565b8355505b6001600288020188555050505b50505050505056fea2646970667358221220a904bd1c3bec96a31e91a7a303b9b5dfcb9a9185f65c68d188e1fe7090c77dd164736f6c63430008210033"

# ── Colors ──

if [ -t 1 ]; then
  CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'
  YELLOW='\033[1;33m'; BOLD='\033[1m'; RESET='\033[0m'
else
  CYAN=''; GREEN=''; RED=''; YELLOW=''; BOLD=''; RESET=''
fi

# ── Load rollup.env ──

echo ""
echo -e "${CYAN}========================================"
echo -e "  MULTI-CALL CROSS-CHAIN TEST (issue #256)"
echo -e "========================================${RESET}"
echo ""
echo "Loading rollup.env..."

eval "$($DOCKER_COMPOSE_CMD exec -T builder cat /shared/rollup.env 2>/dev/null)"
if [ -z "${ROLLUPS_ADDRESS:-}" ]; then
  echo -e "${RED}ERROR: Could not load rollup.env${RESET}"
  exit 1
fi

CCM_L2="${CROSS_CHAIN_MANAGER_ADDRESS:-}"
ROLLUP_ID="${ROLLUP_ID:-1}"
BRIDGE_ADDR="${BRIDGE_L1_ADDRESS:-${BRIDGE_ADDRESS:-}}"
echo "ROLLUPS=$ROLLUPS_ADDRESS  CCM=$CCM_L2  ROLLUP_ID=$ROLLUP_ID"
echo "Test account: $TEST_ADDR"
echo ""

# ── Blockscout URLs ──
L1_EXPLORER="${L1_EXPLORER:-}"
L2_EXPLORER="${L2_EXPLORER:-}"

verify_on_blockscout() {
    local explorer_url="$1"
    local addr="$2"
    local name="$3"
    local source="$4"
    local compiler="${5:-v0.8.28+commit.7893614a}"

    if [ -z "$explorer_url" ]; then return 0; fi

    echo "  Verifying $name on Blockscout..."
    local payload
    payload=$(cat <<JSONEOF
{
  "compiler_version": "$compiler",
  "source_code": $(python3 -c "import json; print(json.dumps(open('$source').read()))" 2>/dev/null || echo '""'),
  "is_optimization_enabled": false,
  "evm_version": "default",
  "contract_name": "$name"
}
JSONEOF
    )
    curl -s -X POST "$explorer_url/api/v2/smart-contracts/$addr/verification/via/flattened-code" \
        -H "Content-Type: application/json" -d "$payload" > /dev/null 2>&1 || true
    sleep 3
    local verified
    verified=$(curl -s "$explorer_url/api/v2/smart-contracts/$addr" 2>/dev/null | \
        grep -oP '"is_verified"\s*:\s*\K[a-z]+' || echo "unknown")
    echo "    verified=$verified"
}

# ══════════════════════════════════════════
#  PRE-FLIGHT
# ══════════════════════════════════════════

echo "========================================"
echo "  PRE-FLIGHT"
echo "========================================"
start_timer

echo "Waiting for builder (up to 90s)..."
MODE=$(wait_for_builder_ready 90)
assert "Builder is in Builder mode" '[ "$MODE" = "Builder" ]'

echo "Stopping crosschain-tx-sender (avoids §4f interference)..."
$DOCKER_COMPOSE_CMD stop crosschain-tx-sender > /dev/null 2>&1 || true
wait_for_pending_zero 30 >/dev/null || true

# Fund test account
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
L1_BAL=$(cast balance --rpc-url "$L1_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
if [ "$L1_BAL" = "0" ] || [ "$L1_BAL" = "0x0" ]; then
    echo "Funding $TEST_ADDR on L1 with 100 ETH..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$TEST_ADDR" --value 100ether --gas-limit 21000 > /dev/null 2>&1
    sleep 2
fi

L2_BAL=$(cast balance --rpc-url "$L2_RPC" "$TEST_ADDR" 2>/dev/null || echo "0")
MIN_BAL=50000000000000000
if [ "$(printf '%d' "$L2_BAL" 2>/dev/null || echo 0)" -lt "$MIN_BAL" ] 2>/dev/null; then
    echo "Bridging 0.5 ETH to L2..."
    DEPOSIT_STATUS=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
        "$BRIDGE_ADDR" "bridgeEther(uint256,address)" "$ROLLUP_ID" "$TEST_ADDR" \
        --value 0.5ether --gas-limit 800000 2>&1 | grep "^status" | awk '{print $2}')
    assert "Bridge deposit succeeded" '[ "$DEPOSIT_STATUS" = "1" ]'
    L2_BLK=$(get_block_number "$L2_RPC")
    wait_for_block_advance "$L2_RPC" "$L2_BLK" 3 60 >/dev/null || true
    wait_for_pending_zero 60 >/dev/null || true
fi

print_elapsed "PRE-FLIGHT"
echo ""

# ══════════════════════════════════════════
#  STEP 1: Deploy contracts
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 1: Deploy contracts"
echo "========================================"
start_timer

deploy_contract() {
    local rpc="$1" bytecode="$2" label="$3"
    local result addr status
    result=$(cast send --rpc-url "$rpc" --private-key "$TEST_KEY" \
        --create "$bytecode" --json 2>&1 || echo "{}")
    addr=$(echo "$result" | grep -oP '"contractAddress"\s*:\s*"\K[^"]+' || echo "")
    status=$(echo "$result" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
    echo "  $label: $addr (status=$status)"
    assert "STEP1: $label deployed" '[ "$status" = "0x1" ] && [ -n "$addr" ]'
    eval "${4}='$addr'"
}

deploy_contract "$L2_RPC" "$COUNTER_BYTECODE" "Counter A (L2)" C_A_L2
deploy_contract "$L2_RPC" "$COUNTER_BYTECODE" "Counter B (L2)" C_B_L2
deploy_contract "$L1_RPC" "$CALLTWICE_BYTECODE" "CallTwice (L1)" CALLTWICE_L1
deploy_contract "$L1_RPC" "$CALLTWODIFF_BYTECODE" "CallTwoDifferent (L1)" CALLTWODIFF_L1
deploy_contract "$L1_RPC" "$LOGGER_BYTECODE" "Logger (L1)" L_L1
deploy_contract "$L2_RPC" "$LOGGER_BYTECODE" "Logger (L2)" L_L2

# Verify on Blockscout (source files in contracts/test-multi-call/src/)
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SRC_DIR="$REPO_ROOT/contracts/test-multi-call/src"
if [ -n "$L1_EXPLORER" ] && [ -d "$SRC_DIR" ]; then
    verify_on_blockscout "$L2_EXPLORER" "$C_A_L2" "Counter" "$SRC_DIR/Counter.sol" || true
    verify_on_blockscout "$L2_EXPLORER" "$C_B_L2" "Counter" "$SRC_DIR/Counter.sol" || true
    verify_on_blockscout "$L1_EXPLORER" "$CALLTWICE_L1" "CallTwice" "$SRC_DIR/CallTwice.sol" || true
    verify_on_blockscout "$L1_EXPLORER" "$CALLTWODIFF_L1" "CallTwoDifferent" "$SRC_DIR/CallTwoDifferent.sol" || true
fi

print_elapsed "STEP 1"
echo ""

# ══════════════════════════════════════════
#  STEP 2: Create cross-chain proxies
# ══════════════════════════════════════════

echo "========================================"
echo "  STEP 2: Create cross-chain proxies"
echo "========================================"
start_timer

create_proxy_l1() {
    local l2_addr="$1" label="$2" var="$3"
    cast send --rpc-url "$L1_RPC" --private-key "$TEST_KEY" \
        "$ROLLUPS_ADDRESS" "createCrossChainProxy(address,uint256)" "$l2_addr" "$ROLLUP_ID" \
        --gas-limit 500000 --json > /dev/null 2>&1
    local proxy
    proxy=$(cast call --rpc-url "$L1_RPC" \
        "$ROLLUPS_ADDRESS" "computeCrossChainProxyAddress(address,uint256)(address)" "$l2_addr" "$ROLLUP_ID" 2>/dev/null || echo "")
    echo "  $label: $proxy"
    local code
    code=$(cast code --rpc-url "$L1_RPC" "$proxy" 2>/dev/null || echo "0x")
    assert "STEP2: $label has code" '[ "$code" != "0x" ]'
    eval "${var}='$proxy'"
}

create_proxy_l2() {
    local l1_addr="$1" label="$2" var="$3"
    cast send --rpc-url "$L2_RPC" --private-key "$TEST_KEY" \
        "$CCM_L2" "createCrossChainProxy(address,uint256)" "$l1_addr" 0 \
        --gas-limit 500000 --json > /dev/null 2>&1
    local proxy
    proxy=$(cast call --rpc-url "$L2_RPC" \
        "$CCM_L2" "computeCrossChainProxyAddress(address,uint256)(address)" "$l1_addr" 0 2>/dev/null || echo "")
    echo "  $label: $proxy"
    local code
    code=$(cast code --rpc-url "$L2_RPC" "$proxy" 2>/dev/null || echo "0x")
    assert "STEP2: $label has code" '[ "$code" != "0x" ]'
    eval "${var}='$proxy'"
}

create_proxy_l1 "$C_A_L2" "Counter A proxy on L1" C_A_PROXY_L1
create_proxy_l1 "$C_B_L2" "Counter B proxy on L1" C_B_PROXY_L1
create_proxy_l2 "$L_L1" "Logger L1 proxy on L2" L1_LOGGER_PROXY_L2

print_elapsed "STEP 2"
echo ""

# ══════════════════════════════════════════
#  TEST A: CallTwice → same proxy (x2)
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST A: EOA → CallTwice → L2CounterProxy (x2)"
echo "========================================"
start_timer

COUNTER_A_BEFORE=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
echo "Counter A before: $COUNTER_A_BEFORE"

wait_for_pending_zero 30 >/dev/null || true

echo "Sending: CallTwice.callCounterTwice(CounterA_proxy)..."
CALLDATA_A=$(cast calldata "callCounterTwice(address)" "$C_A_PROXY_L1")
RESULT_A=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
    "$CALLTWICE_L1" "$CALLDATA_A" \
    --gas-limit 2000000 --json 2>&1 || echo "{}")
STATUS_A=$(echo "$RESULT_A" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
echo "L1 tx status: $STATUS_A"

echo "Waiting for settlement..."
wait_for_pending_zero 90 >/dev/null || true
L2_BLK=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true

EXPECTED_A=$((COUNTER_A_BEFORE + 2))
COUNTER_A_AFTER="$COUNTER_A_BEFORE"
for _poll in $(seq 1 10); do
    COUNTER_A_AFTER=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    if [ "$COUNTER_A_AFTER" = "$EXPECTED_A" ]; then break; fi
    sleep 6
done
echo "Counter A after: $COUNTER_A_AFTER (expected: $EXPECTED_A)"
assert "TEST_A: Counter A incremented by 2 (same proxy x2)" \
    '[ "$COUNTER_A_AFTER" = "$EXPECTED_A" ]' \
    "got=$COUNTER_A_AFTER expected=$EXPECTED_A"

print_elapsed "TEST A"
echo ""

# ══════════════════════════════════════════
#  TEST B: CallTwoDifferent → 2 different proxies
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST B: EOA → CallTwoDifferent → CounterA + CounterB"
echo "========================================"
start_timer

CA_BEFORE=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
CB_BEFORE=$(cast call --rpc-url "$L2_RPC" "$C_B_L2" "counter()(uint256)" 2>/dev/null || echo "0")
echo "Counter A before: $CA_BEFORE, Counter B before: $CB_BEFORE"

wait_for_pending_zero 30 >/dev/null || true

echo "Sending: CallTwoDifferent.callBothCounters(A_proxy, B_proxy)..."
CALLDATA_B=$(cast calldata "callBothCounters(address,address)" "$C_A_PROXY_L1" "$C_B_PROXY_L1")
RESULT_B=$(cast send --rpc-url "$L1_PROXY" --private-key "$TEST_KEY" \
    "$CALLTWODIFF_L1" "$CALLDATA_B" \
    --gas-limit 2000000 --json 2>&1 || echo "{}")
STATUS_B=$(echo "$RESULT_B" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
echo "L1 tx status: $STATUS_B"

echo "Waiting for settlement..."
wait_for_pending_zero 90 >/dev/null || true
L2_BLK=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true

EXPECTED_CA=$((CA_BEFORE + 1))
EXPECTED_CB=$((CB_BEFORE + 1))
CA_AFTER="$CA_BEFORE"
CB_AFTER="$CB_BEFORE"
for _poll in $(seq 1 10); do
    CA_AFTER=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    CB_AFTER=$(cast call --rpc-url "$L2_RPC" "$C_B_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    if [ "$CA_AFTER" = "$EXPECTED_CA" ] && [ "$CB_AFTER" = "$EXPECTED_CB" ]; then break; fi
    sleep 6
done
echo "Counter A: $CA_BEFORE → $CA_AFTER (expected $EXPECTED_CA)"
echo "Counter B: $CB_BEFORE → $CB_AFTER (expected $EXPECTED_CB)"
assert "TEST_B: Counter A incremented" '[ "$CA_AFTER" = "$EXPECTED_CA" ]' \
    "got=$CA_AFTER expected=$EXPECTED_CA"
assert "TEST_B: Counter B incremented" '[ "$CB_AFTER" = "$EXPECTED_CB" ]' \
    "got=$CB_AFTER expected=$EXPECTED_CB"

print_elapsed "TEST B"
echo ""

# ══════════════════════════════════════════
#  TEST C: Depth-2 + CallTwice
# ══════════════════════════════════════════

echo "========================================"
echo "  TEST C: L2 Logger → L1 Logger → CallTwice → Counter (x2)"
echo "========================================"
start_timer

C_BEFORE=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
echo "Counter A before: $C_BEFORE"

wait_for_pending_zero 30 >/dev/null || true

# Inner: Logger_L1.execute(CallTwice, callCounterTwice(CounterA_proxy))
CALLTWICE_CALL=$(cast calldata "callCounterTwice(address)" "$C_A_PROXY_L1")
INNER=$(cast calldata "execute(address,bytes)" "$CALLTWICE_L1" "$CALLTWICE_CALL")

echo "Sending: L2 Logger → L1 Logger → CallTwice → Counter A (x2)..."
RESULT_C=$(cast send --rpc-url "$L2_PROXY" --private-key "$TEST_KEY" \
    "$L_L2" "execute(address,bytes)" "$L1_LOGGER_PROXY_L2" "$INNER" \
    --gas-limit 3000000 --json 2>&1 || echo "{}")
STATUS_C=$(echo "$RESULT_C" | grep -oP '"status"\s*:\s*"\K[^"]+' || echo "")
echo "L2 tx status: $STATUS_C"
assert "TEST_C: L2 tx succeeded" '[ "$STATUS_C" = "0x1" ]'

echo "Waiting for settlement..."
wait_for_pending_zero 90 >/dev/null || true
L2_BLK=$(get_block_number "$L2_RPC")
wait_for_block_advance "$L2_RPC" "$L2_BLK" 5 90 >/dev/null || true

EXPECTED_C=$((C_BEFORE + 2))
C_AFTER="$C_BEFORE"
for _poll in $(seq 1 10); do
    C_AFTER=$(cast call --rpc-url "$L2_RPC" "$C_A_L2" "counter()(uint256)" 2>/dev/null || echo "0")
    if [ "$C_AFTER" = "$EXPECTED_C" ]; then break; fi
    sleep 6
done
echo "Counter A: $C_BEFORE → $C_AFTER (expected $EXPECTED_C)"
assert "TEST_C: Counter A incremented by 2 (depth-2 + CallTwice)" \
    '[ "$C_AFTER" = "$EXPECTED_C" ]' \
    "got=$C_AFTER expected=$EXPECTED_C"

print_elapsed "TEST C"
echo ""

# ══════════════════════════════════════════
#  STEP 5: Health check + convergence
# ══════════════════════════════════════════

echo "========================================"
echo "  Health check + convergence"
echo "========================================"
start_timer

ROOTS=$(wait_for_convergence 60)
assert "State roots converge" '[ "$ROOTS" = "MATCH" ]'

HEALTH=$(get_health)
FINAL_MODE=$(echo "$HEALTH" | jq -r '.mode // "?"')
FINAL_REWINDS=$(echo "$HEALTH" | jq -r '.consecutive_rewind_cycles // "?"')
assert "Builder in Builder mode" '[ "$FINAL_MODE" = "Builder" ]'
assert "No rewind cycles" '[ "$FINAL_REWINDS" = "0" ]'

print_elapsed "Health check"
echo ""

# ══════════════════════════════════════════
#  SUMMARY
# ══════════════════════════════════════════

echo "========================================"
echo "  MULTI-CALL CROSS-CHAIN TEST RESULTS"
echo "========================================"
echo ""
echo "  Counter A (L2):         $C_A_L2"
echo "  Counter B (L2):         $C_B_L2"
echo "  CallTwice (L1):         $CALLTWICE_L1"
echo "  CallTwoDifferent (L1):  $CALLTWODIFF_L1"
echo "  Logger L1:              $L_L1"
echo "  Logger L2:              $L_L2"
echo ""
echo "  Passed: $PASS_COUNT"
echo "  Failed: $FAIL_COUNT"
echo "  Total:  $TOTAL_COUNT"
echo ""
print_total_elapsed
echo ""

# Restart crosschain-tx-sender
echo "Restarting crosschain-tx-sender..."
$DOCKER_COMPOSE_CMD start crosschain-tx-sender > /dev/null 2>&1 || true

if [ "$FAIL_COUNT" -eq 0 ]; then
  echo -e "  ${GREEN}STATUS: ALL TESTS PASSED${RESET}"
  exit 0
else
  echo -e "  ${RED}STATUS: $FAIL_COUNT TEST(S) FAILED${RESET}"
  exit 1
fi
