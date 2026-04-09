//! Shared types for cross-chain composer RPC modules.
//!
//! Types that are used by both `l1_to_l2` and `l2_to_l1` directions live here
//! to eliminate duplication. These are protocol-generic — no direction-specific
//! logic.
//!
//! Introduced in refactor step 3.2 (PLAN.md §Phase 3).

use alloy_primitives::{Address, U256};

use super::common::{
    detect_cross_chain_proxy_on_l2, encode_authorized_proxies_calldata, eth_call_view,
    parse_address_from_abi_return,
};
use super::trace;
use crate::cross_chain::{ParentLink, ScopePath};

// ---------------------------------------------------------------------------
// Proxy lookup implementations (shared by both directions)
// ---------------------------------------------------------------------------

/// Queries `authorizedProxies(address)` on Rollups.sol (L1) to resolve proxy
/// identity for the generic `trace::walk_trace_tree`.
#[allow(dead_code, reason = "scaffold — callers will migrate from direction-local copies")]
pub(crate) struct L1ProxyLookup<'a> {
    pub client: &'a reqwest::Client,
    pub rpc_url: &'a str,
    pub rollups_address: Address,
}

impl trace::ProxyLookup for L1ProxyLookup<'_> {
    fn lookup_proxy(
        &self,
        address: Address,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<trace::ProxyInfo>> + Send + '_>,
    > {
        Box::pin(async move {
            let calldata = encode_authorized_proxies_calldata(address);

            let hex_data = eth_call_view(
                self.client,
                self.rpc_url,
                self.rollups_address,
                &calldata,
            )
            .await?;

            // First 32 bytes = originalAddress
            let addr = parse_address_from_abi_return(&hex_data)?;

            // Second 32 bytes = originalRollupId (uint256, last 8 bytes as u64)
            let hex_clean = hex_data.strip_prefix("0x").unwrap_or(&hex_data);
            if hex_clean.len() < 128 {
                return None;
            }
            let rid_bytes = hex::decode(&hex_clean[64..128]).ok()?;
            if rid_bytes.len() < 32 {
                return None;
            }
            let mut rid: u64 = 0;
            let start = rid_bytes.len().saturating_sub(8);
            for b in &rid_bytes[start..] {
                rid = (rid << 8) | (*b as u64);
            }

            Some(trace::ProxyInfo {
                original_address: addr,
                original_rollup_id: rid,
            })
        })
    }
}

/// Queries `authorizedProxies(address)` on the L2 CCM to resolve proxy
/// identity for the generic `trace::walk_trace_tree`.
#[allow(dead_code, reason = "scaffold — callers will migrate from direction-local copies")]
pub(crate) struct L2ProxyLookup<'a> {
    pub client: &'a reqwest::Client,
    pub rpc_url: &'a str,
    pub ccm_address: Address,
}

