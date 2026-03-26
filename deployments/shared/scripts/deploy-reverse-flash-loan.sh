#!/usr/bin/env bash
# Deploy L2-to-L1 reverse flash loan contracts.
#
# Prerequisites:
#   - Rollups.sol deployed on L1
#   - CrossChainManagerL2 deployed on L2
#   - Bridge L1 and Bridge L2 deployed and initialized (from deploy.sh)
#   - TestToken on L1 and WrappedToken on L2 available (from flash loan L1->L2 deployment)
#   - Private key with ETH on both chains
#
# Usage:
#   bash scripts/deploy-reverse-flash-loan.sh \
#     --l1-rpc <L1_RPC_URL> \
#     --l2-rpc <L2_RPC_URL> \
#     --pk <PRIVATE_KEY> \
#     --rollups <ROLLUPS_ADDR> \
#     --manager-l2 <MANAGER_L2_ADDR> \
#     --bridge-l1 <BRIDGE_L1_ADDR> \
#     --bridge-l2 <BRIDGE_L2_ADDR> \
#     --token <TEST_TOKEN_L1_ADDR> \
#     --wrapped-token <WRAPPED_TOKEN_L2_ADDR> \
#     --l2-rollup-id <ROLLUP_ID> \
#     [--contracts-dir <path>]
set -euo pipefail
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1

SCRIPT="script/flash-loan-reverse/DeployReverseFlashLoan.s.sol"

# ── Parse args ────────────────────────────────────────────────────────────────
CONTRACTS_DIR="${CONTRACTS_DIR:-$(cd "$(dirname "$0")/../contracts/sync-rollups-protocol" && pwd)}"
L1_ROLLUP_ID=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --l1-rpc)        L1_RPC="$2";          shift 2;;
        --l2-rpc)        L2_RPC="$2";          shift 2;;
        --pk)            PK="$2";              shift 2;;
        --rollups)       ROLLUPS="$2";         shift 2;;
        --manager-l2)    MANAGER_L2="$2";      shift 2;;
        --bridge-l1)     BRIDGE_L1="$2";       shift 2;;
        --bridge-l2)     BRIDGE_L2="$2";       shift 2;;
        --token)         TOKEN="$2";           shift 2;;
        --wrapped-token) WRAPPED_TOKEN_L2="$2";shift 2;;
        --l2-rollup-id)  L2_ROLLUP_ID="$2";   shift 2;;
        --contracts-dir) CONTRACTS_DIR="$2";   shift 2;;
        --l1-proxy-url) L1_PROXY_URL="$2";   shift 2;;
        *) echo "Unknown arg: $1"; exit 1;;
    esac
done

for var in L1_RPC L2_RPC PK ROLLUPS MANAGER_L2 BRIDGE_L1 BRIDGE_L2 TOKEN WRAPPED_TOKEN_L2 L2_ROLLUP_ID; do
    if [[ -z "${!var:-}" ]]; then
        echo "ERROR: Missing required arg: --$(echo "$var" | tr '_' '-' | tr '[:upper:]' '[:lower:]')"
        exit 1
    fi
done

# ── Early exit if flash loan infra was not deployed ───────────────────────────
# FLASH_TOKEN_ADDRESS is zero when the L1->L2 flash loan contracts were skipped
# (e.g. DEPLOY_FLASH_LOAN=false in deploy.sh). Nothing to do here.
ZERO="0x0000000000000000000000000000000000000000"
if [[ -z "$TOKEN" || "$TOKEN" == "$ZERO" ]]; then
    echo "FLASH_TOKEN_ADDRESS is zero or empty — L1->L2 flash loan infra not deployed."
    echo "Skipping reverse flash loan deployment."
    exit 0
fi
if [[ -z "$WRAPPED_TOKEN_L2" || "$WRAPPED_TOKEN_L2" == "$ZERO" ]]; then
    echo "WRAPPED_TOKEN_L2 is zero or empty — L1->L2 flash loan infra not deployed."
    echo "Skipping reverse flash loan deployment."
    exit 0
fi

extract() { echo "$1" | grep "$2=" | sed "s/.*$2=//" | awk '{print $1}'; }

cd "$CONTRACTS_DIR"
echo "Working dir: $(pwd)"
echo ""

# ══════════════════════════════════════════════
#  Step 1: Deploy ReverseNFTL1 + ReverseExecutorL1 on L1
# ══════════════════════════════════════════════
echo "====== Step 1: Deploy L1 contracts (ReverseNFTL1 + ReverseExecutorL1) ======"
L1_OUTPUT=$(forge script "$SCRIPT:DeployReverseFlashLoanL1" \
    --rpc-url "$L1_RPC" --broadcast --private-key "$PK" \
    --sig "run(address,address)" "$BRIDGE_L1" "$TOKEN" 2>&1)
