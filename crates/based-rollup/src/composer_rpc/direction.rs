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

use alloy_primitives::{Address, U256};
use serde_json::Value;

use super::model::DiscoveredCall;
use super::sim_client::ChainTarget;
use crate::cross_chain::ScopePath;

// ---------------------------------------------------------------------------
// Sealed trait plumbing
// ---------------------------------------------------------------------------

#[allow(dead_code, reason = "scaffold — callers migrate in 3.4b+")]
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
#[allow(dead_code, reason = "scaffold — callers migrate in 3.4b+")]
pub(crate) trait Direction: sealed::Sealed + Send + Sync + 'static {
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

    /// Manager addresses used for trace walking on the source chain.
    ///
    /// L1→L2: `[rollups_address]` (Rollups.sol is the L1 manager)
    /// L2→L1: `[ccm_address]` (CrossChainManagerL2 is the L2 manager)
    fn source_manager_addresses(&self) -> Vec<Address>;

    /// Default target rollup ID for calls discovered on the source chain.
    ///
    /// L1→L2: 0 (target resolved later from proxy identity)
    /// L2→L1: 0 (L1 = rollup ID 0)
    fn default_target_rollup_id(&self) -> u64 {
        0
    }

    /// Pre-retrace enrichment: populate delivery return data on calls
    /// that don't have it yet.
    ///
    /// Called before `build_retrace_bundle` each iteration. The delivery
    /// return data is needed for correct entry construction — without it,
    /// the retrace won't discover calls hidden behind ABI decode failures.
    ///
    /// L1→L2: no-op (delivery data comes from L2 simulation post-convergence)
    /// L2→L1: simulates each call's delivery on L1 to get return data
    fn enrich_calls_before_retrace(
        &self,
        _calls: &mut [DiscoveredCall],
        _user_tx: &UserTxContext,
        _iteration: usize,
    ) -> impl std::future::Future<Output = Vec<super::model::ReturnEdge>> + Send {
        async { vec![] }
    }

    /// Build a retrace bundle for the iterative discovery loop.
    ///
    /// Given the current set of discovered calls, construct the
    /// `debug_traceCallMany` bundle that loads entries and replays
    /// the user tx so new calls become visible.
    ///
    /// Returns `None` on failure (caller should break the loop).
    ///
    /// L1→L2: `[postBatch(entries), userTx]` on L1
    /// L2→L1: `[loadExecutionTable(entries), userTx]` on L2
    fn build_retrace_bundle(
        &self,
        calls: &[DiscoveredCall],
        user_tx: &UserTxContext,
        iteration: usize,
    ) -> impl std::future::Future<Output = Option<Value>> + Send;
}

/// User transaction context needed for retrace bundle construction.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "scaffold — used by Direction::build_retrace_bundle")]
pub(crate) struct UserTxContext {
    /// Sender address.
    pub from: String,
    /// Target address.
    pub to: String,
    /// Calldata (hex, 0x-prefixed).
    pub data: String,
    /// Raw RLP-encoded transaction bytes (for L2TX action hash in simulate_l1_delivery).
    pub raw_tx_bytes: Vec<u8>,
    /// Value (hex, 0x-prefixed).
    pub value: String,
}

// ---------------------------------------------------------------------------
// L1ToL2 — deposits and L1→L2 cross-chain calls
// ---------------------------------------------------------------------------

/// Marker type for the L1→L2 direction.
///
/// The user sends a tx on L1 that creates cross-chain entries targeting L2.
/// The composer intercepts, simulates delivery on L2, builds entries, then
/// forwards the user's L1 tx.
#[allow(dead_code, reason = "scaffold — instantiated when callers migrate")]
pub(crate) struct L1ToL2 {
    /// CCM address on L2 (delivery target).
    pub l2_ccm: Address,
    /// CCM address on L1 (Rollups.sol, source for trace walking).
    pub l1_ccm: Address,
    /// Rollup ID for entry construction.
    pub rollup_id: u64,
    /// Builder private key for signing postBatch proofs.
    pub builder_key: alloy_signer_local::PrivateKeySigner,
    /// HTTP client for L1 view calls during retrace bundle construction.
    pub client: reqwest::Client,
    /// L1 RPC URL for view calls.
    pub l1_rpc_url: String,
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

    fn source_manager_addresses(&self) -> Vec<Address> {
        vec![self.l1_ccm] // Rollups.sol acts as the L1 manager
    }

