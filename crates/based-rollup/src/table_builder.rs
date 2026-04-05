//! Execution table builder for continuation entries (multi-call continuations, multi-hop cross-chain).
//!
//! Generates L2 and L1 execution entries from a sequence of detected cross-chain calls.
//! Simple deposits/withdrawals use the legacy path in `cross_chain.rs`; this module
//! handles multi-call patterns where CALL_A triggers CALL_B (continuation) which may
//! itself trigger CALL_C (child call in the opposite direction).
//!
//! The entry structure matches `IntegrationTestFlashLoan.t.sol`.

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::SolType;
use serde::{Deserialize, Serialize};

use crate::cross_chain::{
    CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
    ICrossChainManagerL2,
};

/// Direction of a cross-chain call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    /// L1 → L2 (deposit-like): executed on L2 via executeIncomingCrossChainCall
    L1ToL2,
    /// L2 → L1 (withdrawal-like): executed on L1 via executeCrossChainCall
    L2ToL1,
}

/// A cross-chain call detected by the L1 proxy's trace analysis.
///
/// Calls are ordered by detection sequence. Continuation calls follow
/// the call they continue from; child calls are linked via `parent_call_index`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedCall {
    /// Direction of this cross-chain call.
    pub direction: CallDirection,
    /// The CALL action for this detected call.
    pub call_action: CrossChainAction,
    /// Index of the parent call whose L2 execution triggers this call.
    /// `None` for root-level calls; `Some(i)` for calls made within call[i]'s
    /// execution on L2 (e.g., bridgeTokens inside claimAndBridgeBack).
    pub parent_call_index: Option<usize>,
    /// Whether this call is a continuation — chained after a previous L1→L2 call
    /// returns (i.e., the L2 execution table returns this CALL as nextAction
    /// instead of a terminal RESULT).
    pub is_continuation: bool,
    /// Nesting depth of this call in the cross-chain call tree.
    /// 0 for root-level calls, 1 for children of root calls, 2 for grandchildren, etc.
    /// Used for logging/debugging only — does NOT affect scope computation
    /// (each reentrant `executeCrossChainCall` starts its own scope tree from `[]`).
    #[serde(default)]
    pub depth: usize,
    /// Return data from the L1 delivery simulation for this call.
    /// Used to construct the delivery RESULT action hash on L1 entries.
    /// When non-empty, the RESULT action includes this data (matching what
    /// `_processCallAtScope` builds with `data: returnData` after `executeOnBehalf`).
    /// Empty for calls where the delivery returns no data (e.g., ETH transfers to EOAs).
    #[serde(default)]
    pub delivery_return_data: Vec<u8>,
    /// Return data from simulating the return call's execution on L2.
    /// Used for the L2 scope resolution RESULT hash. When the return call's target
    /// returns data (e.g., Counter.increment() → uint256), the RESULT hash must include
    /// that data. Otherwise result_void is used and the hash mismatches (issue #245).
    #[serde(default)]
    pub l2_return_data: Vec<u8>,
    /// Whether the L2 return call simulation reverted (#246 audit: failed flag).
    #[serde(default)]
    pub l2_delivery_failed: bool,
    /// Scope for this call's L1 delivery CALL action.
    /// Accumulates across hops: hop1=[0], hop2=[0,0], hop3=[0,0,0].
    /// Computed as accumulated_prefix ++ local_tree_path.
    /// Used by table_builder instead of hardcoded vec![U256::ZERO].
    #[serde(default)]
    pub scope: Vec<U256>,
    /// Iterative discovery iteration when this call was first detected.
    /// 0 = initial trace / same iteration as parent. >0 = discovered in a
    /// later iteration (reentrant: child's L1 execution triggered this call).
    #[serde(default)]
    pub discovery_iteration: usize,
    /// Whether this call is inside a reverted frame on L2 (try/catch that reverts).
    /// Used for partial revert patterns (revertContinueL2): the reverted call
    /// gets scope [0,0], REVERT(scope=[0]) undoes it, REVERT_CONTINUE continues
    /// with the non-reverted call at scope [1].
    #[serde(default)]
    pub in_reverted_frame: bool,
}

/// Output of `build_continuation_entries`: L2 table entries and L1 deferred entries.
#[derive(Debug, Clone)]
pub struct ContinuationEntries {
    /// Entries loaded into L2 execution table via `loadExecutionTable`.
    /// These have empty state deltas (filled later by attach_*_state_deltas).
    pub l2_entries: Vec<CrossChainExecutionEntry>,
    /// Entries posted to L1 via `postBatch`.
    /// These have empty state deltas (filled later by the driver).
    pub l1_entries: Vec<CrossChainExecutionEntry>,
}

/// Compute the action hash for a `CrossChainAction` using Solidity ABI encoding.
///
/// `actionHash = keccak256(abi.encode(action))`
pub fn compute_action_hash(action: &CrossChainAction) -> B256 {
    keccak256(ICrossChainManagerL2::Action::abi_encode(
        &action.to_sol_action(),
    ))
}

/// Reorder entries within same-`action_hash` groups so that Solidity's
/// forward-iteration + swap-and-pop in `_consumeExecution` produces FIFO
/// consumption order.
///
/// Solidity consumes by finding the first entry matching `actionHash`,
/// then swapping the last element into the consumed position. For N entries
/// `[E0, E1, ..., E(N-1)]` consumed in creation order, the required storage
/// layout is `[E0, E(N-1), E(N-2), ..., E1]` — first element stays in place,
/// remaining are reversed.
///
/// **Proof (N=3)**: `[E0, E2, E1]` → consume E0, swap with last(E1) →
/// `[E1, E2]` → consume E1, swap with last(E2) → `[E2]` → consume E2. FIFO.
///
/// For groups of size 1 or 2 this is a no-op (reversing 0 or 1 elements is
/// identity), so existing 2-entry continuation output is unchanged.
fn reorder_for_swap_and_pop(entries: &mut Vec<CrossChainExecutionEntry>) {
    use std::collections::HashMap;

    // The Solidity CCM uses a single flat array for all entries.
    // `_consumeExecution` scans from index 0 (FIFO) and uses swap-and-pop to remove.
    // When an entry is consumed, the LAST array element is moved to the consumed slot.
    //
    // For groups of same-hash entries, this swap-and-pop can disrupt FIFO ordering
    // when OTHER entries (different hash) are consumed in between, because the swap
    // moves entries from the end of the array into earlier positions.
    //
    // Fix: partition the array so that same-hash groups (which need FIFO ordering among
    // themselves) are placed FIRST, and unique-hash entries are placed LAST. This way,
    // consuming a unique entry swaps in another unique entry from the end, leaving the
    // same-hash group undisturbed at the front.
    //
    // Within each same-hash group, apply the [E0, E(N-1), ..., E1] reorder so that
    // FIFO+swap-and-pop within the group produces the correct consumption order.

    // Build a map from action_hash → list of indices (in order of appearance).
    let mut groups: HashMap<B256, Vec<usize>> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        groups.entry(entry.action_hash).or_default().push(i);
    }

    // Check if any group has 3+ entries (needs reordering).
    let has_large_groups = groups.values().any(|v| v.len() >= 3);
    if !has_large_groups {
        return;
    }

    // Rebuild the array: same-hash groups with 2+ entries first (reordered),
    // then unique entries (in original order).
    let mut reordered: Vec<CrossChainExecutionEntry> = Vec::with_capacity(entries.len());

    // Collect multi-entry groups first.
    let mut multi_group_hashes: Vec<B256> = groups
        .iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(h, _)| *h)
        .collect();
    // Sort for determinism (HashMap iteration order is random).
    multi_group_hashes.sort();

    for hash in &multi_group_hashes {
        let indices = &groups[hash];
        let group_entries: Vec<CrossChainExecutionEntry> =
            indices.iter().map(|&i| entries[i].clone()).collect();

        if group_entries.len() >= 3 {
            // Apply [E0, E(N-1), E(N-2), ..., E1] reorder for FIFO+swap-and-pop.
            reordered.push(group_entries[0].clone());
            for e in group_entries[1..].iter().rev() {
                reordered.push(e.clone());
            }
        } else {
            // 2-entry group: no reorder needed (FIFO works with 2 entries).
            reordered.extend(group_entries);
        }
    }

    // Append unique entries (group size 1) in original order.
    let mut unique_indices: Vec<usize> = groups
        .iter()
        .filter(|(_, v)| v.len() == 1)
        .map(|(_, v)| v[0])
        .collect();
    unique_indices.sort();
    for &idx in &unique_indices {
        reordered.push(entries[idx].clone());
    }

    *entries = reordered;
}

/// Build a void RESULT action for the given rollup_id.
///
/// Used as terminal entries in the execution table — signals that a call
/// completed with no return data and no further continuation.
fn result_void(rollup_id: U256) -> CrossChainAction {
    // The `data` field must match what Solidity's `_processCallAtScope` builds.
    // `executeOnBehalf` uses assembly `return(add(result, 0x20), mload(result))`
    // which returns the RAW inner call result, NOT ABI-encoded `bytes memory`.
    // For void functions, the inner result is empty → returnData = vec![].
    // For functions that return data, returnData = raw ABI-encoded return value.
    CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id,
        destination: alloy_primitives::Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: alloy_primitives::Address::ZERO,
        source_rollup: U256::ZERO,
        scope: vec![],
    }
}

