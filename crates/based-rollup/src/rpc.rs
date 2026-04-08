//! Custom RPC namespace `syncrollups_*` for synchronous composability.
//!
//! Provides methods for transaction simulation, state root queries, and
//! action hash computation — used by the execution planner and external
//! tooling (UI dashboards, RPC proxies).

use crate::config::RollupConfig;
use crate::cross_chain::{
    CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, ICrossChainManagerL2,
};
use crate::evm_config::RollupEvmConfig;
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, Bytes, I256, U256, keccak256};
use alloy_sol_types::SolType;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use reth_provider::{BlockNumReader, HeaderProvider, StateProviderFactory};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ──────────────────────────────────────────────
//  Serde helpers
// ──────────────────────────────────────────────

/// Serde default for `bool` fields that should default to `true`.
fn default_true() -> bool {
    true
}

// ──────────────────────────────────────────────
//  RPC types
// ──────────────────────────────────────────────

/// Result of simulating a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulationResult {
    /// Whether the transaction executed successfully.
    pub success: bool,
    /// Gas consumed by the transaction.
    pub gas_used: u64,
    /// Return data from the transaction (or revert reason).
    pub return_data: Bytes,
    /// State root before execution.
    pub pre_state_root: B256,
    /// State root after execution.
    pub post_state_root: B256,
    /// Computed action hash for this transaction.
    pub action_hash: B256,
    /// Pre-built execution entry ready for L1 submission.
    pub execution_entry: SerializableExecutionEntry,
}

/// JSON-serializable execution entry (mirrors CrossChainExecutionEntry).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableExecutionEntry {
    pub state_deltas: Vec<SerializableStateDelta>,
    pub action_hash: B256,
    pub next_action: SerializableAction,
}

/// JSON-serializable state delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableStateDelta {
    pub rollup_id: U256,
    pub current_state: B256,
    pub new_state: B256,
    pub ether_delta: I256,
}

/// JSON-serializable action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableAction {
    pub action_type: String,
    pub rollup_id: U256,
    pub destination: Address,
    pub value: U256,
    pub data: Bytes,
    pub failed: bool,
    pub source_address: Address,
    pub source_rollup: U256,
    pub scope: Vec<U256>,
}

/// Parameters for initiating a cross-chain call via L1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrossChainCallParams {
    /// Target contract address on L2.
    pub destination: Address,
    /// Calldata to execute on the destination contract.
    pub data: Bytes,
    /// ETH value to send with the cross-chain call (e.g., bridgeEther deposits).
    /// Must match `msg.value` in the L1 proxy call for action hash to be correct.
    #[serde(default)]
    pub value: U256,
    /// The address initiating the cross-chain call (set as sourceAddress).
    pub source_address: Address,
    /// The rollup originating the call (0 for L1-originated).
    pub source_rollup: U256,
    /// Effective gas price of the user's L1 tx (for ordering chained state deltas).
    /// The L1 miner orders txs by gas price descending, so we must match that order.
    #[serde(default)]
    pub gas_price: u128,
    /// Raw signed L1 transaction to forward after `postBatch`.
    /// Previously sent via a separate `queueL1ForwardTx` call; now bundled atomically.
    #[serde(default)]
    pub raw_l1_tx: Bytes,
    /// Pre-computed L2 return data from the composer RPC's chained simulation.
    /// When present, skips independent simulate_call and uses this directly.
    /// Required for identical-call patterns where each call's return data
    /// depends on state changes from previous calls.
    #[serde(default)]
    pub l2_return_data: Option<Bytes>,
    /// Whether the L2 call succeeded (from composer RPC chained simulation).
    #[serde(default)]
    pub l2_call_success: Option<bool>,
}

/// Parameters for initiating an L2→L1 cross-chain call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2CrossChainCallParams {
    /// Target contract address on L1 (originalAddress from proxy).
    pub destination: Address,
    /// Calldata to execute on the L1 target.
    #[serde(default)]
    pub data: Bytes,
    /// ETH value for the cross-chain call.
    #[serde(default)]
    pub value: U256,
    /// Address that initiated the call on L2 (tx sender, NOT bridge).
    pub source_address: Address,
    /// Return data from L1 delivery simulation (empty for simple calls/EOA targets).
    #[serde(default)]
    pub delivery_return_data: Bytes,
    /// Whether the L1 delivery simulation reverted.
    #[serde(default)]
    pub delivery_failed: bool,
    /// Held raw L2 transaction for hold-then-forward pattern.
    /// When non-empty, the proxy held this tx instead of forwarding to upstream;
    /// the driver injects it into the pool after loading entries.
    #[serde(default)]
    pub raw_l2_tx: Bytes,
    /// Pre-computed L2 return data from the composer RPC's chained simulation.
    /// When present, overrides independent L2 simulation for the return call.
    /// Required for identical-call patterns where each call's return data
    /// depends on state changes from previous calls.
    #[serde(default)]
    pub l2_return_data: Option<Bytes>,
    /// Whether the L2 call succeeded (from composer RPC chained simulation).
    /// When present alongside `l2_return_data`, skips independent L2 simulation.
    #[serde(default)]
    pub l2_call_success: Option<bool>,
    /// Scope array for the L1 delivery CALL action (determines newScope nesting
    /// depth in executeL2TX). Computed from trace_depth of the proxy call in the
    /// L2 trace: scope = vec![0; trace_depth]. Empty for direct proxy calls.
    #[serde(default)]
    pub l1_delivery_scope: Vec<U256>,
    /// Whether the L2 tx reverts AFTER making cross-chain calls.
    /// When `Revert`, L1 entries include REVERT/REVERT_CONTINUE to undo L1 state changes.
    #[serde(default)]
    pub tx_reverts: crate::cross_chain::TxOutcome,
}

