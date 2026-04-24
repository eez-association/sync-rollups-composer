use super::*;
use alloy_primitives::Address;

#[test]
fn test_encode_set_context_calldata() {
    let l1_info = L1BlockInfo {
        l1_block_number: 1000,
        l1_block_hash: B256::ZERO,
    };
    let data = encode_set_context_calldata(&l1_info);
    // Should produce valid ABI-encoded calldata (4 byte selector + 4 * 32 bytes)
    assert_eq!(data.len(), 4 + 2 * 32);
}

#[test]
fn test_encode_set_context_with_context() {
    use crate::config::RollupConfig;
    use std::sync::Arc;

    let config = Arc::new(RollupConfig {
        l1_rpc_url: String::new(),
        l2_context_address: Address::new([0x42; 20]),
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
        bootstrap_accounts: Vec::new(),
    });

    let l1_info = L1BlockInfo {
        l1_block_number: 1005,
        l1_block_hash: B256::with_last_byte(0xBB),
    };

    let timestamp = config.l2_timestamp(5);
    assert_eq!(timestamp, 1_700_000_072);

    let calldata = encode_set_context_calldata(&l1_info);
    assert_eq!(calldata.len(), 4 + 2 * 32);
}
