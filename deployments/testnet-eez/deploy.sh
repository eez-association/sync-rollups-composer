#!/usr/bin/env bash
# Deploy Rollups contract (from sync-rollups submodule) to L1 and write config to /shared/rollup.env.
# Single deployment script — replaces deploy-inbox.sh and deploy-crosschain.sh.
# Usage: deploy.sh [L1_RPC_URL]
#
# WARNING: The private key below is the well-known anvil default key.
# It is PUBLIC and MUST NEVER be used on mainnet, testnets, or any chain
# where real value is at stake. This script is for LOCAL DEVELOPMENT ONLY.
set -euo pipefail

L1_RPC="${1:-http://l1:8545}"
DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
DEPLOYER_ADDR=$(cast wallet address --private-key "$DEPLOYER_KEY")
SHARED_DIR="${SHARED_DIR:-/shared}"
CONTRACTS_DIR="${CONTRACTS_DIR:-/app/contracts}"

# Builder address (dev account #0) — used to compute deterministic L2 contract addresses.
# L2Context = CREATE(builder, nonce=0), CCM = CREATE(builder, nonce=1).
BUILDER_ADDRESS="${BUILDER_ADDRESS:-0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266}"

# If rollup.env already exists, skip deployment (idempotent)
if [ -f "${SHARED_DIR}/rollup.env" ]; then
    echo "rollup.env already exists — skipping deployment."
    cat "${SHARED_DIR}/rollup.env"
    exit 0
fi

echo "Waiting for L1 at ${L1_RPC}..."
WAIT_COUNT=0
MAX_WAIT=120
until cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1; do
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ "$WAIT_COUNT" -ge "$MAX_WAIT" ]; then
        echo "ERROR: Timed out waiting for L1 after ${MAX_WAIT}s"
        exit 1
    fi
    sleep 1
done
echo "L1 is ready."

# Fund additional addresses on L1 using dev#9 (dedicated funder, not used elsewhere).
# Using dev#0 (deployer/builder) causes nonce collisions with postBatch.
# Using dev#1 (tx-sender) could collide with the tx-sender Docker service.
FUNDER_KEY="0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
for FUND_ADDR in \
    "0xF35960302a07022aBa880DFFaEC2Fdd64d5BF1c1" \
    "0x7B2e78D4dFaABA045A167a70dA285E30E8FcA196" \
    "0x079e29ae526947310d3c088abd5348FFeBdCF27C" \
    "0xCC563C3F7d49bAC23725Ec5aC2B269747e4Cd491" \
    "0xBcd4042DE499D14e55001CcbB24a551F3b954096" \
    "0x71bE63f3384f5fb98995898A86B02Fb2426c5788" \
    "0xFABB0ac9d68B0B445fB7357272Ff202C5651694a" \
    "0x1CBd3b2770909D4e10f157cABC84C7264073C9Ec" \
    "0xdF3e18d64BC6A983f673Ab319CCaE4f1a57C7097" \
    "0xcd3B766CCDd6AE721141F452C550Ca635964ce71" \
    "0x2546BcD3c84621e976D8185a91A922aE77ECEc30" \
    "0x8943545177806ED17B9F23F0a21ee5948eCaa776"; do
    echo "Funding $FUND_ADDR on L1 with 100 ETH..."
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
        "$FUND_ADDR" --value 100ether --gas-limit 21000 2>/dev/null && echo "  OK" || echo "  FAILED"
    sleep 2
done

# Build sync-rollups contracts (include test/ for flash loan TestToken)
echo "Building contracts..."
cd "$CONTRACTS_DIR/sync-rollups"
forge build

# 1. Deploy verifier contract.
# MOCK_VERIFIER=true → MockECDSAVerifier (always returns true, for flash loan dev)
# Otherwise         → tmpECDSAVerifier (ECDSA signature verification)
if [ "${MOCK_VERIFIER:-false}" = "true" ]; then
    echo "Deploying MockECDSAVerifier (always-true, flash loan dev mode)..."
    VERIFIER_OUTPUT=$(forge create \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        --broadcast \
        "$CONTRACTS_DIR/MockECDSAVerifier.sol:MockECDSAVerifier" 2>&1)