/// A queued cross-chain call with its entry pair, gas price, and raw L1 tx.
/// Replaces the previous dual-queue design (separate entry queue + L1 tx queue)
/// with a single unified struct that keeps entries and their L1 tx associated.
///
/// For simple deposits, only `call_entry` + `result_entry` are populated.
/// For continuation patterns (multi-call continuations), `extra_l2_entries` and `l1_entries`
/// are also populated by the table builder.
#[derive(Debug, Clone)]
pub struct QueuedCrossChainCall {
    /// The CALL execution entry (L2 table: consumed by executeIncomingCrossChainCall).
    pub call_entry: CrossChainExecutionEntry,
    /// The RESULT execution entry (L2 table: consumed after the call returns).
    pub result_entry: CrossChainExecutionEntry,
    /// Effective gas price of the user's L1 tx (for ordering).
    pub effective_gas_price: u128,
    /// Raw signed L1 transaction to forward after `postBatch`.
    pub raw_l1_tx: Bytes,
    /// Additional L2 table entries for continuation patterns (multi-call continuations).
    /// Loaded via `loadExecutionTable` alongside the primary CALL+RESULT pair.
    /// Empty for simple deposits.
    pub extra_l2_entries: Vec<CrossChainExecutionEntry>,
    /// Pre-built L1 entries for continuation patterns (multi-call continuations).
    /// When non-empty, these are used AS-IS in `flush_to_l1` instead of
    /// converting the CALL+RESULT pair via `convert_pairs_to_l1_entries`.
    /// Empty for simple deposits (legacy path applies).
    pub l1_entries: Vec<CrossChainExecutionEntry>,
    /// Whether the L2 tx reverts after cross-chain calls (atomicity revert).
    pub tx_reverts: crate::cross_chain::TxOutcome,
    /// L1 entries are independent (not chained state deltas). For L1→L2 partial
    /// revert: the reverted call's state is rolled back by try/catch on L1.
    pub l1_independent_entries: crate::cross_chain::EntryGroupMode,
}

/// A queued L2→L1 call with L2 table entries and L1 deferred entries.
/// The driver drains these into the next block alongside any L1→L2 entries
/// (unified intermediate roots handle both types in the same block).
#[derive(Debug, Clone)]
pub struct QueuedL2ToL1Call {
    /// L2 table entries (loaded via loadExecutionTable).
    pub l2_table_entries: Vec<crate::cross_chain::CrossChainExecutionEntry>,
    /// L1 deferred entries (posted via postBatch, consumed by executeL2TX trigger).
    pub l1_deferred_entries: Vec<crate::cross_chain::CrossChainExecutionEntry>,
    /// User address (L2→L1 call initiator).
    pub user: Address,
    /// ETH amount in wei.
    pub amount: U256,
    /// Held raw L2 transaction for hold-then-forward pattern.
    /// When non-empty, the proxy held this tx instead of forwarding to upstream;
    /// the driver injects it into the pool after loading entries.
    pub raw_l2_tx: Bytes,
    /// RLP-encoded L2 transaction for the L2TX trigger on L1.
    /// The driver calls `Rollups.executeL2TX(rollupId, rlpEncodedTx)` to trigger
    /// consumption of the L1 deferred entries.
    pub rlp_encoded_tx: Vec<u8>,
    /// Number of `executeL2TX` calls needed.
    /// Simple withdrawals = 1. Multi-call patterns with N root L2→L1 calls = N.
    pub trigger_count: usize,
    /// Whether the L2 tx reverts after cross-chain calls (atomicity revert).
    pub tx_reverts: crate::cross_chain::TxOutcome,
}

/// Result of simulating a contract call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulateCallResult {
    /// Whether the call succeeded.
    pub success: bool,
    /// Return data (or revert data).
    pub return_data: Bytes,
}

/// Parameters for building a multi-call execution table (multi-call continuations).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildExecutionTableParams {
    /// Detected L1→L2 cross-chain calls (in execution order).
    pub calls: Vec<BuildExecutionTableCall>,
    /// Effective gas price of the user's L1 tx (for ordering).
    #[serde(default)]
    pub gas_price: u128,
    /// Raw signed L1 transaction to forward after `postBatch`.
    #[serde(default)]
    pub raw_l1_tx: Bytes,
}

/// A single detected call for the execution table builder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildExecutionTableCall {
    /// Target address on L2.
    pub destination: Address,
    /// Calldata (e.g., receiveTokens calldata from proxy detection).
    pub data: Bytes,
    /// ETH value for the cross-chain call.
    #[serde(default)]
    pub value: U256,
    /// Address that initiated the call on L1.
    pub source_address: Address,
    /// Return data from simulating this L1->L2 call on L2.
    /// When non-empty, the RESULT action hash includes this data
    /// (contracts/sync-rollups-protocol/docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md §C.2).
    #[serde(default)]
    pub l2_return_data: Bytes,
    /// Whether the L2 call succeeded. Defaults to `true` when not provided.
    #[serde(default = "default_true")]
    pub call_success: bool,
    /// Index of the parent call whose L2 execution triggers this child.
    /// `Root` for top-level L1→L2 calls; `Child(i)` for L2→L1 child calls
    /// discovered inside call[i]'s L2 simulation (the L1→L2→L1 pattern).
    #[serde(default)]
    pub parent_call_index: crate::cross_chain::ParentLink,
    /// Target rollup ID. 0 = L1 (mainnet). For L2→L1 children, this is 0
    /// (they target L1). Not set for normal L1→L2 calls (defaults to None,
    /// meaning the target is our L2 rollup).
    #[serde(default)]
    pub target_rollup_id: Option<u64>,
    /// Accumulated scope for this call.
    #[serde(default)]
    pub scope: Vec<U256>,
    /// Iterative discovery iteration when this call was first detected.
    #[serde(default)]
    pub discovery_iteration: usize,
    /// Original L1 trace depth from walk_trace_tree.
    #[serde(default)]
    pub l1_trace_depth: usize,
    /// Whether this call is inside a reverted frame on L1 (try/catch that reverts).
    /// Used for partial revert patterns (revertContinue L1→L2): the reverted call
    /// needs REVERT/REVERT_CONTINUE on L2, while calls outside the reverted frame
    /// continue normally.
    #[serde(default)]
    pub in_reverted_frame: bool,
}