/// Build continuation entries for a sequence of detected cross-chain calls.
///
/// This handles the general pattern where an L1 transaction triggers multiple
/// cross-chain calls that form a continuation chain. The canonical example is
/// a multi-call continuation:
///
/// ```text
/// CALL_A (L1→L2): first cross-chain call
/// CALL_B (L1→L2): continuation of A
///   └─ CALL_C (L2→L1): child of B, executed at scope=[0]
/// ```
///
/// # L2 Entries (consumed during executeIncomingCrossChainCall on L2)
///
/// For each L1→L2 call that has a continuation after it:
///   - Entry: `hash(RESULT(target_rollup, void))` → next continuation CALL
///
/// For each L2→L1 child call within an L2 execution:
///   - Entry: `hash(CALL_unscoped)` → `RESULT(source_rollup, void)`
///
/// Terminal entry (after last continuation):
///   - Entry: `hash(RESULT(target_rollup, void))` → `RESULT(target_rollup, void)`
///
/// # L1 Entries (posted via postBatch, consumed during user tx execution on L1)
///
/// For each L1→L2 call without children:
///   - Entry: `hash(CALL)` → `RESULT(target_rollup, void)` (terminal)
///
/// For each L1→L2 call with L2→L1 child calls:
///   - Entry: `hash(CALL)` → child CALL with `scope=[child_index]`
///
/// For each L2→L1 child resolution:
///   - Entry: `hash(RESULT(child_source_rollup, void))` → `RESULT(target_rollup, void)`
///
/// # Arguments
///
/// * `calls` - Detected cross-chain calls in order. Must have at least one call.
/// * `our_rollup_id` - The L2 rollup ID (e.g., 1).
///
/// # Returns
///
/// `ContinuationEntries` with L2 and L1 entries (state deltas are empty).
pub fn build_continuation_entries(
    calls: &[DetectedCall],
    our_rollup_id: U256,
) -> ContinuationEntries {
    if calls.is_empty() {
        return ContinuationEntries {
            l2_entries: vec![],
            l1_entries: vec![],
        };
    }

    let mainnet_rollup_id = U256::ZERO;
    let l2_result_void = result_void(our_rollup_id);
    let l1_result_void = result_void(mainnet_rollup_id);
    let empty_deltas: Vec<CrossChainStateDelta> = vec![];

    let mut l2_entries = Vec::new();
    let mut l1_entries = Vec::new();

    // Collect L1→L2 calls for L2 entry generation
    let l1_to_l2_calls: Vec<(usize, &DetectedCall)> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.direction == CallDirection::L1ToL2)
        .collect();

    // ── L2 entries ──
    // Detect reentrant pattern: any L1→L2 call (except the last) that has
    // a L2→L1 child. In reentrant patterns, the child's scope navigation
    // triggers the NEXT L1→L2 call, so entries must be ordered:
    //   Phase 1: ALL child entries (consumed during scope navigation, FIFO)
    //   Phase 2: ALL result propagation entries (consumed as scopes unwind, bottom-up)
    // In continuation patterns (flash-loan), children are leaves — no reentrant.
    // Detect reentrant vs continuation using L1 trace depth ordering.
    //
    // Reentrant (deepCall): each L1→L2 call is nested DEEPER inside scope
    // navigation → STRICTLY INCREASING trace depths (1, 7, 13).
    //
    // Continuation (multi-call-nested, flash-loan): L1→L2 calls are siblings
    // in the user's call tree → same or non-increasing depths (3,3,3 or 4,3).
    // Flash-loan has different depths (4,3) because calls go through different
    // intermediary contracts, but the second call is NOT deeper (3 < 4).
    //
    // Rule: is_reentrant = each successive L1→L2 call is STRICTLY DEEPER.
    let depths: Vec<usize> = l1_to_l2_calls.iter().map(|&(_, c)| c.depth).collect();
    let is_strictly_increasing = depths.windows(2).all(|w| w[1] > w[0]);
    let is_reentrant = l1_to_l2_calls.len() > 1 && is_strictly_increasing;

    if is_reentrant {
        // ── REENTRANT MODEL ──
        // Phase 1: Child entries (ALL children first, in order).
        // Each child's nextAction is CALL(L2, nextLevel, scope=[0]) for scope
        // navigation, or RESULT(MAINNET, delivery_data) for the innermost leaf.
        for (pos, &(call_idx, _)) in l1_to_l2_calls.iter().enumerate() {
            let is_last = pos == l1_to_l2_calls.len() - 1;
            let children: Vec<(usize, &DetectedCall)> = calls
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.parent_call_index == Some(call_idx) && c.direction == CallDirection::L2ToL1
                })
                .collect();

            for (_ci, child) in &children {
                let child_action_hash = compute_action_hash(&child.call_action);
                let child_next = if !is_last {
                    // Scope navigation to next deeper level
                    let next = l1_to_l2_calls[pos + 1].1;
                    CrossChainAction {
                        action_type: CrossChainActionType::Call,
                        rollup_id: our_rollup_id,
                        destination: next.call_action.destination,
                        value: next.call_action.value,
                        data: next.call_action.data.clone(),
                        failed: false,
                        source_address: next.call_action.source_address,
                        source_rollup: mainnet_rollup_id,
                        scope: vec![U256::ZERO],
                    }
                } else if child.delivery_return_data.is_empty() && !child.l2_delivery_failed {
                    l1_result_void.clone()
                } else {
                    CrossChainAction {
                        action_type: CrossChainActionType::Result,
                        rollup_id: mainnet_rollup_id,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: child.delivery_return_data.clone(),
                        failed: child.l2_delivery_failed,
                        source_address: Address::ZERO,
                        source_rollup: U256::ZERO,
                        scope: vec![],
                    }
                };
                l2_entries.push(CrossChainExecutionEntry {
                    state_deltas: empty_deltas.clone(),
                    action_hash: child_action_hash,
                    next_action: child_next,
                });
            }
        }

        // Phase 2: Result propagation entries (bottom-up, innermost first).
        // As scopes unwind after each level returns, _processCallAtScope builds
        // RESULT(L2, l2_return) and looks up the next entry.
        //
        // For each level (innermost to outermost):
        //   trigger = hash(RESULT(L2, l2_return[level]))
        //   nextAction:
        //     - intermediate: RESULT(MAINNET, delivery_return[child_of_next_outer_level])
        //     - terminal (outermost): RESULT(L2, l2_return) self-ref
        for (pos, &(_call_idx, current_call)) in l1_to_l2_calls.iter().enumerate().rev() {
            let is_last = pos == l1_to_l2_calls.len() - 1;
            let is_first = pos == 0;

            // Build the RESULT(L2) trigger from this call's l2_return_data.
            let l2_result =
                if !current_call.l2_return_data.is_empty() || current_call.l2_delivery_failed {
                    CrossChainAction {
                        action_type: CrossChainActionType::Result,
                        rollup_id: our_rollup_id,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: current_call.l2_return_data.clone(),
                        failed: current_call.l2_delivery_failed,
                        source_address: Address::ZERO,
                        source_rollup: U256::ZERO,
                        scope: vec![],
                    }
                } else {
                    l2_result_void.clone()
                };
            let result_hash = compute_action_hash(&l2_result);

            let next_action = if is_first {
                // Terminal (outermost call): RESULT(L2, l2_return) self-ref
                l2_result
            } else {
                // Intermediate: RESULT(MAINNET, delivery_return of the child
                // belonging to the NEXT OUTER level).
                // The next outer level is at pos-1. Its child has the delivery
                // return data from L1 execution.
                let outer_call_idx = l1_to_l2_calls[pos - 1].0;
                let outer_child = calls.iter().find(|c| {
                    c.parent_call_index == Some(outer_call_idx)
                        && c.direction == CallDirection::L2ToL1
                });

                if let Some(child) = outer_child {
                    if child.delivery_return_data.is_empty() && !child.l2_delivery_failed {
                        // Void delivery return
                        CrossChainAction {
                            action_type: CrossChainActionType::Result,
                            rollup_id: mainnet_rollup_id,
                            destination: Address::ZERO,
                            value: U256::ZERO,
                            data: vec![],
                            failed: false,
                            source_address: Address::ZERO,
                            source_rollup: U256::ZERO,
                            scope: vec![],
                        }
                    } else {
                        CrossChainAction {
                            action_type: CrossChainActionType::Result,
                            rollup_id: mainnet_rollup_id,
                            destination: Address::ZERO,
                            value: U256::ZERO,
                            data: child.delivery_return_data.clone(),
                            failed: child.l2_delivery_failed,
                            source_address: Address::ZERO,
                            source_rollup: U256::ZERO,
                            scope: vec![],
                        }
                    }
                } else {
                    // Fallback: void result
                    l2_result_void.clone()
                }
            };

            tracing::info!(
                target: "based_rollup::table_builder",
                "L2 result propagation: pos={} is_last={} is_first={} trigger_hash={} next_type={:?} next_rollup={} next_data_len={}",
                pos, is_last, is_first, result_hash,
                next_action.action_type, next_action.rollup_id, next_action.data.len()
            );

            l2_entries.push(CrossChainExecutionEntry {
                state_deltas: empty_deltas.clone(),
                action_hash: result_hash,
                next_action,
            });
        }
    } else {
        // ── CONTINUATION MODEL (original) ──
        // Interleaved child + result entries per call. Used for flash-loan,
        // multi-call-nested, and other non-reentrant patterns.
        for (pos, &(call_idx, _call)) in l1_to_l2_calls.iter().enumerate() {
            let is_last_l1_to_l2 = pos == l1_to_l2_calls.len() - 1;

            let children: Vec<(usize, &DetectedCall)> = calls
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.parent_call_index == Some(call_idx) && c.direction == CallDirection::L2ToL1
                })
                .collect();

            for (_child_idx, child) in &children {
                let child_action_hash = compute_action_hash(&child.call_action);
                let child_next =
                    if child.delivery_return_data.is_empty() && !child.l2_delivery_failed {
                        l1_result_void.clone()
                    } else {
                        CrossChainAction {
                            action_type: CrossChainActionType::Result,
                            rollup_id: mainnet_rollup_id,
                            destination: Address::ZERO,
                            value: U256::ZERO,
                            data: child.delivery_return_data.clone(),
                            failed: child.l2_delivery_failed,
                            source_address: Address::ZERO,
                            source_rollup: U256::ZERO,
                            scope: vec![],
                        }
                    };
                l2_entries.push(CrossChainExecutionEntry {
                    state_deltas: empty_deltas.clone(),
                    action_hash: child_action_hash,
                    next_action: child_next,
                });
            }

            let current_call = l1_to_l2_calls[pos].1;
            let l2_result_for_call =
                if !current_call.l2_return_data.is_empty() || current_call.l2_delivery_failed {
                    CrossChainAction {
                        action_type: CrossChainActionType::Result,
                        rollup_id: our_rollup_id,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: current_call.l2_return_data.clone(),
                        failed: current_call.l2_delivery_failed,
                        source_address: Address::ZERO,
                        source_rollup: U256::ZERO,
                        scope: vec![],
                    }
                } else {
                    l2_result_void.clone()
                };

            if is_last_l1_to_l2 {
                let result_hash = compute_action_hash(&l2_result_for_call);
                l2_entries.push(CrossChainExecutionEntry {
                    state_deltas: empty_deltas.clone(),
                    action_hash: result_hash,
                    next_action: l2_result_for_call,
                });
            } else {
                let next_call = &l1_to_l2_calls[pos + 1].1.call_action;
                let result_hash = compute_action_hash(&l2_result_for_call);
                l2_entries.push(CrossChainExecutionEntry {
                    state_deltas: empty_deltas.clone(),
                    action_hash: result_hash,
                    next_action: next_call.clone(),
                });
            }
        }
    }

    // ── Reorder L2 entries ──
    // The Solidity test shows L2 entries in this order:
    // 1. Continuation entries (RESULT hash → next CALL) for calls that have continuations
    // 2. Child call entries (CALL hash → RESULT)
    // 3. Terminal entry (RESULT hash → RESULT)
    //
    // Our loop generates: for each call [children..., continuation/terminal].
    // For the multi-call continuation case with calls [A, B(cont), C(child of B)]:
    // - Call A (pos=0, no children): continuation entry (RESULT→CALL_B)
    // - Call B (pos=1, child C): child entry (CALL_C→RESULT), terminal (RESULT→RESULT)
    //
    // This matches the Solidity test order:
    // Entry 1: RESULT(L2,void) hash → CALL_B
    // Entry 2: CALL(bridgeReturn) hash → RESULT(MAINNET,void)
    // Entry 3: RESULT(L2,void) hash → RESULT(L2,void)

    // ── L1 entries ──
    // One entry per L1→L2 call, plus resolution entries for L2→L1 children.
    for &(call_idx, detected) in &l1_to_l2_calls {
        let call_action_hash = compute_action_hash(&detected.call_action);

        // Build state delta placeholder with correct ether_delta from the call's value.
        // The driver will replace currentState/newState with actual intermediate roots,
        // but PRESERVES the ether_delta. This ensures the simulation and real postBatch
        // both have correct ether accounting (required by Rollups.sol EtherDeltaMismatch check).
        let call_value = detected.call_action.value;
        let ether_delta = if call_value.is_zero() {
            alloy_primitives::I256::ZERO
        } else {
            alloy_primitives::I256::try_from(call_value).unwrap_or(alloy_primitives::I256::ZERO)
        };
        tracing::info!(
            target: "based_rollup::table_builder",
            call_idx,
            dest = %detected.call_action.destination,
            call_value = %call_value,
            ether_delta = %ether_delta,
            "L1 entry ether_delta computation"
        );
        let l1_entry_deltas = vec![CrossChainStateDelta {
            rollup_id: our_rollup_id,
            current_state: alloy_primitives::B256::ZERO, // placeholder — driver fills
            new_state: alloy_primitives::B256::ZERO,     // placeholder — driver fills
            ether_delta,
        }];

        // Find L2→L1 children of this call
        let children: Vec<(usize, &DetectedCall)> = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.parent_call_index == Some(call_idx) && c.direction == CallDirection::L2ToL1
            })
            .collect();

        if children.is_empty() {
            // Simple: CALL → RESULT(L2) terminal.
            // The next_action RESULT data is what Rollups.sol returns to the L1 caller.
            // Use l2_return_data when the L2 call returns data (future non-void L1→L2
            // calls). Current continuation uses are void so result_void
            // is correct.
            let l1_terminal = if !detected.l2_return_data.is_empty() || detected.l2_delivery_failed
            {
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: our_rollup_id,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: detected.l2_return_data.clone(),
                    failed: detected.l2_delivery_failed,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            } else {
                l2_result_void.clone()
            };
            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: l1_entry_deltas.clone(),
                action_hash: call_action_hash,
                next_action: l1_terminal,
            });
        } else {
            // Has children: CALL → first child CALL with accumulated scope.
            // The child's scope is its pre-computed accumulated scope from detection.
            let first_child = &children[0].1;
            let mut scoped_child_action = first_child.call_action.clone();
            scoped_child_action.scope = if first_child.scope.is_empty() {
                vec![U256::ZERO] // fallback: depth 1 if no scope computed
            } else {
                first_child.scope.clone()
            };

            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: l1_entry_deltas.clone(),
                action_hash: call_action_hash,
                next_action: scoped_child_action,
            });

            // Resolution entries for each child.
            // After child executes on L1, _processCallAtScope builds
            // RESULT{rollupId=child_target_rollup, data=returnData, failed=!success}.
            // The action_hash must match that RESULT hash. Use delivery_return_data
            // when the child's L1 target returns data (e.g., non-void L1 contracts).
            for (child_pos, child) in &children {
                let _ = child_pos;
                let child_target_rollup = child.call_action.rollup_id;
                let child_result =
                    if !child.delivery_return_data.is_empty() || child.l2_delivery_failed {
                        CrossChainAction {
                            action_type: CrossChainActionType::Result,
                            rollup_id: child_target_rollup,
                            destination: Address::ZERO,
                            value: U256::ZERO,
                            data: child.delivery_return_data.clone(),
                            failed: child.l2_delivery_failed,
                            source_address: Address::ZERO,
                            source_rollup: U256::ZERO,
                            scope: vec![],
                        }
                    } else {
                        result_void(child_target_rollup)
                    };
                let child_result_hash = compute_action_hash(&child_result);

                tracing::info!(
                    target: "based_rollup::table_builder",
                    "L1 scope resolution FULL: child_pos={} hash={} type={:?} rollupId={} dest={} value={} data_hex=0x{} data_len={} failed={} sourceAddr={} sourceRollup={} scope_len={}",
                    child_pos,
                    child_result_hash,
                    child_result.action_type,
                    child_result.rollup_id,
                    child_result.destination,
                    child_result.value,
                    hex::encode(&child_result.data),
                    child_result.data.len(),
                    child_result.failed,
                    child_result.source_address,
                    child_result.source_rollup,
                    child_result.scope.len()
                );

                // Terminal next_action: the RESULT of the OUTER scope after
                // the child's scope resolves. This is NOT the child's delivery
                // return — it's the parent L1→L2 call's L2 delivery result.
                //
                // After newScope resolves the child CALL (scope=[0]),
                // _findAndApplyExecution matches this entry and returns
                // resolution_terminal as nextAction. This propagates back
                // through newScope → executeCrossChainCall as the final
                // RESULT of the entire scope chain.
                //
                // rollupId: our_rollup_id (L2), because the outer CALL targets L2.
                // data: detected.l2_return_data (what the L2 delivery returned),
                //       typically void for incrementProxy-style functions.
                let resolution_terminal =
                    if !detected.l2_return_data.is_empty() || detected.l2_delivery_failed {
                        CrossChainAction {
                            action_type: CrossChainActionType::Result,
                            rollup_id: our_rollup_id,
                            destination: Address::ZERO,
                            value: U256::ZERO,
                            data: detected.l2_return_data.clone(),
                            failed: detected.l2_delivery_failed,
                            source_address: Address::ZERO,
                            source_rollup: U256::ZERO,
                            scope: vec![],
                        }
                    } else {
                        l2_result_void.clone()
                    };
                l1_entries.push(CrossChainExecutionEntry {
                    state_deltas: empty_deltas.clone(),
                    action_hash: child_result_hash,
                    next_action: resolution_terminal,
                });
            }
        }
    }

    // Reorder same-hash groups for Solidity swap-and-pop FIFO consumption.
    reorder_for_swap_and_pop(&mut l2_entries);

    ContinuationEntries {
        l2_entries,
        l1_entries,
    }
}

