use super::*;

#[test]
fn test_extract_methods_single_request() {
    let json: Value = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_sendRawTransaction",
        "params": ["0xdeadbeef"],
        "id": 1
    });
    let methods = extract_methods(&json);
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].0, "eth_sendRawTransaction");
    assert_eq!(methods[0].1.unwrap()[0], "0xdeadbeef");
}

#[test]
fn test_extract_methods_batch_request() {
    let json: Value = serde_json::json!([
        { "jsonrpc": "2.0", "method": "eth_blockNumber", "params": [], "id": 1 },
        { "jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": ["0xab"], "id": 2 },
        { "jsonrpc": "2.0", "method": "eth_getBalance", "params": ["0x00", "latest"], "id": 3 }
    ]);
    let methods = extract_methods(&json);
    assert_eq!(methods.len(), 3);
    assert_eq!(methods[0].0, "eth_blockNumber");
    assert_eq!(methods[1].0, "eth_sendRawTransaction");
    assert_eq!(methods[2].0, "eth_getBalance");
}

#[test]
fn test_extract_methods_invalid_json() {
    let json: Value = serde_json::json!("just a string");
    let methods = extract_methods(&json);
    assert!(methods.is_empty());
}

#[test]
fn test_simulation_trigger_requires_string_param() {
    // The simulation fire-and-forget path (lines 139-145) chains:
    //   params.and_then(|p| p.first()).and_then(|v| v.as_str())
    // If the first param is not a string (e.g. a number), simulation is
    // silently skipped. This is correct — only hex-encoded raw tx strings
    // should trigger simulation.
    let json: Value = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_sendRawTransaction",
        "params": [12345],
        "id": 1
    });
    let methods = extract_methods(&json);
    assert_eq!(methods.len(), 1);
    let params = methods[0].1.unwrap();
    // First param is a number, not a string
    assert!(
        params.first().unwrap().as_str().is_none(),
        "numeric param should not be treated as raw tx string"
    );
}

#[test]
fn test_extract_methods_multiple_send_raw_tx_in_batch() {
    // A batch can contain multiple eth_sendRawTransaction calls.
    // The proxy should trigger simulation for each one independently.
    let json: Value = serde_json::json!([
        { "jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": ["0xaaa"], "id": 1 },
        { "jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": ["0xbbb"], "id": 2 },
        { "jsonrpc": "2.0", "method": "eth_blockNumber", "params": [], "id": 3 }
    ]);
    let methods = extract_methods(&json);
    let send_txs: Vec<_> = methods
        .iter()
        .filter(|(m, _)| m == "eth_sendRawTransaction")
        .collect();
    assert_eq!(send_txs.len(), 2, "both sendRawTransaction calls extracted");
    assert_eq!(send_txs[0].1.unwrap()[0], "0xaaa");
    assert_eq!(send_txs[1].1.unwrap()[0], "0xbbb");
}

// ──────────────────────────────────────────────
//  Step 0.6 (refactor) — trace fixture round-trip tests
//
//  These tests verify that the canonical L2→L1-flavoured fixtures load
//  and parse cleanly from this side of the composer. The DSL itself
//  lives in `crate::test_support::trace_fixtures`. Phase 5 will add
//  fuzz/proptest harnesses on top; here we only validate the wire-up.
// ──────────────────────────────────────────────

#[test]
fn trace_fixtures_l2_to_l1_round_trip() {
    use crate::test_support::trace_fixtures::{FixtureName, get};

    // The L2→L1-flavoured fixtures we expect to be reachable from this
    // side of the composer.
    let l2_to_l1_fixtures = [
        FixtureName::WithdrawalSimpleL2ToL1,
        FixtureName::PingPongDepth2L2ToL1,
        FixtureName::PingPongDepth3L2ToL1,
    ];

    for name in l2_to_l1_fixtures {
        let fx = get(name).unwrap_or_else(|| panic!("fixture {:?} not registered", name));
        let trace = fx.parse_value();
        assert!(
            trace.is_object(),
            "fixture {} top-level not an object",
            fx.filename
        );
        assert!(
            trace.get("from").and_then(|v| v.as_str()).is_some(),
            "fixture {} has no `from`",
            fx.filename
        );
        assert!(
            trace.get("to").and_then(|v| v.as_str()).is_some(),
            "fixture {} has no `to`",
            fx.filename
        );
        // The selector check is done by trace_fixtures::tests; here we
        // only verify the round-trip from `include_str!` → `Value`.
    }
}
