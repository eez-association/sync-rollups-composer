//! Simulation strategy selection for cross-chain delivery.
//!
//! [`SimulationPlan`] is a closed enum of the 2 simulation strategies.
//! [`simulation_plan_for`] is the single decision point that selects the
//! strategy from the input shape — **closing invariants #17 and #21**:
//!
//! - **#17**: multi-call NEVER uses per-call `simulate_l1_delivery`. The
//!   `CombinedThenAnalytical` variant enforces combined simulation.
//! - **#21**: single L2→L1 call with a terminal return call is PROMOTED to
//!   `CombinedThenAnalytical` via [`PromotionDecision::PromoteToContinuation`].
//!
//! Introduced in refactor step 3.6 (PLAN.md §Phase 3).

use super::model::{DiscoveredCall, PromotionDecision};

/// The two simulation strategies used by both directions.
///
/// A closed enum — no new strategies can be added without updating this
/// file and the decision function below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimulationPlan {
    /// Single call: simulate one delivery in isolation.
    ///
    /// Used when there is exactly one forward call and no return calls.
    /// The simplest and fastest path.
    Single,

    /// Combined simulation with analytical fallback.
    ///
    /// Used for multi-call patterns (≥2 forward calls) OR when a terminal
    /// return call promotes a single call to continuation mode (#21).
    ///
    /// 1. Try `debug_traceCallMany` bundling all triggers so later calls
    ///    see state effects from earlier ones.
    /// 2. On failure, fall back to analytical construction from the forward
    ///    trip's parameters (e.g., `receiveTokens` args for flash loan return).
    CombinedThenAnalytical,
}

/// Select the simulation strategy from the discovery result shape.
///
/// This is the **single decision point** for invariants #17 and #21.
/// Every simulation call flows through here — there is no other path
/// that selects a strategy.
pub(crate) fn simulation_plan_for(
    calls: &[DiscoveredCall],
    promotion: PromotionDecision,
) -> SimulationPlan {
    match promotion {
        // Terminal return call present → always combined (invariant #21).
        PromotionDecision::PromoteToContinuation => SimulationPlan::CombinedThenAnalytical,
        PromotionDecision::KeepSimple => {
            if calls.len() > 1 {
                // Multi-call → combined (invariant #17).
                SimulationPlan::CombinedThenAnalytical
            } else {
                SimulationPlan::Single
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_chain::ParentLink;
    use alloy_primitives::{Address, U256};

    fn dummy_call() -> DiscoveredCall {
        DiscoveredCall {
            destination: Address::ZERO,
            calldata: vec![],
            value: U256::ZERO,
            source_address: Address::ZERO,
            parent_call_index: ParentLink::Root,
            trace_depth: 0,
            discovery_iteration: 0,
            in_reverted_frame: false,
            delivery_return_data: vec![],
            delivery_failed: false,
            target_rollup_id: 0,
        }
    }

    #[test]
    fn single_call_keep_simple() {
        let calls = vec![dummy_call()];
        assert_eq!(
            simulation_plan_for(&calls, PromotionDecision::KeepSimple),
            SimulationPlan::Single,
        );
    }

    #[test]
    fn multi_call_always_combined() {
        let calls = vec![dummy_call(), dummy_call()];
        assert_eq!(
            simulation_plan_for(&calls, PromotionDecision::KeepSimple),
            SimulationPlan::CombinedThenAnalytical,
        );
    }

    #[test]
    fn single_call_promoted_to_continuation() {
        // Invariant #21: single call + terminal return → combined.
        let calls = vec![dummy_call()];
        assert_eq!(
            simulation_plan_for(&calls, PromotionDecision::PromoteToContinuation),
            SimulationPlan::CombinedThenAnalytical,
        );
    }

    #[test]
    fn multi_call_promoted_still_combined() {
        let calls = vec![dummy_call(), dummy_call()];
        assert_eq!(
            simulation_plan_for(&calls, PromotionDecision::PromoteToContinuation),
            SimulationPlan::CombinedThenAnalytical,
        );
    }
}
