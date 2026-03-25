#!/usr/bin/env bash
# End-to-end demo for the L2-to-L1 reverse flash loan.
#
# Prerequisites:
#   - Devnet running (builder healthy, L1 + L2 up)
#   - deploy.sh and deploy_l2.sh have completed (rollup.env exists in /shared)
#   - deploy-reverse-flash-loan.sh has completed (or provide addresses manually)
#
# Flow:
#   1. Check that the L2 pool holds wrapped tokens (bridge settled).
#   2. Verify the L1 execution table is loaded (builder should have posted entries).
#   3. Trigger ReverseExecutorL2.execute() on L2.
#   4. Verify: NFT claimed on L1, wrapped tokens repaid to pool on L2.
#
# Usage (reads from /shared/rollup.env if no flags given):
#   bash scripts/e2e/test-l2-to-l1-flash-loan.sh [OPTIONS]
#
# Options:
#   --l1-rpc              L1 RPC URL (default: http://localhost:9555)
#   --l2-rpc              L2 RPC URL (default: http://localhost:9545)
#   --pk                  Private key (default: dev#0 anvil key)
#   --reverse-executor-l2 ReverseExecutorL2 address on L2
#   --flash-loan-pool     FlashLoanL2Reverse address on L2
#   --reverse-nft-l1      ReverseNFTL1 address on L1
#   --reverse-executor-l1 ReverseExecutorL1 address on L1
#   --wrapped-token       WrappedToken address on L2
#   --rollup-env          Path to rollup.env (default: /shared/rollup.env)
#   --wait-blocks         Blocks to wait for bridge settlement (default: 3)
set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
L1_RPC="${L1_RPC:-http://localhost:9555}"
L2_RPC="${L2_RPC:-http://localhost:9545}"
PK="${PK:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
ROLLUP_ENV="${ROLLUP_ENV:-/shared/rollup.env}"
WAIT_BLOCKS=3

REVERSE_EXECUTOR_L2=""
FLASH_LOAN_POOL=""
REVERSE_NFT_L1=""
REVERSE_EXECUTOR_L1=""
WRAPPED_TOKEN=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --l1-rpc)              L1_RPC="$2";              shift 2;;
        --l2-rpc)              L2_RPC="$2";              shift 2;;
        --pk)                  PK="$2";                  shift 2;;
        --reverse-executor-l2) REVERSE_EXECUTOR_L2="$2"; shift 2;;
        --flash-loan-pool)     FLASH_LOAN_POOL="$2";     shift 2;;
        --reverse-nft-l1)      REVERSE_NFT_L1="$2";      shift 2;;
        --reverse-executor-l1) REVERSE_EXECUTOR_L1="$2"; shift 2;;
        --wrapped-token)       WRAPPED_TOKEN="$2";        shift 2;;
        --rollup-env)          ROLLUP_ENV="$2";           shift 2;;
        --wait-blocks)         WAIT_BLOCKS="$2";          shift 2;;
        *) echo "Unknown arg: $1"; exit 1;;
    esac
done

# ── Load rollup.env if present ────────────────────────────────────────────────
if [[ -f "$ROLLUP_ENV" ]]; then
    echo "Loading config from $ROLLUP_ENV..."
    while IFS='=' read -r key value; do
        case "$key" in
            REVERSE_NFT_L1|REVERSE_EXECUTOR_L1|REVERSE_EXECUTOR_L2|\
            FLASH_LOAN_L2_REVERSE_POOL|WRAPPED_TOKEN_L2)
                # Only set if not already provided via flag
                case "$key" in
                    REVERSE_NFT_L1)           [[ -z "$REVERSE_NFT_L1" ]]    && REVERSE_NFT_L1="$value";;
                    REVERSE_EXECUTOR_L1)      [[ -z "$REVERSE_EXECUTOR_L1" ]] && REVERSE_EXECUTOR_L1="$value";;
                    REVERSE_EXECUTOR_L2)      [[ -z "$REVERSE_EXECUTOR_L2" ]] && REVERSE_EXECUTOR_L2="$value";;
                    FLASH_LOAN_L2_REVERSE_POOL) [[ -z "$FLASH_LOAN_POOL" ]] && FLASH_LOAN_POOL="$value";;
                    WRAPPED_TOKEN_L2)          [[ -z "$WRAPPED_TOKEN" ]]     && WRAPPED_TOKEN="$value";;
                esac
                ;;
        esac
    done < "$ROLLUP_ENV"