/// Result of building a multi-call execution table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildExecutionTableResult {
    /// Number of L2 table entries generated.
    pub l2_entry_count: usize,
    /// Number of L1 deferred entries generated.
    pub l1_entry_count: usize,
    /// Action hash of the first CALL (tracking ID).
    pub call_id: B256,
}

/// Parameters for building an L2→L1 multi-call execution table (reverse multi-call continuations).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildL2ToL1ExecutionTableParams {
    /// L2→L1 calls detected from the L2 tx trace (in execution order).
    pub l2_calls: Vec<BuildL2ToL1Call>,
    /// L1→L2 return calls discovered from L1 delivery simulation.
    #[serde(default)]
    pub return_calls: Vec<BuildL2ToL1ReturnCall>,
    /// Effective gas price (unused for L2→L1 but kept for API consistency).
    #[serde(default)]
    pub gas_price: u128,
    /// Raw signed L2 transaction (held for driver injection).
    #[serde(default)]
    pub raw_l2_tx: Bytes,
    /// Whether the L2 tx reverts AFTER making cross-chain calls.
    #[serde(default)]
    pub tx_reverts: crate::cross_chain::TxOutcome,
}

/// A single L2→L1 call for the reverse multi-call continuation execution table builder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildL2ToL1Call {
    /// Target address on L1 (proxy's originalAddress).
    pub destination: Address,
    /// Calldata to execute on the L1 target.
    pub data: Bytes,
    /// ETH value for the cross-chain call.
    #[serde(default)]
    pub value: U256,
    /// Address that initiated the call on L2.
    pub source_address: Address,
    /// Return data from the L1 delivery simulation for this call.
    /// When non-empty, the L1 RESULT entry hash includes this data.
    #[serde(default)]
    pub delivery_return_data: Bytes,
    /// Whether the L1 delivery simulation reverted.
    #[serde(default)]
    pub delivery_failed: bool,
    /// Accumulated scope for L1 delivery.
    #[serde(default)]
    pub scope: Vec<U256>,
    /// Whether this call is inside a reverted frame on L2 (try/catch that reverts).
    /// Used for partial revert patterns where some calls need REVERT/REVERT_CONTINUE.
    #[serde(default)]
    pub in_reverted_frame: bool,
}

/// A return call (L1→L2) for the reverse multi-call continuation execution table builder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildL2ToL1ReturnCall {
    /// Target address (L1 contract receiving the return call, e.g., Bridge_L1).
    pub destination: Address,
    /// Calldata (e.g., receiveTokens).
    pub data: Bytes,
    /// ETH value sent with the call.
    #[serde(default)]
    pub value: U256,
    /// Address that initiated the call on L1 (e.g., Bridge_L2's proxy).
    pub source_address: Address,
    /// Index of the L2→L1 call whose L1 execution produces this return call.
    /// `Root` means assign to the last L2→L1 call (backward-compatible default).
    #[serde(default)]
    pub parent_call_index: crate::cross_chain::ParentLink,
    /// Return data from simulating this call on L2 (for L2 RESULT hash).
    #[serde(default)]
    pub l2_return_data: Option<Bytes>,
    /// Whether the L2 simulation reverted.
    #[serde(default)]
    pub l2_delivery_failed: bool,
    /// Accumulated scope for this return call.
    #[serde(default)]
    pub scope: Vec<U256>,
}

/// Parameters for computing an action hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionParams {
    pub action_type: String,
    pub rollup_id: U256,
    pub destination: Address,
    pub value: U256,
    pub data: Bytes,
    pub failed: bool,
    pub source_address: Address,
    pub source_rollup: U256,
    pub scope: Vec<U256>,
}

// ──────────────────────────────────────────────
//  RPC trait definition
// ──────────────────────────────────────────────

/// The `syncrollups` RPC namespace for synchronous composability.
#[rpc(server, namespace = "syncrollups")]
pub trait SyncRollupsApi {
    /// Simulate a signed transaction and return the execution result with
    /// state deltas and a pre-built execution entry for L1 submission.
    #[method(name = "simulateTransaction")]
    fn simulate_transaction(&self, signed_tx: Bytes) -> RpcResult<SimulationResult>;

    /// Return the current L2 state root (latest canonical block).
    #[method(name = "getStateRoot")]
    fn get_state_root(&self) -> RpcResult<B256>;

    /// Return whether this node is fully synced with L1.
    #[method(name = "isSynced")]
    fn is_synced(&self) -> RpcResult<bool>;

    /// Compute the action hash for the given action parameters.
    /// Must match the Solidity `keccak256(abi.encode(action))` computation.
    #[method(name = "computeActionHash")]
    fn compute_action_hash(&self, action: ActionParams) -> RpcResult<B256>;

    /// Initiate a cross-chain call via L1. Builds two execution entries
    /// (CALL + RESULT) and submits them to the Rollups contract on L1
    /// via `postBatch()`. Returns the L1 transaction hash.
    #[method(name = "initiateCrossChainCall")]
    async fn initiate_cross_chain_call(&self, params: CrossChainCallParams) -> RpcResult<B256>;

    /// Simulate a contract call against current L2 state. Returns (success, returnData).
    /// Used by the L1 proxy to predict cross-chain call results for entry construction.
    #[method(name = "simulateCall")]
    fn simulate_call(&self, destination: Address, data: Bytes) -> RpcResult<SimulateCallResult>;

    /// Queue a raw signed L1 transaction for forwarding by the driver.
    /// The driver forwards these after `postBatch`, ensuring correct ordering.
    /// Returns the transaction hash (decoded from the signed envelope).
    #[method(name = "queueL1ForwardTx")]
    fn queue_l1_forward_tx(&self, raw_tx: Bytes) -> RpcResult<B256>;

