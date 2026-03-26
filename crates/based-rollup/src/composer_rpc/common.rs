//! Shared utilities for both composer RPC directions.
//!
//! Contains HTTP helpers, JSON-RPC parsing, ABI utilities, and proxy detection
//! functions used by both L1→L2 and L2→L1 composer RPC modules.

use alloy_primitives::{Address, U256};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};
use serde_json::Value;

// ──────────────────────────────────────────────────────────────────────────────
//  JSON-RPC parsing
// ──────────────────────────────────────────────────────────────────────────────

/// Extract (method, params) pairs from a JSON-RPC request (single or batch).
pub(crate) fn extract_methods(json: &Value) -> Vec<(String, Option<&Vec<Value>>)> {
    let mut result = Vec::new();
    match json {
        Value::Object(obj) => {
            if let Some(Value::String(method)) = obj.get("method") {
                let params = obj.get("params").and_then(|p| p.as_array());
                result.push((method.clone(), params));
            }
        }
        Value::Array(arr) => {
            for item in arr {
                if let Value::Object(obj) = item {
                    if let Some(Value::String(method)) = obj.get("method") {
                        let params = obj.get("params").and_then(|p| p.as_array());
                        result.push((method.clone(), params));
                    }
                }
            }
        }
        _ => {}
    }
    result
}

// ──────────────────────────────────────────────────────────────────────────────
//  HTTP helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Add CORS headers to a response.
pub(crate) fn cors_response(mut resp: Response<Full<Bytes>>) -> Response<Full<Bytes>> {
    let headers = resp.headers_mut();
    headers.insert(
        "Access-Control-Allow-Origin",
        "*".parse().expect("valid header"),
    );
    headers.insert(
        "Access-Control-Allow-Methods",
        "POST, OPTIONS".parse().expect("valid header"),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        "Content-Type".parse().expect("valid header"),
    );
    resp
}

/// Build a JSON-RPC error response.
pub(crate) fn error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": -32603, "message": message },
        "id": null
    });
    cors_response(
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .expect("valid response"),
    )
}

// ──────────────────────────────────────────────────────────────────────────────
//  Transaction hash computation
// ──────────────────────────────────────────────────────────────────────────────

/// Compute the tx hash from a raw signed transaction hex string.
/// Returns the hash as a `0x`-prefixed hex string.
pub(crate) fn compute_tx_hash(raw_tx_hex: &str) -> Option<String> {
    use alloy_consensus::transaction::TxEnvelope;
    use alloy_rlp::Decodable;

    let hex_str = raw_tx_hex.strip_prefix("0x").unwrap_or(raw_tx_hex);
    let bytes = hex::decode(hex_str).ok()?;
    let envelope = TxEnvelope::decode(&mut bytes.as_slice()).ok()?;
    Some(format!("{:#x}", envelope.tx_hash()))
}

// ──────────────────────────────────────────────────────────────────────────────
//  ABI parsing helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Parse an address from a 32-byte ABI-encoded hex return value.
///
/// Expects a `0x`-prefixed hex string of at least 64 hex characters (32 bytes).
/// The address occupies the last 20 bytes (bytes 12..32) of the ABI word.
/// Returns `None` if the address is zero or the format is invalid.
pub(crate) fn parse_address_from_abi_return(hex_str: &str) -> Option<Address> {
    let clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    if clean.len() < 64 {
        return None;
    }
    let bytes = hex::decode(&clean[..64]).ok()?;
    if bytes.len() < 32 {
        return None;
    }
    let addr = Address::from_slice(&bytes[12..32]);
    if addr.is_zero() {
        return None;
    }
    Some(addr)
}