echo "$L1_OUTPUT" | grep -E "REVERSE_|Error|error" || true
REVERSE_NFT_L1=$(extract "$L1_OUTPUT" "REVERSE_NFT_L1")
REVERSE_EXECUTOR_L1=$(extract "$L1_OUTPUT" "REVERSE_EXECUTOR_L1")
echo "REVERSE_NFT_L1=$REVERSE_NFT_L1"
echo "REVERSE_EXECUTOR_L1=$REVERSE_EXECUTOR_L1"

if [[ -z "$REVERSE_NFT_L1" || -z "$REVERSE_EXECUTOR_L1" ]]; then
    echo "ERROR: L1 deployment failed"
    exit 1
fi

# ══════════════════════════════════════════════
#  Step 2: Create L2-side proxy for ReverseExecutorL1 on L2
# ══════════════════════════════════════════════
echo ""
echo "====== Step 2: Create L2-side proxy for ReverseExecutorL1 ======"
PROXY_OUTPUT=$(forge script "$SCRIPT:CreateReverseExecutorProxy" \
    --rpc-url "$L2_RPC" --broadcast --private-key "$PK" \
    --sig "run(address,address)" "$MANAGER_L2" "$REVERSE_EXECUTOR_L1" 2>&1)
echo "$PROXY_OUTPUT" | grep -E "REVERSE_|Error|error" || true
REVERSE_EXECUTOR_L1_PROXY=$(extract "$PROXY_OUTPUT" "REVERSE_EXECUTOR_L1_PROXY")
echo "REVERSE_EXECUTOR_L1_PROXY=$REVERSE_EXECUTOR_L1_PROXY"

if [[ -z "$REVERSE_EXECUTOR_L1_PROXY" ]]; then
    echo "ERROR: Proxy creation failed"
    exit 1
fi

# ══════════════════════════════════════════════
#  Step 3: Deploy FlashLoanL2Reverse + ReverseExecutorL2 on L2
# ══════════════════════════════════════════════
echo ""
echo "====== Step 3: Deploy L2 contracts (FlashLoanL2Reverse + ReverseExecutorL2) ======"
L2_OUTPUT=$(forge script "$SCRIPT:DeployReverseFlashLoanL2Full" \
    --rpc-url "$L2_RPC" --broadcast --private-key "$PK" \
    --sig "run(address,address,address,address,address,address,address,uint256,uint256)" \
    "$BRIDGE_L2" \
    "$REVERSE_EXECUTOR_L1_PROXY" \
    "$REVERSE_EXECUTOR_L1" \
    "$TOKEN" \
    "$WRAPPED_TOKEN_L2" \
    "$REVERSE_NFT_L1" \
    "$BRIDGE_L1" \
    "$L1_ROLLUP_ID" \
    "$L2_ROLLUP_ID" 2>&1)
echo "$L2_OUTPUT" | grep -E "FLASH_LOAN|REVERSE_|Error|error" || true
FLASH_LOAN_L2_REVERSE_POOL=$(extract "$L2_OUTPUT" "FLASH_LOAN_L2_REVERSE_POOL")
REVERSE_EXECUTOR_L2=$(extract "$L2_OUTPUT" "REVERSE_EXECUTOR_L2")
echo "FLASH_LOAN_L2_REVERSE_POOL=$FLASH_LOAN_L2_REVERSE_POOL"
echo "REVERSE_EXECUTOR_L2=$REVERSE_EXECUTOR_L2"

if [[ -z "$FLASH_LOAN_L2_REVERSE_POOL" || -z "$REVERSE_EXECUTOR_L2" ]]; then
    echo "ERROR: L2 deployment failed"
    exit 1
fi

# ══════════════════════════════════════════════
#  Step 4: Fund the L2 pool by bridging TestToken from L1 to L2
# ══════════════════════════════════════════════
echo ""
echo "====== Step 4: Fund FlashLoanL2Reverse pool (bridge 10k tokens L1->L2) ======"

# Use dev key #5 ($PK) for funding — deploy.sh pre-transfers 10k TestToken to it
# during L1 deployment (before builder starts using key #0 for postBatch).
FUNDER_ADDR=$(cast wallet address --private-key "$PK")
TOKEN_BALANCE=$(cast call --rpc-url "$L1_RPC" "$TOKEN" \
    "balanceOf(address)(uint256)" "$FUNDER_ADDR" 2>&1 || echo "0")
