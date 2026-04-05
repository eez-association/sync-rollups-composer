#!/usr/bin/env bash
# Verify all deployed contracts on Blockscout explorers.
# Uses forge verify-contract which handles standard-json-input verification.
#
# Runs as a Docker service — waits for explorers to be ready, then verifies.
# Retries each verification up to 10 times (Blockscout needs time to index).
#
# Usage: verify-contracts.sh
# Requires: forge, cast, curl
set -euo pipefail

SHARED_DIR="${SHARED_DIR:-/shared}"
L1_EXPLORER="${L1_EXPLORER:-http://l1-explorer:4000}"
L2_EXPLORER="${L2_EXPLORER:-http://l2-explorer:4000}"
CONTRACTS_DIR="${CONTRACTS_DIR:-/app/contracts}"
SYNC_DIR="$CONTRACTS_DIR/sync-rollups-protocol"

log() { echo "[verify] $*"; }

# ─── Wait for explorers to be ready ──────────────────────────────────────
log "Waiting 90s for Blockscout explorers to initialize and index contracts..."
sleep 90

# ─── Load addresses ───────────────────────────────────────────────────────
if [ -f "$SHARED_DIR/rollup.env" ]; then
    ROLLUPS_ADDRESS=$(grep "^ROLLUPS_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    VERIFIER_ADDRESS=$(grep "^VERIFIER_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    L2_CONTEXT_ADDRESS=$(grep "^L2_CONTEXT_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    CROSS_CHAIN_MANAGER_ADDRESS=$(grep "^CROSS_CHAIN_MANAGER_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    BUILDER_ADDRESS=$(grep "^BUILDER_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    ROLLUP_ID=$(grep "^ROLLUP_ID=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "1")
    BRIDGE_ADDRESS=$(grep "^BRIDGE_L1_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    BRIDGE_L2_ADDRESS=$(grep "^BRIDGE_L2_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    FLASH_TOKEN_ADDRESS=$(grep "^FLASH_TOKEN_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    FLASH_POOL_ADDRESS=$(grep "^FLASH_POOL_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    FLASH_EXECUTOR_L2_ADDRESS=$(grep "^FLASH_EXECUTOR_L2_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    FLASH_NFT_ADDRESS=$(grep "^FLASH_NFT_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    FLASH_EXECUTOR_L2_PROXY_ADDRESS=$(grep "^FLASH_EXECUTOR_L2_PROXY_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    FLASH_EXECUTOR_L1_ADDRESS=$(grep "^FLASH_EXECUTOR_L1_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_WETH_ADDRESS=$(grep "^AGG_WETH_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_USDC_ADDRESS=$(grep "^AGG_USDC_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_L1_AMM_ADDRESS=$(grep "^AGG_L1_AMM_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_AGGREGATOR_ADDRESS=$(grep "^AGG_AGGREGATOR_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_L2_EXECUTOR_ADDRESS=$(grep "^AGG_L2_EXECUTOR_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_L2_AMM_ADDRESS=$(grep "^AGG_L2_AMM_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_L2_EXECUTOR_PROXY_ADDRESS=$(grep "^AGG_L2_EXECUTOR_PROXY_ADDRESS=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_WRAPPED_WETH_L2=$(grep "^AGG_WRAPPED_WETH_L2=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
    AGG_WRAPPED_USDC_L2=$(grep "^AGG_WRAPPED_USDC_L2=" "$SHARED_DIR/rollup.env" | cut -d= -f2 || echo "")
else
    ROLLUPS_ADDRESS="${ROLLUPS_ADDRESS:-}"
    VERIFIER_ADDRESS="${VERIFIER_ADDRESS:-}"
    L2_CONTEXT_ADDRESS="${L2_CONTEXT_ADDRESS:-}"
    CROSS_CHAIN_MANAGER_ADDRESS="${CROSS_CHAIN_MANAGER_ADDRESS:-}"
    BUILDER_ADDRESS="${BUILDER_ADDRESS:-}"
    ROLLUP_ID="${ROLLUP_ID:-1}"
    BRIDGE_ADDRESS="${BRIDGE_ADDRESS:-}"
    BRIDGE_L2_ADDRESS="${BRIDGE_L2_ADDRESS:-}"
    FLASH_TOKEN_ADDRESS="${FLASH_TOKEN_ADDRESS:-}"
    FLASH_POOL_ADDRESS="${FLASH_POOL_ADDRESS:-}"
    FLASH_EXECUTOR_L2_ADDRESS="${FLASH_EXECUTOR_L2_ADDRESS:-}"
    FLASH_NFT_ADDRESS="${FLASH_NFT_ADDRESS:-}"
    FLASH_EXECUTOR_L2_PROXY_ADDRESS="${FLASH_EXECUTOR_L2_PROXY_ADDRESS:-}"
    FLASH_EXECUTOR_L1_ADDRESS="${FLASH_EXECUTOR_L1_ADDRESS:-}"
    AGG_WETH_ADDRESS="${AGG_WETH_ADDRESS:-}"
    AGG_USDC_ADDRESS="${AGG_USDC_ADDRESS:-}"
    AGG_L1_AMM_ADDRESS="${AGG_L1_AMM_ADDRESS:-}"
    AGG_AGGREGATOR_ADDRESS="${AGG_AGGREGATOR_ADDRESS:-}"
    AGG_L2_EXECUTOR_ADDRESS="${AGG_L2_EXECUTOR_ADDRESS:-}"
    AGG_L2_AMM_ADDRESS="${AGG_L2_AMM_ADDRESS:-}"
    AGG_L2_EXECUTOR_PROXY_ADDRESS="${AGG_L2_EXECUTOR_PROXY_ADDRESS:-}"
    AGG_WRAPPED_WETH_L2="${AGG_WRAPPED_WETH_L2:-}"
    AGG_WRAPPED_USDC_L2="${AGG_WRAPPED_USDC_L2:-}"
fi

log "Rollups: $ROLLUPS_ADDRESS"
log "Verifier: $VERIFIER_ADDRESS"
log "L2Context: $L2_CONTEXT_ADDRESS"
log "CCM: $CROSS_CHAIN_MANAGER_ADDRESS"
log "Builder: $BUILDER_ADDRESS"
log "Bridge L1: $BRIDGE_ADDRESS"
log "Bridge L2: $BRIDGE_L2_ADDRESS"

# Detect L1 chain ID dynamically (reth --dev = 1337, anvil = 31337)
L1_RPC_URL="${L1_RPC_URL:-http://l1:8545}"
L1_CHAIN_ID=$(cast chain-id --rpc-url "$L1_RPC_URL" 2>/dev/null || echo "1337")
log "L1 chain ID: $L1_CHAIN_ID"

# ─── Verification helper with retries ─────────────────────────────────────
verify_with_retry() {
    local name="$1"
    shift
    local max_retries=10
    for attempt in $(seq 1 $max_retries); do
        log "Verifying $name (attempt $attempt/$max_retries)..."
        OUTPUT=$("$@" 2>&1) || true
        if echo "$OUTPUT" | grep -qi "already verified\|success\|pass.*verified\|successfully verified"; then
            log "✓ $name verified"
            return 0
        fi
        log "  Result: $(echo "$OUTPUT" | tail -3)"
        if [ "$attempt" -lt "$max_retries" ]; then
            sleep 15
        fi
    done
    log "✗ $name verification failed after $max_retries attempts"
    return 0  # Don't fail the whole script
}

# ─── L1 Contracts ─────────────────────────────────────────────────────────
log ""
log "═══ L1 Contracts (chain $L1_CHAIN_ID) ═══"

# Try MockECDSAVerifier first (deployed when MOCK_VERIFIER=true); fall back to tmpECDSAVerifier.
VERIFIER_VERIFIED=false
cd "$CONTRACTS_DIR"
MOCK_OUTPUT=$(forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
    --verifier-url "$L1_EXPLORER/api/" \
    "$VERIFIER_ADDRESS" MockECDSAVerifier.sol:MockECDSAVerifier 2>&1) || true
if echo "$MOCK_OUTPUT" | grep -qi "already verified\|success\|pass.*verified\|successfully verified"; then
    log "✓ MockECDSAVerifier verified"
    VERIFIER_VERIFIED=true
fi
if [ "$VERIFIER_VERIFIED" = "false" ]; then
    cd "$SYNC_DIR"
    verify_with_retry "tmpECDSAVerifier" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$VERIFIER_ADDRESS" src/verifier/tmpECDSAVerifier.sol:tmpECDSAVerifier
fi

verify_with_retry "Rollups" \
    forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
    --verifier-url "$L1_EXPLORER/api/" \
    "$ROLLUPS_ADDRESS" src/Rollups.sol:Rollups \
    --constructor-args "$(cast abi-encode 'constructor(address,uint256)' "$VERIFIER_ADDRESS" 1)"

# Bridge L1 — no constructor args, initialized via initialize() after deployment.
# IMPORTANT: Bridge is compiled from $CONTRACTS_DIR (solc 0.8.33), not $SYNC_DIR (solc 0.8.28).
# The deploy script runs `forge build` from contracts/, so verification must use the same root.
if [ -n "$BRIDGE_ADDRESS" ] && [ "$BRIDGE_ADDRESS" != "0x0000000000000000000000000000000000000000" ]; then
    cd "$CONTRACTS_DIR"
    verify_with_retry "Bridge (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$BRIDGE_ADDRESS" sync-rollups-protocol/src/periphery/Bridge.sol:Bridge
    cd "$SYNC_DIR"
fi

# ─── L2 Contracts (chain 42069) ──────────────────────────────────────────
log ""
log "═══ L2 Contracts ═══"

# L2Context is deployed by the builder at block 1 via CREATE(builder, nonce=0).
cd "$CONTRACTS_DIR"
verify_with_retry "L2Context" \
    forge verify-contract --chain-id 42069 --verifier blockscout \
    --verifier-url "$L2_EXPLORER/api/" \
    "$L2_CONTEXT_ADDRESS" L2Context.sol:L2Context \
    --constructor-args "$(cast abi-encode 'constructor(address)' "$BUILDER_ADDRESS")"

# CCM is deployed by the builder at block 1 via CREATE(builder, nonce=1).
cd "$SYNC_DIR"
verify_with_retry "CrossChainManagerL2" \
    forge verify-contract --chain-id 42069 --verifier blockscout \
    --verifier-url "$L2_EXPLORER/api/" \
    "$CROSS_CHAIN_MANAGER_ADDRESS" src/CrossChainManagerL2.sol:CrossChainManagerL2 \
    --constructor-args "$(cast abi-encode 'constructor(uint256,address)' "$ROLLUP_ID" "$BUILDER_ADDRESS")"

# Bridge L2 — no constructor args, deployed by builder at block 1 (nonce=2).
# Initialized via initialize(manager=CCM, rollupId=1, admin=builder) in the same block.
# IMPORTANT: Bridge is compiled from $CONTRACTS_DIR (solc 0.8.33), not $SYNC_DIR (solc 0.8.28).
# WrappedToken is NOT verified here — it is deployed dynamically by Bridge via CREATE2
# when a token is first bridged. Its address is not known at deploy time.
if [ -n "$BRIDGE_L2_ADDRESS" ] && [ "$BRIDGE_L2_ADDRESS" != "0x0000000000000000000000000000000000000000" ]; then
    cd "$CONTRACTS_DIR"
    verify_with_retry "Bridge (L2)" \
        forge verify-contract --chain-id 42069 --verifier blockscout \
        --verifier-url "$L2_EXPLORER/api/" \
        "$BRIDGE_L2_ADDRESS" sync-rollups-protocol/src/periphery/Bridge.sol:Bridge
    cd "$SYNC_DIR"
fi

# ─── Counter contract (deployed at runtime by crosschain-tx-sender) ───────
# Wait for counter.env to appear (crosschain-tx-sender writes it after deploy)
if [ ! -f "$SHARED_DIR/counter.env" ]; then
    log ""
    log "Waiting for Counter contract to be deployed (up to 5 min)..."
    WAITED=0
    while [ ! -f "$SHARED_DIR/counter.env" ] && [ "$WAITED" -lt 300 ]; do
        sleep 5
        WAITED=$((WAITED + 5))
    done
fi

if [ -f "$SHARED_DIR/counter.env" ]; then
    COUNTER_ADDRESS=$(grep "^COUNTER_ADDRESS=" "$SHARED_DIR/counter.env" | cut -d= -f2 || echo "")
    if [ -n "$COUNTER_ADDRESS" ]; then
        log ""
        log "═══ Counter Contract (L2) ═══"
        cd "$SYNC_DIR"
        verify_with_retry "Counter" \
            forge verify-contract --chain-id 42069 --verifier blockscout \
            --verifier-url "$L2_EXPLORER/api/" \
            "$COUNTER_ADDRESS" test/mocks/CounterContracts.sol:Counter

        # ─── L1 CrossChainProxy (deployed by crosschain-tx-sender via Rollups.createCrossChainProxy) ───
        log ""
        log "═══ L1 CrossChainProxy ═══"
        # Use Rollups.computeCrossChainProxyAddress(originalAddress, originalRollupId)
        L1_PROXY_ADDRESS=$(cast call --rpc-url "$L1_RPC_URL" "$ROLLUPS_ADDRESS" \
            "computeCrossChainProxyAddress(address,uint256)(address)" \
            "$COUNTER_ADDRESS" "$ROLLUP_ID" 2>/dev/null || echo "")
        if [ -n "$L1_PROXY_ADDRESS" ]; then
            L1_PROXY_CODE=$(cast code --rpc-url "$L1_RPC_URL" "$L1_PROXY_ADDRESS" 2>/dev/null || echo "0x")
            if [ "$L1_PROXY_CODE" != "0x" ] && [ ${#L1_PROXY_CODE} -gt 2 ]; then
                log "L1 CrossChainProxy at: $L1_PROXY_ADDRESS"
                cd "$SYNC_DIR"
                verify_with_retry "L1 CrossChainProxy" \
                    forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
                    --verifier-url "$L1_EXPLORER/api/" \
                    "$L1_PROXY_ADDRESS" src/CrossChainProxy.sol:CrossChainProxy \
                    --constructor-args "$(cast abi-encode 'constructor(address,address,uint256)' "$ROLLUPS_ADDRESS" "$COUNTER_ADDRESS" "$ROLLUP_ID")"
            else
                log "L1 CrossChainProxy not deployed yet at $L1_PROXY_ADDRESS, skipping."
            fi
        else
            log "Could not compute L1 CrossChainProxy address, skipping."
        fi

        # ─── L2 CrossChainProxy (deployed by CCM via CREATE2 when first cross-chain call arrives) ───
        log ""
        log "═══ L2 CrossChainProxy ═══"
        # The L2 proxy represents the L1 sender (crosschain-tx-sender = dev account #4).
        # constructor(manager=CCM, originalAddress=sender, originalRollupId=0)
        CROSSCHAIN_SENDER="0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
        L2_RPC_URL="${L2_RPC_URL:-http://builder:8545}"
        # Use CCM.computeCrossChainProxyAddress(originalAddress, originalRollupId)
        L2_PROXY_ADDRESS=$(cast call --rpc-url "$L2_RPC_URL" "$CROSS_CHAIN_MANAGER_ADDRESS" \
            "computeCrossChainProxyAddress(address,uint256)(address)" \
            "$CROSSCHAIN_SENDER" 0 2>/dev/null || echo "")
        if [ -n "$L2_PROXY_ADDRESS" ]; then
            L2_PROXY_CODE=$(cast code --rpc-url "$L2_RPC_URL" "$L2_PROXY_ADDRESS" 2>/dev/null || echo "0x")
            if [ "$L2_PROXY_CODE" != "0x" ] && [ ${#L2_PROXY_CODE} -gt 2 ]; then
                log "L2 CrossChainProxy at: $L2_PROXY_ADDRESS"
                cd "$SYNC_DIR"
                verify_with_retry "L2 CrossChainProxy" \
                    forge verify-contract --chain-id 42069 --verifier blockscout \
                    --verifier-url "$L2_EXPLORER/api/" \
                    "$L2_PROXY_ADDRESS" src/CrossChainProxy.sol:CrossChainProxy \
                    --constructor-args "$(cast abi-encode 'constructor(address,address,uint256)' "$CROSS_CHAIN_MANAGER_ADDRESS" "$CROSSCHAIN_SENDER" 0)"
            else
                log "L2 CrossChainProxy not deployed yet at $L2_PROXY_ADDRESS, skipping."
            fi
        else
            log "Could not compute L2 CrossChainProxy address, skipping."
        fi
    fi
else
    log "Counter contract not deployed yet, skipping verification."
fi

# ─── Flash Loan Contracts ─────────────────────────────────────────────────
if [ -n "$FLASH_EXECUTOR_L1_ADDRESS" ] && [ "$FLASH_EXECUTOR_L1_ADDRESS" != "0x0000000000000000000000000000000000000000" ]; then
    log ""
    log "═══ Flash Loan Contracts (L1, chain $L1_CHAIN_ID) ═══"

    cd "$SYNC_DIR"

    # TestToken — deployed from test/IntegrationTestFlashLoan.t.sol, no constructor args
    verify_with_retry "TestToken (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$FLASH_TOKEN_ADDRESS" test/IntegrationTestFlashLoan.t.sol:TestToken

    # FlashLoan pool — no constructor args
    verify_with_retry "FlashLoan pool (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$FLASH_POOL_ADDRESS" src/periphery/defiMock/FlashLoan.sol:FlashLoan

    # FlashLoanBridgeExecutor (L1) — 9 constructor args
    verify_with_retry "FlashLoanBridgeExecutor (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$FLASH_EXECUTOR_L1_ADDRESS" src/periphery/defiMock/FlashLoanBridgeExecutor.sol:FlashLoanBridgeExecutor \
        --constructor-args "$(cast abi-encode \
            'constructor(address,address,address,address,address,address,address,uint256,address)' \
            "$FLASH_POOL_ADDRESS" \
            "$BRIDGE_ADDRESS" \
            "$FLASH_EXECUTOR_L2_PROXY_ADDRESS" \
            "$FLASH_EXECUTOR_L2_ADDRESS" \
            "0x0000000000000000000000000000000000000000" \
            "$FLASH_NFT_ADDRESS" \
            "$BRIDGE_L2_ADDRESS" \
            1 \
            "$FLASH_TOKEN_ADDRESS")"

    # CrossChainProxy for ExecutorL2 — (manager=ROLLUPS_ADDRESS, originalAddress=FLASH_EXECUTOR_L2_ADDRESS, originalRollupId=1)
    verify_with_retry "CrossChainProxy for ExecutorL2 (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$FLASH_EXECUTOR_L2_PROXY_ADDRESS" src/CrossChainProxy.sol:CrossChainProxy \
        --constructor-args "$(cast abi-encode \
            'constructor(address,address,uint256)' \
            "$ROLLUPS_ADDRESS" \
            "$FLASH_EXECUTOR_L2_ADDRESS" \
            1)"

    log ""
    log "═══ Flash Loan Contracts (L2, chain 42069) ═══"

    # FlashLoanBridgeExecutor (L2) — all-zero constructor args
    verify_with_retry "FlashLoanBridgeExecutor (L2)" \
        forge verify-contract --chain-id 42069 --verifier blockscout \
        --verifier-url "$L2_EXPLORER/api/" \
        "$FLASH_EXECUTOR_L2_ADDRESS" src/periphery/defiMock/FlashLoanBridgeExecutor.sol:FlashLoanBridgeExecutor \
        --constructor-args "$(cast abi-encode \
            'constructor(address,address,address,address,address,address,address,uint256,address)' \
            "0x0000000000000000000000000000000000000000" \
            "0x0000000000000000000000000000000000000000" \
            "0x0000000000000000000000000000000000000000" \
            "0x0000000000000000000000000000000000000000" \
            "0x0000000000000000000000000000000000000000" \
            "0x0000000000000000000000000000000000000000" \
            "0x0000000000000000000000000000000000000000" \
            0 \
            "0x0000000000000000000000000000000000000000")"

    # FlashLoanersNFT (L2) — constructor(address) with zero address
    verify_with_retry "FlashLoanersNFT (L2)" \
        forge verify-contract --chain-id 42069 --verifier blockscout \
        --verifier-url "$L2_EXPLORER/api/" \
        "$FLASH_NFT_ADDRESS" src/periphery/defiMock/FlashLoanersNFT.sol:FlashLoanersNFT \
        --constructor-args "$(cast abi-encode \
            'constructor(address)' \
            "0x0000000000000000000000000000000000000000")"
fi

# ─── Aggregator Contracts ─────────────────────────────────────────────────
ZERO="0x0000000000000000000000000000000000000000"
AGG_DIR="$CONTRACTS_DIR/test-multi-call"
if [ -n "$AGG_AGGREGATOR_ADDRESS" ] && [ "$AGG_AGGREGATOR_ADDRESS" != "$ZERO" ]; then
    log ""
    log "═══ Aggregator Contracts (L1, chain $L1_CHAIN_ID) ═══"

    cd "$AGG_DIR"

    # WETH — no constructor args
    verify_with_retry "WETH (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$AGG_WETH_ADDRESS" src/WETH.sol:WETH

    # MockERC20 (USDC) — constructor(string,string,uint8)
    verify_with_retry "USDC MockERC20 (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$AGG_USDC_ADDRESS" src/MockERC20.sol:MockERC20 \
        --constructor-args "$(cast abi-encode 'constructor(string,string,uint8)' 'USD Coin' 'USDC' 6)"

    # SimpleAMM (L1) — constructor(address,address)
    verify_with_retry "SimpleAMM (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$AGG_L1_AMM_ADDRESS" src/SimpleAMM.sol:SimpleAMM \
        --constructor-args "$(cast abi-encode 'constructor(address,address)' "$AGG_WETH_ADDRESS" "$AGG_USDC_ADDRESS")"

    # CrossChainAggregator — constructor(address,address,address,address,uint256)
    verify_with_retry "CrossChainAggregator (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$AGG_AGGREGATOR_ADDRESS" src/CrossChainAggregator.sol:CrossChainAggregator \
        --constructor-args "$(cast abi-encode \
            'constructor(address,address,address,address,uint256)' \
            "$AGG_L1_AMM_ADDRESS" \
            "$BRIDGE_ADDRESS" \
            "$AGG_WETH_ADDRESS" \
            "$AGG_USDC_ADDRESS" \
            "$ROLLUP_ID")"

    # CrossChainProxy for L2Executor — constructor(address,address,uint256)
    verify_with_retry "CrossChainProxy for L2Executor (L1)" \
        forge verify-contract --chain-id "$L1_CHAIN_ID" --verifier blockscout \
        --verifier-url "$L1_EXPLORER/api/" \
        "$AGG_L2_EXECUTOR_PROXY_ADDRESS" "$SYNC_DIR/src/CrossChainProxy.sol:CrossChainProxy" \
        --constructor-args "$(cast abi-encode \
            'constructor(address,address,uint256)' \
            "$ROLLUPS_ADDRESS" \
            "$AGG_L2_EXECUTOR_ADDRESS" \
            "$ROLLUP_ID")"

    log ""
    log "═══ Aggregator Contracts (L2, chain 42069) ═══"

    # SimpleAMM (L2) — constructor(address,address)
    verify_with_retry "SimpleAMM (L2)" \
        forge verify-contract --chain-id 42069 --verifier blockscout \
        --verifier-url "$L2_EXPLORER/api/" \
        "$AGG_L2_AMM_ADDRESS" src/SimpleAMM.sol:SimpleAMM \
        --constructor-args "$(cast abi-encode 'constructor(address,address)' "$AGG_WRAPPED_WETH_L2" "$AGG_WRAPPED_USDC_L2")"

    # L2Executor — constructor(address,address,address,address)
    verify_with_retry "L2Executor (L2)" \
        forge verify-contract --chain-id 42069 --verifier blockscout \
        --verifier-url "$L2_EXPLORER/api/" \
        "$AGG_L2_EXECUTOR_ADDRESS" src/L2Executor.sol:L2Executor \
        --constructor-args "$(cast abi-encode \
            'constructor(address,address,address,address)' \
            "$AGG_L2_AMM_ADDRESS" \
            "$BRIDGE_L2_ADDRESS" \
            "$AGG_WRAPPED_WETH_L2" \
            "$AGG_WRAPPED_USDC_L2")"
fi

log ""
log "═══ Verification complete ═══"
