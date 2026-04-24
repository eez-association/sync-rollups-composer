//! Benchmarks for rollup critical-path operations.
//!
//! Run with: `cargo bench -p based-rollup`

use alloy_consensus::Header;
use alloy_eips::eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE};
use alloy_primitives::{Address, B256, U256, keccak256};
use based_rollup::config::RollupConfig;
use based_rollup::evm_config::RollupEvmConfig;
use criterion::{Criterion, criterion_group, criterion_main};
use reth_chainspec::{ChainSpecBuilder, EthereumHardfork, ForkCondition, MAINNET};
use reth_ethereum_primitives::{Block, BlockBody};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_primitives_traits::RecoveredBlock;
use revm::database::{CacheDB, EmptyDB};
use revm::state::{AccountInfo, Bytecode};
use std::sync::Arc;

const L2_CONTEXT_ADDRESS: Address = Address::new([
    0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

const L2_CONTEXT_BYTECODE: &[u8] = &[
    0x60, 0x04, 0x35, 0x60, 0x01, 0x55, 0x60, 0x24, 0x35, 0x60, 0x02, 0x55, 0x60, 0x44, 0x35, 0x60,
    0x03, 0x55, 0x60, 0x64, 0x35, 0x60, 0x04, 0x55, 0x00,
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
        l1_builder_rpc_url: None,
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
        bootstrap_accounts: vec![],
    }
}

fn create_test_db() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(Default::default());
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

fn bench_empty_block_execution(c: &mut Criterion) {
    let config = Arc::new(test_rollup_config());
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    c.bench_function("execute_empty_block_with_system_call", |b| {
        b.iter(|| {
            let db = create_test_db();
            let evm_config = RollupEvmConfig::new(chain_spec.clone(), config.clone());
            let mut executor = BasicBlockExecutor::new(evm_config, db);

            let l2_block_number = 5u64;
            let l2_timestamp = config.l2_timestamp(l2_block_number);

            let header = Header {
                number: l2_block_number,
                timestamp: l2_timestamp,
                parent_beacon_block_root: Some(B256::with_last_byte(0xAA)),
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
        });
    });
}

fn bench_system_call_encoding(c: &mut Criterion) {
    use based_rollup::payload_builder::{L1BlockInfo, encode_set_context_calldata};

    let l1_info = L1BlockInfo {
        l1_block_number: 1005,
        l1_block_hash: B256::with_last_byte(0xAA),
    };

    c.bench_function("encode_set_context_calldata", |b| {
        b.iter(|| {
            encode_set_context_calldata(&l1_info);
        });
    });
}

fn bench_rlp_encode_empty_txs(c: &mut Criterion) {
    let txs: Vec<reth_ethereum_primitives::TransactionSigned> = vec![];

    c.bench_function("rlp_encode_empty_tx_list", |b| {
        b.iter(|| {
            let mut buf = Vec::new();
            alloy_rlp::encode_list(&txs, &mut buf);
        });
    });
}

criterion_group!(
    benches,
    bench_empty_block_execution,
    bench_system_call_encoding,
    bench_rlp_encode_empty_txs,
);
criterion_main!(benches);