else
    echo "Deploying tmpECDSAVerifier..."
    VERIFIER_OUTPUT=$(forge create \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        --broadcast \
        src/verifier/tmpECDSAVerifier.sol:tmpECDSAVerifier \
        --constructor-args "$DEPLOYER_ADDR" "$DEPLOYER_ADDR" 2>&1)
fi
echo "$VERIFIER_OUTPUT"
VERIFIER_ADDRESS=$(echo "$VERIFIER_OUTPUT" | grep "Deployed to:" | awk '{print $3}')

if [ -z "$VERIFIER_ADDRESS" ]; then
    echo "ERROR: Failed to deploy verifier"
    exit 1
fi
echo "Verifier deployed at: ${VERIFIER_ADDRESS}"

# 2. Deploy Rollups contract with verifier address and starting rollup ID = 1
echo "Deploying Rollups contract..."
ROLLUPS_OUTPUT=$(forge create \
    --rpc-url "$L1_RPC" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    src/Rollups.sol:Rollups \
    --constructor-args "$VERIFIER_ADDRESS" 1 2>&1)
echo "$ROLLUPS_OUTPUT"
ROLLUPS_ADDRESS=$(echo "$ROLLUPS_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
DEPLOY_TX=$(echo "$ROLLUPS_OUTPUT" | grep "Transaction hash:" | awk '{print $3}')

if [ -z "$ROLLUPS_ADDRESS" ] || [ -z "$DEPLOY_TX" ]; then
    echo "ERROR: Failed to deploy Rollups"
    exit 1
fi
echo "Rollups deployed at: ${ROLLUPS_ADDRESS}"

# 3. Register Rollup (rollup_id = 1)
echo "Registering rollup..."
# createRollup(bytes32 initialState, bytes32 verificationKey, address owner) returns (uint256)
# Compute deterministic CCM address early so we can inject its pre-mint balance into genesis.json
# before computing the state root (state root must include the CCM balance).
CROSS_CHAIN_MANAGER_ADDRESS_EARLY=$(cast compute-address "$BUILDER_ADDRESS" --nonce 1 | awk '{print $NF}')
echo "CCM address (for genesis pre-mint): ${CROSS_CHAIN_MANAGER_ADDRESS_EARLY}"

# Inject CCM pre-mint balance into genesis.json (1,000,000 ETH = 1e24 wei = 0xD3C21BCECCEDA1000000)
GENESIS_JSON="${GENESIS_JSON:-/etc/based-rollup/genesis.json}"
CCM_ADDR_LOWER=$(echo "${CROSS_CHAIN_MANAGER_ADDRESS_EARLY#0x}" | tr '[:upper:]' '[:lower:]')
echo "Injecting CCM pre-mint balance into ${GENESIS_JSON} for address ${CCM_ADDR_LOWER}..."
# Insert CCM alloc entry with 1M ETH balance into genesis.json.
# Uses sed (no jq dependency — deploy container has only bash/sed/grep/curl/forge/cast).
# Genesis format is: "alloc": {\n    "addr": { ... },\n ...}
# We insert a new entry right after the "alloc": { line.
# Copy genesis to shared dir, inject CCM balance there.
# All nodes read genesis from /shared/genesis.json (set via GENESIS env var).
cp "$GENESIS_JSON" "${SHARED_DIR}/genesis.json"
sed -i "/\"alloc\": {/a\\    \"${CCM_ADDR_LOWER}\": { \"balance\": \"0xD3C21BCECCEDA1000000\" }," "${SHARED_DIR}/genesis.json"
GENESIS_JSON="${SHARED_DIR}/genesis.json"
# Verify injection succeeded
echo "CCM pre-mint injected. Verifying..."
grep -q "$CCM_ADDR_LOWER" "$GENESIS_JSON" && echo "  ✓ CCM address found in genesis" || { echo "  ✗ FATAL: CCM address not in genesis"; exit 1; }
grep -q "0xD3C21BCECCEDA1000000" "$GENESIS_JSON" && echo "  ✓ Pre-mint balance found" || { echo "  ✗ FATAL: Pre-mint balance not in genesis"; exit 1; }

# Compute genesis state root from genesis.json allocations (includes CCM pre-mint).
GENESIS_STATE_ROOT=$(based-rollup genesis-state-root --chain "$GENESIS_JSON")
echo "Computed genesis state root: ${GENESIS_STATE_ROOT}"
DEPLOYER_ADDR=$(cast wallet address --private-key "$DEPLOYER_KEY")
REGISTER_OUTPUT=$(cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
    "$ROLLUPS_ADDRESS" \
    "createRollup(bytes32,bytes32,address)(uint256)" \
    "$GENESIS_STATE_ROOT" \
    "0x0000000000000000000000000000000000000000000000000000000000000001" \
    "$DEPLOYER_ADDR" 2>&1)
echo "createRollup result: ${REGISTER_OUTPUT}"

# Verify rollup was created with expected ID
ROLLUP_ID=$(cast call --rpc-url "$L1_RPC" "$ROLLUPS_ADDRESS" "rollupCounter()(uint256)" 2>&1)
echo "Rollup counter after registration: ${ROLLUP_ID}"

# 4. Get deployment metadata
DEPLOY_BLOCK=$(cast receipt --rpc-url "$L1_RPC" "$DEPLOY_TX" blockNumber)
DEPLOY_TIMESTAMP=$(cast block --rpc-url "$L1_RPC" "$DEPLOY_BLOCK" --field timestamp)

echo ""
echo "=== Deployment Summary ==="
echo "tmpECDSAVerifier:  ${VERIFIER_ADDRESS}"
echo "Rollups:           ${ROLLUPS_ADDRESS}"
echo "Rollup ID:         1"
echo "Deployment block:  ${DEPLOY_BLOCK}"
echo "Deployment time:   ${DEPLOY_TIMESTAMP}"

# 5. Compute deterministic L2 contract addresses (CREATE from builder at nonces 0, 1, 2)
L2_CONTEXT_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 0 | awk '{print $NF}')
CROSS_CHAIN_MANAGER_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 1 | awk '{print $NF}')
BRIDGE_L2_ADDRESS=$(cast compute-address "$BUILDER_ADDRESS" --nonce 2 | awk '{print $NF}')
echo "L2Context address (CREATE nonce=0):  ${L2_CONTEXT_ADDRESS}"
echo "CCM address (CREATE nonce=1):        ${CROSS_CHAIN_MANAGER_ADDRESS}"
echo "Bridge L2 address (CREATE nonce=2):  ${BRIDGE_L2_ADDRESS}"

