use super::*;
use alloy_consensus::Header;
use alloy_primitives::{Address, B256};

fn test_consensus() -> RollupConsensus {
    let config = RollupConfig {
        l1_rpc_url: String::new(),
        l2_context_address: Default::default(),
        deployment_l1_block: 1000,
        deployment_timestamp: 1_700_000_000,
        block_time: 12,
        builder_mode: false,
        builder_private_key: None,
        l1_rpc_url_fallback: None,
        l1_builder_rpc_url: None,
        builder_ws_url: None,
        health_port: 0,
        rollups_address: Default::default(),
        cross_chain_manager_address: Default::default(),
        rollup_id: 0,
        proxy_port: 0,
        l1_proxy_port: 0,
        l1_gas_overbid_pct: 10,
        builder_address: Address::ZERO,
        bridge_l2_address: Address::ZERO,
        bridge_l1_address: Address::ZERO,
        bootstrap_accounts_raw: String::new(),
        bootstrap_accounts: Vec::new(),
    };
    RollupConsensus::new(Arc::new(config))
}

#[test]
fn test_valid_timestamp() {
    let consensus = test_consensus();
    let header = Header {
        number: 5,
        timestamp: 1_700_000_000 + 6 * 12,
        ..Default::default()
    };
    let sealed = SealedHeader::new(header, B256::ZERO);
    assert!(consensus.validate_header(&sealed).is_ok());
}

#[test]
fn test_invalid_timestamp() {
    let consensus = test_consensus();
    let header = Header {
        number: 5,
        timestamp: 9999,
        ..Default::default()
    };
    let sealed = SealedHeader::new(header, B256::ZERO);
    assert!(consensus.validate_header(&sealed).is_err());
}

#[test]
fn test_valid_parent_relationship() {
    let consensus = test_consensus();

    let parent_hash = B256::with_last_byte(0x01);
    let parent = Header {
        number: 4,
        timestamp: 1_700_000_000 + 5 * 12,
        ..Default::default()
    };
    let sealed_parent = SealedHeader::new(parent, parent_hash);

    let child = Header {
        number: 5,
        timestamp: 1_700_000_000 + 6 * 12,
        parent_hash,
        ..Default::default()
    };
    let sealed_child = SealedHeader::new(child, B256::with_last_byte(0x02));

    assert!(
        consensus
            .validate_header_against_parent(&sealed_child, &sealed_parent)
            .is_ok()
    );
}

#[test]
fn test_parent_hash_mismatch() {
    let consensus = test_consensus();

    let parent = Header {
        number: 4,
        timestamp: 1_700_000_000 + 5 * 12,
        ..Default::default()
    };
    let sealed_parent = SealedHeader::new(parent, B256::with_last_byte(0x01));

    let child = Header {
        number: 5,
        timestamp: 1_700_000_000 + 6 * 12,
        parent_hash: B256::with_last_byte(0xFF), // wrong parent hash
        ..Default::default()
    };
    let sealed_child = SealedHeader::new(child, B256::with_last_byte(0x02));

    assert!(
        consensus
            .validate_header_against_parent(&sealed_child, &sealed_parent)
            .is_err()
    );
}

#[test]
fn test_parent_number_gap() {
    let consensus = test_consensus();

    let parent_hash = B256::with_last_byte(0x01);
    let parent = Header {
        number: 3, // gap: 3 -> 5
        timestamp: 1_700_000_000 + 4 * 12,
        ..Default::default()
    };
    let sealed_parent = SealedHeader::new(parent, parent_hash);

    let child = Header {
        number: 5,
        timestamp: 1_700_000_000 + 6 * 12,
        parent_hash,
        ..Default::default()
    };
    let sealed_child = SealedHeader::new(child, B256::with_last_byte(0x02));

    assert!(
        consensus
            .validate_header_against_parent(&sealed_child, &sealed_parent)
            .is_err()
    );
}

#[test]
fn test_parent_valid_but_child_wrong_timestamp() {
    let consensus = test_consensus();

    let parent_hash = B256::with_last_byte(0x01);
    let parent = Header {
        number: 4,
        timestamp: 1_700_000_000 + 5 * 12,
        ..Default::default()
    };
    let sealed_parent = SealedHeader::new(parent, parent_hash);

    let child = Header {
        number: 5,
        timestamp: 9999, // wrong timestamp
        parent_hash,
        ..Default::default()
    };
    let sealed_child = SealedHeader::new(child, B256::with_last_byte(0x02));

    assert!(
        consensus
            .validate_header_against_parent(&sealed_child, &sealed_parent)
            .is_err()
    );
}

#[test]
fn test_genesis_block_timestamp() {
    let consensus = test_consensus();
    let header = Header {
        number: 0,
        timestamp: 1_700_000_012, // deployment_timestamp + (0+1) * 12
        ..Default::default()
    };
    let sealed = SealedHeader::new(header, B256::ZERO);
    assert!(consensus.validate_header(&sealed).is_ok());
}

// --- Reject invalid headers: bad timestamp, bad difficulty ---
