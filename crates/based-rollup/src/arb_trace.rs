use crate::cross_chain::{ActionHash, CrossChainExecutionEntry};
use alloy_primitives::{Address, B256, U256, keccak256};
use serde_json::{Map, Value, json};

pub const TARGET: &str = "based_rollup::arb_trace";

const BRIDGE_ETHER_SELECTOR: [u8; 4] = [0xf4, 0x02, 0xd9, 0xf3];
const BRIDGE_TOKENS_SELECTOR: [u8; 4] = [0x33, 0xb1, 0x5a, 0xad];
const RECEIVE_TOKENS_SELECTOR: [u8; 4] = [0x6b, 0x39, 0x96, 0xb0];
const EXECUTION_NOT_FOUND_SELECTOR: [u8; 4] = [0xed, 0x6b, 0xc7, 0x50];

#[derive(Debug, Clone)]
pub struct ArbTraceMeta {
    pub trace_id: B256,
    pub source_address: Address,
    pub destination: Address,
    pub amount: Option<U256>,
    pub call_kind: &'static str,
    pub call_data_hex: String,
    pub call_data_keccak: B256,
    pub entry_action_hashes: Vec<ActionHash>,
    pub state_delta_before: Option<B256>,
    pub state_delta_after: Option<B256>,
}

#[derive(Debug, Clone, Default)]
pub struct RevertSummary {
    pub selector: Option<String>,
    pub outer_selector: Option<String>,
    pub execution_not_found_arg: Option<String>,
}

pub fn trace_id_from_raw_tx_bytes(raw_tx: &[u8]) -> Option<B256> {
    if raw_tx.is_empty() {
        None
    } else {
        Some(keccak256(raw_tx))
    }
}

pub fn build_trace_meta(
    raw_tx: &[u8],
    source_address: Address,
    destination: Address,
    call_data: &[u8],
    value: U256,
) -> Option<ArbTraceMeta> {
    let trace_id = trace_id_from_raw_tx_bytes(raw_tx)?;
    let (call_kind, amount) = summarize_amount(call_data, value);
    Some(ArbTraceMeta {
        trace_id,
        source_address,
        destination,
        amount,
        call_kind,
        call_data_hex: format!("0x{}", hex::encode(call_data)),
        call_data_keccak: keccak256(call_data),
        entry_action_hashes: Vec::new(),
        state_delta_before: None,
        state_delta_after: None,
    })
}

pub fn call_summary_json(
    source_address: Address,
    destination: Address,
    call_data: &[u8],
    value: U256,
) -> Value {
    let (call_kind, amount) = summarize_amount(call_data, value);
    json!({
        "source_address": format!("{source_address}"),
        "destination": format!("{destination}"),
        "call_kind": call_kind,
        "amount_wei": amount.map(|v| v.to_string()),
        "call_data_hex": format!("0x{}", hex::encode(call_data)),
        "call_data_keccak": format!("{}", keccak256(call_data)),
    })
}

pub fn with_entry_action_hashes(
    meta: &ArbTraceMeta,
    entries: &[CrossChainExecutionEntry],
) -> ArbTraceMeta {
    let mut updated = meta.clone();
    updated.entry_action_hashes = entries.iter().map(|e| e.action_hash).collect();
    let (before, after) = state_delta_bounds(entries);
    updated.state_delta_before = before;
    updated.state_delta_after = after;
    updated
}

pub fn state_delta_bounds(entries: &[CrossChainExecutionEntry]) -> (Option<B256>, Option<B256>) {
    let before = entries
        .first()
        .and_then(|e| e.state_deltas.first())
        .map(|d| d.current_state);
    let after = entries
        .last()
        .and_then(|e| e.state_deltas.first())
        .map(|d| d.new_state);
    (before, after)
}

pub fn emit_phase(phase: &str, trace_id: B256, payload: Value) {
    let mut obj = Map::new();
    obj.insert("phase".into(), json!(phase));
    obj.insert("trace_id".into(), json!(format!("{trace_id}")));
    if let Value::Object(map) = payload {
        obj.extend(map);
    }
    tracing::info!(target: TARGET, "{}", serde_json::Value::Object(obj));
}

