//! Integration test for the custom EVM executor with L2Context system call
//! and cross-chain execution via CrossChainManagerL2.

use alloy_consensus::Header;
use alloy_eips::eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE};
use alloy_primitives::{Address, B256, Bytes as AlloBytes, U256, keccak256};
use based_rollup::config::RollupConfig;
use based_rollup::cross_chain::{
    CROSS_CHAIN_MANAGER_L2_ADDRESS, CrossChainAction, CrossChainActionType,
    CrossChainExecutionEntry, ICrossChainManagerL2,
};
use based_rollup::evm_config::RollupEvmConfig;
use reth_chainspec::{ChainSpecBuilder, EthereumHardfork, ForkCondition, MAINNET};
use reth_ethereum_primitives::{Block, BlockBody};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_primitives_traits::RecoveredBlock;
use revm::Database;
use revm::database::{CacheDB, EmptyDB};
use revm::state::{AccountInfo, Bytecode};
use std::sync::Arc;

/// Address where L2Context contract is deployed in tests.
const L2_CONTEXT_ADDRESS: Address = Address::new([
    0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

/// Minimal EVM bytecode that stores ABI-encoded setContext arguments.
/// Arguments start at calldata offset 4 (after the 4-byte function selector).
///
/// Assembly:
///   PUSH1 0x04  CALLDATALOAD  PUSH1 0x01  SSTORE   // slot[1] = l1BlockNumber
///   PUSH1 0x24  CALLDATALOAD  PUSH1 0x02  SSTORE   // slot[2] = l1BlockHash
///   PUSH1 0x44  CALLDATALOAD  PUSH1 0x03  SSTORE   // slot[3] = l2BlockNumber
///   PUSH1 0x64  CALLDATALOAD  PUSH1 0x04  SSTORE   // slot[4] = l2Timestamp
///   STOP
const L2_CONTEXT_BYTECODE: &[u8] = &[
    0x60, 0x04, 0x35, 0x60, 0x01, 0x55, // PUSH1 4, CALLDATALOAD, PUSH1 1, SSTORE
    0x60, 0x24, 0x35, 0x60, 0x02, 0x55, // PUSH1 36, CALLDATALOAD, PUSH1 2, SSTORE
    0x60, 0x44, 0x35, 0x60, 0x03, 0x55, // PUSH1 68, CALLDATALOAD, PUSH1 3, SSTORE
    0x60, 0x64, 0x35, 0x60, 0x04, 0x55, // PUSH1 100, CALLDATALOAD, PUSH1 4, SSTORE
    0x00, // STOP
];

fn test_rollup_config() -> RollupConfig {
    RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: L2_CONTEXT_ADDRESS,
        deployment_l1_block: 1000,
        deployment_timestamp: 1_700_000_000,
        block_time: 12,
        builder_mode: false,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        builder_ws_url: None,
        health_port: 0,
        rollups_address: Address::ZERO,
        cross_chain_manager_address: Address::ZERO,
        rollup_id: 0,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: Address::ZERO,
        bridge_l2_address: Address::ZERO,
        bridge_l1_address: Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    }
}

fn create_test_db() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(Default::default());

    // Deploy beacon root contract (required by Cancun pre-execution)
    let beacon_root_code = BEACON_ROOTS_CODE.clone();
    db.insert_account_info(
        BEACON_ROOTS_ADDRESS,
        AccountInfo {
            balance: U256::ZERO,
            code_hash: keccak256(&beacon_root_code),
            nonce: 1,
            code: Some(Bytecode::new_raw(beacon_root_code)),
            account_id: None,
        },
    );

    // Deploy our L2Context test contract
    let l2_context_code = alloy_primitives::Bytes::from_static(L2_CONTEXT_BYTECODE);
    db.insert_account_info(
        L2_CONTEXT_ADDRESS,
        AccountInfo {
            balance: U256::ZERO,
            code_hash: keccak256(&l2_context_code),
            nonce: 1,
            code: Some(Bytecode::new_raw(l2_context_code)),
            account_id: None,
        },
    );

    db
}

#[test]
fn test_system_call_executes_during_block() {
    let config = Arc::new(test_rollup_config());
    let db = create_test_db();

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let evm_config = RollupEvmConfig::new(chain_spec, config.clone());
    let mut executor = BasicBlockExecutor::new(evm_config, db);

    // L2 block number 5 → timestamp = 1_700_000_000 + 5 * 12 = 1_700_000_060
    // L1 block number carried via mix_hash (prevrandao) = 1005
    let l2_block_number = 5u64;
    let l2_timestamp = config.l2_timestamp(l2_block_number);
    let l1_block_hash = B256::with_last_byte(0xAA);
    let l1_block_number = 1005u64;

    let header = Header {
        number: l2_block_number,
        timestamp: l2_timestamp,
        parent_beacon_block_root: Some(l1_block_hash),
        mix_hash: B256::from(U256::from(l1_block_number)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        },
        vec![],
    );

    // Execute the block — only beacon root (EIP-4788) runs in pre-execution.
    // L2Context is NOT written — that requires builder-signed transactions.
    executor.execute_one(&block).unwrap();

    // Verify L2Context contract storage was NOT written (system calls removed).
    let l1_block_number_storage =
        executor.with_state_mut(|state| state.storage(L2_CONTEXT_ADDRESS, U256::from(1)).unwrap());
    assert_eq!(
        l1_block_number_storage,
        U256::ZERO,
        "L2Context should not be written — no system calls"
    );

    let l2_block_storage =
        executor.with_state_mut(|state| state.storage(L2_CONTEXT_ADDRESS, U256::from(3)).unwrap());
    assert_eq!(
        l2_block_storage,
        U256::ZERO,
        "L2Context should not be written — no system calls"
    );
}

