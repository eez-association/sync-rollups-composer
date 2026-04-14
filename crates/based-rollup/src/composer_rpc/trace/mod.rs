//! Generic trace-based cross-chain call detection.
//!
//! Walks a `callTracer` trace tree to find cross-chain proxy calls using
//! protocol-level detection only (`ICrossChainManager` interface).
//!
//! Two detection mechanisms:
//! 1. **Persistent proxies**: looked up via [`ProxyLookup`] (typically
//!    `authorizedProxies(address)` on the manager contract).
//! 2. **Ephemeral proxies**: `createCrossChainProxy(address, uint256)` calls
//!    that appear within the same trace. The created proxy address is decoded
//!    from the call's output, and the identity is stored in an in-memory map
//!    so that a subsequent proxy call in the same trace can be detected without
//!    an on-chain query.
//!
//! The single entry point is [`walk_trace_tree`], which recurses depth-first
//! through a `callTracer` JSON trace and populates a `Vec<DetectedCall>`.

pub mod proxy;
pub mod types;
pub mod walker;

// ── Public API re-exports ────────────────────────────────────────────────────
// External callers use `trace::walk_trace_tree`, `trace::DetectedCall`, etc.
// These re-exports preserve the existing public surface.

pub use proxy::extract_ephemeral_proxies_from_trace;
pub use types::{DetectedCall, ProxyInfo, ProxyLookup};
pub use walker::walk_trace_tree;

// CallTraceNode is pub(crate) and currently only used within this module.
// External callers can access it via `trace::types::CallTraceNode` if needed.

#[cfg(test)]
mod tests {
    use super::types::{
        create_cross_chain_proxy_selector, execute_cross_chain_call_selector, has_selector,
    };
    use super::*;
    use serde_json::{Value, json};
    use std::collections::{HashMap, HashSet};
    use std::future::Future;
    use std::pin::Pin;

    use alloy_primitives::{Address, U256};

    /// A mock ProxyLookup that returns a fixed set of registered proxies.
    struct MockLookup {
        proxies: HashMap<Address, ProxyInfo>,
    }

    impl MockLookup {
        fn new() -> Self {
            Self {
                proxies: HashMap::new(),
            }
        }

        fn register(&mut self, proxy_addr: Address, info: ProxyInfo) {
            self.proxies.insert(proxy_addr, info);
        }
    }