/// Parameters for a cross-chain call detected by the L1 proxy.
/// This is the "raw" form before continuation analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L1DetectedCall {
    /// Target address on L2 (proxy's originalAddress).
    pub destination: Address,
    /// Calldata sent to the destination (e.g., receiveTokens calldata).
    pub data: Vec<u8>,
    /// ETH value for the cross-chain call.
    pub value: U256,
    /// Address that initiated the call on L1 (e.g., bridge_l1 or executor).
    pub source_address: Address,
    /// Return data from simulating this L1->L2 call on L2.
    /// When non-empty, the L2 RESULT action hash includes this data
    /// (contracts/sync-rollups-protocol/docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md §C.2).
    #[serde(default)]
    pub l2_return_data: Vec<u8>,
    /// Whether the L2 call succeeded. Defaults to `true`.
    #[serde(default = "default_call_success")]
    pub call_success: bool,
    /// Index of the parent L1→L2 call whose L2 execution triggers this child.
    /// `None` for root-level L1→L2 calls; `Some(i)` for L2→L1 child calls
    /// (the L1→L2→L1 pattern) discovered inside call[i]'s L2 simulation.
    #[serde(default)]
    pub parent_call_index: Option<usize>,
    /// Target rollup ID. `None` means L1→L2 (targets our L2 rollup).
    /// `Some(0)` means L2→L1 (targets L1/mainnet). Used to distinguish
    /// L1→L2 continuation calls from L2→L1 child calls.
    #[serde(default)]
    pub target_rollup_id: Option<u64>,
    /// Accumulated scope for this call's delivery action.
    #[serde(default)]
    pub scope: Vec<U256>,
    /// Iterative discovery iteration when this call was first detected.
    /// 0 = initial trace, 1+ = discovered during iterative expansion.
    /// Used to distinguish reentrant (calls across multiple iterations)
    /// from continuation (all calls in same iteration) patterns.
    #[serde(default)]
    pub discovery_iteration: usize,
    /// Original L1 trace depth from walk_trace_tree. L1→L2 sibling calls have
    /// the same depth; reentrant calls (inside scope navigation) have increasing
    /// depths. Used to distinguish reentrant from continuation patterns.
    #[serde(default)]
    pub l1_trace_depth: usize,
}

/// Serde default for `call_success` — defaults to `true` (success).
fn default_call_success() -> bool {
    true
}

/// Map detected calls into `DetectedCall` entries for continuation entry building.
///
/// Handles both L1→L2 calls and L2→L1 child calls (the L1→L2→L1 nested pattern).
/// L2→L1 children are identified by `target_rollup_id == Some(0)` and `parent_call_index.is_some()`.
///
/// For pure L1→L2 calls: 1 call produces a simple CALL+RESULT pair,
/// 2+ calls produce a continuation chain.
///
/// For L1→L2 calls with L2→L1 children: the parent gets a CALL→CALL(scope=[0]) entry
/// instead of CALL→RESULT, and the child gets its own resolution entry.
///
/// # Arguments
/// * `calls` - Detected calls from the L1 proxy (in execution order). May include
///   both L1→L2 calls and L2→L1 children.
/// * `our_rollup_id` - The L2 rollup ID
pub fn analyze_continuation_calls(
    calls: &[L1DetectedCall],
    our_rollup_id: u64,
) -> Vec<DetectedCall> {
    let our_rollup = U256::from(our_rollup_id);
    let mainnet_rollup = U256::ZERO;

    // Count L1→L2 calls (for is_continuation tracking).
    let mut l1_to_l2_count = 0usize;

    calls
        .iter()
        .enumerate()
        .map(|(i, call)| {
            let is_l2_to_l1_child =
                call.target_rollup_id == Some(0) && call.parent_call_index.is_some();

            if is_l2_to_l1_child {
                // L2→L1 child: targets L1 (rollup 0), source is on L2
                let call_action = CrossChainAction {
                    action_type: CrossChainActionType::Call,
                    rollup_id: mainnet_rollup, // target = L1
                    destination: call.destination,
                    value: call.value,
                    data: call.data.clone(),
                    failed: false,
                    source_address: call.source_address,
                    source_rollup: our_rollup, // source = our L2 rollup
                    scope: vec![],
                };

                DetectedCall {
                    direction: CallDirection::L2ToL1,
                    call_action,
                    parent_call_index: call.parent_call_index,
                    is_continuation: false,
                    depth: 1,
                    delivery_return_data: call.l2_return_data.clone(),
                    l2_return_data: vec![],
                    l2_delivery_failed: !call.call_success,
                    scope: call.scope.clone(),
                    discovery_iteration: call.discovery_iteration,
                    in_reverted_frame: false, // L1→L2 children: not applicable
                }
            } else {
                // L1→L2 call (root-level or continuation)
                let call_action = CrossChainAction {
                    action_type: CrossChainActionType::Call,
                    rollup_id: our_rollup,
                    destination: call.destination,
                    value: call.value,
                    data: call.data.clone(),
                    failed: false,
                    source_address: call.source_address,
                    source_rollup: mainnet_rollup,
                    scope: vec![],
                };

                let is_continuation = l1_to_l2_count > 0;
                l1_to_l2_count += 1;
                let _ = i; // suppress unused warning

                DetectedCall {
                    direction: CallDirection::L1ToL2,
                    call_action,
                    parent_call_index: None,
                    is_continuation,
                    depth: call.l1_trace_depth, // L1 trace depth for reentrant detection
                    delivery_return_data: vec![],
                    l2_return_data: call.l2_return_data.clone(),
                    l2_delivery_failed: !call.call_success,
                    scope: vec![], // L1→L2 root calls start with empty scope
                    discovery_iteration: call.discovery_iteration,
                    in_reverted_frame: false, // L1→L2 calls: not applicable
                }
            }
        })
        .collect()
}