    /// Initiate a general L2→L1 cross-chain call. Builds L2 table entries and
    /// L1 deferred entries with delivery calldata and return data. Queues them
    /// for the builder. Returns the L2 CALL action hash.
    #[method(name = "initiateL2CrossChainCall")]
    fn initiate_l2_cross_chain_call(&self, params: L2CrossChainCallParams) -> RpcResult<B256>;

    /// Build a multi-call execution table for continuation patterns (multi-call continuations).
    /// Takes multiple L1→L2 calls detected from a single L1 tx, discovers child
    /// L2→L1 calls analytically, and queues all entries as a single atomic unit.
    #[method(name = "buildExecutionTable")]
    fn build_execution_table(
        &self,
        params: BuildExecutionTableParams,
    ) -> RpcResult<BuildExecutionTableResult>;

    /// Build execution table for L2→L1 continuation patterns (reverse multi-call continuations).
    /// Takes L2→L1 calls and L1→L2 return calls, builds the 3-entry L1 deferred
    /// structure and L2 table entries, and queues them as a unified withdrawal.
    #[method(name = "buildL2ToL1ExecutionTable")]
    fn build_l2_to_l1_execution_table(
        &self,
        params: BuildL2ToL1ExecutionTableParams,
    ) -> RpcResult<BuildExecutionTableResult>;
}

// Keep backward-compatible type alias for tests
pub type PendingL1ForwardTxs = Arc<std::sync::Mutex<Vec<Bytes>>>;

// ──────────────────────────────────────────────
//  RPC implementation
// ──────────────────────────────────────────────

/// Implementation of the `syncrollups` RPC namespace.
pub struct SyncRollupsRpc<Provider> {
    provider: Provider,
    evm_config: RollupEvmConfig,
    config: Arc<RollupConfig>,
    synced: Arc<std::sync::atomic::AtomicBool>,
    /// Unified queue for cross-chain calls. Each entry bundles the CALL+RESULT
    /// execution entries with the user's gas price and raw L1 tx.
    /// The driver drains, sorts by gas price, then submits to L1.
    queued_cross_chain_calls: Arc<std::sync::Mutex<Vec<QueuedCrossChainCall>>>,
    /// Legacy queue for raw signed L1 txs (kept for backward compatibility with
    /// `queueL1ForwardTx` RPC method — the unified path uses `queued_cross_chain_calls`).
    pending_l1_forward_txs: Arc<std::sync::Mutex<Vec<Bytes>>>,
    /// Queue for L2→L1 calls. Each entry bundles L2 table entries and L1
    /// deferred entries. The driver drains these alongside L1→L2 entries (unified roots).
    queued_l2_to_l1_calls: Arc<std::sync::Mutex<Vec<QueuedL2ToL1Call>>>,
}

impl<Provider> SyncRollupsRpc<Provider> {
    /// Create a new `SyncRollupsRpc` instance.
    pub fn new(
        provider: Provider,
        evm_config: RollupEvmConfig,
        config: Arc<RollupConfig>,
        synced: Arc<std::sync::atomic::AtomicBool>,
        queued_cross_chain_calls: Arc<std::sync::Mutex<Vec<QueuedCrossChainCall>>>,
        pending_l1_forward_txs: Arc<std::sync::Mutex<Vec<Bytes>>>,
        queued_l2_to_l1_calls: Arc<std::sync::Mutex<Vec<QueuedL2ToL1Call>>>,
    ) -> Self {
        Self {
            provider,
            evm_config,
            config,
            synced,
            queued_cross_chain_calls,
            pending_l1_forward_txs,
            queued_l2_to_l1_calls,
        }
    }
}