// ──────────────────────────────────────────────────────────────────────────────
//  RPC view-call helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Make a read-only `eth_call` to a contract and return the hex result string.
///
/// `data` should be the full `0x`-prefixed hex-encoded calldata (selector + args).
/// Returns `None` if the call fails or returns no result.
pub(crate) async fn eth_call_view(
    client: &reqwest::Client,
    rpc_url: &str,
    to: Address,
    data: &str,
) -> Option<String> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": format!("{to}"), "data": data}, "latest"],
        "id": 99995
    });
    let resp = client.post(rpc_url).json(&req).send().await.ok()?;
    let body: Value = resp.json().await.ok()?;
    if body.get("error").is_some() {
        return None;
    }
    body.get("result")?.as_str().map(|s| s.to_string())
}

// ──────────────────────────────────────────────────────────────────────────────
//  L1 block context
// ──────────────────────────────────────────────────────────────────────────────

/// Get L1 block context: (block_number, block_hash, parent_hash).
///
/// For real `postBatch`, use `(number + 1, hash)` as `(target_block, parent_hash)`.
/// For `traceCallMany` at "latest", use `(number, parent_hash)` since the trace
/// executes at the current block where `block.number = number` and
/// `blockhash(block.number - 1) = parent_hash`.
pub(crate) async fn get_l1_block_context(
    client: &reqwest::Client,
    l1_rpc_url: &str,
) -> eyre::Result<(u64, alloy_primitives::B256, alloy_primitives::B256)> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getBlockByNumber",
        "params": ["latest", false],
        "id": 99997
    });

    let resp = client
        .post(l1_rpc_url)
        .json(&req)
        .send()
        .await?
        .json::<Value>()
        .await?;

    let block = resp
        .get("result")
        .ok_or_else(|| eyre::eyre!("no result from eth_getBlockByNumber"))?;

    let number_hex = block
        .get("number")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no block number"))?;
    let number = u64::from_str_radix(number_hex.strip_prefix("0x").unwrap_or(number_hex), 16)
        .map_err(|e| eyre::eyre!("invalid block number: {e}"))?;

    let hash_hex = block
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no block hash"))?;
    let hash = hash_hex
        .parse::<alloy_primitives::B256>()
        .map_err(|e| eyre::eyre!("invalid block hash: {e}"))?;

    let parent_hash_hex = block
        .get("parentHash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no parent hash"))?;
    let parent_hash = parent_hash_hex
        .parse::<alloy_primitives::B256>()
        .map_err(|e| eyre::eyre!("invalid parent hash: {e}"))?;

    Ok((number, hash, parent_hash))
}

// ──────────────────────────────────────────────────────────────────────────────
//  Verification key query
// ──────────────────────────────────────────────────────────────────────────────

/// Query the verification key for a rollup from the Rollups contract on L1.
pub(crate) async fn get_verification_key(
    client: &reqwest::Client,
    l1_rpc_url: &str,
    rollups_address: Address,
    rollup_id: u64,
) -> eyre::Result<alloy_primitives::B256> {
    // rollups(uint256) — selector + uint256(rollup_id)
    let selector = &alloy_primitives::keccak256(b"rollups(uint256)")[..4];
    let calldata = format!("0x{}{:0>64x}", hex::encode(selector), rollup_id);

    let result_hex = eth_call_view(client, l1_rpc_url, rollups_address, &calldata)
        .await
        .ok_or_else(|| eyre::eyre!("eth_call for rollups() returned no result"))?;

    // rollups() returns (address owner, bytes32 verificationKey, bytes32 stateRoot, uint256 etherBalance)
    // VK is at offset 32..64 (word 1, after address at word 0)
    let hex_clean = result_hex.strip_prefix("0x").unwrap_or(&result_hex);
    if hex_clean.len() < 128 {
        return Err(eyre::eyre!("rollups() return too short for VK"));
    }
    let vk_hex = &hex_clean[64..128];
    let vk_str = format!("0x{vk_hex}");
    vk_str
        .parse::<alloy_primitives::B256>()
        .map_err(|e| eyre::eyre!("invalid VK: {e}"))
}

// ──────────────────────────────────────────────────────────────────────────────
//  L2 proxy detection
// ──────────────────────────────────────────────────────────────────────────────