# 5b. Extract creation bytecodes from forge build artifacts for builder deployment at block 1.
# Read directly from out/ JSON artifacts (produced by forge build at line 49) to guarantee
# bytecode consistency. forge inspect can produce subtly different bytecodes — see #flash-loan-debug.
echo "Extracting L2 contract bytecodes from build artifacts..."
cd "$CONTRACTS_DIR"
forge build --skip test --skip script --skip "visualizat*"
# Helper to extract bytecode.object from forge JSON artifacts (no python3 in container)
_bc() { (grep -o '"object":"0x[0-9a-fA-F]*"' "$1" || true) | head -1 | sed 's/"object":"//;s/"//'; }
L2_CONTEXT_BYTECODE=$(_bc "$CONTRACTS_DIR/out/L2Context.sol/L2Context.json")
CCM_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups/out/CrossChainManagerL2.sol/CrossChainManagerL2.json")
BRIDGE_BYTECODE=$(_bc "$CONTRACTS_DIR/sync-rollups/out/Bridge.sol/Bridge.json")
cd "$CONTRACTS_DIR"
echo "L2Context bytecode length: ${#L2_CONTEXT_BYTECODE}"
echo "CCM bytecode length: ${#CCM_BYTECODE}"
echo "Bridge bytecode length: ${#BRIDGE_BYTECODE}"
if [ "$L2_CONTEXT_BYTECODE" = "null" ] || [ -z "$L2_CONTEXT_BYTECODE" ]; then
    echo "ERROR: Failed to extract L2Context bytecode"
    exit 1
fi
if [ "$CCM_BYTECODE" = "null" ] || [ -z "$CCM_BYTECODE" ]; then
    echo "ERROR: Failed to extract CCM bytecode"
    exit 1
fi
if [ "$BRIDGE_BYTECODE" = "null" ] || [ -z "$BRIDGE_BYTECODE" ]; then
    echo "ERROR: Failed to extract Bridge bytecode"
    exit 1
fi

