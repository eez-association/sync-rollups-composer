//! Entry builder façade — single boundary for all cross-chain entry construction.
//!
//! Delegates to `cross_chain.rs` and `table_builder.rs` internals. Both directions
//! call this module instead of reaching into the builders directly, making the
//! entry construction API auditable from a single file.
//!
//! Introduced in refactor step 4.6 (PLAN.md §Phase 4).

#![allow(clippy::too_many_arguments)]

use alloy_primitives::{Address, Bytes, U256};

use crate::cross_chain::{
    self, CrossChainExecutionEntry, RollupId, TxOutcome,
};
use crate::table_builder;

// ──────────────────────────────────────────────────────────────────────────────
//  Simple entry pairs (CALL + RESULT)
// ──────────────────────────────────────────────────────────────────────────────

/// Build a simple CALL + RESULT entry pair for an L1→L2 cross-chain call.
pub(crate) fn build_simple_pair(
    rollup_id: RollupId,
    destination: Address,
    data: Vec<u8>,
    value: U256,
    source_address: Address,
    source_rollup: RollupId,
    call_success: bool,
    return_data: Vec<u8>,
) -> (CrossChainExecutionEntry, CrossChainExecutionEntry) {
    cross_chain::build_cross_chain_call_entries(
        rollup_id,
        destination,
        data,
        value,
        source_address,
        source_rollup,
        call_success,
        return_data,
    )
}

// ──────────────────────────────────────────────────────────────────────────────
//  L2→L1 entry set
// ──────────────────────────────────────────────────────────────────────────────

/// Build the full entry set for an L2→L1 cross-chain call.
pub(crate) fn build_l2_to_l1_entries(
    destination: Address,
    calldata: Vec<u8>,
    value: U256,
    source_address: Address,
    rollup_id: u64,
    rlp_encoded_tx: Vec<u8>,
    delivery_return_data: Vec<u8>,
    delivery_failed: bool,
    l1_delivery_scope: Vec<U256>,
    tx_outcome: TxOutcome,
) -> cross_chain::WithdrawalEntries {
    cross_chain::build_l2_to_l1_call_entries(
        destination,
        calldata,
        value,
        source_address,
        rollup_id,
        rlp_encoded_tx,
        delivery_return_data,
        delivery_failed,
        l1_delivery_scope,
        tx_outcome,
    )
}

// ──────────────────────────────────────────────────────────────────────────────
//  Continuation entries (multi-call patterns)
// ──────────────────────────────────────────────────────────────────────────────

/// Analyze L1→L2 calls for continuation patterns (flash-loan, reentrant).
pub(crate) fn analyze_l1_to_l2_continuations(
    calls: &[table_builder::L1DetectedCall],
    rollup_id: u64,
) -> Vec<table_builder::DetectedCall> {
    table_builder::analyze_continuation_calls(calls, rollup_id)
}

/// Analyze L2→L1 calls for continuation patterns.
pub(crate) fn analyze_l2_to_l1_continuations(
    l2_calls: &[table_builder::L2DetectedCall],
    return_calls: &[table_builder::L2ReturnCall],
    rollup_id: u64,
) -> Vec<table_builder::DetectedCall> {
    table_builder::analyze_l2_to_l1_continuation_calls(l2_calls, return_calls, rollup_id)
}

/// Build continuation entries from analyzed calls.
pub(crate) fn build_continuations(
    calls: &[table_builder::DetectedCall],
    rollup_id: RollupId,
) -> table_builder::ContinuationEntries {
    table_builder::build_continuation_entries(calls, rollup_id)
}

// ──────────────────────────────────────────────────────────────────────────────
//  Format conversion + encoding
// ──────────────────────────────────────────────────────────────────────────────

/// Convert L2-format entry pairs to L1-format entries.
pub(crate) fn pairs_to_l1_format(
    entries: &[CrossChainExecutionEntry],
) -> Vec<CrossChainExecutionEntry> {
    cross_chain::convert_pairs_to_l1_entries(entries)
}

/// Encode `loadExecutionTable(entries)` calldata.
pub(crate) fn encode_load_table(entries: &[CrossChainExecutionEntry]) -> Bytes {
    cross_chain::encode_load_execution_table_calldata(entries)
}

/// Build placeholder entries for L2 simulation (empty state deltas).
#[allow(dead_code)]
pub(crate) fn build_placeholder_entries(
    destination: Address,
    calldata: Vec<u8>,
    value: U256,
    source_address: Address,
    rollup_id: RollupId,
    source_rollup: RollupId,
    call_success: bool,
    return_data: Vec<u8>,
) -> Vec<CrossChainExecutionEntry> {
    let (call_entry, result_entry) = build_simple_pair(
        rollup_id,
        destination,
        calldata,
        value,
        source_address,
        source_rollup,
        call_success,
        return_data,
    );
    vec![call_entry, result_entry]
}