impl trace::ProxyLookup for L2ProxyLookup<'_> {
    fn lookup_proxy(
        &self,
        address: Address,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<trace::ProxyInfo>> + Send + '_>,
    > {
        Box::pin(async move {
            let result = detect_cross_chain_proxy_on_l2(
                self.client,
                self.rpc_url,
                address,
                self.ccm_address,
            )
            .await;
            result.map(|(addr, rid)| trace::ProxyInfo {
                original_address: addr,
                original_rollup_id: rid,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Shared discovery types (used by the direction-parameterized engine)
// ---------------------------------------------------------------------------

/// A cross-chain call discovered by tracing on the source chain.
///
/// Unified representation replacing direction-specific types
/// (`DetectedInternalCall`, `DetectedL2InternalCall`). Both L1→L2 and
/// L2→L1 produce instances of this type from `trace::walk_trace_tree`.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "scaffold for 3.4-3.7 migration — callers still use direction-specific types")]
pub(crate) struct DiscoveredCall {
    /// Destination address on the target chain.
    pub destination: Address,
    /// Calldata to execute on the destination.
    pub calldata: Vec<u8>,
    /// ETH value sent with the call.
    pub value: U256,
    /// The address that initiated the call on the source chain.
    pub source_address: Address,
    /// Index of the parent call that triggered this one.
    pub parent_call_index: ParentLink,
    /// Depth in the source chain trace (root = 0).
    pub trace_depth: usize,
    /// Iterative discovery iteration in which this call was first detected.
    pub discovery_iteration: usize,
    /// Whether this call is inside a reverted frame.
    pub in_reverted_frame: bool,
    /// Return data from simulating delivery on the target chain.
    /// Empty when simulation was not performed or the call returns void.
    pub delivery_return_data: Vec<u8>,
    /// Whether the delivery simulation reverted.
    pub delivery_failed: bool,
    /// Rollup ID of the target (0 = L1/mainnet, 1+ = L2 rollups).
    /// Used to distinguish forward vs return calls in nested patterns.
    pub target_rollup_id: u64,
}

/// A return call edge discovered during target-chain simulation.
///
/// Represents an L1→L2 or L2→L1 return call that closes a previous
/// forward call via scope navigation (`callReturn{scope=[...]}`).
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "scaffold for 3.4-3.7 migration")]
pub(crate) struct ReturnEdge {
    /// Destination address (on source chain, returning to caller).
    pub destination: Address,
    /// Calldata for the return delivery.
    pub data: Vec<u8>,
    /// ETH value forwarded.
    pub value: U256,
    /// Source address on the target chain.
    pub source_address: Address,
    /// Index of the forward call this return closes.
    pub parent_call_index: ParentLink,
    /// Return data from the L2/L1 scope resolution.
    pub return_data: Vec<u8>,
    /// Whether the scope resolution failed.
    pub delivery_failed: bool,
    /// Scope path for this return call's entries.
    pub scope: ScopePath,
}

/// Whether the discovered call set should be promoted to continuation
/// (multi-entry) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "scaffold for 3.4-3.7 migration")]
pub(crate) enum PromotionDecision {
    /// Single call, single entry pair (CALL + RESULT).
    KeepSimple,
    /// Promote to continuation entries even if there's only one forward call.
    /// Triggered when a terminal return call is present (invariant #21).
    PromoteToContinuation,
}

/// Result of the fixed-point discovery loop (`discover_until_stable`).
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "scaffold for 3.4-3.7 migration")]
pub(crate) struct DiscoveredSet {
    /// Forward calls discovered on the source chain.
    pub calls: Vec<DiscoveredCall>,
    /// Return edges discovered during target-chain simulation.
    pub returns: Vec<ReturnEdge>,
    /// Whether to promote to continuation mode.
    pub promotion: PromotionDecision,
}

// ---------------------------------------------------------------------------
// Parent link rebasing (shared helper — refactor 3.3)
// ---------------------------------------------------------------------------

/// Rebase parent_call_index values by a global offset.
///
/// When calls are discovered in nested slices (e.g., per-iteration in the
/// discovery loop), their `parent_call_index` is relative to the slice.
/// This helper shifts `Child(i)` values by `offset` to produce absolute
/// indices in the combined vector.
///
/// `Root` links are left unchanged.
#[allow(dead_code, reason = "scaffold for 3.4 migration")]
pub(crate) fn rebase_parent_links(calls: &mut [DiscoveredCall], offset: usize) {
    for call in calls {
        if let Some(idx) = call.parent_call_index.child_index_mut() {
            *idx = crate::cross_chain::AbsoluteCallIndex::from_usize_at_boundary(
                idx.as_usize() + offset,
            );
        }
    }
}

/// Rebase parent_call_index values on return edges.
#[allow(dead_code, reason = "scaffold for 3.4 migration")]
pub(crate) fn rebase_return_parent_links(returns: &mut [ReturnEdge], offset: usize) {
    for ret in returns {
        if let Some(idx) = ret.parent_call_index.child_index_mut() {
            *idx = crate::cross_chain::AbsoluteCallIndex::from_usize_at_boundary(
                idx.as_usize() + offset,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: convert trace::DetectedCall → DiscoveredCall
// ---------------------------------------------------------------------------

/// Convert a `trace::DetectedCall` (from the generic walker) into a
/// `DiscoveredCall` with direction-neutral defaults.
///
/// `target_rollup_id` must be supplied by the caller since the generic
/// trace walker doesn't track it — the direction module resolves it
/// from the proxy info.
#[allow(dead_code, reason = "scaffold for 3.4 migration")]
pub(crate) fn from_trace_detected(
    call: &trace::DetectedCall,
    target_rollup_id: u64,
    discovery_iteration: usize,
) -> DiscoveredCall {
    DiscoveredCall {
        destination: call.destination,
        calldata: call.calldata.clone(),
        value: call.value,
        source_address: call.source_address,
        parent_call_index: ParentLink::Root,
        trace_depth: call.trace_depth,
        discovery_iteration,
        in_reverted_frame: call.in_reverted_frame,
        delivery_return_data: Vec::new(),
        delivery_failed: false,
        target_rollup_id,
    }
}