# 5c. Deploy Bridge contract on L1 for ETH/token bridging
echo ""
echo "Deploying Bridge contract on L1..."
# Deploy Bridge L1 using the SAME bytecode that goes into rollup.env (BRIDGE_BYTECODE from line 169).
# CRITICAL: both Bridge L1 and Bridge L2 (via BRIDGE_BYTECODE env var) must use the same
# compilation artifacts. The WrappedToken CREATE2 address depends on the creation code
# embedded in the Bridge bytecode — using different compilations breaks the address.
BRIDGE_BYTECODE_FILE="/tmp/bridge_bytecode.txt"
echo "$BRIDGE_BYTECODE" > "$BRIDGE_BYTECODE_FILE"
if [ -s "$BRIDGE_BYTECODE_FILE" ] && [ "$BRIDGE_BYTECODE" != "null" ] && [ -n "$BRIDGE_BYTECODE" ]; then
    BRIDGE_DEPLOY_OUTPUT=$(cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
        --create "$BRIDGE_BYTECODE" 2>&1)
    BRIDGE_ADDRESS=$(echo "$BRIDGE_DEPLOY_OUTPUT" | grep "contractAddress" | awk '{print $NF}')
    if [ -n "$BRIDGE_ADDRESS" ] && [ "$BRIDGE_ADDRESS" != "null" ]; then
        echo "Bridge deployed at: ${BRIDGE_ADDRESS}"
        # Initialize: manager=Rollups, rollupId=0 (L1), admin=deployer
        cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
            "$BRIDGE_ADDRESS" \
            "initialize(address,uint256,address)" \
            "$ROLLUPS_ADDRESS" 0 "$DEPLOYER_ADDR" > /dev/null 2>&1
        echo "Bridge initialized (manager=${ROLLUPS_ADDRESS}, rollupId=0, admin=${DEPLOYER_ADDR})"

        # Set canonical bridge address on L1 Bridge → points to L2 Bridge address.
        # Required for flash loan continuation entries where bridgeTokens resolves
        # _bridgeAddress() to the counterpart chain's bridge.
        echo "Setting canonicalBridgeAddress on L1 Bridge → ${BRIDGE_L2_ADDRESS}..."
        cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
            "$BRIDGE_ADDRESS" \
            "setCanonicalBridgeAddress(address)" \
            "$BRIDGE_L2_ADDRESS" > /dev/null 2>&1
        echo "L1 Bridge canonicalBridgeAddress set to ${BRIDGE_L2_ADDRESS}"
    else
        echo "WARNING: Bridge deployment failed — skipping"
        echo "Deploy output: $BRIDGE_DEPLOY_OUTPUT" | head -3
        BRIDGE_ADDRESS="0x0000000000000000000000000000000000000000"
    fi
    rm -f "$BRIDGE_BYTECODE_FILE"
else
    echo "WARNING: Bridge bytecode not found — skipping deployment"
    BRIDGE_ADDRESS="0x0000000000000000000000000000000000000000"
fi

# 6. Deploy flash loan contracts (gated by DEPLOY_FLASH_LOAN=true)
FLASH_TOKEN_ADDRESS="0x0000000000000000000000000000000000000000"
FLASH_POOL_ADDRESS="0x0000000000000000000000000000000000000000"
FLASH_EXECUTOR_L2_ADDRESS="0x0000000000000000000000000000000000000000"
FLASH_NFT_ADDRESS="0x0000000000000000000000000000000000000000"
FLASH_EXECUTOR_L2_PROXY_ADDRESS="0x0000000000000000000000000000000000000000"
FLASH_EXECUTOR_L1_ADDRESS="0x0000000000000000000000000000000000000000"
WRAPPED_TOKEN_L2="0x0000000000000000000000000000000000000000"