#[test]
fn test_system_call_skipped_when_no_l2_context_address() {
    let mut config = test_rollup_config();
    config.l2_context_address = Address::ZERO; // No L2Context contract
    let config = Arc::new(config);
    let db = create_test_db();

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let evm_config = RollupEvmConfig::new(chain_spec, config.clone());
    let mut executor = BasicBlockExecutor::new(evm_config, db);

    let header = Header {
        number: 1,
        timestamp: config.l2_timestamp(1),
        parent_beacon_block_root: Some(B256::ZERO),
        mix_hash: B256::from(U256::from(1u64)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        },
        vec![],
    );

    // Should succeed without error — system call skipped since address is zero
    executor.execute_one(&block).unwrap();

    // L2Context contract should have no storage writes
    let storage =
        executor.with_state_mut(|state| state.storage(L2_CONTEXT_ADDRESS, U256::from(1)).unwrap());
    assert_eq!(
        storage,
        U256::ZERO,
        "No storage should be written when address is zero"
    );
}

#[test]
fn test_system_call_different_block_numbers() {
    let config = Arc::new(test_rollup_config());
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    // Execute multiple blocks in sequence and verify L2Context is updated for each
    for l2_block_number in [1u64, 50, 100] {
        let db = create_test_db();
        let evm_config = RollupEvmConfig::new(chain_spec.clone(), config.clone());
        let mut executor = BasicBlockExecutor::new(evm_config, db);

        let l2_timestamp = config.l2_timestamp(l2_block_number);
        let l1_block_hash = B256::with_last_byte(l2_block_number as u8);
        let expected_l1_number = l2_block_number + config.deployment_l1_block;

        let header = Header {
            number: l2_block_number,
            timestamp: l2_timestamp,
            parent_beacon_block_root: Some(l1_block_hash),
            mix_hash: B256::from(U256::from(expected_l1_number)),
            excess_blob_gas: Some(0),
            ..Header::default()
        };

        let block = RecoveredBlock::new_unhashed(
            Block {
                header,
                body: BlockBody {
                    transactions: vec![],
                    ommers: vec![],
                    withdrawals: Some(Default::default()),
                },
            },
            vec![],
        );

        executor.execute_one(&block).unwrap();

        // L2Context is not written — system calls have been removed.
        // Only beacon root (EIP-4788) runs in pre-execution.
        let stored_l1_number = executor
            .with_state_mut(|state| state.storage(L2_CONTEXT_ADDRESS, U256::from(1)).unwrap());
        assert_eq!(
            stored_l1_number,
            U256::ZERO,
            "L2Context should not be written for L2 block {l2_block_number}"
        );
    }
}

#[test]
fn test_system_call_with_zero_l1_block_hash() {
    let config = Arc::new(test_rollup_config());
    let db = create_test_db();

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let evm_config = RollupEvmConfig::new(chain_spec, config.clone());
    let mut executor = BasicBlockExecutor::new(evm_config, db);

    // parent_beacon_block_root = B256::ZERO → L1 hash should be zero
    // mix_hash carries L1 block number (1001 = 1 + 1000)
    let header = Header {
        number: 1,
        timestamp: config.l2_timestamp(1),
        parent_beacon_block_root: Some(B256::ZERO),
        mix_hash: B256::from(U256::from(1001u64)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        },
        vec![],
    );

    executor.execute_one(&block).unwrap();

    let l1_hash_storage =
        executor.with_state_mut(|state| state.storage(L2_CONTEXT_ADDRESS, U256::from(2)).unwrap());
    assert_eq!(
        l1_hash_storage,
        U256::ZERO,
        "L1 block hash should be zero when parent_beacon_block_root is B256::ZERO"
    );
}

// ═══════════════════════════════════════════════════════════════
//  Cross-chain integration tests (mirrors IntegrationTest.t.sol)
// ═══════════════════════════════════════════════════════════════

/// Load compiled contract bytecodes from forge artifacts.
fn load_contract_bytecode(artifact_path: &str) -> AlloBytes {
    let content =
        std::fs::read_to_string(artifact_path).unwrap_or_else(|e| {
            panic!(
                "failed to read artifact {artifact_path}: {e} — run `forge build` in contracts/sync-rollups/"
            )
        });
    let artifact: serde_json::Value = serde_json::from_str(&content).unwrap();
    let hex_str = artifact["deployedBytecode"]["object"]
        .as_str()
        .expect("deployedBytecode.object should be a string");
    hex_str.parse::<AlloBytes>().expect("invalid hex bytecode")
}