/// Query `authorizedProxies(address)` on the L2 CrossChainManagerL2 contract.
///
/// Returns `Some((originalAddress, originalRollupId))` if the address is a
/// registered proxy, `None` otherwise.
pub(crate) async fn detect_cross_chain_proxy_on_l2(
    client: &reqwest::Client,
    upstream_url: &str,
    address: Address,
    ccm_address: Address,
) -> Option<(Address, u64)> {
    // authorizedProxies(address) — selector 0x360d95b6
    let calldata = format!("0x360d95b6{:0>64}", hex::encode(address.as_slice()));

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": format!("{ccm_address}"), "data": calldata}, "latest"],
        "id": 99996
    });

    let resp = client.post(upstream_url).json(&req).send().await.ok()?;
    let body: Value = resp.json().await.ok()?;

    if body.get("error").is_some() {
        return None;
    }

    let hex_data = body.get("result")?.as_str()?;
    let hex_clean = hex_data.strip_prefix("0x").unwrap_or(hex_data);

    // Return data: 64 hex chars (32 bytes) for address + 64 hex chars for uint64
    if hex_clean.len() < 128 {
        return None;
    }

    // First 32 bytes = originalAddress (address, right-padded in 32 bytes)
    let addr_bytes = hex::decode(&hex_clean[..64]).ok()?;
    if addr_bytes.len() < 32 {
        return None;
    }
    let original_address = Address::from_slice(&addr_bytes[12..32]);

    // If originalAddress is zero, the proxy isn't registered
    if original_address.is_zero() {
        return None;
    }

    // Second 32 bytes = originalRollupId (uint64 ABI-encoded as uint256)
    let rid_bytes = hex::decode(&hex_clean[64..128]).ok()?;
    if rid_bytes.len() < 32 {
        return None;
    }
    // Read last 8 bytes as u64 big-endian
    let mut val: u64 = 0;
    let start = rid_bytes.len().saturating_sub(8);
    for b in &rid_bytes[start..] {
        val = (val << 8) | (*b as u64);
    }

    Some((original_address, val))
}

/// Information about an internal cross-chain proxy call detected via trace.
#[derive(Clone)]
pub struct DiscoveredProxyCall {
    /// The proxy's `originalAddress` from `authorizedProxies`.
    pub original_address: Address,
    /// The proxy's `originalRollupId` from `authorizedProxies`.
    pub _original_rollup_id: u64,
    /// The `from` field of the trace node (caller of the proxy, i.e. source_address).
    pub source_address: Address,
    /// Unwrapped calldata (real destination call data after stripping executeOnBehalf).
    pub data: Vec<u8>,
    /// ETH value sent with the call.
    pub value: U256,
    /// Whether the trace node has an "error" field (reverted).
    pub reverted: bool,
}