/// Parameters for an L2→L1 cross-chain call detected by the L2 proxy.
/// This is the "raw" form before continuation analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2DetectedCall {
    /// Target address on L1 (proxy's originalAddress).
    pub destination: Address,
    /// Calldata sent to the destination.
    pub data: Vec<u8>,
    /// ETH value for the cross-chain call.
    pub value: U256,
    /// Address that initiated the call on L2 (e.g., the executor contract).
    pub source_address: Address,
    /// Return data from the L1 delivery simulation for this call.
    /// When non-empty, the L1 RESULT entry hash includes this data.
    #[serde(default)]
    pub delivery_return_data: Vec<u8>,
    /// Whether the L1 delivery simulation reverted.
    /// When true, the RESULT entry's `failed` field is set to true,
    /// matching what `_processCallAtScope` computes on-chain.
    #[serde(default)]
    pub delivery_failed: bool,
    /// Accumulated scope for this call's L1 delivery CALL action.
    /// Includes accumulated prefix from parent hops + local trace depth.
    #[serde(default)]
    pub scope: Vec<U256>,
    /// Whether this call is inside a reverted frame on L2 (try/catch that reverts).
    /// Used for partial revert patterns where some calls need REVERT/REVERT_CONTINUE.
    #[serde(default)]
    pub in_reverted_frame: bool,
}

/// Parameters for an L1→L2 return call discovered during L1 delivery simulation.
/// These are return trips (e.g., Bridge_L1.bridgeTokens back to L2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2ReturnCall {
    /// Target address on L2 (proxy's originalAddress on L1 → targets L2).
    pub destination: Address,
    /// Calldata (e.g., receiveTokens).
    pub data: Vec<u8>,
    /// ETH value sent with the call.
    pub value: U256,
    /// Address that initiated the call on L1.
    pub source_address: Address,
    /// Index of the L2→L1 call whose L1 execution produces this return call.
    /// `None` means assign to the last L2→L1 call (backward-compatible default).
    /// `Some(i)` explicitly links this return call to `l2_calls[i]`.
    #[serde(default)]
    pub parent_call_index: Option<usize>,
    /// Return data from simulating this call on L2 (eth_call to destination with data).
    /// Used for the L2 RESULT entry hash. Without this, result_void is used, which
    /// mismatches when the target returns data (issue #245).
    #[serde(default)]
    pub l2_return_data: Vec<u8>,
    /// Whether the L2 simulation of this call reverted.
    #[serde(default)]
    pub l2_delivery_failed: bool,
    /// Accumulated scope for this return call. Inherited from parent + local trace depth.
    #[serde(default)]
    pub scope: Vec<U256>,
}

/// Output of `build_l2_to_l1_continuation_entries`: L2 table entries and L1 deferred entries.
#[derive(Debug, Clone)]
pub struct L2ToL1ContinuationEntries {
    /// Entries loaded into L2 execution table via `loadExecutionTable`.
    /// Each L2→L1 call gets a CALL+RESULT pair consumed by `executeCrossChainCall`.
    pub l2_entries: Vec<CrossChainExecutionEntry>,
    /// Entries posted to L1 via `postBatch` (3-entry continuation structure).
    pub l1_entries: Vec<CrossChainExecutionEntry>,
}

/// Analyze L2→L1 calls and L1→L2 return calls to discover the continuation pattern
/// for L2→L1 multi-call continuations (the mirror of `analyze_continuation_calls`).
///
/// Each L2→L1 call becomes a root `DetectedCall`. Return calls (from L1 delivery
/// simulation) become children linked via `parent_call_index`. If no return calls
/// are provided and there are 2+ L2 calls, a warning is logged -- the entries will
/// be built without children (the simulation is the only source of child discovery).
///
/// # Arguments
/// * `l2_calls` - L2→L1 calls detected from the L2 tx trace (in execution order)
/// * `return_calls` - L1→L2 return calls discovered from L1 delivery simulation
/// * `our_rollup_id` - The L2 rollup ID (e.g., 1)
///
/// # Returns
/// Vec of `DetectedCall` suitable for `build_l2_to_l1_continuation_entries`.
pub fn analyze_l2_to_l1_continuation_calls(
    l2_calls: &[L2DetectedCall],
    return_calls: &[L2ReturnCall],
    our_rollup_id: u64,
) -> Vec<DetectedCall> {
    let our_rollup = U256::from(our_rollup_id);
    let mainnet_rollup = U256::ZERO;

    if l2_calls.is_empty() {
        return vec![];
    }

    let mut result: Vec<DetectedCall> = Vec::new();

    // Each L2→L1 call produces a DetectedCall with direction=L2ToL1.
    for (i, call) in l2_calls.iter().enumerate() {
        let is_continuation = i > 0;

        // Build the L2→L1 CALL action:
        // rollupId = 0 (L1, target), destination = L1 target,
        // source_address = L2 initiator, source_rollup = our_rollup_id
        let call_action = CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: mainnet_rollup, // target = L1
            destination: call.destination,
            value: call.value,
            data: call.data.clone(),
            failed: false,
            source_address: call.source_address,
            source_rollup: our_rollup,
            scope: vec![],
        };

        result.push(DetectedCall {
            direction: CallDirection::L2ToL1,
            call_action,
            parent_call_index: None,
            is_continuation,
            depth: 0,
            delivery_return_data: call.delivery_return_data.clone(),
            l2_return_data: vec![],
            l2_delivery_failed: call.delivery_failed,
            scope: call.scope.clone(),
            discovery_iteration: 0,
            in_reverted_frame: call.in_reverted_frame,
        });
    }

    // Return calls are L1→L2 direction (child calls spawned during execution on L1).
    // Each return call is linked to the L2→L1 call whose L1 execution produced it.
    // When `parent_call_index` is set, use it; otherwise default to last L2→L1 call.
    let last_l2_to_l1_idx = l2_calls.len() - 1;

    // First try explicitly provided return calls (from L1 delivery simulation).
    // Return calls are L1→L2 direction: they execute on L2 (rollup_id = our_rollup),
    // originating from L1 (source_rollup = mainnet/0). This mirrors how
    // analyze_continuation_calls builds L2→L1 children for L1→L2 patterns.
    if !return_calls.is_empty() {
        for rc in return_calls {
            // Use explicit parent_call_index if provided, otherwise default to last call.
            let parent_idx = rc.parent_call_index.unwrap_or(last_l2_to_l1_idx);

            let child_action = CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: our_rollup, // L2 — return call executes on L2
                destination: rc.destination,
                value: rc.value,
                data: rc.data.clone(),
                failed: false,
                source_address: rc.source_address,
                source_rollup: mainnet_rollup, // L1 — return call originates from L1
                scope: vec![],
            };

            tracing::debug!(
                target: "based_rollup::table_builder",
                parent_idx,
                destination = %rc.destination,
                source = %rc.source_address,
                explicit_parent = rc.parent_call_index.is_some(),
                "return call linked to L2→L1 call"
            );

            result.push(DetectedCall {
                direction: CallDirection::L2ToL1,
                call_action: child_action,
                parent_call_index: Some(parent_idx),
                is_continuation: false,
                depth: 1,
                delivery_return_data: vec![], // child executes on L2, no L1 delivery data
                l2_return_data: rc.l2_return_data.clone(),
                l2_delivery_failed: rc.l2_delivery_failed,
                scope: rc.scope.clone(),
                discovery_iteration: 0,
                in_reverted_frame: false, // return calls: not in reverted frame
            });
        }
    } else if l2_calls.len() >= 2 {
        // Multi-call L2→L1 with no return calls from simulation. The entries will be
        // built without children. If the pattern requires return calls (e.g., token
        // bridging with scope navigation), the tx will fail on-chain -- the simulation
        // is the only source of child discovery.
        tracing::warn!(
            target: "based_rollup::table_builder",
            num_l2_calls = l2_calls.len(),
            "multi-call L2→L1 pattern with no return calls from simulation; \
             entries will be built without children"
        );
    }

    result
}

/// Find direct children of a call by its index in the `detected` array.
///
/// Returns `(original_index, &DetectedCall)` pairs for all calls whose
/// `parent_call_index == Some(parent_idx)`.
fn find_children(detected: &[DetectedCall], parent_idx: usize) -> Vec<(usize, &DetectedCall)> {
    detected
        .iter()
        .enumerate()
        .filter(|(_, c)| c.parent_call_index == Some(parent_idx))
        .collect()
}