#[jsonrpsee::core::__reexports::async_trait]
impl<Provider> SyncRollupsApiServer for SyncRollupsRpc<Provider>
where
    Provider: StateProviderFactory
        + HeaderProvider<Header = alloy_consensus::Header>
        + BlockNumReader
        + Send
        + Sync
        + 'static,
{
    fn simulate_transaction(&self, signed_tx: Bytes) -> RpcResult<SimulationResult> {
        use crate::execution_planner;
        use jsonrpsee::types::ErrorObjectOwned;

        execution_planner::simulate_transaction(
            &self.provider,
            &self.evm_config,
            &self.config,
            signed_tx,
        )
        .map_err(|e| ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>))
    }

    fn get_state_root(&self) -> RpcResult<B256> {
        use jsonrpsee::types::ErrorObjectOwned;

        let best = self
            .provider
            .best_block_number()
            .map_err(|e| ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>))?;

        let header = self
            .provider
            .sealed_header(best)
            .map_err(|e| ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>))?
            .ok_or_else(|| ErrorObjectOwned::owned(-32000, "no header found", None::<()>))?;

        Ok(header.state_root())
    }

    fn is_synced(&self) -> RpcResult<bool> {
        Ok(self.synced.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn compute_action_hash(&self, action: ActionParams) -> RpcResult<B256> {
        use jsonrpsee::types::ErrorObjectOwned;

        compute_action_hash_from_params(&action)
            .map_err(|e| ErrorObjectOwned::owned(-32602, e.to_string(), None::<()>))
    }

    async fn initiate_cross_chain_call(&self, params: CrossChainCallParams) -> RpcResult<B256> {
        use jsonrpsee::types::ErrorObjectOwned;

        if self.config.rollups_address.is_zero() {
            return Err(ErrorObjectOwned::owned(
                -32000,
                "ROLLUPS_ADDRESS not configured — cross-chain mode disabled",
                None::<()>,
            ));
        }

        let rollup_id = crate::cross_chain::RollupId::new(U256::from(self.config.rollup_id));

        // Simulate the call against current L2 state to capture the actual
        // return data. CrossChainManagerL2._processCallAtScope() builds a
        // RESULT action with `data = returnData` from the real call and
        // looks up the execution table by `keccak256(abi.encode(resultAction))`.
        // If our pre-built RESULT has different data, _consumeExecution reverts
        // with ExecutionNotFound.
        //
        // Bridge-to-bridge deposits (receiveTokens on Bridge L2) cannot be
        // simulated directly because Bridge L2 requires calls to come through
        // the CCM proxy. simulate_call uses Address::ZERO as caller, which
        // triggers UnauthorizedCaller revert. receiveTokens always succeeds
        // (mints wrapped tokens) and returns no data, so we skip simulation.
        let receive_tokens_selector: [u8; 4] = [0x6b, 0x39, 0x96, 0xb0];
        let is_bridge_receive_tokens = params.data.len() >= 4
            && params.data[..4] == receive_tokens_selector
            && params.destination == self.config.bridge_l2_address;

        let (call_success, return_data) = if let (Some(pre_data), Some(pre_success)) =
            (&params.l2_return_data, params.l2_call_success)
        {
            tracing::info!(
                target: "based_rollup::rpc",
                data_len = pre_data.len(),
                success = pre_success,
                "using pre-computed L2 return data from composer RPC chained simulation"
            );
            (pre_success, pre_data.to_vec())
        } else if is_bridge_receive_tokens {
            tracing::info!(
                target: "based_rollup::rpc",
                destination = %params.destination,
                "skipping simulation for bridge receiveTokens — always succeeds"
            );
            (true, vec![])
        } else {
            crate::execution_planner::simulate_call(
                &self.provider,
                &self.evm_config,
                params.destination,
                params.data.to_vec(),
            )
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    -32000,
                    format!("failed to simulate cross-chain call: {e}"),
                    None::<()>,
                )
            })?
        };

        let (call_entry, result_entry) = crate::cross_chain::build_cross_chain_call_entries(
            rollup_id,
            params.destination,
            params.data.to_vec(),
            params.value,
            params.source_address,
            crate::cross_chain::RollupId::from_abi_boundary(params.source_rollup),
            call_success,
            return_data,
        );

        // Compute a deterministic ID for this cross-chain call (for tracking)
        let call_id = call_entry.action_hash;

        tracing::info!(
            target: "based_rollup::rpc",
            destination = %params.destination,
            source_address = %params.source_address,
            rollup_id = %rollup_id,
            %call_id,
            "queued cross-chain call entries for L1 submission"
        );

        // Push into the unified queue; the driver will drain, sort by gas price,
        // then build entries with correctly-ordered chained state deltas.
        //
        // NOTE: The RESULT entry's return_data was simulated against the current
        // state. If state changes before the block is built (e.g. pending txs),
        // the actual executeIncomingCrossChainCall may produce different return
        // data, causing _consumeExecution(resultHash) to revert with
        // ExecutionNotFound. With same-block execution the window is very small
        // (sub-second), but in high-contention scenarios this can still occur.
        {
            let mut queue = self
                .queued_cross_chain_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if queue.len() >= 500 {
                return Err(ErrorObjectOwned::owned(
                    -32000,
                    "cross-chain call queue full",
                    None::<()>,
                ));
            }
            queue.push(QueuedCrossChainCall {
                call_entry,
                result_entry,
                effective_gas_price: params.gas_price,
                raw_l1_tx: params.raw_l1_tx.clone(),
                extra_l2_entries: vec![],
                l1_entries: vec![],
                tx_reverts: crate::cross_chain::TxOutcome::Success,
                l1_independent_entries: crate::cross_chain::EntryGroupMode::Chained,
            });
        }

        // Return the CALL action hash as a tracking identifier.
        Ok(call_id.as_b256())
    }

    fn simulate_call(&self, destination: Address, data: Bytes) -> RpcResult<SimulateCallResult> {
        use jsonrpsee::types::ErrorObjectOwned;

        let (success, return_data) = crate::execution_planner::simulate_call(
            &self.provider,
            &self.evm_config,
            destination,
            data.to_vec(),
        )
        .map_err(|e| {
            ErrorObjectOwned::owned(-32000, format!("failed to simulate call: {e}"), None::<()>)
        })?;

        Ok(SimulateCallResult {
            success,
            return_data: Bytes::from(return_data),
        })
    }

    fn queue_l1_forward_tx(&self, raw_tx: Bytes) -> RpcResult<B256> {
        use alloy_consensus::transaction::TxEnvelope;
        use alloy_rlp::Decodable;
        use jsonrpsee::types::ErrorObjectOwned;

        // Decode to get tx hash
        let tx_envelope = TxEnvelope::decode(&mut raw_tx.as_ref()).map_err(|e| {
            ErrorObjectOwned::owned(-32000, format!("invalid tx envelope: {e}"), None::<()>)
        })?;
        let tx_hash = *tx_envelope.tx_hash();

        // Push raw bytes into the shared queue
        {
            let mut queue = self
                .pending_l1_forward_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if queue.len() >= 1000 {
                return Err(ErrorObjectOwned::owned(
                    -32000,
                    "L1 forward tx queue full",
                    None::<()>,
                ));
            }
            queue.push(raw_tx);
        }

        tracing::info!(
            target: "based_rollup::rpc",
            %tx_hash,
            "queued L1 forward tx for driver submission"
        );

        Ok(tx_hash)
    }

    fn initiate_l2_cross_chain_call(&self, params: L2CrossChainCallParams) -> RpcResult<B256> {
        use jsonrpsee::types::ErrorObjectOwned;

        if self.config.rollups_address.is_zero() {
            return Err(ErrorObjectOwned::owned(
                -32000,
                "ROLLUPS_ADDRESS not configured — cross-chain mode disabled",
                None::<()>,
            ));
        }

        let entries = crate::cross_chain::build_l2_to_l1_call_entries(
            params.destination,
            params.data.to_vec(),
            params.value,
            params.source_address, // L2 sender: msg.sender in L2 proxy fallback
            self.config.rollup_id,
            params.raw_l2_tx.to_vec(), // RLP-encoded L2 tx for L2TX trigger on L1
            params.delivery_return_data.to_vec(),
            params.delivery_failed,
            params.l1_delivery_scope, // scope from trace depth
            params.tx_reverts,
        );

        let call_id = entries.l2_table_entries[0].action_hash;

        if params.tx_reverts.is_revert() {
            tracing::info!(
                target: "based_rollup::rpc",
                destination = %params.destination,
                source = %params.source_address,
                delivery_return_data_hex = %format!("0x{}", hex::encode(&params.delivery_return_data)),
                delivery_return_data_len = params.delivery_return_data.len(),
                delivery_failed = params.delivery_failed,
                tx_reverts = params.tx_reverts.is_revert(),
                l2_entries = entries.l2_table_entries.len(),
                l1_entries = entries.l1_deferred_entries.len(),
                %call_id,
                "initiate_l2_cross_chain_call: REVERT mode — entries built"
            );
        }

        tracing::info!(
            target: "based_rollup::rpc",
            destination = %params.destination,
            source = %params.source_address,
            value = %params.value,
            data_len = params.data.len(),
            %call_id,
            "queued L2→L1 cross-chain call"
        );

        {
            let mut queue = self
                .queued_l2_to_l1_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if queue.len() >= 100 {
                return Err(ErrorObjectOwned::owned(
                    -32000,
                    "L2→L1 queue full",
                    None::<()>,
                ));
            }
            queue.push(QueuedL2ToL1Call {
                l2_table_entries: entries.l2_table_entries,
                l1_deferred_entries: entries.l1_deferred_entries,
                user: entries.user,
                amount: entries.amount,
                raw_l2_tx: params.raw_l2_tx.clone(),
                rlp_encoded_tx: params.raw_l2_tx.to_vec(),
                trigger_count: 1, // Simple L2→L1 call: one executeL2TX
                tx_reverts: params.tx_reverts,
            });
        }

        Ok(call_id.as_b256())
    }

    fn build_execution_table(
        &self,
        params: BuildExecutionTableParams,
    ) -> RpcResult<BuildExecutionTableResult> {
        use crate::table_builder::{
            L1DetectedCall, analyze_continuation_calls, build_continuation_entries,
        };
        use jsonrpsee::types::ErrorObjectOwned;

        if self.config.rollups_address.is_zero() {
            return Err(ErrorObjectOwned::owned(
                -32000,
                "ROLLUPS_ADDRESS not configured — cross-chain mode disabled",
                None::<()>,
            ));
        }

        if params.calls.is_empty() {
            return Err(ErrorObjectOwned::owned(
                -32602,
                "buildExecutionTable requires at least 1 call",
                None::<()>,
            ));
        }

        let rollup_id = crate::cross_chain::RollupId::new(U256::from(self.config.rollup_id));

        // Convert RPC params to L1DetectedCall structs
        let l1_calls: Vec<L1DetectedCall> = params
            .calls
            .iter()
            .map(|c| L1DetectedCall {
                destination: c.destination,
                data: c.data.to_vec(),
                value: c.value,
                source_address: c.source_address,
                l2_return_data: c.l2_return_data.to_vec(),
                call_success: c.call_success,
                parent_call_index: c.parent_call_index,
                target_rollup_id: c.target_rollup_id,
                scope: crate::cross_chain::ScopePath::from_parts(c.scope.clone()),
                discovery_iteration: c.discovery_iteration,
                l1_trace_depth: c.l1_trace_depth,
                in_reverted_frame: c.in_reverted_frame,
            })
            .collect();

        // Analyze calls to discover continuation patterns and child L2→L1 calls
        let detected_calls = analyze_continuation_calls(&l1_calls, self.config.rollup_id);

        if detected_calls.is_empty() {
            return Err(ErrorObjectOwned::owned(
                -32000,
                "no continuation pattern detected — use initiateCrossChainCall instead",
                None::<()>,
            ));
        }

        // Build continuation entries using the table builder
        let continuation = build_continuation_entries(&detected_calls, rollup_id);

        if continuation.l2_entries.is_empty() || continuation.l1_entries.is_empty() {
            return Err(ErrorObjectOwned::owned(
                -32000,
                "table builder produced no entries",
                None::<()>,
            ));
        }

        let l2_count = continuation.l2_entries.len();
        let l1_count = continuation.l1_entries.len();

        // The primary CALL+RESULT pair is the first L1→L2 call (CALL_A).
        // Build it using the existing build_cross_chain_call_entries for compatibility.
        let first_call = &params.calls[0];

        // Use L2 return data from the L1 proxy's simulation when available.
        // Fall back to local EVM simulation (legacy path).
        let (call_success, return_data) =
            if !first_call.l2_return_data.is_empty() || !first_call.call_success {
                (first_call.call_success, first_call.l2_return_data.to_vec())
            } else {
                crate::execution_planner::simulate_call(
                    &self.provider,
                    &self.evm_config,
                    first_call.destination,
                    first_call.data.to_vec(),
                )
                .unwrap_or_else(|_| {
                    // Simulation may fail (e.g., proxy doesn't exist yet).
                    // For multi-call continuations, receiveTokens returns void anyway.
                    (true, vec![])
                })
            };

        let (call_entry, result_entry) = crate::cross_chain::build_cross_chain_call_entries(
            rollup_id,
            first_call.destination,
            first_call.data.to_vec(),
            first_call.value,
            first_call.source_address,
            crate::cross_chain::RollupId::MAINNET, // source_rollup = MAINNET
            call_success,
            return_data,
        );

        let call_id = call_entry.action_hash;

        tracing::info!(
            target: "based_rollup::rpc",
            call_count = params.calls.len(),
            l2_entries = l2_count,
            l1_entries = l1_count,
            %call_id,
            "built execution table for continuation pattern"
        );

        // Queue as a single atomic unit with extra_l2_entries and l1_entries
        {
            let mut queue = self
                .queued_cross_chain_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if queue.len() >= 500 {
                return Err(ErrorObjectOwned::owned(
                    -32000,
                    "cross-chain call queue full",
                    None::<()>,
                ));
            }
            queue.push(QueuedCrossChainCall {
                call_entry,
                result_entry,
                effective_gas_price: params.gas_price,
                raw_l1_tx: params.raw_l1_tx.clone(),
                extra_l2_entries: continuation.l2_entries,
                l1_entries: continuation.l1_entries,
                tx_reverts: crate::cross_chain::TxOutcome::Success,
                l1_independent_entries: continuation.l1_independent_entries,
            });
        }

        Ok(BuildExecutionTableResult {
            l2_entry_count: l2_count,
            l1_entry_count: l1_count,
            call_id: call_id.as_b256(),
        })
    }

    fn build_l2_to_l1_execution_table(
        &self,
        params: BuildL2ToL1ExecutionTableParams,
    ) -> RpcResult<BuildExecutionTableResult> {
        use crate::table_builder::{
            L2DetectedCall, L2ReturnCall, analyze_l2_to_l1_continuation_calls,
            build_l2_to_l1_continuation_entries,
        };
        use jsonrpsee::types::ErrorObjectOwned;

        if self.config.rollups_address.is_zero() {
            return Err(ErrorObjectOwned::owned(
                -32000,
                "ROLLUPS_ADDRESS not configured — cross-chain mode disabled",
                None::<()>,
            ));
        }

        if params.l2_calls.is_empty() {
            return Err(ErrorObjectOwned::owned(
                -32602,
                "buildL2ToL1ExecutionTable requires at least one L2→L1 call",
                None::<()>,
            ));
        }

        let rollup_id = crate::cross_chain::RollupId::new(U256::from(self.config.rollup_id));

        // Convert RPC params to table_builder types.
        let l2_calls: Vec<L2DetectedCall> = params
            .l2_calls
            .iter()
            .map(|c| L2DetectedCall {
                destination: c.destination,
                data: c.data.to_vec(),
                value: c.value,
                source_address: c.source_address,
                delivery_return_data: c.delivery_return_data.to_vec(),
                delivery_failed: c.delivery_failed,
                scope: crate::cross_chain::ScopePath::from_parts(c.scope.clone()),
                in_reverted_frame: c.in_reverted_frame,
            })
            .collect();

        let return_calls: Vec<L2ReturnCall> = params
            .return_calls
            .iter()
            .map(|c| L2ReturnCall {
                destination: c.destination,
                data: c.data.to_vec(),
                value: c.value,
                source_address: c.source_address,
                parent_call_index: c.parent_call_index,
                l2_return_data: c
                    .l2_return_data
                    .as_ref()
                    .map(|b| b.to_vec())
                    .unwrap_or_default(),
                l2_delivery_failed: c.l2_delivery_failed,
                scope: crate::cross_chain::ScopePath::from_parts(c.scope.clone()),
            })
            .collect();

        // Log hex of return call data for hash comparison debugging
        for (ri, rc) in return_calls.iter().enumerate() {
            tracing::info!(
                target: "based_rollup::rpc",
                ri,
                dest = %rc.destination,
                l2_return_data_hex = %format!("0x{}", hex::encode(&rc.l2_return_data)),
                l2_return_data_len = rc.l2_return_data.len(),
                l2_delivery_failed = rc.l2_delivery_failed,
                "RPC received return call l2_return_data"
            );
        }
        for (ci, c) in l2_calls.iter().enumerate() {
            tracing::info!(
                target: "based_rollup::rpc",
                ci,
                dest = %c.destination,
                delivery_return_data_hex = %format!("0x{}", hex::encode(&c.delivery_return_data)),
                delivery_return_data_len = c.delivery_return_data.len(),
                delivery_failed = c.delivery_failed,
                "RPC received L2 call delivery_return_data"
            );
        }

        // Analyze calls to discover the continuation structure.
        let detected =
            analyze_l2_to_l1_continuation_calls(&l2_calls, &return_calls, self.config.rollup_id);

        if detected.is_empty() {
            return Err(ErrorObjectOwned::owned(
                -32000,
                "no L2→L1 continuation pattern detected",
                None::<()>,
            ));
        }

        for (i, dc) in detected.iter().enumerate() {
            let action_hash = crate::table_builder::compute_action_hash(&dc.call_action);
            tracing::info!(
                target: "based_rollup::rpc",
                idx = i,
                direction = ?dc.direction,
                is_continuation = dc.is_continuation,
                parent = ?dc.parent_call_index,
                action_hash = %action_hash,
                destination = %dc.call_action.destination,
                source = %dc.call_action.source_address,
                data_len = dc.call_action.data.len(),
                "analyzed L2->L1 continuation call"
            );
        }

        // Build L2 table entries and L1 deferred entries for the continuation pattern.
        // Pass rlp_encoded_tx for the L2TX trigger entries on L1.
        let continuation = build_l2_to_l1_continuation_entries(
            &detected,
            rollup_id,
            params.raw_l2_tx.as_ref(),
            params.tx_reverts,
        );

        let l2_count = continuation.l2_entries.len();
        let l1_count = continuation.l1_entries.len();

        for (i, e) in continuation.l2_entries.iter().enumerate() {
            tracing::info!(
                target: "based_rollup::rpc",
                idx = i,
                action_hash = %e.action_hash,
                next_action_type = ?e.next_action.action_type,
                next_action_dest = %e.next_action.destination,
                "L2->L1 continuation: L2 table entry"
            );
        }
        for (i, e) in continuation.l1_entries.iter().enumerate() {
            tracing::info!(
                target: "based_rollup::rpc",
                idx = i,
                action_hash = %e.action_hash,
                next_action_type = ?e.next_action.action_type,
                next_action_dest = %e.next_action.destination,
                next_action_data_len = e.next_action.data.len(),
                "L2->L1 continuation: L1 deferred entry"
            );
        }

        if continuation.l2_entries.is_empty() || continuation.l1_entries.is_empty() {
            return Err(ErrorObjectOwned::owned(
                -32000,
                "L2→L1 table builder produced no entries",
                None::<()>,
            ));
        }

        // Use the first L2→L1 call's action hash as the tracking ID.
        let call_id = continuation.l2_entries[0].action_hash;

        tracing::info!(
            target: "based_rollup::rpc",
            l2_calls = params.l2_calls.len(),
            return_calls_count = params.return_calls.len(),
            l2_entries = l2_count,
            l1_entries = l1_count,
            %call_id,
            "built L2→L1 execution table for reverse multi-call continuation"
        );

        // Queue as a QueuedL2ToL1Call with L2 table entries and L1 deferred entries.
        // The L1 deferred entries go to pending_l1_entries in the driver,
        // which handles them via the trigger flow (postBatch + createProxy + trigger).
        {
            let mut queue = self
                .queued_l2_to_l1_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if queue.len() >= 100 {
                return Err(ErrorObjectOwned::owned(
                    -32000,
                    "L2→L1 queue full",
                    None::<()>,
                ));
            }
            // L2TX trigger: ONE executeL2TX for the entire chained entry set.
            // In the chained model, all entries chain from a single L2TX trigger:
            //   L2TX → CALL(A,scope=[0]) → RESULT → CALL(B,scope=[1]) → RESULT → terminal
            // The single executeL2TX consumes all entries via scope navigation.
            // (The old per-call model used N triggers for N separate L2TX entries,
            // but chaining replaced that with RESULT→CALL links.)
            queue.push(QueuedL2ToL1Call {
                l2_table_entries: continuation.l2_entries,
                l1_deferred_entries: continuation.l1_entries,
                user: params.l2_calls[0].source_address,
                amount: params
                    .l2_calls
                    .iter()
                    .map(|c| c.value)
                    .fold(U256::ZERO, |a, b| a + b),
                raw_l2_tx: params.raw_l2_tx.clone(),
                rlp_encoded_tx: params.raw_l2_tx.to_vec(),
                trigger_count: 1,
                tx_reverts: params.tx_reverts,
            });
        }

        {
            tracing::info!(
                target: "based_rollup::rpc",
                l2_call_count = params.l2_calls.len(),
                return_call_count = params.return_calls.len(),
                "queued L2->L1 multi-call with L2TX trigger"
            );
        }

        Ok(BuildExecutionTableResult {
            l2_entry_count: l2_count,
            l1_entry_count: l1_count,
            call_id: call_id.as_b256(),
        })
    }
}

