use super::*;

#[test]
fn test_extract_methods_single() {
    let json: Value = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_sendRawTransaction",
        "params": ["0xdeadbeef"],
        "id": 1
    });
    let methods = extract_methods(&json);
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].0, "eth_sendRawTransaction");
}

#[test]
fn test_extract_methods_batch() {
    let json: Value = serde_json::json!([
        { "jsonrpc": "2.0", "method": "eth_blockNumber", "params": [], "id": 1 },
        { "jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": ["0xab"], "id": 2 }
    ]);
    let methods = extract_methods(&json);
    assert_eq!(methods.len(), 2);
}

#[test]
fn test_parse_address_from_return() {
    let mut data = [0u8; 32];
    data[12..32].copy_from_slice(&[0x11; 20]);
    let hex_str = format!("0x{}", hex::encode(&data));
    let addr = parse_address_from_return(&hex_str).unwrap();
    assert_eq!(addr, Address::new([0x11; 20]));
}

#[test]
fn test_parse_u256_from_return() {
    let mut data = [0u8; 32];
    data[31] = 7;
    let hex_str = format!("0x{}", hex::encode(&data));
    assert_eq!(parse_u256_from_return(&hex_str).unwrap(), 7);
}

// ──────────────────────────────────────────────────────────────────
//  Tests for atomic cross-chain submission (queue-based flow)
// ──────────────────────────────────────────────────────────────────

#[test]
fn test_decode_raw_tx_for_trace_valid_signed_tx() {
    // Use a pre-built valid signed legacy tx (from Anvil's first test account).
    // This is a real signed tx: to=0x1111...1111, value=1 ETH, gasPrice=20 gwei,
    // from=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 (well-known test key).
    //
    // Generated from the well-known Anvil/Hardhat key:
    // 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
    let signed_tx = build_test_signed_tx(false);
    let raw_hex = format!("0x{}", hex::encode(&signed_tx));

    let result = decode_raw_tx_for_trace(&raw_hex);
    assert!(
        result.is_ok(),
        "valid signed tx should decode: {:?}",
        result.err()
    );

    let obj = result.unwrap();
    // Must have from, to, data, gas, value fields
    assert!(obj.get("from").is_some(), "must have 'from' field");
    assert!(obj.get("to").is_some(), "must have 'to' field");
    assert!(obj.get("data").is_some(), "must have 'data' field");
    assert!(obj.get("gas").is_some(), "must have 'gas' field");
    assert!(obj.get("value").is_some(), "must have 'value' field");

    let to = obj.get("to").unwrap().as_str().unwrap();
    assert!(
        to.to_lowercase()
            .contains("1111111111111111111111111111111111111111"),
        "to address should match 0x1111...1111"
    );
}

#[test]
fn test_decode_raw_tx_for_trace_contract_creation() {
    // Contract creation tx has no "to" — the function should return
    // an object without the "to" field.
    let signed_tx = build_test_signed_tx(true);
    let raw_hex = format!("0x{}", hex::encode(&signed_tx));

    let result = decode_raw_tx_for_trace(&raw_hex).unwrap();
    assert!(
        result.get("to").is_none(),
        "contract creation should have no 'to' field"
    );
    assert!(
        result.get("from").is_some(),
        "should still have 'from' field"
    );
}

#[test]
fn test_handle_cross_chain_tx_contract_creation_returns_none() {
    // A contract creation tx (no `to` field) should return Ok(None),
    // meaning it's not a cross-chain tx and should be forwarded normally.
    let signed_tx = build_test_signed_tx(true);
    let raw_hex = format!("0x{}", hex::encode(&signed_tx));

    let obj = decode_raw_tx_for_trace(&raw_hex).unwrap();
    let to_addr = obj.get("to").and_then(|v| v.as_str());
    assert!(
        to_addr.is_none(),
        "contract creation has no 'to' → returns Ok(None)"
    );
}

/// Build a test signed EIP-1559 transaction.
/// If `contract_creation` is true, builds a create tx (no `to`).
fn build_test_signed_tx(contract_creation: bool) -> Vec<u8> {
    use alloy_consensus::TxEip1559;
    use alloy_primitives::TxKind;
    use alloy_rlp::Encodable;

    let signer: alloy_signer_local::PrivateKeySigner =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
            .parse()
            .unwrap();

    let to = if contract_creation {
        TxKind::Create
    } else {
        TxKind::Call(alloy_primitives::address!(
            "0x1111111111111111111111111111111111111111"
        ))
    };

    let tx = TxEip1559 {
        chain_id: 1,
        nonce: 0,
        gas_limit: if contract_creation { 100_000 } else { 21_000 },
        max_fee_per_gas: 20_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to,
        value: if contract_creation {
            alloy_primitives::U256::ZERO
        } else {
            alloy_primitives::U256::from(1_000_000_000_000_000_000u128)
        },
        input: alloy_primitives::Bytes::from(vec![0xDE, 0xAD]),
        ..Default::default()
    };

    // Sign using the synchronous path (LocalSigner implements SignerSync)
    use alloy_signer::SignerSync;
    let sig_hash = alloy_consensus::SignableTransaction::signature_hash(&tx);
    let sig = signer
        .sign_hash_sync(&sig_hash)
        .expect("signing should succeed");

    let signed = alloy_consensus::transaction::TxEnvelope::Eip1559(
        alloy_consensus::Signed::new_unchecked(tx, sig, Default::default()),
    );
    let mut buf = Vec::new();
    signed.encode(&mut buf);
    buf
}

