//! Shared utilities for both composer RPC directions.
//!
//! Contains HTTP helpers, JSON-RPC parsing, ABI utilities, and proxy detection
//! functions used by both L1→L2 and L2→L1 composer RPC modules.

use alloy_primitives::{Address, U256};
use alloy_sol_types::sol;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};
use serde_json::Value;

// ──────────────────────────────────────────────────────────────────────────────
//  ABI bindings via sol! macro — selectors derived at compile time
// ──────────────────────────────────────────────────────────────────────────────

sol! {
    /// Read-only view functions on the cross-chain manager / Rollups contracts.
    /// Used for proxy identity queries (authorizedProxies), system address lookups,
    /// and proxy calldata detection (executeOnBehalf).
    interface IProxyRegistry {
        /// Query proxy identity. Returns (originalAddress, originalRollupId) for
        /// registered proxies; zero address if not registered.
        function authorizedProxies(address proxy)
            external
            view
            returns (address originalAddress, uint64 originalRollupId);

        /// The system address authorized for admin operations (CCM getter).
        function SYSTEM_ADDRESS() external view returns (address);

        /// CrossChainProxy.executeOnBehalf — used to unwrap proxy calldata.
        function executeOnBehalf(address destination, bytes calldata data) external payable;
    }
}

sol! {
    /// Known Rollups.sol / CrossChainManagerL2 error types for debug log decoding.
    /// Selectors are derived at compile time — NEVER hardcode hex selectors.
    interface IRollupsErrors {
        error ExecutionNotFound();
        error InvalidRevertData();
        error EtherDeltaMismatch();
        error StateAlreadyUpdatedThisBlock();
        error StateRootMismatch();
        error CallExecutionFailed();
        error UnauthorizedProxy();
    }
}

sol! {
    /// Bridge.sol error for wrapped proxy call failures.
    interface IBridgeErrors {
        error ProxyCallFailed(bytes reason);
    }
}

sol! {
    /// Standard Solidity revert error — `Error(string)`.
    interface ISolidityErrors {
        error Error(string);
    }
}

/// 4-byte selector for `authorizedProxies(address)`.
/// Used indirectly via `encode_authorized_proxies_calldata()`.
#[allow(dead_code)]
pub(crate) const AUTHORIZED_PROXIES_SELECTOR: [u8; 4] =
    <IProxyRegistry::authorizedProxiesCall as alloy_sol_types::SolCall>::SELECTOR;

/// 4-byte selector for `SYSTEM_ADDRESS()`.
/// Used indirectly via `encode_system_address_calldata()`.
#[allow(dead_code)]
pub(crate) const SYSTEM_ADDRESS_SELECTOR: [u8; 4] =
    <IProxyRegistry::SYSTEM_ADDRESSCall as alloy_sol_types::SolCall>::SELECTOR;

/// 4-byte selector for `executeOnBehalf(address,bytes)`.
/// Retained for potential future use (e.g., if manual ABI stripping is ever needed).
#[allow(dead_code)]
pub(crate) const EXECUTE_ON_BEHALF_SELECTOR: [u8; 4] =
    <IProxyRegistry::executeOnBehalfCall as alloy_sol_types::SolCall>::SELECTOR;

/// 4-byte selector for `Error(string)` — standard Solidity revert.
pub(crate) const ERROR_STRING_SELECTOR: [u8; 4] =
    <ISolidityErrors::Error as alloy_sol_types::SolError>::SELECTOR;

/// 4-byte selector for `ExecutionNotFound()`.
pub(crate) const EXECUTION_NOT_FOUND_SELECTOR: [u8; 4] =
    <IRollupsErrors::ExecutionNotFound as alloy_sol_types::SolError>::SELECTOR;

/// 4-byte selector for `InvalidRevertData()`.
pub(crate) const INVALID_REVERT_DATA_SELECTOR: [u8; 4] =
    <IRollupsErrors::InvalidRevertData as alloy_sol_types::SolError>::SELECTOR;

/// 4-byte selector for `EtherDeltaMismatch()`.
pub(crate) const ETHER_DELTA_MISMATCH_SELECTOR: [u8; 4] =
    <IRollupsErrors::EtherDeltaMismatch as alloy_sol_types::SolError>::SELECTOR;

/// 4-byte selector for `StateAlreadyUpdatedThisBlock()`.
pub(crate) const STATE_ALREADY_UPDATED_SELECTOR: [u8; 4] =
    <IRollupsErrors::StateAlreadyUpdatedThisBlock as alloy_sol_types::SolError>::SELECTOR;

/// 4-byte selector for `StateRootMismatch()`.
pub(crate) const STATE_ROOT_MISMATCH_SELECTOR: [u8; 4] =
    <IRollupsErrors::StateRootMismatch as alloy_sol_types::SolError>::SELECTOR;

/// 4-byte selector for `CallExecutionFailed()`.
pub(crate) const CALL_EXECUTION_FAILED_SELECTOR: [u8; 4] =
    <IRollupsErrors::CallExecutionFailed as alloy_sol_types::SolError>::SELECTOR;

/// 4-byte selector for `UnauthorizedProxy()`.
pub(crate) const UNAUTHORIZED_PROXY_SELECTOR: [u8; 4] =
    <IRollupsErrors::UnauthorizedProxy as alloy_sol_types::SolError>::SELECTOR;

/// 4-byte selector for `ProxyCallFailed(bytes)`.
pub(crate) const PROXY_CALL_FAILED_SELECTOR: [u8; 4] =
    <IBridgeErrors::ProxyCallFailed as alloy_sol_types::SolError>::SELECTOR;

/// Build a `0x`-prefixed hex string for `authorizedProxies(address)` calldata.
///
/// Uses typed ABI encoding via `sol!` macro — NEVER hardcode selectors.
pub(crate) fn encode_authorized_proxies_calldata(address: Address) -> String {
    use alloy_sol_types::SolCall;
    let calldata = IProxyRegistry::authorizedProxiesCall { proxy: address }.abi_encode();
    format!("0x{}", hex::encode(&calldata))
}

/// Build a `0x`-prefixed hex string for `SYSTEM_ADDRESS()` calldata.
///
/// Uses typed ABI encoding via `sol!` macro — NEVER hardcode selectors.
pub(crate) fn encode_system_address_calldata() -> String {
    use alloy_sol_types::SolCall;
    let calldata = IProxyRegistry::SYSTEM_ADDRESSCall {}.abi_encode();
    format!("0x{}", hex::encode(&calldata))
}

/// Format a 4-byte selector as a `0x`-prefixed hex string for matching against
/// trace output values (e.g., `"0xed6bc750"`).
pub(crate) fn selector_hex_prefixed(sel: &[u8; 4]) -> String {
    format!("0x{}", hex::encode(sel))
}

/// Format a 4-byte selector as a non-prefixed hex string for matching against
/// inner error data (e.g., `"ed6bc750"`).
pub(crate) fn selector_hex_bare(sel: &[u8; 4]) -> String {
    hex::encode(sel)
}

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
    // authorizedProxies(address) — typed ABI encoding via sol! macro
    let calldata = encode_authorized_proxies_calldata(address);

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

// Legacy `find_failed_proxy_calls_in_l2_trace` removed — replaced by
// `walk_l2_trace_for_discovered_proxy_calls` in l2_to_l1.rs (uses
// `trace::walk_trace_tree` with ephemeral proxy support).