/// Load the CrossChainManagerL2 deployed bytecode from forge artifacts.
/// CCM is no longer in genesis — it's deployed by the builder at block 1.
fn load_ccm_deployed_bytecode() -> AlloBytes {
    let artifact_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/sync-rollups/out/CrossChainManagerL2.sol/CrossChainManagerL2.json"
    );
    load_contract_bytecode(artifact_path)
}

/// Address where Counter is deployed in cross-chain tests.
const COUNTER_ADDRESS: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xC0, 0x01, 0x01,
]);

/// Fake L1 address representing CounterAndProxy (source of the cross-chain call).
const COUNTER_AND_PROXY_ADDRESS: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xCA, 0xFE, 0x01,
]);

/// Chain ID matching genesis.json (required for CrossChainProxy CREATE2 salt).
const ROLLUP_CHAIN_ID: u64 = 42069;

/// SYSTEM_ADDRESS used for system calls.
const SYSTEM_ADDRESS: Address = Address::new([0xFF; 20]);

fn cross_chain_rollup_config() -> RollupConfig {
    RollupConfig {
        l1_rpc_url: "http://localhost:8545".to_string(),
        l2_context_address: L2_CONTEXT_ADDRESS,
        deployment_l1_block: 1000,
        deployment_timestamp: 1_700_000_000,
        block_time: 12,
        builder_mode: false,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        builder_ws_url: None,
        health_port: 0,
        rollups_address: Address::ZERO, // not needed for L2-side test
        cross_chain_manager_address: CROSS_CHAIN_MANAGER_L2_ADDRESS,
        rollup_id: 1,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: Address::ZERO,
        bridge_l2_address: Address::ZERO,
        bridge_l1_address: Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    }
}

fn create_cross_chain_test_db() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(Default::default());

    // Deploy beacon root contract (required by Cancun)
    let beacon_root_code = BEACON_ROOTS_CODE.clone();
    db.insert_account_info(
        BEACON_ROOTS_ADDRESS,
        AccountInfo {
            balance: U256::ZERO,
            code_hash: keccak256(&beacon_root_code),
            nonce: 1,
            code: Some(Bytecode::new_raw(beacon_root_code)),
            account_id: None,
        },
    );

    // Deploy L2Context contract
    let l2_context_code = alloy_primitives::Bytes::from_static(L2_CONTEXT_BYTECODE);
    db.insert_account_info(
        L2_CONTEXT_ADDRESS,
        AccountInfo {
            balance: U256::ZERO,
            code_hash: keccak256(&l2_context_code),
            nonce: 1,
            code: Some(Bytecode::new_raw(l2_context_code)),
            account_id: None,
        },
    );

    // Deploy CrossChainManagerL2 from forge artifact (deployed bytecode without immutables)
    let ccm_code = load_ccm_deployed_bytecode();
    db.insert_account_info(
        CROSS_CHAIN_MANAGER_L2_ADDRESS,
        AccountInfo {
            balance: U256::ZERO,
            code_hash: keccak256(&ccm_code),
            nonce: 1,
            code: Some(Bytecode::new_raw(ccm_code)),
            account_id: None,
        },
    );

    // Deploy Counter contract from compiled artifact
    let counter_artifact = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/sync-rollups/out/CounterContracts.sol/Counter.json"
    );
    let counter_code = load_contract_bytecode(counter_artifact);
    db.insert_account_info(
        COUNTER_ADDRESS,
        AccountInfo {
            balance: U256::ZERO,
            code_hash: keccak256(&counter_code),
            nonce: 1,
            code: Some(Bytecode::new_raw(counter_code)),
            account_id: None,
        },
    );

    // Give SYSTEM_ADDRESS some balance so it can make calls
    db.insert_account_info(
        SYSTEM_ADDRESS,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            code_hash: keccak256(&[]),
            nonce: 0,
            code: None,
            account_id: None,
        },
    );

    db
}