    impl ProxyLookup for MockLookup {
        fn lookup_proxy(
            &self,
            address: Address,
        ) -> Pin<Box<dyn Future<Output = Option<ProxyInfo>> + Send + '_>> {
            let result = self.proxies.get(&address).copied();
            Box::pin(async move { result })
        }
    }

    /// Helper to build hex-prefixed input from a selector and ABI-encoded params.
    fn encode_input(selector: &[u8; 4], params: &[u8]) -> String {
        format!("0x{}{}", hex::encode(selector), hex::encode(params))
    }

    /// Build a minimal trace node JSON.
    fn trace_node(to: &str, from: &str, input: &str, value: &str, calls: Vec<Value>) -> Value {
        json!({
            "to": to,
            "from": from,
            "input": input,
            "value": value,
            "calls": calls,
            "output": "0x",
            "type": "CALL"
        })
    }

    #[tokio::test]
    async fn test_selectors_are_consistent() {
        // Verify the sol!-derived selectors are the expected 4-byte values.
        let exec_sel = execute_cross_chain_call_selector();
        let create_sel = create_cross_chain_proxy_selector();
        // These should be stable keccak256 prefixes of the function signatures.
        // executeCrossChainCall(address,bytes) = keccak256("executeCrossChainCall(address,bytes)")
        let expected_exec =
            &alloy_primitives::keccak256(b"executeCrossChainCall(address,bytes)")[..4];
        assert_eq!(
            exec_sel, expected_exec,
            "executeCrossChainCall selector mismatch"
        );

        let expected_create =
            &alloy_primitives::keccak256(b"createCrossChainProxy(address,uint256)")[..4];
        assert_eq!(
            create_sel, expected_create,
            "createCrossChainProxy selector mismatch"
        );
    }

    #[tokio::test]
    async fn test_simple_proxy_call_detected() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        // Build a trace: caller -> proxy -> manager.executeCrossChainCall
        let exec_input = encode_input(
            &execute_cross_chain_call_selector(),
            &[0u8; 64], // dummy ABI params
        );
        let child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let root = trace_node(
            &format!("{proxy_addr}"),
            &format!("{caller}"),
            "0xdeadbeef",
            "0x0",
            vec![child],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(detected.len(), 1, "should detect exactly one proxy call");
        assert_eq!(detected[0].destination, original);
        assert_eq!(detected[0].source_address, caller);
        assert_eq!(detected[0].calldata, hex::decode("deadbeef").unwrap());
    }

    #[tokio::test]
    async fn test_manager_originated_call_skipped() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        // Manager calls the proxy (forward delivery) — should be skipped.
        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        // from=manager — this is a forward delivery
        let root = trace_node(
            &format!("{proxy_addr}"),
            &format!("{manager}"),
            "0xdeadbeef",
            "0x0",
            vec![child],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert!(
            detected.is_empty(),
            "manager-originated calls should be skipped"
        );
    }

    #[tokio::test]
    async fn test_ephemeral_proxy_detected() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000005555"
            .parse()
            .unwrap();
        let bridge: Address = "0x0000000000000000000000000000000000006666"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let lookup = MockLookup::new(); // no persistent proxies

        // Build createCrossChainProxy input: selector + address(32) + uint256(32)
        let mut create_params = vec![0u8; 64];
        // originalAddress at bytes 12..32 of first word
        create_params[12..32].copy_from_slice(original.as_slice());
        // originalRollupId = 1 in second word (last byte)
        create_params[63] = 1;

        let create_input = encode_input(&create_cross_chain_proxy_selector(), &create_params);

        // The output of createCrossChainProxy is an ABI-encoded address
        let mut output_bytes = vec![0u8; 32];
        output_bytes[12..32].copy_from_slice(proxy_addr.as_slice());
        let create_output = format!("0x{}", hex::encode(&output_bytes));

        // createCrossChainProxy node (bridge -> manager)
        let create_node = json!({
            "to": format!("{manager}"),
            "from": format!("{bridge}"),
            "input": create_input,
            "value": "0x0",
            "output": create_output,
            "calls": [],
            "type": "CALL"
        });

        // Proxy call node (bridge -> proxy -> manager.executeCrossChainCall)
        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let exec_child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let proxy_call_node = trace_node(
            &format!("{proxy_addr}"),
            &format!("{bridge}"),
            "0xaabbccdd",
            "0x0",
            vec![exec_child],
        );

        // Root: bridge call that first creates proxy, then calls it
        // Bridge is called by caller
        let bridge_node = trace_node(
            &format!("{bridge}"),
            &format!("{caller}"),
            "0x11223344",
            "0x0",
            vec![create_node, proxy_call_node],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &bridge_node,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(ephemeral.len(), 1, "should detect one ephemeral proxy");
        assert!(ephemeral.contains_key(&proxy_addr));
        assert_eq!(ephemeral[&proxy_addr].original_address, original);
        assert_eq!(ephemeral[&proxy_addr].original_rollup_id, 1);

        assert_eq!(
            detected.len(),
            1,
            "should detect one proxy call using ephemeral proxy"
        );
        assert_eq!(detected[0].destination, original);
        assert_eq!(detected[0].source_address, bridge);
    }

    #[tokio::test]
    async fn test_multiple_proxy_calls_in_one_tx() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_a: Address = "0x000000000000000000000000000000000000aaaa"
            .parse()
            .unwrap();
        let proxy_b: Address = "0x000000000000000000000000000000000000bbbb"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original_a: Address = "0x000000000000000000000000000000000000aa00"
            .parse()
            .unwrap();
        let original_b: Address = "0x000000000000000000000000000000000000bb00"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_a,
            ProxyInfo {
                original_address: original_a,
                original_rollup_id: 1,
            },
        );
        lookup.register(
            proxy_b,
            ProxyInfo {
                original_address: original_b,
                original_rollup_id: 2,
            },
        );

        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);

        let child_a = trace_node(
            &format!("{manager}"),
            &format!("{proxy_a}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let proxy_call_a = trace_node(
            &format!("{proxy_a}"),
            &format!("{caller}"),
            "0x11111111",
            "0x0",
            vec![child_a],
        );

        let child_b = trace_node(
            &format!("{manager}"),
            &format!("{proxy_b}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let proxy_call_b = trace_node(
            &format!("{proxy_b}"),
            &format!("{caller}"),
            "0x22222222",
            "0x0",
            vec![child_b],
        );

        // A contract that calls both proxies
        let contract: Address = "0x0000000000000000000000000000000000009999"
            .parse()
            .unwrap();
        let root = trace_node(
            &format!("{contract}"),
            &format!("{caller}"),
            "0xdeadbeef",
            "0x0",
            vec![proxy_call_a, proxy_call_b],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(detected.len(), 2, "should detect two proxy calls");
        assert_eq!(detected[0].destination, original_a);
        assert_eq!(detected[1].destination, original_b);
    }

    #[tokio::test]
    async fn test_reverted_proxy_call_still_detected() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        // Build a reverted trace node (has "error" field)
        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let child = json!({
            "to": format!("{manager}"),
            "from": format!("{proxy_addr}"),
            "input": exec_input,
            "value": "0x0",
            "calls": [],
            "output": "0x",
            "type": "CALL",
            "error": "execution reverted"
        });
        let root = json!({
            "to": format!("{proxy_addr}"),
            "from": format!("{caller}"),
            "input": "0xdeadbeef",
            "value": "0x0",
            "calls": [child],
            "output": "0x",
            "type": "CALL",
            "error": "execution reverted"
        });

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(
            detected.len(),
            1,
            "reverted proxy calls should still be detected"
        );
        assert_eq!(detected[0].destination, original);
    }

    #[tokio::test]
    async fn test_non_proxy_node_recurses() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let wrapper: Address = "0x0000000000000000000000000000000000007777"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        // Nested: caller -> wrapper -> proxy -> manager
        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let manager_child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let proxy_node = trace_node(
            &format!("{proxy_addr}"),
            &format!("{wrapper}"),
            "0xaabbccdd",
            "0x0",
            vec![manager_child],
        );
        let wrapper_node = trace_node(
            &format!("{wrapper}"),
            &format!("{caller}"),
            "0x11223344",
            "0x0",
            vec![proxy_node],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &wrapper_node,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(
            detected.len(),
            1,
            "should detect proxy call through wrapper"
        );
        assert_eq!(detected[0].destination, original);
        // source_address is whoever called the proxy — in this case the wrapper
        assert_eq!(detected[0].source_address, wrapper);
    }

    #[tokio::test]
    async fn test_proxy_cache_prevents_repeated_lookups() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        /// Counts how many times lookup_proxy is called.
        struct CountingLookup {
            proxies: HashMap<Address, ProxyInfo>,
            count: AtomicUsize,
        }

        impl ProxyLookup for CountingLookup {
            fn lookup_proxy(
                &self,
                address: Address,
            ) -> Pin<Box<dyn Future<Output = Option<ProxyInfo>> + Send + '_>> {
                self.count.fetch_add(1, Ordering::SeqCst);
                let result = self.proxies.get(&address).copied();
                Box::pin(async move { result })
            }
        }

        let mut proxies = HashMap::new();
        proxies.insert(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );
        let lookup = CountingLookup {
            proxies,
            count: AtomicUsize::new(0),
        };

        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);

        // Two separate proxy calls to the same proxy
        let child1 = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let call1 = trace_node(
            &format!("{proxy_addr}"),
            &format!("{caller}"),
            "0x11111111",
            "0x0",
            vec![child1],
        );

        let child2 = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        let call2 = trace_node(
            &format!("{proxy_addr}"),
            &format!("{caller}"),
            "0x22222222",
            "0x0",
            vec![child2],
        );

        let contract: Address = "0x0000000000000000000000000000000000009999"
            .parse()
            .unwrap();
        let root = trace_node(
            &format!("{contract}"),
            &format!("{caller}"),
            "0xdeadbeef",
            "0x0",
            vec![call1, call2],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(detected.len(), 2);
        // lookup_proxy should only be called once (second call uses cache)
        assert_eq!(
            lookup.count.load(Ordering::SeqCst),
            1,
            "proxy lookup should be cached after first call"
        );
    }

    #[tokio::test]
    async fn test_has_selector() {
        let sel = [0x9a, 0xf5, 0x32, 0x59]; // just an example
        assert!(has_selector(&[0x9a, 0xf5, 0x32, 0x59, 0x00], &sel));
        assert!(has_selector(&[0x9a, 0xf5, 0x32, 0x59], &sel));
        assert!(!has_selector(&[0x9a, 0xf5, 0x32], &sel)); // too short
        assert!(!has_selector(&[0x00, 0x00, 0x00, 0x00], &sel)); // wrong
        assert!(!has_selector(&[], &sel)); // empty
    }

    #[tokio::test]
    async fn test_empty_trace_no_detection() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let lookup = MockLookup::new();

        let root = json!({
            "to": "0x0000000000000000000000000000000000009999",
            "from": "0x0000000000000000000000000000000000003333",
            "input": "0xdeadbeef",
            "value": "0x0",
            "calls": [],
            "output": "0x",
            "type": "CALL"
        });

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert!(detected.is_empty());
    }

    #[tokio::test]
    async fn test_value_propagated() {
        let manager: Address = "0x0000000000000000000000000000000000001111"
            .parse()
            .unwrap();
        let proxy_addr: Address = "0x0000000000000000000000000000000000002222"
            .parse()
            .unwrap();
        let caller: Address = "0x0000000000000000000000000000000000003333"
            .parse()
            .unwrap();
        let original: Address = "0x0000000000000000000000000000000000004444"
            .parse()
            .unwrap();

        let mut lookup = MockLookup::new();
        lookup.register(
            proxy_addr,
            ProxyInfo {
                original_address: original,
                original_rollup_id: 1,
            },
        );

        let exec_input = encode_input(&execute_cross_chain_call_selector(), &[0u8; 64]);
        let child = trace_node(
            &format!("{manager}"),
            &format!("{proxy_addr}"),
            &exec_input,
            "0x0",
            vec![],
        );
        // Send 1 ETH with the proxy call
        let root = trace_node(
            &format!("{proxy_addr}"),
            &format!("{caller}"),
            "0xdeadbeef",
            "0xde0b6b3a7640000", // 1 ETH in hex
            vec![child],
        );

        let mut cache = HashMap::new();
        let mut ephemeral = HashMap::new();
        let mut detected = Vec::new();

        walk_trace_tree(
            &root,
            &[manager],
            &lookup,
            &mut cache,
            &mut ephemeral,
            &mut detected,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].value, U256::from(1_000_000_000_000_000_000u64));
    }
}