/// Push reentrant child entries for L1 deferred entries, recursively handling
/// children that themselves have children (depth > 1).
///
/// Each child call is a reentrant `executeCrossChainCall` triggered when the L1 execution
/// internally calls a proxy (e.g., Bridge_L1.bridgeTokens -> proxy(Bridge_L2, L2)).
///
/// The trigger hash is computed from the child's own routing:
///   - `rollupId = our_rollup_id` (proxy's originalRollupId)
///   - `destination = child.source_address` (proxy's originalAddress, e.g., Bridge_L2)
///   - `sourceAddress = child.destination` (L1 caller to the proxy, e.g., Bridge_L1)
///   - `sourceRollup = 0` (L1)
///   - `data = child's calldata`
///
/// For leaf children (no grandchildren): simple trigger → RESULT(L1, void).
/// For internal children (with grandchildren): trigger → execution with scope=[0],
/// plus recursive reentrant entries for grandchildren, plus scope resolution.
#[allow(clippy::only_used_in_recursion)]
fn push_reentrant_child_entries(
    children: &[(usize, &DetectedCall)],
    detected: &[DetectedCall],
    our_rollup_id: U256,
    parent_idx: usize,
    empty_deltas: &[CrossChainStateDelta],
    l1_result_void: &CrossChainAction,
    l1_entries: &mut Vec<CrossChainExecutionEntry>,
) {
    for (child_pos, &(child_orig_idx, child)) in children.iter().enumerate() {
        // Derive addresses from the child's own routing info.
        //
        // For L2→L1 children (direction same as root calls):
        //   destination = L1 target, source_address = L2 initiator
        //   proxy = proxy(source_address, our_rollup_id) on L1
        //   trigger: destination = source_address (proxy's originalAddress)
        //            source_address = destination (L1 caller to proxy)
        //
        // For L1→L2 return call children (reverse direction):
        //   destination = L2 target, source_address = L1 initiator
        //   proxy = proxy(destination, our_rollup_id) on L1
        //   trigger: destination = destination (proxy's originalAddress)
        //            source_address = source_address (L1 caller to proxy)
        let is_return_call = child.call_action.rollup_id == our_rollup_id;
        let (trigger_dest, trigger_source) = if is_return_call {
            // L1→L2 return call: proxy represents the L2 destination on L1.
            // The L1 source_address is the L1 contract calling the proxy.
            (
                child.call_action.destination,
                child.call_action.source_address,
            )
        } else {
            // L2→L1 child: proxy represents the L2 source on L1.
            // The L1 destination is the L1 contract that called the proxy.
            (
                child.call_action.source_address,
                child.call_action.destination,
            )
        };

        let child_trigger = CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: our_rollup_id, // proxy's originalRollupId
            destination: trigger_dest,
            value: child.call_action.value,
            data: child.call_action.data.clone(),
            failed: false,
            source_address: trigger_source,
            source_rollup: U256::ZERO, // L1
            scope: vec![],
        };
        let child_trigger_hash = compute_action_hash(&child_trigger);

        // Check if this child has its own children (grandchildren of the parent).
        let grandchildren = find_children(detected, child_orig_idx);

        if grandchildren.is_empty() {
            // Leaf child: trigger → RESULT with return data.
            // Rollups.sol returns nextAction.data to the L1 caller (e.g., Logger).
            // Issue #246: For L1→L2 return calls (is_return_call=true), the child
            // executes on L2 so delivery_return_data is empty — use l2_return_data
            // (Counter's L2 return). For L2→L1 forward calls, use delivery_return_data
            // (the L1 target's return from simulation).
            let child_return_data = if is_return_call {
                &child.l2_return_data
            } else {
                &child.delivery_return_data
            };
            let child_failed = child.l2_delivery_failed;
            // rollupId: _processCallAtScope builds RESULT(rollupId=action.rollupId).
            // For L1→L2 return calls, target is our_rollup_id (L2).
            // For L2→L1 forward calls, target is U256::ZERO (L1).
            let leaf_result_rollup = if is_return_call {
                our_rollup_id
            } else {
                U256::ZERO
            };
            let leaf_next = if child_return_data.is_empty() && !child_failed {
                result_void(leaf_result_rollup)
            } else {
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: leaf_result_rollup,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: child_return_data.clone(),
                    failed: child_failed,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            };
            tracing::info!(
                target: "based_rollup::table_builder",
                "L1 Entry {}b[{}] (reentrant child, leaf): hash={} dest={} source={} data_len={} is_return={} return_data_len={}",
                parent_idx, child_pos, child_trigger_hash, child_trigger.destination,
                child_trigger.source_address, child_trigger.data.len(), is_return_call, child_return_data.len()
            );

            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: empty_deltas.to_vec(),
                action_hash: child_trigger_hash,
                next_action: leaf_next,
            });
        } else {
            // Internal child (has grandchildren): trigger → execution with accumulated scope,
            // plus recursive reentrant entries, plus scope resolution.
            // This mirrors the "subsequent call WITH children" pattern for root calls.
            let child_scope = if child.scope.is_empty() {
                vec![U256::ZERO]
            } else {
                child.scope.clone()
            };
            let execution = CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: U256::ZERO, // L1, where the call executes
                destination: child.call_action.destination, // L1 target
                value: child.call_action.value,
                data: child.call_action.data.clone(),
                failed: false,
                source_address: child.call_action.source_address, // L2 initiator
                source_rollup: our_rollup_id,
                scope: child_scope,
            };

            tracing::info!(
                target: "based_rollup::table_builder",
                "L1 Entry {}b[{}] (reentrant child, internal→execution): hash={} exec_dest={} \
                 grandchildren={} depth={}",
                parent_idx, child_pos, child_trigger_hash, execution.destination,
                grandchildren.len(), child.depth
            );

            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: empty_deltas.to_vec(),
                action_hash: child_trigger_hash,
                next_action: execution,
            });

            // Recursive reentrant entries for grandchildren.
            push_reentrant_child_entries(
                &grandchildren,
                detected,
                our_rollup_id,
                child_orig_idx,
                empty_deltas,
                l1_result_void,
                l1_entries,
            );

            // Scope resolution for this child's execution.
            // _processCallAtScope builds RESULT{rollupId: action.rollupId, data: returnData}
            // after calling executeOnBehalf. The rollupId must match the CALL target's rollupId.
            // Issue #246: For L1→L2 return calls, use l2_return_data (child executes
            // on L2, delivery_return_data is empty). For L2→L1 calls, use delivery_return_data.
            let child_scope_data = if is_return_call {
                &child.l2_return_data
            } else {
                &child.delivery_return_data
            };
            let child_scope_failed = child.l2_delivery_failed;
            // rollupId: _processCallAtScope uses action.rollupId (the CALL's target rollup).
            // For L1→L2 return calls (is_return_call=true), target is our_rollup_id (L2).
            // For L2→L1 children, target is U256::ZERO (L1/MAINNET).
            let child_result_rollup = if is_return_call {
                our_rollup_id
            } else {
                U256::ZERO
            };
            let child_scope_result = if child_scope_data.is_empty() && !child_scope_failed {
                result_void(child_result_rollup)
            } else {
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: child_result_rollup,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: child_scope_data.clone(),
                    failed: child_scope_failed,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            };
            let scope_result_hash = compute_action_hash(&child_scope_result);
            // Scope exit terminal: the RESULT of the OUTER scope after the child
            // resolves. This is the L2TX terminal for the L2→L1 root call.
            // Per §C.6, L2TX terminal RESULT is always void with
            // rollupId = triggering rollupId (our_rollup_id for L2→L1).
            let l1_scope_exit = result_void(our_rollup_id);
            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: empty_deltas.to_vec(),
                action_hash: scope_result_hash,
                next_action: l1_scope_exit,
            });
        }
    }
}