/// Walk an L2 callTracer trace tree depth-first, querying `authorizedProxies(to)`
/// on the L2 CCM for each node to identify cross-chain proxy calls.
///
/// For each proxy found:
/// - If `from == cross_chain_manager_address`: this is a FORWARD DELIVERY call
///   (CCM delivering an incoming cross-chain call). Skip classification but
///   recurse into children to find return calls deeper in the tree.
/// - Otherwise: this is a return call. Extract proxy identity, unwrap
///   `executeOnBehalf(address,bytes)` from input to get real calldata.
///   Filter by `originalRollupId` — only include calls where the proxy targets
///   a different rollup than ours. Do NOT recurse into proxy internals.
///
/// Returns all discovered proxy calls (both reverted and successful).
pub async fn find_failed_proxy_calls_in_l2_trace(
    client: &reqwest::Client,
    l2_rpc_url: &str,
    ccm_address: Address,
    node: &serde_json::Value,
    our_rollup_id: u64,
    proxy_cache: &mut std::collections::HashMap<Address, Option<(Address, u64)>>,
    results: &mut Vec<DiscoveredProxyCall>,
) {
    let mut should_recurse = true;

    if let Some(to_str) = node.get("to").and_then(|v| v.as_str()) {
        if let Ok(to_addr) = to_str.parse::<Address>() {
            // Skip the CCM itself — not a proxy
            if to_addr != ccm_address {
                let proxy_info = match proxy_cache.get(&to_addr) {
                    Some(cached) => *cached,
                    None => {
                        let result = detect_cross_chain_proxy_on_l2(
                            client,
                            l2_rpc_url,
                            to_addr,
                            ccm_address,
                        )
                        .await;
                        proxy_cache.insert(to_addr, result);
                        result
                    }
                };

                if let Some((original_address, original_rollup_id)) = proxy_info {
                    let call_from = node
                        .get("from")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<Address>().ok())
                        .unwrap_or(Address::ZERO);

                    if call_from == ccm_address {
                        // Forward delivery call (CCM → proxy → executeCrossChainCall).
                        // Skip classification but DO recurse into children to find
                        // return calls deeper in the tree.
                        tracing::debug!(
                            target: "based_rollup::proxy",
                            proxy = %to_addr,
                            %original_address,
                            "skipping forward delivery proxy call (from=CCM) in L2 trace — recursing for return calls"
                        );
                    } else {
                        // Return call: user contract → proxy.
                        // Filter by originalRollupId — only include calls targeting
                        // a different rollup (e.g., originalRollupId == 0 for L1).
                        if original_rollup_id != our_rollup_id {
                            let input = node.get("input").and_then(|v| v.as_str()).unwrap_or("0x");
                            let input_clean = input.strip_prefix("0x").unwrap_or(input);
                            let mut input_bytes = hex::decode(input_clean).unwrap_or_default();

                            // Strip executeOnBehalf(address,bytes) wrapper if present.
                            // executeOnBehalf selector = 0x532f0839
                            if input_bytes.len() >= 100
                                && input_bytes[..4] == [0x53, 0x2f, 0x08, 0x39]
                            {
                                // ABI decode: executeOnBehalf(address dest, bytes data)
                                // Skip: selector(4) + address(32) + offset(32) = 68
                                // Read length at offset 68
                                if input_bytes.len() >= 100 {
                                    let data_len = U256::from_be_slice(&input_bytes[68..100]);
                                    let data_start = 100usize;
                                    let data_end = data_start + data_len.to::<usize>();
                                    if data_end <= input_bytes.len() {
                                        tracing::debug!(
                                            target: "based_rollup::proxy",
                                            original_len = input_bytes.len(),
                                            unwrapped_len = data_end - data_start,
                                            "stripped executeOnBehalf wrapper from L2 proxy call data"
                                        );
                                        input_bytes = input_bytes[data_start..data_end].to_vec();
                                    }
                                }
                            }

                            let call_value = node
                                .get("value")
                                .and_then(|v| v.as_str())
                                .and_then(|s| {
                                    U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok()
                                })
                                .unwrap_or(U256::ZERO);

                            let reverted = node.get("error").is_some();

                            tracing::info!(
                                target: "based_rollup::proxy",
                                proxy = %to_addr,
                                %original_address,
                                original_rollup_id,
                                source = %call_from,
                                data_len = input_bytes.len(),
                                reverted,
                                "discovered L2 proxy call via authorizedProxies"
                            );

                            results.push(DiscoveredProxyCall {
                                original_address,
                                _original_rollup_id: original_rollup_id,
                                source_address: call_from,
                                data: input_bytes,
                                value: call_value,
                                reverted,
                            });
                        }
                        // Do NOT recurse into proxy internals
                        should_recurse = false;
                    }
                }
            }
        }
    }

    if should_recurse {
        if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
            for child in calls {
                Box::pin(find_failed_proxy_calls_in_l2_trace(
                    client,
                    l2_rpc_url,
                    ccm_address,
                    child,
                    our_rollup_id,
                    proxy_cache,
                    results,
                ))
                .await;
            }
        }
    }
}