/// Mirrors IntegrationTest.t.sol Phase 1 (L2 side):
///
/// 1. SYSTEM loads execution table with a RESULT entry
/// 2. SYSTEM calls executeIncomingCrossChainCall(Counter.increment())
/// 3. CrossChainManagerL2 auto-creates a CrossChainProxy for sourceAddress
/// 4. Proxy calls Counter.increment() → counter goes 0→1, returns 1
/// 5. RESULT action is built from returnData, hash matches → entry consumed
///
/// Asserts:
/// - Counter.counter() == 1
/// - CrossChainManagerL2.pendingEntryCount() == 0
#[test]
fn test_cross_chain_incoming_call_executes_counter() {
    use alloy_sol_types::SolType;

    let config = Arc::new(cross_chain_rollup_config());
    let db = create_cross_chain_test_db();

    // Chain spec with chain_id = 42069 (matches genesis / CrossChainManagerL2 immutables)
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .chain(ROLLUP_CHAIN_ID.into())
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let evm_config = RollupEvmConfig::new(chain_spec, config.clone());

    // ── Build the RESULT entry (loaded into execution table) ──
    // After Counter.increment() returns 1, _processCallAtScope builds:
    //   RESULT { rollupId: 1, data: abi.encode(1), failed: false }
    // The execution table entry has actionHash = keccak256(abi.encode(RESULT))
    let result_data = {
        let mut buf = vec![0u8; 32];
        buf[31] = 1; // abi.encode(uint256(1))
        buf
    };
    let result_action = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: U256::from(1u64),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: result_data,
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let result_action_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &result_action.to_sol_action(),
    ));

    let result_entry = CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: result_action_hash,
        next_action: result_action,
    };

    // ── Build the CALL entry (trigger for executeIncomingCrossChainCall) ──
    let increment_calldata = vec![0xd0, 0x9d, 0xe0, 0x8a]; // Counter.increment()
    let call_action = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::from(1u64), // targeting THIS rollup
        destination: COUNTER_ADDRESS,
        value: U256::ZERO,
        data: increment_calldata,
        failed: false,
        source_address: COUNTER_AND_PROXY_ADDRESS,
        source_rollup: U256::ZERO, // from L1 (mainnet)
        scope: vec![],
    };
    let call_action_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &call_action.to_sol_action(),
    ));

    let call_entry = CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: call_action_hash,
        next_action: call_action,
    };

    // Cross-chain entries are no longer loaded via system calls — they are
    // now handled through builder-signed transactions. Keep entries constructed
    // above for reference but do not load them into the EVM config.
    let _entries = vec![result_entry, call_entry];

    // ── Execute the block ──
    let l2_block_number = 1u64;
    let l2_timestamp = config.l2_timestamp(l2_block_number);
    let l1_block_hash = B256::with_last_byte(0xAA);
    let l1_block_number = 1001u64;

    let header = Header {
        number: l2_block_number,
        timestamp: l2_timestamp,
        parent_beacon_block_root: Some(l1_block_hash),
        mix_hash: B256::from(U256::from(l1_block_number)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        },
        vec![],
    );

    let mut executor = BasicBlockExecutor::new(evm_config, db);
    executor.execute_one(&block).unwrap();

    // Cross-chain system calls no longer run during block execution, so
    // Counter is not incremented and pendingEntryCount remains 0.
    let counter_value =
        executor.with_state_mut(|state| state.storage(COUNTER_ADDRESS, U256::ZERO).unwrap());
    assert_eq!(
        counter_value,
        U256::ZERO,
        "Counter.counter should be 0 — cross-chain entries no longer executed via system calls"
    );

    let pending_count = executor.with_state_mut(|state| {
        state
            .storage(CROSS_CHAIN_MANAGER_L2_ADDRESS, U256::from(2))
            .unwrap()
    });
    assert_eq!(
        pending_count,
        U256::ZERO,
        "pendingEntryCount should be 0 — no entries loaded"
    );

    // L2Context is no longer written via system calls — it requires builder-signed txs.
    let l1_block_number_storage =
        executor.with_state_mut(|state| state.storage(L2_CONTEXT_ADDRESS, U256::from(1)).unwrap());
    assert_eq!(
        l1_block_number_storage,
        U256::ZERO,
        "L2Context should not be updated — no system calls"
    );
}

/// Load creation bytecode from a forge artifact (for deploying contracts via revm).
fn load_creation_bytecode(artifact_path: &str) -> AlloBytes {
    let content = std::fs::read_to_string(artifact_path)
        .unwrap_or_else(|e| panic!("failed to read artifact {artifact_path}: {e}"));
    let artifact: serde_json::Value = serde_json::from_str(&content).unwrap();
    let hex_str = artifact["bytecode"]["object"]
        .as_str()
        .expect("bytecode.object should be a string");
    hex_str.parse::<AlloBytes>().expect("invalid hex bytecode")
}

/// Execute a call against a CacheDB using revm directly (for pre-block setup).
/// Returns (success, return_data).
fn revm_call(
    db: &mut CacheDB<EmptyDB>,
    caller: Address,
    to: Address,
    calldata: Vec<u8>,
    value: U256,
) -> (bool, Vec<u8>) {
    use revm::context::TxEnv;
    use revm::context_interface::result::{ExecutionResult, Output};
    use revm::database::DatabaseCommit;
    use revm::primitives::TxKind;
    use revm::{Context, ExecuteEvm, MainBuilder, MainContext};

    // Read caller nonce from db
    let nonce = db
        .cache
        .accounts
        .get(&caller)
        .map(|a| a.info.nonce)
        .unwrap_or(0);

    let mut evm = Context::mainnet()
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = ROLLUP_CHAIN_ID;
        })
        .with_db(&mut *db)
        .build_mainnet();

    let tx = TxEnv::builder()
        .caller(caller)
        .kind(TxKind::Call(to))
        .data(calldata.into())
        .value(value)
        .nonce(nonce)
        .gas_limit(10_000_000)
        .chain_id(Some(ROLLUP_CHAIN_ID))
        .build_fill();

    let result = evm.transact(tx).expect("revm_call transact failed");

    let (success, output) = match &result.result {
        ExecutionResult::Success { output, .. } => {
            let data = match output {
                Output::Call(d) => d.to_vec(),
                Output::Create(d, _) => d.to_vec(),
            };
            (true, data)
        }
        _ => (false, vec![]),
    };

    // Commit state changes back to db
    db.commit(result.state);

    (success, output)
}