echo "Dev#5 TestToken balance on L1: $TOKEN_BALANCE"

FUND_AMOUNT="10000000000000000000000"  # 10,000 tokens (18 decimals)

# Approve Bridge L1 to spend tokens
cast send --rpc-url "$L1_RPC" --private-key "$PK" \
    "$TOKEN" \
    "approve(address,uint256)" \
    "$BRIDGE_L1" "$FUND_AMOUNT" > /dev/null
echo "Approved Bridge L1 to spend $FUND_AMOUNT tokens"

# Bridge tokens to the L2 pool address.
# MUST go through the L1 proxy so the builder detects the cross-chain call
# and creates execution entries. Direct L1 calls fail with ExecutionNotInCurrentBlock
# because Rollups.sol requires a postBatch in the same block.
# The L1 proxy runs on the builder. Default to builder:9556 for Docker.
# Override via --l1-proxy-url for non-Docker usage.
L1_PROXY_URL="${L1_PROXY_URL:-http://builder:9556}"
# Use generous gas limit — gas estimation fails for cross-chain txs because
# the execution table isn't loaded at estimation time. The proxy returns a
# calldata-based estimate, but bridgeTokens needs extra gas for CREATE2 proxy
# deployment + token transfer + Rollups.executeCrossChainCall.
cast send --rpc-url "$L1_PROXY_URL" --private-key "$PK" \
    --gas-limit 500000 \
    "$BRIDGE_L1" \
    "bridgeTokens(address,uint256,uint256,address)" \
    "$TOKEN" "$FUND_AMOUNT" "$L2_ROLLUP_ID" "$FLASH_LOAN_L2_REVERSE_POOL" > /dev/null
echo "Bridged 10,000 tokens to FlashLoanL2Reverse pool on L2 (rollupId=$L2_ROLLUP_ID)"
echo "NOTE: Pool will receive wrapped tokens after builder processes the bridge."

# ══════════════════════════════════════════════
#  Summary
# ══════════════════════════════════════════════
echo ""
echo "====== Deployment Complete ======"
echo ""
echo "L1 Contracts:"
echo "  REVERSE_NFT_L1=$REVERSE_NFT_L1"
echo "  REVERSE_EXECUTOR_L1=$REVERSE_EXECUTOR_L1"
echo ""
echo "L2 Contracts:"
echo "  REVERSE_EXECUTOR_L1_PROXY=$REVERSE_EXECUTOR_L1_PROXY  (proxy on L2 for L1 executor)"
echo "  FLASH_LOAN_L2_REVERSE_POOL=$FLASH_LOAN_L2_REVERSE_POOL"
echo "  REVERSE_EXECUTOR_L2=$REVERSE_EXECUTOR_L2"
echo ""
echo "To trigger the reverse flash loan (after L2 pool receives wrapped tokens):"
echo "  cast send --rpc-url \$L2_RPC --private-key \$PK $REVERSE_EXECUTOR_L2 'execute()'"
echo ""
echo "Or use the test script:"
echo "  bash scripts/e2e/test-l2-to-l1-flash-loan.sh \\"
echo "    --l1-rpc \$L1_RPC --l2-rpc \$L2_RPC --pk \$PK \\"
echo "    --reverse-executor-l2 $REVERSE_EXECUTOR_L2 \\"
echo "    --flash-loan-pool $FLASH_LOAN_L2_REVERSE_POOL \\"
echo "    --wrapped-token \$WRAPPED_TOKEN_L2"

# ══════════════════════════════════════════════
#  Append reverse flash loan addresses to rollup.env
# ══════════════════════════════════════════════
ROLLUP_ENV="/shared/rollup.env"
if [ -f "$ROLLUP_ENV" ]; then
    echo ""
    echo "====== Writing reverse flash loan addresses to rollup.env ======"
    # Remove any pre-existing entries to avoid duplicates on re-run
    sed -i '/^REVERSE_NFT_L1=/d;/^REVERSE_EXECUTOR_L1=/d;/^REVERSE_EXECUTOR_L2=/d' "$ROLLUP_ENV"
    cat >> "$ROLLUP_ENV" <<ENVEOF
REVERSE_NFT_L1=${REVERSE_NFT_L1}
REVERSE_EXECUTOR_L1=${REVERSE_EXECUTOR_L1}
REVERSE_EXECUTOR_L2=${REVERSE_EXECUTOR_L2}
ENVEOF
    echo "Appended REVERSE_NFT_L1, REVERSE_EXECUTOR_L1, REVERSE_EXECUTOR_L2 to $ROLLUP_ENV"
fi