pub fn emit_with_meta(
    phase: &str,
    meta: &ArbTraceMeta,
    state_delta_before: Option<B256>,
    state_delta_after: Option<B256>,
    extra: Value,
) {
    let mut obj = Map::new();
    obj.insert(
        "source_address".into(),
        json!(format!("{}", meta.source_address)),
    );
    obj.insert("destination".into(), json!(format!("{}", meta.destination)));
    obj.insert("call_kind".into(), json!(meta.call_kind));
    obj.insert(
        "amount_wei".into(),
        meta.amount
            .map(|v| json!(v.to_string()))
            .unwrap_or(Value::Null),
    );
    obj.insert("call_data_hex".into(), json!(meta.call_data_hex));
    obj.insert(
        "call_data_keccak".into(),
        json!(format!("{}", meta.call_data_keccak)),
    );
    obj.insert(
        "entry_action_hashes".into(),
        json!(
            meta.entry_action_hashes
                .iter()
                .map(|h| format!("{h}"))
                .collect::<Vec<_>>()
        ),
    );
    obj.insert(
        "state_delta_before".into(),
        state_delta_before
            .or(meta.state_delta_before)
            .map(|v| json!(format!("{v}")))
            .unwrap_or(Value::Null),
    );
    obj.insert(
        "state_delta_after".into(),
        state_delta_after
            .or(meta.state_delta_after)
            .map(|v| json!(format!("{v}")))
            .unwrap_or(Value::Null),
    );
    if let Value::Object(map) = extra {
        obj.extend(map);
    }
    emit_phase(phase, meta.trace_id, Value::Object(obj));
}

pub fn parse_revert_summary(output_hex: &str) -> RevertSummary {
    let bytes = match hex::decode(output_hex.strip_prefix("0x").unwrap_or(output_hex)) {
        Ok(bytes) => bytes,
        Err(_) => return RevertSummary::default(),
    };
    parse_revert_summary_bytes(&bytes)
}

fn parse_revert_summary_bytes(bytes: &[u8]) -> RevertSummary {
    if bytes.len() < 4 {
        return RevertSummary::default();
    }

    let outer_selector = format!("0x{}", hex::encode(&bytes[..4]));
    let execution_not_found_arg = if bytes.len() >= 36 && bytes[..4] == EXECUTION_NOT_FOUND_SELECTOR
    {
        Some(format!("0x{}", hex::encode(&bytes[4..36])))
    } else {
        None
    };

    if bytes.len() > 68 {
        let params = &bytes[4..];
        if let Some(len_bytes) = params.get(32..64) {
            let inner_len = U256::from_be_slice(len_bytes).saturating_to::<usize>();
            if inner_len > 0 && params.len() >= 64 + inner_len {
                let inner = &params[64..64 + inner_len];
                let mut summary = parse_revert_summary_bytes(inner);
                if summary.selector.is_some() {
                    summary.outer_selector = Some(outer_selector);
                    return summary;
                }
            }
        }
    }

    RevertSummary {
        selector: Some(outer_selector.clone()),
        outer_selector: Some(outer_selector),
        execution_not_found_arg,
    }
}

fn summarize_amount(call_data: &[u8], value: U256) -> (&'static str, Option<U256>) {
    if call_data.len() >= 4 {
        let selector = [call_data[0], call_data[1], call_data[2], call_data[3]];
        if selector == RECEIVE_TOKENS_SELECTOR && call_data.len() >= 4 + 32 * 4 {
            let amount = U256::from_be_slice(&call_data[4 + 32 * 3..4 + 32 * 4]);
            return ("receiveTokens", Some(amount));
        }
        if selector == BRIDGE_TOKENS_SELECTOR && call_data.len() >= 4 + 32 * 2 {
            let amount = U256::from_be_slice(&call_data[4 + 32..4 + 32 * 2]);
            return ("bridgeTokens", Some(amount));
        }
        if selector == BRIDGE_ETHER_SELECTOR {
            return ("bridgeEther", Some(value));
        }
    }
    ("generic", (!value.is_zero()).then_some(value))
}