/// Deploy a contract using revm (creation bytecode → returns deployed address).
fn revm_deploy(
    db: &mut CacheDB<EmptyDB>,
    deployer: Address,
    creation_bytecode: Vec<u8>,
) -> Address {
    use revm::context::TxEnv;
    use revm::context_interface::result::{ExecutionResult, Output};
    use revm::database::DatabaseCommit;
    use revm::primitives::TxKind;
    use revm::{Context, ExecuteEvm, MainBuilder, MainContext};

    // Read deployer nonce from db
    let nonce = db
        .cache
        .accounts
        .get(&deployer)
        .map(|a| a.info.nonce)
        .unwrap_or(0);

    let mut evm = Context::mainnet()
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = ROLLUP_CHAIN_ID;
        })
        .with_db(&mut *db)
        .build_mainnet();

    let tx = TxEnv::builder()
        .caller(deployer)
        .kind(TxKind::Create)
        .data(creation_bytecode.into())
        .nonce(nonce)
        .gas_limit(10_000_000)
        .chain_id(Some(ROLLUP_CHAIN_ID))
        .build_fill();

    let result = evm.transact(tx).expect("revm_deploy transact failed");

    let addr = match &result.result {
        ExecutionResult::Success { output, .. } => match output {
            Output::Create(_, Some(addr)) => *addr,
            _ => panic!("deploy did not return an address"),
        },
        other => panic!("deploy failed: {other:?}"),
    };

    db.commit(result.state);
    addr
}