// ──────────────────────────────────────────────
//  Helpers
// ──────────────────────────────────────────────

/// Compute the action hash matching the Solidity `keccak256(abi.encode(action))`.
///
/// Returns an error if `action_type` is not one of the known variants:
/// CALL, RESULT, L2TX, REVERT, REVERT_CONTINUE.
pub fn compute_action_hash_from_params(params: &ActionParams) -> Result<B256, String> {
    let action_type = match params.action_type.to_uppercase().as_str() {
        "CALL" => ICrossChainManagerL2::ActionType::CALL,
        "RESULT" => ICrossChainManagerL2::ActionType::RESULT,
        "L2TX" => ICrossChainManagerL2::ActionType::L2TX,
        "REVERT" => ICrossChainManagerL2::ActionType::REVERT,
        "REVERT_CONTINUE" => ICrossChainManagerL2::ActionType::REVERT_CONTINUE,
        other => {
            return Err(format!(
                "unknown action type '{other}': expected one of CALL, RESULT, L2TX, REVERT, REVERT_CONTINUE"
            ));
        }
    };

    let sol_action = ICrossChainManagerL2::Action {
        actionType: action_type,
        rollupId: params.rollup_id,
        destination: params.destination,
        value: params.value,
        data: params.data.clone(),
        failed: params.failed,
        sourceAddress: params.source_address,
        sourceRollup: params.source_rollup,
        scope: params.scope.clone(),
    };

    Ok(keccak256(ICrossChainManagerL2::Action::abi_encode(
        &sol_action,
    )))
}

