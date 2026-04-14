//! Iterative fixed-point discovery loop for cross-chain calls.
//!
//! `discover_until_stable` is the shared skeleton for both L1→L2 and L2→L1
//! directions. It implements the trace-walk-dedupe-converge pattern:
//!
//! 1. Initial trace of user tx on source chain
//! 2. Walk trace to detect cross-chain proxy calls
//! 3. If top-level reverted and calls found → iterative loop:
//!    a. Direction builds retrace bundle (entries loaded + user tx)
//!    b. Execute debug_traceCallMany on source chain
//!    c. Walk retrace for new calls
//!    d. Dedup against known calls
//!    e. If converged (no new calls), break
//! 4. Correct in_reverted_frame from converged retrace
//! 5. Return DiscoveredSet
//!
//! The direction-specific logic lives in [`Direction`] hooks:
//! - [`Direction::build_retrace_bundle`] — entry loading strategy
//! - [`Direction::source_manager_addresses`] — trace walker addresses
//! - [`Direction::default_target_rollup_id`] — default rollup ID
//!
//! Introduced in refactor step 3.4 (PLAN.md §Phase 3).

use std::collections::HashMap;

use alloy_primitives::Address;
use serde_json::Value;

use super::direction::{Direction, UserTxContext};
use super::model::{
    DiscoveredCall, DiscoveredSet, MAX_DISCOVERY_ITERATIONS, PromotionDecision,
    correct_in_reverted_frame, dedup_discovered_calls, walk_trace_to_discovered,
};
use super::sim_client::SimulationClient;
use super::trace;

/// Run the iterative discovery loop for cross-chain calls.
///
/// Given an initial trace result and user tx context, discovers all cross-chain
/// calls by iteratively expanding the trace with entries loaded until no new
/// calls appear.
///
/// Returns a [`DiscoveredSet`] with all discovered forward calls.
/// Return edges are populated later by delivery simulation (step 3.6).
#[allow(
    dead_code,
    reason = "scaffold — callers migrate from direction-specific functions"
)]
pub(crate) async fn discover_until_stable<D: Direction, S: SimulationClient>(
    direction: &D,
    sim: &S,
    initial_trace: &Value,
    user_tx: &UserTxContext,
    proxy_lookup: &dyn trace::ProxyLookup,
    proxy_cache: &mut HashMap<Address, Option<trace::ProxyInfo>>,
    initial_calls: Option<Vec<DiscoveredCall>>,
) -> eyre::Result<DiscoveredSet> {
    let managers = direction.source_manager_addresses();
    let default_rid = direction.default_target_rollup_id();

    // Step 1: Walk initial trace for cross-chain calls, or use pre-supplied calls.
    let mut all_calls = if let Some(calls) = initial_calls {
        calls
    } else {
        walk_trace_to_discovered(
            proxy_lookup,
            &managers,
            initial_trace,
            proxy_cache,
            default_rid,
            0, // iteration 0 = initial trace
        )
        .await
    };

    if all_calls.is_empty() {
        return Ok(DiscoveredSet {
            calls: vec![],
            returns: vec![],
            promotion: PromotionDecision::KeepSimple,
            user_tx_reverted: false,
        });
    }

    // Check if the top-level call reverted — indicates entries need pre-loading.
    let top_level_error =
        initial_trace.get("error").is_some() || initial_trace.get("revertReason").is_some();

    // Step 2: Iterative discovery if top-level reverted.
    let mut last_retrace_results: Vec<DiscoveredCall> = Vec::new();
    let mut user_tx_reverted = top_level_error;
    let mut accumulated_returns: Vec<super::model::ReturnEdge> = Vec::new();

    if top_level_error {
        for iteration in 1..=MAX_DISCOVERY_ITERATIONS {
            tracing::info!(
                target: "based_rollup::discover",
                direction = D::name(),
                iteration,
                known_calls = all_calls.len(),
                "iterative discovery: retracing with entries loaded"
            );

            // Direction-specific: enrich calls with delivery return data.
            let enrichment_returns = direction
                .enrich_calls_before_retrace(&mut all_calls, user_tx, iteration)
                .await;
            if !enrichment_returns.is_empty() {
                accumulated_returns.extend(enrichment_returns);
            }

            // Log enrichment results for debugging
            for (ci, c) in all_calls.iter().enumerate() {
                tracing::info!(
                    target: "based_rollup::discover",
                    iteration,
                    ci,
                    delivery_failed = c.delivery_failed,
                    delivery_data_len = c.delivery_return_data.len(),
                    "post-enrichment call state"
                );
            }

            // Direction-specific: build the retrace bundle.
            let bundle = match direction
                .build_retrace_bundle(&all_calls, user_tx, iteration)
                .await
            {
                Some(b) => b,
                None => {
                    tracing::warn!(
                        target: "based_rollup::discover",
                        direction = D::name(),
                        iteration,
                        "build_retrace_bundle failed — stopping discovery"
                    );
                    break;
                }
            };

            // Execute the retrace on the source chain.
            let trace_result = sim
                .trace_call_many(D::source_chain(), &[bundle], None)
                .await;

            let traces = match trace_result {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        target: "based_rollup::discover",
                        direction = D::name(),
                        iteration,
                        %e,
                        "traceCallMany failed — stopping discovery"
                    );
                    break;
                }
            };

            // Extract user tx trace from the bundle response.
            // Bundle format: result[0] = array of per-tx traces.
            // Tx[0] = entry loading (loadTable/postBatch), Tx[1] = user tx.
            let user_trace = match traces.get(0).and_then(|b| b.as_array()) {
                Some(arr) if arr.len() >= 2 => &arr[1],
                _ => {
                    tracing::warn!(
                        target: "based_rollup::discover",
                        direction = D::name(),
                        iteration,
                        "unexpected trace bundle shape — stopping discovery"
                    );
                    break;
                }
            };

            // Track whether user tx still reverts after entries loaded.
            let user_error_str = user_trace
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            user_tx_reverted = user_error_str != "none";
            tracing::info!(
                target: "based_rollup::discover",
                iteration,
                user_error = user_error_str,
                user_tx_reverted,
                "retrace user tx error status"
            );

            // Walk the retrace for new calls.
            let new_detected = walk_trace_to_discovered(
                proxy_lookup,
                &managers,
                user_trace,
                proxy_cache,
                default_rid,
                iteration,
            )
            .await;

            // Save for in_reverted_frame correction.
            last_retrace_results = new_detected.clone();

            // Dedup: keep only truly new calls.
            let new_calls = dedup_discovered_calls(new_detected, &all_calls);

            if new_calls.is_empty() {
                tracing::info!(
                    target: "based_rollup::discover",
                    direction = D::name(),
                    iteration,
                    total = all_calls.len(),
                    "iterative discovery converged — no new calls"
                );
                break;
            }

            tracing::info!(
                target: "based_rollup::discover",
                direction = D::name(),
                iteration,
                new = new_calls.len(),
                "discovered new cross-chain calls"
            );
            all_calls.extend(new_calls);
        }
    }

    // Step 3: Correct in_reverted_frame from converged retrace.
    correct_in_reverted_frame(&mut all_calls, &last_retrace_results);

    Ok(DiscoveredSet {
        calls: all_calls,
        returns: accumulated_returns,
        promotion: PromotionDecision::KeepSimple,
        user_tx_reverted,
    })
}