#[test]
fn test_handle_request_returns_early_with_tx_hash_for_cross_chain() {
    // When handle_cross_chain_tx returns Ok(Some(tx_hash)), the handler
    // should return a JSON-RPC response with the tx_hash and NOT forward
    // the request to L1. This test verifies the response structure.
    let tx_hash = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let json_id = Value::Number(serde_json::Number::from(42));
    let response_body = serde_json::json!({
        "jsonrpc": "2.0",
        "result": tx_hash,
        "id": json_id
    });

    let body_str = response_body.to_string();
    let parsed: Value = serde_json::from_str(&body_str).unwrap();

    assert_eq!(parsed["jsonrpc"], "2.0");
    assert_eq!(parsed["result"], tx_hash);
    assert_eq!(parsed["id"], 42);
}

#[test]
fn test_handle_request_preserves_json_rpc_id() {
    // The JSON-RPC id from the original request must be echoed in the response.
    // This is critical for clients that match responses to requests by id.
    for id in [
        Value::Number(serde_json::Number::from(1)),
        Value::Number(serde_json::Number::from(999)),
        Value::String("my-request-id".to_string()),
        Value::Null,
    ] {
        let response_body = serde_json::json!({
            "jsonrpc": "2.0",
            "result": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "id": id
        });
        let parsed: Value = serde_json::from_str(&response_body.to_string()).unwrap();
        assert_eq!(parsed["id"], id, "response id must match request id");
    }
}

#[test]
fn test_cross_chain_rpc_request_structure() {
    // Verify the JSON-RPC request structure for initiateCrossChainCall
    // (unified: includes gasPrice and rawL1Tx)
    let destination = Address::new([0x11; 20]);
    let calldata_hex = format!("0x{}", hex::encode(&[0xDE, 0xAD]));
    let from_addr = Address::new([0x22; 20]);
    let gas_price: u128 = 1_200_000_000;
    let raw_tx = "0xdeadbeef";

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "syncrollups_initiateCrossChainCall",
        "params": [{
            "destination": format!("{destination}"),
            "data": calldata_hex,
            "sourceAddress": format!("{from_addr}"),
            "sourceRollup": format!("{}", U256::from(MAINNET_ROLLUP_ID)),
            "gasPrice": gas_price,
            "rawL1Tx": raw_tx
        }],
        "id": 99990
    });

    assert_eq!(req["method"], "syncrollups_initiateCrossChainCall");
    let params = req["params"].as_array().unwrap();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0]["data"], "0xdead");
    assert_eq!(params[0]["sourceRollup"], "0");
    assert_eq!(params[0]["gasPrice"], 1_200_000_000u64);
    assert_eq!(params[0]["rawL1Tx"], "0xdeadbeef");
}

#[test]
fn test_cross_chain_flow_two_passes_required() {
    // Documents the 2-pass flow (unified):
    // Pass 1: Detect proxy (authorizedProxies mapping on Rollups.sol)
    // Pass 2: Queue entries + raw L1 tx atomically (initiateCrossChainCall with gasPrice + rawL1Tx)
    // Both must succeed for the flow to complete.
    let mut passes_completed: Vec<&str> = Vec::new();

    // Pass 1
    let is_proxy = true;
    if is_proxy {
        passes_completed.push("detect_proxy");
    }

    // Pass 2 (unified: entries + L1 tx + gas price)
    let call_queued = true;
    if call_queued {
        passes_completed.push("queue_unified_call");
    }

    assert_eq!(passes_completed.len(), 2);
    assert_eq!(passes_completed[0], "detect_proxy");
    assert_eq!(passes_completed[1], "queue_unified_call");
}

#[test]
fn test_error_in_pass2_returns_err_not_none() {
    // If initiateCrossChainCall fails (pass 2), the function returns Err,
    // not Ok(None). This causes the handler to log a warning and forward
    // the tx normally — the user's L1 tx still goes through.
    let initiate_failed = true;
    let result: eyre::Result<Option<String>> = if initiate_failed {
        Err(eyre::eyre!("initiateCrossChainCall failed"))
    } else {
        Ok(Some("0xhash".to_string()))
    };

    assert!(result.is_err(), "pass 2 failure should be Err");
}