/// Mirrors NestedIntegrationTest.t.sol:
///
/// executeIncomingCrossChainCall → CounterAndProxy → CrossChainProxy → executeCrossChainCall
///
/// This tests that our RollupBlockExecutor correctly handles nested cross-chain calls:
/// a contract called via executeIncomingCrossChainCall makes an outgoing cross-chain
/// call through a proxy, consuming TWO execution table entries in a single block.
///
/// Flow:
///   1. Pre-setup: createCrossChainProxy for remote Counter, deploy CounterAndProxy
///   2. Load execution table: inner CALL entry + outer RESULT entry
///   3. executeIncomingCrossChainCall(CounterAndProxy.increment())
///   4. CounterAndProxy calls remoteProxy → executeCrossChainCall → consumes inner entry
///   5. CounterAndProxy finishes → outer RESULT consumed
///
/// Asserts:
///   - CounterAndProxy.counter == 1
///   - CounterAndProxy.targetCounter == 1
///   - pendingEntryCount == 0
#[test]
fn test_cross_chain_nested_call_counter_and_proxy() {
    use alloy_sol_types::SolType;

    let config = Arc::new(cross_chain_rollup_config());
    let mut db = create_cross_chain_test_db();

    // Pre-setup deployer account (needs nonce for deployments)
    let deployer = Address::with_last_byte(0xDD);
    db.insert_account_info(
        deployer,
        AccountInfo {
            balance: U256::from(10_000_000_000_000_000_000u128),
            code_hash: keccak256(&[]),
            nonce: 0,
            code: None,
            account_id: None,
        },
    );

    // ── Pre-setup: create CrossChainProxy for remote Counter ──
    // Call manager.createCrossChainProxy(0xC001, 0)
    let remote_counter = Address::new([
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0xC0, 0x01,
    ]);
    let create_proxy_calldata = {
        use alloy_sol_types::SolCall;
        // createCrossChainProxy(address, uint256)
        alloy_sol_types::sol! {
            function createCrossChainProxy(address originalAddress, uint256 originalRollupId) external returns (address);
        }
        createCrossChainProxyCall {
            originalAddress: remote_counter,
            originalRollupId: U256::ZERO,
        }
        .abi_encode()
    };

    let (success, return_data) = revm_call(
        &mut db,
        deployer,
        CROSS_CHAIN_MANAGER_L2_ADDRESS,
        create_proxy_calldata,
        U256::ZERO,
    );
    assert!(success, "createCrossChainProxy should succeed");
    // Decode the returned proxy address
    let remote_proxy_address: Address =
        alloy_sol_types::sol_data::Address::abi_decode(&return_data).unwrap();
    assert_ne!(
        remote_proxy_address,
        Address::ZERO,
        "proxy address should be non-zero"
    );

    // ── Pre-setup: deploy CounterAndProxy(target=remoteProxy) ──
    let cap_artifact = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/sync-rollups/out/CounterContracts.sol/CounterAndProxy.json"
    );
    let cap_creation = load_creation_bytecode(cap_artifact);
    // Append constructor arg: abi.encode(address remoteProxy)
    let ctor_arg = alloy_sol_types::sol_data::Address::abi_encode(&remote_proxy_address);
    let mut deploy_data = cap_creation.to_vec();
    deploy_data.extend_from_slice(&ctor_arg);

    let cap_address = revm_deploy(&mut db, deployer, deploy_data);
    assert_ne!(cap_address, Address::ZERO, "CounterAndProxy should deploy");

    // ── Build execution entries ──
    let increment_calldata = vec![0xd0, 0x9d, 0xe0, 0x8a]; // Counter.increment()

    // Inner CALL: executeCrossChainCall builds this when CounterAndProxy calls the proxy.
    // ProxyInfo for remoteProxy: originalAddress=0xC001, originalRollupId=0
    // CALL{rollupId=0, dest=0xC001, source=CounterAndProxy, sourceRollup=1}
    let inner_call = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::ZERO,
        destination: remote_counter,
        value: U256::ZERO,
        data: increment_calldata.clone(),
        failed: false,
        source_address: cap_address,
        source_rollup: U256::from(1u64),
        scope: vec![],
    };
    let inner_call_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &inner_call.to_sol_action(),
    ));

    // Inner RESULT: simulated remote Counter.increment() returns 1
    let inner_result = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: U256::ZERO,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: {
            let mut buf = vec![0u8; 32];
            buf[31] = 1;
            buf
        },
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };

    let inner_entry = CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: inner_call_hash,
        next_action: inner_result,
    };

    // Outer RESULT: _processCallAtScope builds this after CounterAndProxy.increment() finishes.
    // CounterAndProxy.increment() is void → empty returnData
    let outer_result = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: U256::from(1u64),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![], // void function
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let outer_result_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &outer_result.to_sol_action(),
    ));

    let outer_result_entry = CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: outer_result_hash,
        next_action: outer_result,
    };

    // Trigger: CALL targeting this rollup → executeIncomingCrossChainCall
    let trigger_call = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::from(1u64), // targeting THIS rollup
        destination: cap_address,
        value: U256::ZERO,
        data: increment_calldata,
        failed: false,
        source_address: Address::with_last_byte(0xAA), // some L1 address
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let trigger_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &trigger_call.to_sol_action(),
    ));

    let trigger_entry = CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: trigger_hash,
        next_action: trigger_call,
    };

    // ── Set up EVM config and execute block ──
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .chain(ROLLUP_CHAIN_ID.into())
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let evm_config = RollupEvmConfig::new(chain_spec, config.clone());

    // Cross-chain entries are no longer loaded via system calls.
    let _entries = vec![inner_entry, outer_result_entry, trigger_entry];

    let l2_block_number = 1u64;
    let header = Header {
        number: l2_block_number,
        timestamp: config.l2_timestamp(l2_block_number),
        parent_beacon_block_root: Some(B256::with_last_byte(0xBB)),
        mix_hash: B256::from(U256::from(1001u64)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        },
        vec![],
    );

    let mut executor = BasicBlockExecutor::new(evm_config, db);
    executor.execute_one(&block).unwrap();

    // ── Assertions ──
    // Cross-chain system calls no longer run during block execution.
    // CounterAndProxy is not called, so counters remain at 0.

    let cap_counter =
        executor.with_state_mut(|state| state.storage(cap_address, U256::from(2)).unwrap());
    assert_eq!(
        cap_counter,
        U256::ZERO,
        "CounterAndProxy.counter should be 0 — cross-chain entries no longer executed via system calls"
    );

    let cap_target_counter =
        executor.with_state_mut(|state| state.storage(cap_address, U256::from(1)).unwrap());
    assert_eq!(
        cap_target_counter,
        U256::ZERO,
        "CounterAndProxy.targetCounter should be 0 — cross-chain entries no longer executed via system calls"
    );

    let pending_count = executor.with_state_mut(|state| {
        state
            .storage(CROSS_CHAIN_MANAGER_L2_ADDRESS, U256::from(2))
            .unwrap()
    });
    assert_eq!(
        pending_count,
        U256::ZERO,
        "pendingEntryCount should be 0 — no entries loaded"
    );
}