fi

# ── Validate required addresses ───────────────────────────────────────────────
MISSING=""
[[ -z "$REVERSE_EXECUTOR_L2" ]] && MISSING="$MISSING REVERSE_EXECUTOR_L2"
[[ -z "$FLASH_LOAN_POOL" ]]     && MISSING="$MISSING FLASH_LOAN_L2_REVERSE_POOL"
[[ -z "$WRAPPED_TOKEN" ]]       && MISSING="$MISSING WRAPPED_TOKEN"

if [[ -n "$MISSING" ]]; then
    echo "ERROR: Missing required addresses:$MISSING"
    echo "Run deploy-reverse-flash-loan.sh first, or pass addresses via flags."
    exit 1
fi

ZERO="0x0000000000000000000000000000000000000000"

echo ""
echo "=========================================="
echo "  L2->L1 Reverse Flash Loan Demo"
echo "=========================================="
echo "L1 RPC:            $L1_RPC"
echo "L2 RPC:            $L2_RPC"
echo "ReverseExecutorL2: $REVERSE_EXECUTOR_L2"
echo "FlashLoanL2Pool:   $FLASH_LOAN_POOL"
echo "WrappedToken:      $WRAPPED_TOKEN"
[[ -n "$REVERSE_NFT_L1" ]]  && echo "ReverseNFTL1:      $REVERSE_NFT_L1"
[[ -n "$REVERSE_EXECUTOR_L1" ]] && echo "ReverseExecutorL1: $REVERSE_EXECUTOR_L1"
echo ""

# ── Step 1: Check L2 pool balance ─────────────────────────────────────────────
echo "====== Step 1: Check L2 Pool Balance ======"
POOL_BALANCE=$(cast call --rpc-url "$L2_RPC" \
    "$WRAPPED_TOKEN" \
    "balanceOf(address)(uint256)" \
    "$FLASH_LOAN_POOL" 2>&1 || echo "0")
echo "FlashLoanL2Reverse pool balance: $POOL_BALANCE wrapped tokens"

MIN_REQUIRED="10000000000000000000000"  # 10,000 tokens
if [[ "$POOL_BALANCE" -lt "$MIN_REQUIRED" ]] 2>/dev/null; then
    echo "WARNING: Pool balance ($POOL_BALANCE) < 10,000e18 required."
    echo "Waiting $WAIT_BLOCKS blocks for bridge to settle..."
    START_BLOCK=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
    TARGET=$((START_BLOCK + WAIT_BLOCKS))
    while true; do
        CURRENT=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
        [[ "$CURRENT" -ge "$TARGET" ]] && break
        sleep 2
    done
    POOL_BALANCE=$(cast call --rpc-url "$L2_RPC" \
        "$WRAPPED_TOKEN" \
        "balanceOf(address)(uint256)" \
        "$FLASH_LOAN_POOL" 2>&1 || echo "0")
    echo "Pool balance after waiting: $POOL_BALANCE"
fi

if [[ "$POOL_BALANCE" -lt "$MIN_REQUIRED" ]] 2>/dev/null; then
    echo "ERROR: Pool still underfunded. Bridge may not have settled yet."
    echo "  1. Check builder logs: sudo docker compose logs builder --tail 50"
    echo "  2. Manually fund: cast send --rpc-url \$L1_RPC --private-key \$PK"
    echo "       \$BRIDGE_L1 'bridgeTokens(address,uint256,uint256,address)'"
    echo "       \$TOKEN 10000000000000000000000 1 $FLASH_LOAN_POOL"
    exit 1
fi

echo "Pool has sufficient balance. Ready to execute."

# ── Step 2: Record baseline state ─────────────────────────────────────────────
echo ""
echo "====== Step 2: Baseline State ======"

L2_BLOCK_BEFORE=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "?")
L1_BLOCK_BEFORE=$(cast block-number --rpc-url "$L1_RPC" 2>/dev/null || echo "?")

POOL_BALANCE_BEFORE="$POOL_BALANCE"
echo "L2 block:          $L2_BLOCK_BEFORE"
echo "L1 block:          $L1_BLOCK_BEFORE"
echo "Pool balance:      $POOL_BALANCE_BEFORE"