if [ "${DEPLOY_FLASH_LOAN:-false}" = "true" ]; then
    echo ""
    echo "=== Deploying Flash Loan Contracts ==="

    # dev#5 key — used for L2 flash loan deploys (has 100 ETH via BOOTSTRAP_ACCOUNTS)
    L2_DEPLOY_KEY="0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba"
    DEV5_ADDR="0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
    ZERO="0x0000000000000000000000000000000000000000"
    L2_ROLLUP_ID=1

    # Pre-compute L2 flash loan contract addresses (dev#5 at nonce 0 and 1)
    EXECUTOR_L2=$(cast compute-address "$DEV5_ADDR" --nonce 0 | awk '{print $NF}')
    FLASH_NFT=$(cast compute-address "$DEV5_ADDR" --nonce 1 | awk '{print $NF}')
    echo "Pre-computed ExecutorL2 address (dev#5 nonce=0): $EXECUTOR_L2"
    echo "Pre-computed FlashNFT address   (dev#5 nonce=1): $FLASH_NFT"

    # Contracts already built at script start (forge build includes test/).
    cd "$CONTRACTS_DIR/sync-rollups"

    # 6a. Deploy TestToken on L1
    echo "Deploying TestToken on L1..."
    TOKEN_OUTPUT=$(forge create \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        --broadcast \
        test/IntegrationTestFlashLoan.t.sol:TestToken 2>&1)
    echo "$TOKEN_OUTPUT" | tail -3
    FLASH_TOKEN_ADDRESS=$(echo "$TOKEN_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
    if [ -z "$FLASH_TOKEN_ADDRESS" ]; then
        echo "WARNING: TestToken deployment failed — skipping flash loan contracts"
        DEPLOY_FLASH_LOAN=false
    fi
fi

if [ "${DEPLOY_FLASH_LOAN:-false}" = "true" ]; then
    # 6b. Deploy FlashLoan pool on L1
    echo "Deploying FlashLoan pool on L1..."
    FLASH_POOL_OUTPUT=$(forge create \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        --broadcast \
        src/periphery/defiMock/FlashLoan.sol:FlashLoan 2>&1)
    echo "$FLASH_POOL_OUTPUT" | tail -3
    FLASH_POOL_ADDRESS=$(echo "$FLASH_POOL_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
    if [ -z "$FLASH_POOL_ADDRESS" ]; then
        echo "WARNING: FlashLoan pool deployment failed — skipping remaining flash loan contracts"
        DEPLOY_FLASH_LOAN=false
    fi
fi

if [ "${DEPLOY_FLASH_LOAN:-false}" = "true" ]; then
    # 6c. Transfer 10,000 tokens to pool
    echo "Transferring 10,000 tokens to FlashLoan pool..."
    cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
        "$FLASH_TOKEN_ADDRESS" "transfer(address,uint256)" \
        "$FLASH_POOL_ADDRESS" "10000000000000000000000" > /dev/null 2>&1
    POOL_BAL=$(cast call --rpc-url "$L1_RPC" "$FLASH_TOKEN_ADDRESS" \
        "balanceOf(address)(uint256)" "$FLASH_POOL_ADDRESS" 2>&1)
    echo "Pool token balance: $POOL_BAL"

    # 6c2. Transfer 10,000 tokens to dev key #5 for reverse flash loan L2 pool funding.
    # deploy-reverse-flash-loan.sh runs after the builder starts (key #0 is busy with postBatch),
    # so it needs tokens on a different key to fund the L2 pool via bridgeTokens.
    DEV5_ADDR="0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
    echo "Transferring 10,000 tokens to dev#5 for reverse flash loan..."
    cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
        "$FLASH_TOKEN_ADDRESS" "transfer(address,uint256)" \
        "$DEV5_ADDR" "10000000000000000000000" > /dev/null 2>&1
    DEV5_BAL=$(cast call --rpc-url "$L1_RPC" "$FLASH_TOKEN_ADDRESS" \
        "balanceOf(address)(uint256)" "$DEV5_ADDR" 2>&1)
    echo "Dev#5 token balance: $DEV5_BAL"

    # 6d. createCrossChainProxy for ExecutorL2 on L1
    echo "Creating CrossChainProxy for ExecutorL2=$EXECUTOR_L2 on rollupId=$L2_ROLLUP_ID..."
    EXECUTOR_L2_PROXY=$(cast call --rpc-url "$L1_RPC" \
        "$ROLLUPS_ADDRESS" \
        "createCrossChainProxy(address,uint256)(address)" \
        "$EXECUTOR_L2" "$L2_ROLLUP_ID" 2>&1)
    echo "Predicted proxy address: $EXECUTOR_L2_PROXY"
    cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
        "$ROLLUPS_ADDRESS" \
        "createCrossChainProxy(address,uint256)" \
        "$EXECUTOR_L2" "$L2_ROLLUP_ID" --gas-limit 5000000 > /dev/null 2>&1
    FLASH_EXECUTOR_L2_PROXY_ADDRESS="$EXECUTOR_L2_PROXY"

    # 6e. Pre-compute WrappedToken CREATE2 address on L2
    # Bridge_L2 deploys WrappedToken via CREATE2 with:
    #   salt       = keccak256(abi.encodePacked(token, originRollupId))
    #   initCode   = type(WrappedToken).creationCode ++ abi.encode(name, symbol, decimals, bridgeL2)
    # We read the creation bytecode directly from out/ (same artifacts used for Bridge deployment)
    # to guarantee deterministic results. No recompilation — pure artifact + cast computation.
    echo "Computing WrappedToken L2 CREATE2 address from artifacts..."
    WT_CREATION_CODE=$(_bc "$CONTRACTS_DIR/sync-rollups/out/WrappedToken.sol/WrappedToken.json")
    WT_CONSTRUCTOR_ARGS=$(cast abi-encode "f(string,string,uint8,address)" "Test Token" "TT" 18 "$BRIDGE_L2_ADDRESS")
    WT_INIT_CODE="${WT_CREATION_CODE}${WT_CONSTRUCTOR_ARGS#0x}"
    WT_INIT_HASH=$(cast keccak256 "$WT_INIT_CODE")
    # salt = keccak256(abi.encodePacked(address(20 bytes), uint256(32 bytes)))
    TOKEN_LOWER=$(echo "${FLASH_TOKEN_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')
    WT_SALT=$(cast keccak256 "0x${TOKEN_LOWER}0000000000000000000000000000000000000000000000000000000000000000")
    # CREATE2: keccak256(0xff ++ deployer ++ salt ++ initCodeHash)[12:]
    DEPLOYER_LOWER=$(echo "${BRIDGE_L2_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')
    WT_FULL_HASH=$(cast keccak256 "0xff${DEPLOYER_LOWER}${WT_SALT#0x}${WT_INIT_HASH#0x}")
    WRAPPED_TOKEN_L2="0x${WT_FULL_HASH:26}"
    echo "WrappedToken L2 (CREATE2): $WRAPPED_TOKEN_L2"
    echo "  salt:     $WT_SALT"
    echo "  initHash: $WT_INIT_HASH"

    # 6f. Deploy FlashLoanBridgeExecutor on L1
    echo "Deploying FlashLoanBridgeExecutor on L1..."
    EXECUTOR_L1_OUTPUT=$(forge create \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        --broadcast \
        src/periphery/defiMock/FlashLoanBridgeExecutor.sol:FlashLoanBridgeExecutor \
        --constructor-args \
            "$FLASH_POOL_ADDRESS" \
            "$BRIDGE_ADDRESS" \
            "$EXECUTOR_L2_PROXY" \
            "$EXECUTOR_L2" \
            "$WRAPPED_TOKEN_L2" \
            "$FLASH_NFT" \
            "$BRIDGE_L2_ADDRESS" \
            "$L2_ROLLUP_ID" \
            "$FLASH_TOKEN_ADDRESS" 2>&1)
    echo "$EXECUTOR_L1_OUTPUT" | tail -3
    FLASH_EXECUTOR_L1_ADDRESS=$(echo "$EXECUTOR_L1_OUTPUT" | grep "Deployed to:" | awk '{print $3}')
    if [ -z "$FLASH_EXECUTOR_L1_ADDRESS" ]; then
        echo "WARNING: FlashLoanBridgeExecutor L1 deployment failed"
        FLASH_EXECUTOR_L1_ADDRESS="0x0000000000000000000000000000000000000000"
    fi

    echo ""
    echo "=== Flash Loan L1 Deployment Summary ==="
    echo "TestToken:            $FLASH_TOKEN_ADDRESS"
    echo "FlashLoan Pool:       $FLASH_POOL_ADDRESS"
    echo "ExecutorL2 (pre-comp):$EXECUTOR_L2"
    echo "FlashNFT   (pre-comp):$FLASH_NFT"
    echo "WrappedToken L2:      $WRAPPED_TOKEN_L2"
    echo "ExecutorL2 Proxy:     $FLASH_EXECUTOR_L2_PROXY_ADDRESS"
    echo "ExecutorL1:           $FLASH_EXECUTOR_L1_ADDRESS"

    FLASH_EXECUTOR_L2_ADDRESS="$EXECUTOR_L2"
    FLASH_NFT_ADDRESS="$FLASH_NFT"
fi

# 7. Generate faucet keypair for UI drip functionality.
# A random account is created at each deploy so no hardcoded key is needed.
echo ""
echo "Generating faucet keypair..."
FAUCET_OUTPUT=$(cast wallet new 2>&1)
FAUCET_PRIVATE_KEY=$(echo "$FAUCET_OUTPUT" | grep "Private key:" | awk '{print $NF}')
FAUCET_ADDRESS=$(echo "$FAUCET_OUTPUT" | grep "Address:" | awk '{print $NF}')
if [ -z "$FAUCET_PRIVATE_KEY" ] || [ -z "$FAUCET_ADDRESS" ]; then
    echo "ERROR: Failed to generate faucet keypair"
    echo "$FAUCET_OUTPUT"
    exit 1
fi
echo "Faucet address: $FAUCET_ADDRESS"

# Fund faucet from dev#9 (dedicated funder key)
echo "Funding faucet with 1000 ETH..."
cast send --rpc-url "$L1_RPC" --private-key "$FUNDER_KEY" \
    "$FAUCET_ADDRESS" --value 1000ether --gas-limit 21000 2>/dev/null && echo "  OK" || echo "  FAILED"

# Write private key to a separate file (not in rollup.env for security)
echo "$FAUCET_PRIVATE_KEY" > "${SHARED_DIR}/faucet.key"
echo "Faucet private key written to ${SHARED_DIR}/faucet.key"

# 8. Write single config file (atomic: write to .tmp then rename)
mkdir -p "$SHARED_DIR"
cat > "${SHARED_DIR}/rollup.env.tmp" <<EOF
L1_RPC_URL=${L1_RPC}
ROLLUPS_ADDRESS=${ROLLUPS_ADDRESS}
ROLLUP_ID=1
VERIFIER_ADDRESS=${VERIFIER_ADDRESS}
BUILDER_ADDRESS=${BUILDER_ADDRESS}
L2_CONTEXT_ADDRESS=${L2_CONTEXT_ADDRESS}
CROSS_CHAIN_MANAGER_ADDRESS=${CROSS_CHAIN_MANAGER_ADDRESS}
DEPLOYMENT_L1_BLOCK=${DEPLOY_BLOCK}
DEPLOYMENT_TIMESTAMP=${DEPLOY_TIMESTAMP}
BRIDGE_ADDRESS=${BRIDGE_ADDRESS}
BRIDGE_L1_ADDRESS=${BRIDGE_ADDRESS}
BRIDGE_L2_ADDRESS=${BRIDGE_L2_ADDRESS}
BOOTSTRAP_ACCOUNTS=${BOOTSTRAP_ACCOUNTS:-}
FLASH_TOKEN_ADDRESS=${FLASH_TOKEN_ADDRESS}
FLASH_POOL_ADDRESS=${FLASH_POOL_ADDRESS}
FLASH_EXECUTOR_L2_ADDRESS=${FLASH_EXECUTOR_L2_ADDRESS}
FLASH_NFT_ADDRESS=${FLASH_NFT_ADDRESS}
FLASH_EXECUTOR_L2_PROXY_ADDRESS=${FLASH_EXECUTOR_L2_PROXY_ADDRESS}
FLASH_EXECUTOR_L1_ADDRESS=${FLASH_EXECUTOR_L1_ADDRESS}
WRAPPED_TOKEN_L2=${WRAPPED_TOKEN_L2}
FAUCET_ADDRESS=${FAUCET_ADDRESS}
# BUILDER_MODE and BUILDER_PRIVATE_KEY are per-node settings.
# Set them in docker-compose.yml environment, NOT here.
# Bytecodes MUST be last — they are 20KB+ lines that can cause bash read
# to drop subsequent variables if placed before config vars.
L2_CONTEXT_BYTECODE=${L2_CONTEXT_BYTECODE}
CCM_BYTECODE=${CCM_BYTECODE}
BRIDGE_BYTECODE=${BRIDGE_BYTECODE}
EOF
mv "${SHARED_DIR}/rollup.env.tmp" "${SHARED_DIR}/rollup.env"

echo ""
echo "Config written to ${SHARED_DIR}/rollup.env"
cat "${SHARED_DIR}/rollup.env"

# L2 deployment (canonical bridge + flash loan contracts) is handled by deploy_l2.sh
# which runs as a separate Docker service after the builder is healthy.
echo ""
echo "L1 deployment complete. L2 setup will run via deploy-l2 service."