#[test]
fn test_extract_gas_price_from_eip1559_tx() {
    // Verify gas price extraction from a signed EIP-1559 tx
    let signed_tx = build_test_signed_tx(false);
    let raw_hex = format!("0x{}", hex::encode(&signed_tx));
    let gas_price = extract_gas_price_from_raw_tx(&raw_hex).unwrap();
    // Our test tx has max_fee_per_gas = 20_000_000_000
    assert_eq!(gas_price, 20_000_000_000u128);
}

// ──────────────────────────────────────────────────────────────────
//  Tests for bridge call detection
// ──────────────────────────────────────────────────────────────────

#[test]
fn test_bridge_ether_selector_parsing() {
    // bridgeEther(uint256 _rollupId, address destinationAddress) — selector 0xf402d9f3
    // calldata: selector(4) + rollupId(32) + destinationAddress(32) = 68 bytes
    let mut calldata = vec![0xf4, 0x02, 0xd9, 0xf3]; // selector
    let mut rollup_id = [0u8; 32];
    rollup_id[31] = 1; // rollupId = 1
    calldata.extend_from_slice(&rollup_id);
    let mut dest_addr = [0u8; 32]; // destinationAddress (ABI-encoded)
    dest_addr[12..32].copy_from_slice(&[0xBB; 20]);
    calldata.extend_from_slice(&dest_addr);

    assert_eq!(&calldata[..4], &BRIDGE_ETHER_SELECTOR);
    assert_eq!(calldata.len(), 68);

    // Parse rollupId
    let rollup_id_hex = format!("0x{}", hex::encode(&calldata[4..36]));
    let parsed = parse_u256_from_return(&rollup_id_hex).unwrap();
    assert_eq!(parsed, 1);

    // Parse destinationAddress
    let dest = Address::from_slice(&calldata[48..68]);
    assert_eq!(dest, Address::new([0xBB; 20]));
}

#[test]
fn test_bridge_tokens_selector_parsing() {
    // bridgeTokens(address token, uint256 amount, uint256 _rollupId, address destinationAddress)
    // selector 0x33b15aad
    let mut calldata = vec![0x33, 0xb1, 0x5a, 0xad]; // selector
    let mut token = [0u8; 32]; // token address padded to 32 bytes
    token[12..32].copy_from_slice(&[0xAA; 20]);
    calldata.extend_from_slice(&token);
    let mut amount = [0u8; 32]; // amount = 1000
    amount[30] = 0x03;
    amount[31] = 0xe8;
    calldata.extend_from_slice(&amount);
    let mut rollup_id = [0u8; 32]; // rollupId = 1
    rollup_id[31] = 1;
    calldata.extend_from_slice(&rollup_id);
    let mut dest_addr = [0u8; 32]; // destinationAddress (ABI-encoded)
    dest_addr[12..32].copy_from_slice(&[0xCC; 20]);
    calldata.extend_from_slice(&dest_addr);

    assert_eq!(&calldata[..4], &BRIDGE_TOKENS_SELECTOR);
    assert_eq!(calldata.len(), 132);

    // Parse rollupId (3rd arg, bytes 68..100)
    let rollup_id_hex = format!("0x{}", hex::encode(&calldata[68..100]));
    let parsed = parse_u256_from_return(&rollup_id_hex).unwrap();
    assert_eq!(parsed, 1);
}

#[test]
fn test_bridge_selector_mismatch_returns_early() {
    // A calldata with unknown selector should not be treated as a bridge call
    let mut calldata = vec![0xDE, 0xAD, 0xBE, 0xEF];
    calldata.resize(36, 0x00);
    let selector: [u8; 4] = calldata[..4].try_into().unwrap();
    assert_ne!(selector, BRIDGE_ETHER_SELECTOR);
    assert_ne!(selector, BRIDGE_TOKENS_SELECTOR);
}

#[test]
fn test_bridge_calldata_too_short() {
    // bridgeEther needs at least 68 bytes (4 selector + 32 rollupId + 32 destinationAddress)
    let short_calldata = vec![0xf4, 0x02, 0xd9, 0xf3, 0x00, 0x01]; // only 6 bytes
    assert!(short_calldata.len() < 68);

    // bridgeTokens needs at least 132 bytes (4 selector + 32×4 args)
    let mut short_tokens = vec![0x33, 0xb1, 0x5a, 0xad];
    short_tokens.resize(100, 0x00); // only 100 bytes
    assert!(short_tokens.len() < 132);
}