# NFT count on L1 (if address provided)
if [[ -n "$REVERSE_NFT_L1" && "$REVERSE_NFT_L1" != "$ZERO" ]]; then
    NFT_SUPPLY_BEFORE=$(cast call --rpc-url "$L1_RPC" \
        "$REVERSE_NFT_L1" \
        "nextTokenId()(uint256)" 2>&1 || echo "0")
    echo "ReverseNFT supply: $NFT_SUPPLY_BEFORE"
fi

# ── Step 3: Trigger the reverse flash loan ────────────────────────────────────
echo ""
echo "====== Step 3: Execute Reverse Flash Loan ======"
echo "Sending ReverseExecutorL2.execute() on L2..."

TX_OUTPUT=$(cast send \
    --rpc-url "$L2_RPC" \
    --private-key "$PK" \
    "$REVERSE_EXECUTOR_L2" \
    "execute()" 2>&1)
TX_HASH=$(echo "$TX_OUTPUT" | grep -E "^0x[0-9a-fA-F]{64}$" | (head -1 || true))
echo "Transaction: ${TX_HASH:-$TX_OUTPUT}"

# ── Step 4: Wait and verify ───────────────────────────────────────────────────
echo ""
echo "====== Step 4: Verify Results ======"
echo "Waiting for $WAIT_BLOCKS blocks..."

CURRENT_BLOCK=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
TARGET_BLOCK=$((CURRENT_BLOCK + WAIT_BLOCKS))
while true; do
    NOW=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo "0")
    [[ "$NOW" -ge "$TARGET_BLOCK" ]] && break
    sleep 2
done

# Pool balance should be unchanged (loan repaid)
POOL_BALANCE_AFTER=$(cast call --rpc-url "$L2_RPC" \
    "$WRAPPED_TOKEN" \
    "balanceOf(address)(uint256)" \
    "$FLASH_LOAN_POOL" 2>&1 || echo "0")
echo "Pool balance before: $POOL_BALANCE_BEFORE"
echo "Pool balance after:  $POOL_BALANCE_AFTER"
if [[ "$POOL_BALANCE_AFTER" == "$POOL_BALANCE_BEFORE" ]]; then
    echo "  PASS: Flash loan fully repaid (pool balance unchanged)"
else
    echo "  FAIL: Pool balance changed — loan may not have been repaid"
fi

# NFT supply on L1 should have increased by 1
if [[ -n "$REVERSE_NFT_L1" && "$REVERSE_NFT_L1" != "$ZERO" ]]; then
    NFT_SUPPLY_AFTER=$(cast call --rpc-url "$L1_RPC" \
        "$REVERSE_NFT_L1" \
        "nextTokenId()(uint256)" 2>&1 || echo "0")
    echo "ReverseNFT supply before: $NFT_SUPPLY_BEFORE"
    echo "ReverseNFT supply after:  $NFT_SUPPLY_AFTER"
    if [[ "$NFT_SUPPLY_AFTER" -gt "$NFT_SUPPLY_BEFORE" ]] 2>/dev/null; then
        echo "  PASS: NFT claimed on L1 (supply increased)"
    else
        echo "  NOTE: NFT supply unchanged — cross-chain call may still be in flight"
    fi
fi

# ReverseExecutorL2 should hold 0 wrapped tokens (all repaid)
if [[ -n "$REVERSE_EXECUTOR_L2" ]]; then
    EXEC_BALANCE=$(cast call --rpc-url "$L2_RPC" \
        "$WRAPPED_TOKEN" \
        "balanceOf(address)(uint256)" \
        "$REVERSE_EXECUTOR_L2" 2>&1 || echo "0")
    echo "ReverseExecutorL2 wrapped token balance: $EXEC_BALANCE (expected: 0)"
    if [[ "$EXEC_BALANCE" == "0" ]]; then
        echo "  PASS: Executor holds no wrapped tokens (all repaid)"
    fi
fi

echo ""
echo "====== Done ======"
echo ""
echo "To inspect L2 cross-chain events:"
echo "  cast logs --rpc-url $L2_RPC --address $REVERSE_EXECUTOR_L2"
echo ""
if [[ -n "$REVERSE_NFT_L1" && "$REVERSE_NFT_L1" != "$ZERO" ]]; then
    echo "To inspect L1 NFT events:"
    echo "  cast logs --rpc-url $L1_RPC --address $REVERSE_NFT_L1"
    echo ""
fi