/// Multiple independent cross-chain calls in a single block.
///
/// Three different source addresses each call Counter.increment() in the same block.
/// Each call has its own trigger CALL entry and corresponding RESULT entry.
/// Counter starts at 0 and should end at 3 after all three calls execute sequentially.
///
/// Entries (6 total):
///   - 3 RESULT entries (loaded into execution table)
///   - 3 CALL trigger entries (drive executeIncomingCrossChainCall)
///
/// Asserts:
///   - Counter.counter() == 3
///   - pendingEntryCount == 0
#[test]
fn test_cross_chain_multi_entry_batch_three_increments() {
    use alloy_sol_types::SolType;

    let config = Arc::new(cross_chain_rollup_config());
    let db = create_cross_chain_test_db();

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .chain(ROLLUP_CHAIN_ID.into())
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let evm_config = RollupEvmConfig::new(chain_spec, config.clone());
    let increment_calldata = vec![0xd0, 0x9d, 0xe0, 0x8a]; // Counter.increment()

    // Three different remote callers
    let sources = [
        Address::with_last_byte(0xA1),
        Address::with_last_byte(0xA2),
        Address::with_last_byte(0xA3),
    ];

    let mut all_entries = Vec::new();

    for (i, &source_addr) in sources.iter().enumerate() {
        let expected_return = (i + 1) as u64; // 1, 2, 3

        // RESULT entry: predicted return value of Counter.increment()
        let result_data = {
            let mut buf = vec![0u8; 32];
            buf[24..32].copy_from_slice(&expected_return.to_be_bytes());
            buf
        };
        let result_action = CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: U256::from(1u64),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: result_data,
            failed: false,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: vec![],
        };
        let result_action_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
            &result_action.to_sol_action(),
        ));

        all_entries.push(CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: result_action_hash,
            next_action: result_action,
        });

        // CALL trigger entry
        let call_action = CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: U256::from(1u64),
            destination: COUNTER_ADDRESS,
            value: U256::ZERO,
            data: increment_calldata.clone(),
            failed: false,
            source_address: source_addr,
            source_rollup: U256::ZERO,
            scope: vec![],
        };
        let call_action_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
            &call_action.to_sol_action(),
        ));

        all_entries.push(CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: call_action_hash,
            next_action: call_action,
        });
    }

    // Cross-chain entries are no longer loaded via system calls.
    let _entries = all_entries;

    let l2_block_number = 1u64;
    let header = Header {
        number: l2_block_number,
        timestamp: config.l2_timestamp(l2_block_number),
        parent_beacon_block_root: Some(B256::with_last_byte(0xBB)),
        mix_hash: B256::from(U256::from(1001u64)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        },
        vec![],
    );

    let mut executor = BasicBlockExecutor::new(evm_config, db);
    executor.execute_one(&block).unwrap();

    // Cross-chain system calls no longer run during block execution.
    let counter_value =
        executor.with_state_mut(|state| state.storage(COUNTER_ADDRESS, U256::ZERO).unwrap());
    assert_eq!(
        counter_value,
        U256::ZERO,
        "Counter.counter should be 0 — cross-chain entries no longer executed via system calls"
    );

    let pending_count = executor.with_state_mut(|state| {
        state
            .storage(CROSS_CHAIN_MANAGER_L2_ADDRESS, U256::from(2))
            .unwrap()
    });
    assert_eq!(
        pending_count,
        U256::ZERO,
        "pendingEntryCount should be 0 — no entries loaded"
    );
}

/// Mixed batch: independent cross-chain calls to different contracts in one block.
///
/// Two Counter contracts at different addresses, each called once.
/// Verifies the system handles multiple destinations correctly.
///
/// Asserts:
///   - Counter A: counter == 1
///   - Counter B: counter == 1
///   - pendingEntryCount == 0
#[test]
fn test_cross_chain_multi_entry_different_destinations() {
    use alloy_sol_types::SolType;

    let config = Arc::new(cross_chain_rollup_config());
    let mut db = create_cross_chain_test_db();

    // Deploy a second Counter at a different address
    let counter_b_address = Address::new([
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xBB, 0xBB, 0x02,
    ]);
    let counter_artifact = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/sync-rollups/out/CounterContracts.sol/Counter.json"
    );
    let counter_code = load_contract_bytecode(counter_artifact);
    db.insert_account_info(
        counter_b_address,
        AccountInfo {
            balance: U256::ZERO,
            code_hash: keccak256(&counter_code),
            nonce: 1,
            code: Some(Bytecode::new_raw(counter_code)),
            account_id: None,
        },
    );

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .chain(ROLLUP_CHAIN_ID.into())
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let evm_config = RollupEvmConfig::new(chain_spec, config.clone());
    let increment_calldata = vec![0xd0, 0x9d, 0xe0, 0x8a]; // Counter.increment()

    let targets = [COUNTER_ADDRESS, counter_b_address];
    let sources = [Address::with_last_byte(0xC1), Address::with_last_byte(0xC2)];

    let mut all_entries = Vec::new();

    for (&target, &source_addr) in targets.iter().zip(sources.iter()) {
        // Each counter starts at 0, so increment returns 1
        let result_data = {
            let mut buf = vec![0u8; 32];
            buf[31] = 1;
            buf
        };
        let result_action = CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: U256::from(1u64),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: result_data,
            failed: false,
            source_address: Address::ZERO,
            source_rollup: U256::ZERO,
            scope: vec![],
        };
        let result_action_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
            &result_action.to_sol_action(),
        ));

        all_entries.push(CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: result_action_hash,
            next_action: result_action,
        });

        let call_action = CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: U256::from(1u64),
            destination: target,
            value: U256::ZERO,
            data: increment_calldata.clone(),
            failed: false,
            source_address: source_addr,
            source_rollup: U256::ZERO,
            scope: vec![],
        };
        let call_action_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
            &call_action.to_sol_action(),
        ));

        all_entries.push(CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: call_action_hash,
            next_action: call_action,
        });
    }

    // Cross-chain entries are no longer loaded via system calls.
    let _entries = all_entries;

    let l2_block_number = 1u64;
    let header = Header {
        number: l2_block_number,
        timestamp: config.l2_timestamp(l2_block_number),
        parent_beacon_block_root: Some(B256::with_last_byte(0xBB)),
        mix_hash: B256::from(U256::from(1001u64)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        },
        vec![],
    );

    let mut executor = BasicBlockExecutor::new(evm_config, db);
    executor.execute_one(&block).unwrap();

    // Cross-chain system calls no longer run during block execution.
    let counter_a =
        executor.with_state_mut(|state| state.storage(COUNTER_ADDRESS, U256::ZERO).unwrap());
    assert_eq!(
        counter_a,
        U256::ZERO,
        "Counter A should be 0 — cross-chain entries no longer executed via system calls"
    );

    let counter_b =
        executor.with_state_mut(|state| state.storage(counter_b_address, U256::ZERO).unwrap());
    assert_eq!(
        counter_b,
        U256::ZERO,
        "Counter B should be 0 — cross-chain entries no longer executed via system calls"
    );

    let pending_count = executor.with_state_mut(|state| {
        state
            .storage(CROSS_CHAIN_MANAGER_L2_ADDRESS, U256::from(2))
            .unwrap()
    });
    assert_eq!(
        pending_count,
        U256::ZERO,
        "pendingEntryCount should be 0 — no entries loaded"
    );
}