/// Build L2 table entries and L1 deferred entries for an L2→L1 continuation pattern
/// (reverse multi-call continuation). Handles N calls with children on any call.
///
/// # L2 Table Entries
///
/// For each L2→L1 call:
/// - **Without children**: `hash(CALL) → RESULT(L1, void)` — simple terminal
/// - **With 1 child**: `hash(CALL) → callReturn{scope=[0]}`, then
///   `hash(RESULT{L2, void}) → RESULT(L1, void)` — scope resolution
/// - **With N children**: `hash(CALL) → callReturn_0{scope=[0]}`, then
///   for each additional child k: `hash(RESULT{L2, void}) → callReturn_k{scope=[k]}`,
///   then `hash(RESULT{L2, void}) → RESULT(L1, void)` — final scope resolution
///
/// Child addresses are derived from each child's own routing:
///   - `callReturn.destination = child.source_address` (L2 contract)
///   - `callReturn.source_address = child.destination` (proxy's originalAddress)
///
/// Example (2-call multi-call continuation):
/// ```text
/// Entry 0: hash(CALL_A) → RESULT(L1, void)          — terminal (no children)
/// Entry 1: hash(CALL_B) → callReturn{scope=[0]}     — scope navigation
/// Entry 2: hash(RESULT{L2, void}) → RESULT(L1, void) — scope exit
/// ```
///
/// # L1 Deferred Entries
///
/// For each L2→L1 call:
/// - **First call (idx=0)**: nested DELIVERY with scope=[0], plus reentrant child entries
///   (if any children during delivery execution), plus delivery RESULT resolution
/// - **Subsequent call with children**: EXECUTION with scope=[0], plus reentrant child
///   entries (one per child) and scope resolution
/// - **Subsequent call without children**: simple trigger → RESULT
///
/// Reentrant child trigger hashes use the child's own routing:
///   - `destination = child.source_address` (proxy's originalAddress)
///   - `source_address = child.destination` (L1 caller to the proxy)
///
/// Example (2-call multi-call continuation, 5 entries):
/// ```text
/// Entry 0:  hash(trigger_A)    → delivery_CALL(dest_A, scope=[0])
/// Entry 0b: hash(RESULT(L1))   → RESULT(L1, void)  — delivery resolution
/// Entry 1:  hash(trigger_B)    → execution_CALL(dest_B, scope=[0])
/// Entry 1b: hash(child_trigger) → RESULT(L1, void)  — reentrant child
/// Entry 2:  hash(RESULT(L1))   → RESULT(L1, void)  — scope resolution
/// ```
///
/// # Arguments
/// * `detected` - Output of `analyze_l2_to_l1_continuation_calls`
/// * `our_rollup_id` - L2 rollup ID as U256
/// * `rlp_encoded_tx` - RLP-encoded L2 transaction for the L2TX trigger on L1
pub fn build_l2_to_l1_continuation_entries(
    detected: &[DetectedCall],
    our_rollup_id: U256,
    rlp_encoded_tx: &[u8],
    tx_reverts: bool,
) -> L2ToL1ContinuationEntries {
    if detected.is_empty() {
        return L2ToL1ContinuationEntries {
            l2_entries: vec![],
            l1_entries: vec![],
        };
    }

    let mainnet_rollup_id = U256::ZERO;
    let l1_result_void = result_void(mainnet_rollup_id);
    let empty_deltas: Vec<CrossChainStateDelta> = vec![];

    // Root calls: L2→L1 direct calls (parent_call_index=None).
    // Track original indices from the `detected` array so that child lookups
    // (via parent_call_index) use the correct index — not the filtered position.
    // This is critical for depth > 1 where a child's parent_call_index points to
    // another child (which is NOT a root call).
    let l2_to_l1_calls: Vec<(usize, &DetectedCall)> = detected
        .iter()
        .enumerate()
        .filter(|(_, c)| c.parent_call_index.is_none())
        .collect();

    // ── Partial revert detection ──
    //
    // When some root calls have `in_reverted_frame=true` and others don't, this is
    // the partial revert pattern (revertContinueL2): a try/catch on L2 reverts some
    // cross-chain calls but not all. On L1, the reverted group needs
    // REVERT/REVERT_CONTINUE to undo their L1 state changes.
    //
    // Example: DualCallerWithRevert.execute() → try { targetA.increment() } catch {} → targetB.increment()
    //   - Call 0 (targetA): in_reverted_frame=true, scope=[0,0]
    //   - Call 1 (targetB): in_reverted_frame=false, scope=[1]
    //   - L1 entries: [L2TX→CALL_A(scope=[0,0])], [RESULT_A→REVERT(scope=[0])],
    //                 [REVERT_CONTINUE→CALL_B(scope=[1])], [RESULT_B→RESULT(terminal)]
    let has_partial_revert = {
        let any_reverted = l2_to_l1_calls.iter().any(|(_, c)| c.in_reverted_frame);
        let any_non_reverted = l2_to_l1_calls.iter().any(|(_, c)| !c.in_reverted_frame);
        any_reverted && any_non_reverted
    };

    if has_partial_revert {
        tracing::info!(
            target: "based_rollup::table_builder",
            reverted = l2_to_l1_calls.iter().filter(|(_, c)| c.in_reverted_frame).count(),
            non_reverted = l2_to_l1_calls.iter().filter(|(_, c)| !c.in_reverted_frame).count(),
            "partial revert pattern detected — will insert REVERT/REVERT_CONTINUE between groups"
        );
    }

    // ── L2 table entries ──
    //
    // Mirror of the L1→L2 continuation's L1 entries (see build_continuation_entries).
    // ANY call with children gets scope navigation (callReturn{scope=[0]}), not just
    // the last call. Calls without children get simple CALL → RESULT(L1, void).
    //
    // For partial revert: only generate L2 entries for non-reverted calls.
    // Reverted calls' entries are consumed then rolled back by EVM revert on L2.
    // If a reverted call has the same actionHash as a non-reverted call (FIFO reuse),
    // the single entry serves both. If different hashes, the reverted call fails with
    // ExecutionNotFound (caught by try/catch).
    //
    // For a call with children, _processCallAtScope on L2 executes the return call
    // (e.g., receiveTokens) via proxy, which delivers assets back within the same tx.
    // After each scoped call, a scope resolution entry closes the scope.
    //
    // For depth > 1, children that themselves have children also get scope navigation
    // entries. Each reentrant executeCrossChainCall starts its own scope tree, so
    // scope is ALWAYS [0] regardless of nesting depth.
    let mut l2_entries = Vec::new();
    let l2_result_void = result_void(our_rollup_id);

    // Recursive helper: generate L2 entries for a call and all its descendants.
    #[allow(clippy::too_many_arguments)]
    fn generate_l2_entries_recursive(
        call_orig_idx: usize,
        call: &DetectedCall,
        detected: &[DetectedCall],
        our_rollup_id: U256,
        l2_result_void: &CrossChainAction,
        l1_result_void: &CrossChainAction,
        empty_deltas: &[CrossChainStateDelta],
        l2_entries: &mut Vec<CrossChainExecutionEntry>,
    ) {
        let call_hash = compute_action_hash(&call.call_action);
        let this_call_children = find_children(detected, call_orig_idx);

        tracing::info!(
            target: "based_rollup::table_builder",
            l2_entry_idx = l2_entries.len(),
            call_orig_idx,
            call_hash = %call_hash,
            destination = %call.call_action.destination,
            source = %call.call_action.source_address,
            children_count = this_call_children.len(),
            depth = call.depth,
            "L2->L1 continuation: L2 CALL entry"
        );

        if !this_call_children.is_empty() {
            // Call with children: CALL → callReturn{scope=[0]} for the first child.
            // _processCallAtScope on L2 will execute the return call (e.g., receiveTokens)
            // via proxy, delivering assets back within the same tx.
            //
            // For the callReturn construction, we derive addresses from the child's own
            // routing info. The proxy routing on L2 for the return trip is:
            //   proxy(child.destination, MAINNET).executeOnBehalf(child.source, retData)
            // So:
            //   callReturn.destination = child.source_address (L2 contract, e.g., Bridge_L2)
            //   callReturn.source_address = child.destination (proxy's originalAddress, e.g., Bridge_L1)
            //   callReturn.source_rollup = MAINNET
            let first_child = this_call_children[0].1;

            // callReturn targets L2 (where the return call executes).
            // _processCallAtScope on L2 does:
            //   proxy(source_address, source_rollup).executeOnBehalf(destination, data)
            //
            // For L2→L1 children (e.g., bridgeTokens return):
            //   destination = child.source_address (L2 contract, e.g., Bridge_L2)
            //   source_address = child.destination (proxy's originalAddress on L1)
            //
            // For L1→L2 return call children (e.g., pong(round, maxRounds)):
            //   destination = child.destination (L2 contract, e.g., PingPongL2)
            //   source_address = child.source_address (proxy's originalAddress on L1)
            let is_return_call = first_child.call_action.rollup_id == our_rollup_id;
            let (cr_dest, cr_source) = if is_return_call {
                (
                    first_child.call_action.destination,
                    first_child.call_action.source_address,
                )
            } else {
                (
                    first_child.call_action.source_address,
                    first_child.call_action.destination,
                )
            };
            let first_child_scope = if first_child.scope.is_empty() {
                vec![U256::ZERO]
            } else {
                first_child.scope.clone()
            };
            let call_return = CrossChainAction {
                action_type: CrossChainActionType::Call,
                rollup_id: our_rollup_id,
                destination: cr_dest,
                value: U256::ZERO,
                data: first_child.call_action.data.clone(),
                failed: false,
                source_address: cr_source,
                source_rollup: U256::ZERO, // MAINNET
                scope: first_child_scope,
            };

            tracing::info!(
                target: "based_rollup::table_builder",
                dest = %call_return.destination,
                source = %call_return.source_address,
                data_len = call_return.data.len(),
                child_count = this_call_children.len(),
                "L2->L1 continuation: L2 CALL → callReturn with scope navigation"
            );

            l2_entries.push(CrossChainExecutionEntry {
                state_deltas: empty_deltas.to_vec(),
                action_hash: call_hash,
                next_action: call_return,
            });

            // Additional children (beyond the first) get their own scope entries.
            // Each child gets scope=[child_index] so _processCallAtScope processes them
            // sequentially within the same scope navigation.
            for (child_pos, &(_child_orig_idx, child)) in
                this_call_children.iter().enumerate().skip(1)
            {
                let child_is_return = child.call_action.rollup_id == our_rollup_id;
                let (ccr_dest, ccr_source) = if child_is_return {
                    (
                        child.call_action.destination,
                        child.call_action.source_address,
                    )
                } else {
                    (
                        child.call_action.source_address,
                        child.call_action.destination,
                    )
                };
                // Sibling scope: use child's pre-computed scope if available,
                // otherwise fall back to simple positional index.
                let sibling_scope = if child.scope.is_empty() {
                    vec![U256::from(child_pos)]
                } else {
                    child.scope.to_vec()
                };
                let child_call_return = CrossChainAction {
                    action_type: CrossChainActionType::Call,
                    rollup_id: our_rollup_id,
                    destination: ccr_dest,
                    value: U256::ZERO,
                    data: child.call_action.data.clone(),
                    failed: false,
                    source_address: ccr_source,
                    source_rollup: U256::ZERO, // MAINNET
                    scope: sibling_scope,
                };

                tracing::info!(
                    target: "based_rollup::table_builder",
                    l2_entry_idx = l2_entries.len(),
                    child_pos,
                    dest = %child_call_return.destination,
                    source = %child_call_return.source_address,
                    "L2->L1 continuation: L2 additional child callReturn scope=[{}]",
                    child_pos
                );

                // The action hash for additional scoped children uses the PREVIOUS child's
                // L2 return data (the scope that just resolved). For void functions, this
                // is result_void. For functions returning data, the RESULT includes that data.
                let prev_child = if child_pos > 1 {
                    this_call_children[child_pos - 1].1
                } else {
                    first_child
                };
                let prev_l2_result =
                    if prev_child.l2_return_data.is_empty() && !prev_child.l2_delivery_failed {
                        l2_result_void.clone()
                    } else {
                        CrossChainAction {
                            action_type: CrossChainActionType::Result,
                            rollup_id: our_rollup_id,
                            destination: Address::ZERO,
                            value: U256::ZERO,
                            data: prev_child.l2_return_data.clone(),
                            failed: prev_child.l2_delivery_failed,
                            source_address: Address::ZERO,
                            source_rollup: U256::ZERO,
                            scope: vec![],
                        }
                    };
                let l2_result_hash = compute_action_hash(&prev_l2_result);
                l2_entries.push(CrossChainExecutionEntry {
                    state_deltas: empty_deltas.to_vec(),
                    action_hash: l2_result_hash,
                    next_action: child_call_return,
                });
            }

            // Scope resolution: after the last child callReturn executes inside its scope,
            // _processCallAtScope builds RESULT{rollupId=callReturn.rollupId=our_rollup_id,
            // data=returnData}. The returnData is the raw bytes from executeOnBehalf,
            // which is what the child's target returns. If the child returns data (e.g.,
            // Counter.increment() → uint256), we must include it in the RESULT hash.
            // Without this, result_void is used and the hash mismatches (issue #245).
            // Use the LAST child's l2_return_data (scope resolution happens after
            // the last child executes).
            let last_child = this_call_children
                .last()
                .map(|(_, c)| *c)
                .unwrap_or(first_child);
            let first_child_l2_data = &last_child.l2_return_data;
            let last_child_l2_failed = last_child.l2_delivery_failed;
            let l2_scope_result = if first_child_l2_data.is_empty() && !last_child_l2_failed {
                l2_result_void.clone()
            } else {
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: our_rollup_id,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: first_child_l2_data.clone(),
                    failed: last_child_l2_failed,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            };
            let l2_result_hash = compute_action_hash(&l2_scope_result);
            tracing::info!(
                target: "based_rollup::table_builder",
                l2_entry_idx = l2_entries.len(),
                result_hash = %l2_result_hash,
                child_count = this_call_children.len(),
                "L2->L1 continuation: L2 scope resolution entry"
            );
            // Issue #246: nextAction.data must carry the L1 delivery return data
            // so _resolveScopes returns it to the L2 caller. The action_hash
            // (computed from l2_return_data) is unchanged — _consumeExecution
            // only matches on hash, returns nextAction as-is.
            let scope_exit_action =
                if call.delivery_return_data.is_empty() && !call.l2_delivery_failed {
                    l1_result_void.clone()
                } else {
                    CrossChainAction {
                        action_type: CrossChainActionType::Result,
                        rollup_id: U256::ZERO,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: call.delivery_return_data.clone(),
                        failed: call.l2_delivery_failed,
                        source_address: Address::ZERO,
                        source_rollup: U256::ZERO,
                        scope: vec![],
                    }
                };
            l2_entries.push(CrossChainExecutionEntry {
                state_deltas: empty_deltas.to_vec(),
                action_hash: l2_result_hash,
                next_action: scope_exit_action,
            });

            // For depth > 1: children that themselves have grandchildren need their own
            // L2 entries (scope navigation consumed by reentrant executeCrossChainCall).
            // Each reentrant call starts a fresh scope tree from [], so scope=[0] is correct.
            for &(child_orig_idx, child) in &this_call_children {
                let grandchildren = find_children(detected, child_orig_idx);
                if !grandchildren.is_empty() {
                    generate_l2_entries_recursive(
                        child_orig_idx,
                        child,
                        detected,
                        our_rollup_id,
                        l2_result_void,
                        l1_result_void,
                        empty_deltas,
                        l2_entries,
                    );
                }
            }
        } else {
            // Simple call (no children): CALL → RESULT terminal.
            // Issue #254 item 8: use delivery_return_data when the L1 delivery
            // returns data, so _resolveScopes returns it to the L2 caller.
            let simple_next = if call.delivery_return_data.is_empty() && !call.l2_delivery_failed {
                l1_result_void.clone()
            } else {
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: U256::ZERO,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: call.delivery_return_data.clone(),
                    failed: call.l2_delivery_failed,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            };
            l2_entries.push(CrossChainExecutionEntry {
                state_deltas: empty_deltas.to_vec(),
                action_hash: call_hash,
                next_action: simple_next,
            });
        }
    }

    for &(orig_idx, call) in &l2_to_l1_calls {
        // For partial revert: skip L2 entries for reverted calls.
        // Their entries are consumed→restored by EVM revert, or the call fails
        // with ExecutionNotFound (caught by try/catch). Either way, the L2 effect
        // is the same: the reverted call has no lasting impact.
        if has_partial_revert && call.in_reverted_frame {
            continue;
        }
        generate_l2_entries_recursive(
            orig_idx,
            call,
            detected,
            our_rollup_id,
            &l2_result_void,
            &l1_result_void,
            &empty_deltas,
            &mut l2_entries,
        );
    }

    // ── L1 deferred entries ──
    //
    // Reference: ExecuteReverseFlashLoan.s.sol:PostReverseFlashLoanEntries (L2→L1 pattern)
    //
    // L1 entries use TRIGGER perspective for the actionHash (what Rollups.executeCrossChainCall
    // computes from the proxy call). The trigger actions have:
    //   rollupId = proxyInfo.originalRollupId = our_rollup_id
    //   destination = proxyInfo.originalAddress (L2 source address)
    //   source = builder_address (msg.sender to the proxy)
    //   source_rollup = 0 (L1)
    //
    // The nextAction fields use the EXECUTION perspective (what _processCallAtScope needs):
    //   rollupId = 0 (L1, where the call executes)
    //   destination = L1 target (where executeOnBehalf calls)
    //   source = L2 source (the proxy's originalAddress)
    //   source_rollup = our_rollup_id
    //
    // For each L2→L1 call:
    //   - The FIRST call (index 0) gets nested DELIVERY (scope=[0]) so that
    //     _processCallAtScope actually executes the delivery (e.g., receiveTokens
    //     on Bridge_L1, releasing tokens). Without nested delivery, Rollups returns
    //     RESULT without executing anything.
    //   - Subsequent calls with children get EXECUTION (scope=[0]) so that
    //     _processCallAtScope executes the call (e.g., claimAndBridgeBack on L1)
    //     which may trigger reentrant child calls.
    //   - Calls without children get simple trigger → RESULT entries.
    //   - Each call with children also generates reentrant entries for each child
    //     and a scope resolution entry.
    //   - For depth > 1, push_reentrant_child_entries recurses into children that
    //     have their own children, generating execution + scope resolution entries.
    let mut l1_entries = Vec::new();

    let num_l2_to_l1 = l2_to_l1_calls.len();
    let is_multi_call = num_l2_to_l1 > 1;

    // Check if ANY call has reentrant children (nested L2→L1→L2 round-trips).
    // When nested calls are present, the protocol executes them within their own
    // executeCrossChainCall context — no sibling scope routing needed (scope=[]).
    // Pure sibling patterns (all simple, no children) use scope=[sibling_index].
    let has_any_nested = l2_to_l1_calls
        .iter()
        .any(|&(orig_idx, _)| !find_children(detected, orig_idx).is_empty());
    // Use sibling scopes ONLY for pure simple multi-call (no nested children).
    let use_sibling_scopes = is_multi_call && !has_any_nested;

    for (root_pos, &(orig_idx, l2_call)) in l2_to_l1_calls.iter().enumerate() {
        // Find children belonging to THIS call using original detected index.
        let this_call_children = find_children(detected, orig_idx);

        // Compute the delivery CALL scope:
        // - Pure simple siblings: scope=[sibling_index] for routing within executeL2TX
        // - Any nested pattern: scope=[] (sequential chaining, no scope navigation)
        // - Single call: scope from trace depth
        let call_scope_for_delivery = if use_sibling_scopes {
            // For simple siblings at depth 1: scope=[root_pos]
            // For deep siblings: scope=[0,...,root_pos]
            if l2_call.scope.is_empty() {
                vec![U256::from(root_pos)]
            } else {
                let mut s = l2_call.scope[..l2_call.scope.len().saturating_sub(1)].to_vec();
                s.push(U256::from(root_pos));
                s
            }
        } else if l2_call.scope.is_empty() {
            vec![] // single call or nested pattern: no scope navigation
        } else if !has_any_nested {
            l2_call.scope.clone() // single call with trace depth scope
        } else {
            vec![] // nested multi-call: always scope=[]
        };

        let delivery = CrossChainAction {
            action_type: CrossChainActionType::Call,
            rollup_id: U256::ZERO,
            destination: l2_call.call_action.destination,
            value: l2_call.call_action.value,
            data: l2_call.call_action.data.clone(),
            failed: false,
            source_address: l2_call.call_action.source_address,
            source_rollup: our_rollup_id,
            scope: call_scope_for_delivery,
        };

        let delivery_ether_delta = if delivery.value.is_zero() {
            alloy_primitives::I256::ZERO
        } else {
            -alloy_primitives::I256::try_from(delivery.value)
                .unwrap_or(alloy_primitives::I256::ZERO)
        };

        // Compute delivery RESULT for this call (used as trigger for next call).
        let delivery_result =
            if l2_call.delivery_return_data.is_empty() && !l2_call.l2_delivery_failed {
                result_void(U256::ZERO)
            } else {
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: U256::ZERO,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: l2_call.delivery_return_data.clone(),
                    failed: l2_call.l2_delivery_failed,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            };
        let delivery_result_hash = compute_action_hash(&delivery_result);

        if root_pos == 0 {
            // FIRST call: L2TX trigger → delivery CALL.
            let trigger = CrossChainAction {
                action_type: CrossChainActionType::L2Tx,
                rollup_id: our_rollup_id,
                destination: Address::ZERO,
                value: U256::ZERO,
                data: rlp_encoded_tx.to_vec(),
                failed: false,
                source_address: Address::ZERO,
                source_rollup: U256::ZERO,
                scope: vec![],
            };
            let trigger_hash = compute_action_hash(&trigger);

            tracing::info!(
                target: "based_rollup::table_builder",
                "L1 Entry {} (L2TX trigger→delivery): hash={} dest={} scope_len={} ether_delta={}",
                root_pos, trigger_hash, delivery.destination, delivery.scope.len(), delivery_ether_delta
            );

            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: our_rollup_id,
                    current_state: alloy_primitives::B256::ZERO,
                    new_state: alloy_primitives::B256::ZERO,
                    ether_delta: alloy_primitives::I256::ZERO,
                }],
                action_hash: trigger_hash,
                next_action: delivery,
            });

            // Reentrant child entries (e.g., flash loan children).
            push_reentrant_child_entries(
                &this_call_children,
                detected,
                our_rollup_id,
                orig_idx,
                &empty_deltas,
                &l1_result_void,
                &mut l1_entries,
            );

            // Scope resolution: RESULT of this call → next action.
            // For sibling multi-call: nextAction = CALL(next_call, scope=[i+1])
            // For single/last call: nextAction = RESULT(terminal)
            let next_action = if root_pos < num_l2_to_l1 - 1 {
                // Chain to next call: RESULT → CALL(next)
                let next_call = l2_to_l1_calls[root_pos + 1].1;
                // Scope: pure simple siblings need scope=[i+1] for routing.
                // Any nested pattern uses scope=[] (sequential chaining).
                let next_scope = if use_sibling_scopes {
                    vec![U256::from(root_pos + 1)]
                } else {
                    vec![]
                };
                CrossChainAction {
                    action_type: CrossChainActionType::Call,
                    rollup_id: U256::ZERO,
                    destination: next_call.call_action.destination,
                    value: next_call.call_action.value,
                    data: next_call.call_action.data.clone(),
                    failed: false,
                    source_address: next_call.call_action.source_address,
                    source_rollup: our_rollup_id,
                    scope: next_scope,
                }
            } else {
                // §C.6: L2TX terminal RESULT
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: our_rollup_id,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: vec![],
                    failed: false,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            };

            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: our_rollup_id,
                    current_state: alloy_primitives::B256::ZERO,
                    new_state: alloy_primitives::B256::ZERO,
                    ether_delta: delivery_ether_delta,
                }],
                action_hash: delivery_result_hash,
                next_action,
            });
        } else if !this_call_children.is_empty() {
            // Subsequent call WITH children.
            // In the chained model, the previous call's RESULT already points to
            // this call's CALL (generated above). No trigger entry needed here.
            // Only generate reentrant child entries + scope resolution.

            // Reentrant entries for EACH child of this call.
            // For depth > 1, this recurses into children with grandchildren.
            push_reentrant_child_entries(
                &this_call_children,
                detected,
                our_rollup_id,
                orig_idx,
                &empty_deltas,
                &l1_result_void,
                &mut l1_entries,
            );

            // Scope resolution entry for this call (same pattern as delivery RESULT).
            let scope_result =
                if l2_call.delivery_return_data.is_empty() && !l2_call.l2_delivery_failed {
                    result_void(U256::ZERO)
                } else {
                    CrossChainAction {
                        action_type: CrossChainActionType::Result,
                        rollup_id: U256::ZERO,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: l2_call.delivery_return_data.clone(),
                        failed: l2_call.l2_delivery_failed,
                        source_address: Address::ZERO,
                        source_rollup: U256::ZERO,
                        scope: vec![],
                    }
                };
            let scope_result_hash = compute_action_hash(&scope_result);

            tracing::info!(
                target: "based_rollup::table_builder",
                "L1 Entry {} (scope resolution): hash={} child_count={} return_data_len={}",
                root_pos, scope_result_hash, this_call_children.len(),
                l2_call.delivery_return_data.len()
            );

            // Scope resolution: chain to next call or terminal.
            // Nested pattern → scope=[] for chaining.
            let scope_next_action = if root_pos < num_l2_to_l1 - 1 {
                let next_call = l2_to_l1_calls[root_pos + 1].1;
                let next_scope = if use_sibling_scopes {
                    vec![U256::from(root_pos + 1)]
                } else {
                    vec![]
                };
                CrossChainAction {
                    action_type: CrossChainActionType::Call,
                    rollup_id: U256::ZERO,
                    destination: next_call.call_action.destination,
                    value: next_call.call_action.value,
                    data: next_call.call_action.data.clone(),
                    failed: false,
                    source_address: next_call.call_action.source_address,
                    source_rollup: our_rollup_id,
                    scope: next_scope,
                }
            } else {
                // Last call: §C.6 terminal
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: our_rollup_id,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: vec![],
                    failed: false,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            };
            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: our_rollup_id,
                    current_state: alloy_primitives::B256::ZERO,
                    new_state: alloy_primitives::B256::ZERO,
                    ether_delta: delivery_ether_delta,
                }],
                action_hash: delivery_result_hash,
                next_action: scope_next_action,
            });
        } else if is_multi_call && this_call_children.is_empty() {
            // Subsequent sibling call WITHOUT children.
            // In the chained model, root_pos=0's RESULT already points to this call's
            // CALL (generated above). We only need the scope resolution entry:
            // RESULT(this_call) → next_action (CALL(next) or terminal).
            let next_action = if root_pos < num_l2_to_l1 - 1 {
                let next_call = l2_to_l1_calls[root_pos + 1].1;
                // Use sibling scope for pure simple patterns, empty for nested.
                let next_scope = if use_sibling_scopes {
                    vec![U256::from(root_pos + 1)]
                } else {
                    vec![]
                };
                CrossChainAction {
                    action_type: CrossChainActionType::Call,
                    rollup_id: U256::ZERO,
                    destination: next_call.call_action.destination,
                    value: next_call.call_action.value,
                    data: next_call.call_action.data.clone(),
                    failed: false,
                    source_address: next_call.call_action.source_address,
                    source_rollup: our_rollup_id,
                    scope: next_scope,
                }
            } else {
                // Last call: §C.6 L2TX terminal RESULT
                CrossChainAction {
                    action_type: CrossChainActionType::Result,
                    rollup_id: our_rollup_id,
                    destination: Address::ZERO,
                    value: U256::ZERO,
                    data: vec![],
                    failed: false,
                    source_address: Address::ZERO,
                    source_rollup: U256::ZERO,
                    scope: vec![],
                }
            };

            tracing::info!(
                target: "based_rollup::table_builder",
                "L1 Entry {} (sibling RESULT→next): hash={} next_type={:?} next_dest={}",
                root_pos, delivery_result_hash, next_action.action_type,
                next_action.destination
            );

            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: our_rollup_id,
                    current_state: alloy_primitives::B256::ZERO,
                    new_state: alloy_primitives::B256::ZERO,
                    ether_delta: delivery_ether_delta,
                }],
                action_hash: delivery_result_hash,
                next_action,
            });
        } else {
            // Subsequent call WITHOUT children AND without multi-call.
            // This should not happen — single calls use root_pos=0 only.
            tracing::error!(
                target: "based_rollup::table_builder",
                root_pos,
                "unexpected: root_pos > 0 in non-multi-call pattern (single calls should not reach here)"
            );
        }
    }

    // When tx_reverts=true, replace the last L1 entry's terminal RESULT with
    // REVERT and append a REVERT_CONTINUE → terminal RESULT entry.
    // This ensures all L1 state changes are undone via ScopeReverted (§D.12).
    // REVERT scope=[0] targets the first child scope of _resolveScopes.
    // This is ALWAYS [0] regardless of delivery depth — per protocol tests
    // revertCounterL2 and deepScopeRevert both use REVERT(scope=[0]).
    if tx_reverts && !l1_entries.is_empty() {
        use crate::cross_chain::{compute_revert_continue_hash, revert_action};

        let last = l1_entries
            .last_mut()
            .expect("l1_entries non-empty (checked above)");
        // Verify the last entry has a terminal RESULT nextAction
        if last.next_action.action_type == CrossChainActionType::Result
            && last.next_action.rollup_id == our_rollup_id
        {
            // Save the terminal for the REVERT_CONTINUE entry
            let terminal = std::mem::replace(
                &mut last.next_action,
                revert_action(our_rollup_id, vec![alloy_primitives::U256::ZERO]), // scope=[0]
            );

            // Append REVERT_CONTINUE → terminal RESULT
            l1_entries.push(CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: our_rollup_id,
                    current_state: alloy_primitives::B256::ZERO,
                    new_state: alloy_primitives::B256::ZERO,
                    ether_delta: alloy_primitives::I256::ZERO,
                }],
                action_hash: compute_revert_continue_hash(our_rollup_id),
                next_action: terminal,
            });

            tracing::info!(
                target: "based_rollup::table_builder",
                l1_entry_count = l1_entries.len(),
                "appended REVERT/REVERT_CONTINUE entries for tx_reverts"
            );
        }
    }

    // When has_partial_revert=true, insert REVERT/REVERT_CONTINUE between the
    // last reverted call and the first non-reverted call on L1.
    //
    // The normal multi-call loop builds entries like:
    //   Entry 0: L2TX → CALL_reverted(scope=[0,0])
    //   Entry 1: RESULT_reverted → CALL_nonreverted(scope=[1])
    //   Entry 2: RESULT_nonreverted → RESULT(terminal)
    //
    // Partial revert transforms this to:
    //   Entry 0: L2TX → CALL_reverted(scope=[0,0])
    //   Entry 1: RESULT_reverted → REVERT(scope=[0])
    //   Entry 2: REVERT_CONTINUE → CALL_nonreverted(scope=[1])
    //   Entry 3: RESULT_nonreverted → RESULT(terminal)
    //
    // The transformation: find the entry whose nextAction is the first non-reverted
    // call's CALL, replace it with REVERT, insert REVERT_CONTINUE → CALL.
    if has_partial_revert && !l1_entries.is_empty() {
        use crate::cross_chain::{compute_revert_continue_hash, revert_action};

        // Find the first non-reverted root call's position in l2_to_l1_calls.
        let first_non_reverted_root_pos = l2_to_l1_calls
            .iter()
            .position(|(_, c)| !c.in_reverted_frame);

        if let Some(non_rev_pos) = first_non_reverted_root_pos {
            tracing::info!(
                target: "based_rollup::table_builder",
                non_rev_pos,
                l1_entry_count = l1_entries.len(),
                "partial revert: searching for boundary entry to insert REVERT"
            );
            for (i, e) in l1_entries.iter().enumerate() {
                tracing::info!(
                    target: "based_rollup::table_builder",
                    idx = i,
                    action_hash = %e.action_hash,
                    next_action_type = ?e.next_action.action_type,
                    next_action_dest = %e.next_action.destination,
                    next_action_scope_len = e.next_action.scope.len(),
                    next_action_data_len = e.next_action.data.len(),
                    "partial revert: L1 entry before transformation"
                );
            }
            // Find the L1 entry whose nextAction chains to the first non-reverted call.
            // The boundary is the entry whose nextAction.scope matches the first
            // non-reverted call's expected scope. For same-target calls (e.g.,
            // SelfCallerWithRevert: both calls target Counter.increment()), we can't
            // distinguish by destination/data alone — use scope to disambiguate.
            // The non-reverted call's scope is computed identically to the main loop's
            // sibling scope logic: last element = root_pos.
            let first_non_reverted = l2_to_l1_calls[non_rev_pos].1;
            let expected_scope = if first_non_reverted.scope.is_empty() {
                vec![U256::from(non_rev_pos)]
            } else {
                let mut s = first_non_reverted.scope
                    [..first_non_reverted.scope.len().saturating_sub(1)]
                    .to_vec();
                s.push(U256::from(non_rev_pos));
                s
            };
            tracing::info!(
                target: "based_rollup::table_builder",
                expected_scope = ?expected_scope.iter().map(|s| format!("{s}")).collect::<Vec<_>>(),
                "partial revert: searching for entry with nextAction.scope matching non-reverted call"
            );
            let boundary_idx = l1_entries.iter().position(|e| {
                e.next_action.action_type == CrossChainActionType::Call
                    && e.next_action.scope == expected_scope
            });

            if let Some(idx) = boundary_idx {
                // Save the non-reverted CALL action
                let continuation_call = l1_entries[idx].next_action.clone();

                // Replace with REVERT(scope=[0])
                l1_entries[idx].next_action =
                    revert_action(our_rollup_id, vec![alloy_primitives::U256::ZERO]);

                // Insert REVERT_CONTINUE → continuation_call
                l1_entries.insert(
                    idx + 1,
                    CrossChainExecutionEntry {
                        state_deltas: vec![CrossChainStateDelta {
                            rollup_id: our_rollup_id,
                            current_state: alloy_primitives::B256::ZERO,
                            new_state: alloy_primitives::B256::ZERO,
                            ether_delta: alloy_primitives::I256::ZERO,
                        }],
                        action_hash: compute_revert_continue_hash(our_rollup_id),
                        next_action: continuation_call,
                    },
                );

                tracing::info!(
                    target: "based_rollup::table_builder",
                    boundary_idx = idx,
                    l1_entry_count = l1_entries.len(),
                    "inserted REVERT/REVERT_CONTINUE for partial revert pattern"
                );
                for (i, e) in l1_entries.iter().enumerate() {
                    tracing::info!(
                        target: "based_rollup::table_builder",
                        idx = i,
                        action_hash = %e.action_hash,
                        next_action_type = ?e.next_action.action_type,
                        next_action_dest = %e.next_action.destination,
                        next_action_rollup_id = %e.next_action.rollup_id,
                        next_action_scope = ?e.next_action.scope.iter().map(|s| format!("{s}")).collect::<Vec<_>>(),
                        next_action_failed = e.next_action.failed,
                        next_action_data_hex = %format!("0x{}", hex::encode(&e.next_action.data)),
                        "partial revert: L1 entry AFTER transformation"
                    );
                }
            } else {
                tracing::warn!(
                    target: "based_rollup::table_builder",
                    "partial revert: could not find boundary entry to insert REVERT"
                );
            }
        }
    }

    // Reorder same-hash groups for Solidity swap-and-pop FIFO consumption.
    reorder_for_swap_and_pop(&mut l2_entries);
    reorder_for_swap_and_pop(&mut l1_entries);

    L2ToL1ContinuationEntries {
        l2_entries,
        l1_entries,
    }
}

#[cfg(test)]
#[path = "table_builder_tests.rs"]
mod tests;
