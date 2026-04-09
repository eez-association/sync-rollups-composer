//! Sealed `Direction` trait — the compile-time axis for composer unification.
//!
//! The two composer RPC modules (`l1_to_l2` and `l2_to_l1`) are near-identical
//! mirrors that differ in direction-specific facts: which chain is the source,
//! which is the target, how to classify trace calls, and how to build the queue
//! payload. This trait captures those differences as associated types and hooks.
//!
//! The trait is **sealed**: only `L1ToL2` and `L2ToL1` can implement it.
//! No external code can add new directions.
//!
//! Introduced in refactor step 3.1 (PLAN.md §Phase 3). Methods are initially
//! stubs (`todo!()`) and are filled in by steps 3.4-3.7 as the shared engine
//! is built.

use alloy_primitives::Address;

use super::sim_client::ChainTarget;

// ---------------------------------------------------------------------------
// Sealed trait plumbing
// ---------------------------------------------------------------------------

mod sealed {
    pub trait Sealed {}
}

// ---------------------------------------------------------------------------
// Direction trait
// ---------------------------------------------------------------------------

/// Direction-specific hooks for the composer's cross-chain detection engine.
///
/// Implementations live on zero-sized marker types ([`L1ToL2`], [`L2ToL1`])
/// and are used via generics: `discover_until_stable::<D: Direction>(...)`.
///
/// **Design rule** (PLAN.md §4b): traits only at real IO seams or ≥2
/// real symmetric implementations. `Direction` qualifies because L1→L2
/// and L2→L1 share 90%+ of the detection logic but differ in the
/// direction-specific hooks defined here.
pub trait Direction: sealed::Sealed + Send + Sync + 'static {
    /// Human-readable name for log messages.
    fn name() -> &'static str;

    /// The chain where simulation runs (the delivery target).
    fn simulation_chain() -> ChainTarget;

    /// The chain where the user's original tx lives (the source).
    fn source_chain() -> ChainTarget;

    /// The CCM address on the target chain (where `executeCrossChainCall`
    /// is dispatched).
    fn ccm_on_target(&self) -> Address;

    /// The CCM address on the source chain (for trace walking).
    fn ccm_on_source(&self) -> Address;
}

// ---------------------------------------------------------------------------
// L1ToL2 — deposits and L1→L2 cross-chain calls
// ---------------------------------------------------------------------------

/// Marker type for the L1→L2 direction.
///
/// The user sends a tx on L1 that creates cross-chain entries targeting L2.
/// The composer intercepts, simulates delivery on L2, builds entries, then
/// forwards the user's L1 tx.
pub struct L1ToL2 {
    /// CCM address on L2 (delivery target).
    pub l2_ccm: Address,
    /// CCM address on L1 (source for trace walking).
    pub l1_ccm: Address,
}

impl sealed::Sealed for L1ToL2 {}

impl Direction for L1ToL2 {
    fn name() -> &'static str {
        "L1→L2"
    }

    fn simulation_chain() -> ChainTarget {
        ChainTarget::L2
    }

    fn source_chain() -> ChainTarget {
        ChainTarget::L1
    }

    fn ccm_on_target(&self) -> Address {
        self.l2_ccm
    }

    fn ccm_on_source(&self) -> Address {
        self.l1_ccm
    }
}

// ---------------------------------------------------------------------------
// L2ToL1 — withdrawals and L2→L1 cross-chain calls
// ---------------------------------------------------------------------------

/// Marker type for the L2→L1 direction.
///
/// The user sends a tx on L2 that creates cross-chain entries targeting L1.
/// The composer intercepts, simulates delivery on L1, builds entries, then
/// holds the user's L2 tx until the block is confirmed.
pub struct L2ToL1 {
    /// CCM address on L1 (delivery target).
    pub l1_ccm: Address,
    /// CCM address on L2 (source for trace walking).
    pub l2_ccm: Address,
}

impl sealed::Sealed for L2ToL1 {}

impl Direction for L2ToL1 {
    fn name() -> &'static str {
        "L2→L1"
    }

    fn simulation_chain() -> ChainTarget {
        ChainTarget::L1
    }

    fn source_chain() -> ChainTarget {
        ChainTarget::L2
    }

    fn ccm_on_target(&self) -> Address {
        self.l1_ccm
    }

    fn ccm_on_source(&self) -> Address {
        self.l2_ccm
    }
}