/// Batch with mixed entry types: cross-chain calls + extra non-trigger entries.
///
/// Loads 4 entries:
///   - 1 RESULT entry for the call's return value (consumed by executeIncomingCrossChainCall)
///   - 1 CALL trigger targeting this rollup (drives execution)
///   - 2 extra CALL entries targeting other rollups (loaded into table, NOT consumed)
///
/// Asserts:
///   - Counter.counter() == 1
///   - pendingEntryCount == 2 (extra entries remain)
#[test]
fn test_cross_chain_batch_with_unconsumed_entries() {
    use alloy_sol_types::SolType;

    let config = Arc::new(cross_chain_rollup_config());
    let db = create_cross_chain_test_db();

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .chain(ROLLUP_CHAIN_ID.into())
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let evm_config = RollupEvmConfig::new(chain_spec, config.clone());
    let increment_calldata = vec![0xd0, 0x9d, 0xe0, 0x8a];

    // RESULT entry for the single call
    let result_data = {
        let mut buf = vec![0u8; 32];
        buf[31] = 1;
        buf
    };
    let result_action = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: U256::from(1u64),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: result_data,
        failed: false,
        source_address: Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let result_action_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &result_action.to_sol_action(),
    ));

    // CALL trigger targeting this rollup
    let call_action = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::from(1u64),
        destination: COUNTER_ADDRESS,
        value: U256::ZERO,
        data: increment_calldata.clone(),
        failed: false,
        source_address: Address::with_last_byte(0xD1),
        source_rollup: U256::ZERO,
        scope: vec![],
    };
    let call_action_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &call_action.to_sol_action(),
    ));

    // Two extra entries targeting OTHER rollups (rollup 0 and rollup 2).
    // These go into the execution table but are NOT consumed by any call in this block.
    let extra_1 = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::ZERO, // targeting rollup 0, NOT this rollup
        destination: Address::with_last_byte(0xE1),
        value: U256::ZERO,
        data: increment_calldata.clone(),
        failed: false,
        source_address: Address::with_last_byte(0xF1),
        source_rollup: U256::from(1u64),
        scope: vec![],
    };
    let extra_1_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &extra_1.to_sol_action(),
    ));

    let extra_2 = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: U256::from(2u64), // targeting rollup 2, NOT this rollup
        destination: Address::with_last_byte(0xE2),
        value: U256::ZERO,
        data: increment_calldata,
        failed: false,
        source_address: Address::with_last_byte(0xF2),
        source_rollup: U256::from(1u64),
        scope: vec![],
    };
    let extra_2_hash = keccak256(ICrossChainManagerL2::Action::abi_encode(
        &extra_2.to_sol_action(),
    ));

    // Cross-chain entries are no longer loaded via system calls.
    let _entries = vec![
        CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: result_action_hash,
            next_action: result_action,
        },
        CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: call_action_hash,
            next_action: call_action,
        },
        CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: extra_1_hash,
            next_action: extra_1,
        },
        CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: extra_2_hash,
            next_action: extra_2,
        },
    ];

    let l2_block_number = 1u64;
    let header = Header {
        number: l2_block_number,
        timestamp: config.l2_timestamp(l2_block_number),
        parent_beacon_block_root: Some(B256::with_last_byte(0xBB)),
        mix_hash: B256::from(U256::from(1001u64)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        },
        vec![],
    );

    let mut executor = BasicBlockExecutor::new(evm_config, db);
    executor.execute_one(&block).unwrap();

    // Cross-chain system calls no longer run during block execution.
    let counter_value =
        executor.with_state_mut(|state| state.storage(COUNTER_ADDRESS, U256::ZERO).unwrap());
    assert_eq!(
        counter_value,
        U256::ZERO,
        "Counter should be 0 — cross-chain entries no longer executed via system calls"
    );

    let pending_count = executor.with_state_mut(|state| {
        state
            .storage(CROSS_CHAIN_MANAGER_L2_ADDRESS, U256::from(2))
            .unwrap()
    });
    assert_eq!(
        pending_count,
        U256::ZERO,
        "pendingEntryCount should be 0 — no entries loaded"
    );
}

// Genesis CCM bytecode tests removed — CrossChainManagerL2 is no longer a genesis
// predeployment. It is deployed by the builder at block 1 via CREATE transaction.