/// Convert a [`CrossChainExecutionEntry`] to the serializable RPC type.
pub fn entry_to_serializable(entry: &CrossChainExecutionEntry) -> SerializableExecutionEntry {
    SerializableExecutionEntry {
        state_deltas: entry
            .state_deltas
            .iter()
            .map(|d| SerializableStateDelta {
                rollup_id: d.rollup_id.as_u256(),
                current_state: d.current_state,
                new_state: d.new_state,
                ether_delta: d.ether_delta,
            })
            .collect(),
        action_hash: entry.action_hash.as_b256(),
        next_action: action_to_serializable(&entry.next_action),
    }
}

fn action_to_serializable(action: &CrossChainAction) -> SerializableAction {
    SerializableAction {
        action_type: match action.action_type {
            CrossChainActionType::Call => "CALL".to_string(),
            CrossChainActionType::Result => "RESULT".to_string(),
            CrossChainActionType::L2Tx => "L2TX".to_string(),
            CrossChainActionType::Revert => "REVERT".to_string(),
            CrossChainActionType::RevertContinue => "REVERT_CONTINUE".to_string(),
        },
        rollup_id: action.rollup_id.as_u256(),
        destination: action.destination,
        value: action.value,
        data: Bytes::from(action.data.clone()),
        failed: action.failed,
        source_address: action.source_address,
        source_rollup: action.source_rollup.as_u256(),
        scope: action.scope.as_slice().to_vec(),
    }
}

#[cfg(test)]
#[path = "rpc_tests.rs"]
mod tests;