    async fn build_retrace_bundle(
        &self,
        calls: &[DiscoveredCall],
        user_tx: &UserTxContext,
        _iteration: usize,
    ) -> Option<Value> {
        use super::common::{get_l1_block_context, get_rollup_state_root, get_verification_key};
        use crate::cross_chain;

        let rollup_id = self.rollup_id;

        // Build L1 entries from discovered calls.
        let l1_detected: Vec<crate::table_builder::L1DetectedCall> = calls
            .iter()
            .map(|c| crate::table_builder::L1DetectedCall {
                destination: c.destination,
                data: c.calldata.clone(),
                value: c.value,
                source_address: c.source_address,
                l2_return_data: c.delivery_return_data.clone(),
                call_success: !c.delivery_failed,
                parent_call_index: c.parent_call_index,
                target_rollup_id: if c.parent_call_index.is_child() && c.target_rollup_id == 0 {
                    Some(0)
                } else {
                    None
                },
                scope: if c.trace_depth <= 1 {
                    ScopePath::root()
                } else {
                    ScopePath::from_parts(vec![U256::ZERO; c.trace_depth])
                },
                discovery_iteration: c.discovery_iteration,
                l1_trace_depth: c.trace_depth,
                in_reverted_frame: c.in_reverted_frame,
            })
            .collect();

        let analyzed =
            crate::table_builder::analyze_continuation_calls(&l1_detected, rollup_id);

        let mut entries = if analyzed.is_empty() {
            let l2_pairs: Vec<_> = l1_detected
                .iter()
                .flat_map(|c| {
                    let (call_entry, result_entry) =
                        cross_chain::build_cross_chain_call_entries(
                            cross_chain::RollupId::new(alloy_primitives::U256::from(rollup_id)),
                            c.destination,
                            c.data.clone(),
                            c.value,
                            c.source_address,
                            cross_chain::RollupId::MAINNET,
                            c.call_success,
                            c.l2_return_data.clone(),
                        );
                    vec![call_entry, result_entry]
                })
                .collect();
            cross_chain::convert_pairs_to_l1_entries(&l2_pairs)
        } else {
            let cont = crate::table_builder::build_continuation_entries(
                &analyzed,
                cross_chain::RollupId::new(alloy_primitives::U256::from(rollup_id)),
            );
            cont.l1_entries
        };

        if entries.is_empty() {
            return None;
        }

        // Fix placeholder state deltas with real on-chain root.
        let on_chain_root = get_rollup_state_root(
            &self.client,
            &self.l1_rpc_url,
            self.l1_ccm,
            rollup_id,
        )
        .await
        .unwrap_or(alloy_primitives::B256::ZERO);
        for e in &mut entries {
            for d in &mut e.state_deltas {
                d.current_state = on_chain_root;
                d.new_state = on_chain_root;
            }
        }

        // Get L1 block context + verification key for proof.
        let (block_number, block_hash, _) =
            get_l1_block_context(&self.client, &self.l1_rpc_url).await.ok()?;
        let vk =
            get_verification_key(&self.client, &self.l1_rpc_url, self.l1_ccm, rollup_id)
                .await
                .ok()?;

        // Sign ECDSA proof for postBatch.
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let call_data_bytes = alloy_primitives::Bytes::new();
        let entry_hashes = cross_chain::compute_entry_hashes(&entries, vk);
        let public_inputs_hash = cross_chain::compute_public_inputs_hash(
            &entry_hashes,
            &call_data_bytes,
            block_hash,
            timestamp,
        );

        use alloy_signer::SignerSync;
        let sig = self.builder_key.sign_hash_sync(&public_inputs_hash).ok()?;
        let sig_bytes = sig.as_bytes();
        let mut proof_bytes = sig_bytes.to_vec();
        if proof_bytes.len() == 65 && proof_bytes[64] < 27 {
            proof_bytes[64] += 27;
        }
        let proof = alloy_primitives::Bytes::from(proof_bytes);

        // Encode postBatch calldata.
        let post_batch_calldata =
            cross_chain::encode_post_batch_calldata(&entries, call_data_bytes, proof);
        let post_batch_hex = format!("0x{}", hex::encode(post_batch_calldata.as_ref()));
        let builder_addr = format!("{}", self.builder_key.address());
        let rollups_hex = format!("{}", self.l1_ccm);
        let next_block = format!("{:#x}", block_number + 1);

        Some(serde_json::json!({
            "transactions": [
                {
                    "from": builder_addr,
                    "to": rollups_hex,
                    "data": post_batch_hex,
                    "gas": "0x1c9c380"
                },
                {
                    "from": user_tx.from,
                    "to": user_tx.to,
                    "data": user_tx.data,
                    "value": user_tx.value,
                    "gas": "0x2faf080"
                }
            ],
            "blockOverride": {
                "number": next_block,
                "time": format!("{:#x}", timestamp)
            }
        }))
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
#[allow(dead_code, reason = "scaffold — instantiated when callers migrate")]
pub(crate) struct L2ToL1 {
    /// CCM address on L1 (delivery target / Rollups.sol).
    pub l1_ccm: Address,
    /// CCM address on L2 (source for trace walking).
    pub l2_ccm: Address,
    /// Builder address (for loadExecutionTable sender + proof signing).
    pub builder_address: Address,
    /// Builder private key hex (for postBatch proof signing in simulate_l1_delivery).
    pub builder_private_key: Option<String>,
    /// Rollup ID for entry construction.
    pub rollup_id: u64,
    /// HTTP client for L1/L2 RPC calls.
    pub client: reqwest::Client,
    /// L1 RPC URL for delivery simulation.
    pub l1_rpc_url: String,
    /// L2 RPC URL (upstream) for L2 enrichment.
    pub l2_rpc_url: String,
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

    fn source_manager_addresses(&self) -> Vec<Address> {
        vec![self.l2_ccm] // CrossChainManagerL2 is the L2 manager
    }

    async fn enrich_calls_before_retrace(
        &self,
        calls: &mut [DiscoveredCall],
        user_tx: &UserTxContext,
        iteration: usize,
    ) -> Vec<super::model::ReturnEdge> {
        use super::model::ReturnEdge;
        let mut all_return_calls: Vec<ReturnEdge> = Vec::new();
        for call in calls.iter_mut() {
            if !call.delivery_return_data.is_empty() || call.delivery_failed {
                continue; // already enriched from a previous iteration
            }

            let scope = vec![alloy_primitives::U256::ZERO; call.trace_depth.max(1)];

            if let Some((ret_data, failed, return_calls)) =
                super::l2_to_l1::simulate_l1_delivery(
                    &self.client,
                    &self.l1_rpc_url,
                    &self.l2_rpc_url,
                    self.l2_ccm,
                    self.l1_ccm,  // rollups_address
                    self.builder_address,
                    self.builder_private_key.as_deref(),
                    self.rollup_id,
                    call.source_address,
                    call.destination,
                    &call.calldata,
                    call.value,
                    &user_tx.raw_tx_bytes,
                    &scope,
                    &call.delivery_return_data,
                    call.delivery_failed,
                )
                .await
            {
                call.delivery_return_data = ret_data;
                call.delivery_failed = failed;
                all_return_calls.extend(return_calls);
            }

            tracing::info!(
                target: "based_rollup::discover",
                iteration,
                destination = %call.destination,
                return_data_len = call.delivery_return_data.len(),
                delivery_failed = call.delivery_failed,
                "enriched call via simulate_l1_delivery"
            );
        }
        all_return_calls
    }

    async fn build_retrace_bundle(
        &self,
        calls: &[DiscoveredCall],
        user_tx: &UserTxContext,
        _iteration: usize,
    ) -> Option<Value> {
        // Build L2 table entries from discovered calls.
        let mut l2_table_entries = Vec::new();
        for call in calls {
            let call_entries = crate::cross_chain::build_l2_to_l1_call_entries(
                call.destination,
                call.calldata.clone(),
                call.value,
                call.source_address,
                self.rollup_id,
                vec![], // tx_bytes placeholder (irrelevant for table loading)
                call.delivery_return_data.clone(),
                call.delivery_failed,
                vec![], // l1_delivery_scope (irrelevant for table loading)
                crate::cross_chain::TxOutcome::Success,
            );
            l2_table_entries.extend(call_entries.l2_table_entries);
        }

        // Encode loadExecutionTable calldata.
        let load_table_calldata =
            crate::cross_chain::encode_load_execution_table_calldata(&l2_table_entries);
        let load_table_hex = format!("0x{}", hex::encode(load_table_calldata.as_ref()));

        // Build the bundle: [loadExecutionTable, userTx] in one bundle
        // so tx1's state is visible to tx2.
        Some(serde_json::json!({
            "transactions": [
                {
                    "from": format!("{}", self.builder_address),
                    "to": format!("{}", self.l2_ccm),
                    "data": load_table_hex,
                    "gas": "0x1c9c380"
                },
                {
                    "from": user_tx.from,
                    "to": user_tx.to,
                    "data": user_tx.data,
                    "value": user_tx.value,
                    "gas": "0x2faf080"
                }
            ]
        }))
    }
}
