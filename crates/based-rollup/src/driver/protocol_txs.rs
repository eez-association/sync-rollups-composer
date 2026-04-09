//! Builder-signed protocol transaction construction + derived-block building.
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1j. This module owns
//! the two methods that physically build L2 block payloads:
//!
//! - [`Driver::build_builder_protocol_txs`] — assembles the RLP-encoded
//!   transaction list for a builder block: deploys (block 1), setContext,
//!   loadExecutionTable, executeIncomingCrossChainCall, plus drained
//!   mempool transactions up to the gas limit. Owns the `max_trigger_count`
//!   parameter used by the §4f rebuild filtering path in `verify.rs`.
//!
//! - [`Driver::build_derived_block`] — builds a block directly from
//!   already-assembled transactions using reth's `builder_for_next_block`
//!   API. Returns a `BuiltBlock` summary and an `ExecutionData` payload
//!   suitable for `engine_newPayload`.

use super::Driver;
use super::types::{BuiltBlock, DESIRED_GAS_LIMIT, calc_gas_limit, encode_block_transactions};
use crate::cross_chain::CrossChainExecutionEntry;
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::ExecutionData;
use eyre::{OptionExt, Result, WrapErr};
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_payload_primitives::PayloadTypes;
use reth_primitives_traits::SignedTransaction;
use reth_provider::{
    BlockHashReader, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use reth_revm::database::StateProviderDatabase;
use revm::database::State;
use std::marker::PhantomData;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// ProtocolTxPlan — typed stage pipeline for protocol transaction assembly.
//
// Enforces at compile time that protocol txs are added in order:
//   Bootstrapped  →  ContextSet  →  EntriesLoaded
//
// You cannot call `load_entries` before `set_context`, or add bootstrap txs
// after context has been set.
// ---------------------------------------------------------------------------

/// Typed stages for [`ProtocolTxPlan`].
pub(super) mod stage {
    /// Bootstrap complete (block 1 deploys, block 2 canonical bridge).
    pub struct Bootstrapped;
    /// L2 context (`setContext`) has been added.
    pub struct ContextSet;
    /// Execution table + triggers have been added — ready to finalize.
    pub struct EntriesLoaded;
}

/// Accumulates builder-signed protocol transactions through typed stages.
///
/// Stage transitions consume `self` and produce a new stage, so the
/// compiler rejects out-of-order calls.
pub(super) struct ProtocolTxPlan<S> {
    txs: Vec<reth_ethereum_primitives::TransactionSigned>,
    _stage: PhantomData<S>,
}

impl ProtocolTxPlan<stage::Bootstrapped> {
    /// Create a plan pre-loaded with bootstrap transactions (deploys,
    /// canonical bridge, bootstrap transfers).
    pub(super) fn new(
        bootstrap_txs: Vec<reth_ethereum_primitives::TransactionSigned>,
    ) -> Self {
        Self {
            txs: bootstrap_txs,
            _stage: PhantomData,
        }
    }

    /// Add the `setContext` tx (if present) and advance to [`stage::ContextSet`].
    pub(super) fn set_context(
        mut self,
        context_tx: Option<reth_ethereum_primitives::TransactionSigned>,
    ) -> ProtocolTxPlan<stage::ContextSet> {
        if let Some(tx) = context_tx {
            self.txs.push(tx);
        }
        ProtocolTxPlan {
            txs: self.txs,
            _stage: PhantomData,
        }
    }
}

impl ProtocolTxPlan<stage::ContextSet> {
    /// Add `loadExecutionTable` + `executeIncomingCrossChainCall` trigger
    /// txs and advance to [`stage::EntriesLoaded`].
    pub(super) fn load_entries(
        mut self,
        table_tx: Option<reth_ethereum_primitives::TransactionSigned>,
        trigger_txs: Vec<reth_ethereum_primitives::TransactionSigned>,
    ) -> ProtocolTxPlan<stage::EntriesLoaded> {
        if let Some(tx) = table_tx {
            self.txs.push(tx);
        }
        self.txs.extend(trigger_txs);
        ProtocolTxPlan {
            txs: self.txs,
            _stage: PhantomData,
        }
    }
}

impl ProtocolTxPlan<stage::EntriesLoaded> {
    /// Consume the plan and return the accumulated protocol transactions.
    pub(super) fn into_txs(self) -> Vec<reth_ethereum_primitives::TransactionSigned> {
        self.txs
    }
}

impl<P, Pool> Driver<P, Pool>
where
    P: DatabaseProviderFactory
        + StageCheckpointReader
        + BlockNumReader
        + BlockHashReader
        + HeaderProvider<Header = alloy_consensus::Header>
        + TransactionsProvider<Transaction = reth_ethereum_primitives::TransactionSigned>
        + StateProviderFactory
        + Send
        + Sync,
    P::ProviderRW: StageCheckpointWriter,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_transaction_pool::PoolTransaction<
                Consensus = reth_ethereum_primitives::TransactionSigned,
            >,
        > + reth_transaction_pool::TransactionPoolExt
        + Send
        + Sync,
{
    /// Construct builder-signed protocol transactions for a builder block.
    ///
    /// Returns RLP-encoded transactions (setContext, deploy, loadTable, executeIncoming,
    /// plus user txs from the mempool).
    ///
    /// Uses [`ProtocolTxPlan`] to enforce stage ordering at compile time:
    ///   Bootstrap → setContext → loadTable/triggers → mempool drain
    ///
    /// `max_trigger_count` limits the number of `executeIncomingCrossChainCall` trigger
    /// transactions generated. `loadExecutionTable` is always generated if table entries
    /// are present (regardless of this limit). Pass `usize::MAX` to generate all triggers.
    pub(super) fn build_builder_protocol_txs(
        &mut self,
        l2_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        execution_entries: &[CrossChainExecutionEntry],
        max_trigger_count: usize,
    ) -> Result<Bytes> {
        use crate::cross_chain;

        let signer = self
            .proposer
            .as_ref()
            .ok_or_else(|| eyre::eyre!("proposer required for builder protocol txs"))?
            .create_signer()?;

        let chain_id = self.evm_config.chain_spec().chain().id();

        // Use next block's base fee (not parent's) for protocol tx gas_price.
        let parent_header = self
            .l2_provider
            .sealed_header(self.l2_head_number)
            .wrap_err("failed to get parent header for gas price")?
            .ok_or_eyre("parent header not found for gas price")?;
        let gas_price = parent_header
            .next_block_base_fee(
                self.evm_config
                    .chain_spec()
                    .base_fee_params_at_timestamp(timestamp),
            )
            .unwrap_or(1)
            .max(1) as u128;

        // --- Stage 1: Bootstrap (block 1 deploys, block 2 canonical bridge) ---
        let mut bootstrap_txs: Vec<reth_ethereum_primitives::TransactionSigned> = Vec::new();

        if l2_block_number == 1 {
            bootstrap_txs.push(cross_chain::build_deploy_l2context_tx(
                self.config.builder_address,
                &signer,
                chain_id,
                gas_price,
            )?);
            if !self.config.rollups_address.is_zero() {
                bootstrap_txs.push(cross_chain::build_deploy_ccm_tx(
                    self.config.rollup_id,
                    self.config.builder_address,
                    &signer,
                    chain_id,
                    gas_price,
                )?);
                bootstrap_txs.push(cross_chain::build_deploy_bridge_tx(
                    &signer, chain_id, gas_price,
                )?);
                bootstrap_txs.push(cross_chain::build_initialize_bridge_tx(
                    self.config.cross_chain_manager_address,
                    self.config.rollup_id,
                    self.config.builder_address,
                    self.config.bridge_l2_address,
                    &signer,
                    chain_id,
                    gas_price,
                )?);
                self.builder_l2_nonce = 4;
            } else {
                self.builder_l2_nonce = 1;
            }
            for account in &self.config.bootstrap_accounts {
                bootstrap_txs.push(cross_chain::build_bootstrap_transfer_tx(
                    account.address,
                    account.amount_wei,
                    &signer,
                    self.builder_l2_nonce,
                    chain_id,
                    gas_price,
                )?);
                self.builder_l2_nonce += 1;
            }
        }

        if l2_block_number == 2
            && !self.config.bridge_l1_address.is_zero()
            && !self.config.bridge_l2_address.is_zero()
        {
            info!(
                target: "based_rollup::driver",
                bridge_l2 = %self.config.bridge_l2_address,
                canonical = %self.config.bridge_l1_address,
                nonce = self.builder_l2_nonce,
                "setting canonical bridge address on L2 bridge (block 2 protocol tx)"
            );
            bootstrap_txs.push(cross_chain::build_set_canonical_bridge_tx(
                self.config.bridge_l2_address,
                self.config.bridge_l1_address,
                &signer,
                self.builder_l2_nonce,
                chain_id,
                gas_price,
            )?);
            self.builder_l2_nonce += 1;
        }

        let plan = ProtocolTxPlan::new(bootstrap_txs);

        // --- Stage 2: setContext (every block) ---
        let context_tx = if !self.config.l2_context_address.is_zero() {
            let tx = cross_chain::build_set_context_tx(
                l1_block_number,
                l1_block_hash,
                self.config.l2_context_address,
                &signer,
                self.builder_l2_nonce,
                chain_id,
                gas_price,
            )?;
            self.builder_l2_nonce += 1;
            Some(tx)
        } else {
            None
        };
        let plan = plan.set_context(context_tx);

        // --- Stage 3: loadExecutionTable + triggers ---
        let (table_tx, trigger_txs) = if !execution_entries.is_empty()
            && !self.config.cross_chain_manager_address.is_zero()
        {
            let our_rollup_id = cross_chain::RollupId::new(alloy_primitives::U256::from(
                self.config.rollup_id,
            ));
            let (table_entries, mut trigger_entries) =
                cross_chain::partition_entries(execution_entries, our_rollup_id);

            // Scope override for REVERT patterns.
            let has_revert = table_entries
                .iter()
                .any(|e| e.next_action.action_type == cross_chain::CrossChainActionType::Revert);
            if has_revert {
                let revert_scope_len = table_entries
                    .iter()
                    .filter(|e| {
                        e.next_action.action_type == cross_chain::CrossChainActionType::Revert
                    })
                    .map(|e| e.next_action.scope.len())
                    .max()
                    .unwrap_or(0);
                let trigger_scope: Vec<alloy_primitives::U256> =
                    vec![alloy_primitives::U256::ZERO; revert_scope_len + 1];
                for trigger in &mut trigger_entries {
                    info!(
                        target: "based_rollup::driver",
                        old_scope_len = trigger.next_action.scope.len(),
                        new_scope_len = trigger_scope.len(),
                        "overriding trigger scope for REVERT pattern"
                    );
                    trigger.next_action.scope =
                        crate::cross_chain::ScopePath::from_parts(trigger_scope.clone());
                }
            }

            let t_tx = if !table_entries.is_empty() {
                let tx = cross_chain::build_load_table_tx(
                    &table_entries,
                    self.config.cross_chain_manager_address,
                    &signer,
                    self.builder_l2_nonce,
                    chain_id,
                    gas_price,
                )?;
                self.builder_l2_nonce += 1;
                Some(tx)
            } else {
                None
            };

            let trigger_limit = trigger_entries.len().min(max_trigger_count);
            let mut t_txs = Vec::with_capacity(trigger_limit);
            for trigger in &trigger_entries[..trigger_limit] {
                t_txs.push(cross_chain::build_execute_incoming_tx(
                    &trigger.next_action,
                    self.config.cross_chain_manager_address,
                    &signer,
                    self.builder_l2_nonce,
                    chain_id,
                    gas_price,
                )?);
                self.builder_l2_nonce += 1;
            }
            (t_tx, t_txs)
        } else {
            (None, vec![])
        };
        let plan = plan.load_entries(table_tx, trigger_txs);

        // Finalize protocol txs via the typed plan.
        let mut block_txs = plan.into_txs();

        // --- Stage 4: Drain user transactions from the mempool ---
        let block_gas_limit = calc_gas_limit(parent_header.gas_limit(), DESIRED_GAS_LIMIT);
        let builder_gas_used = cross_chain::estimate_builder_tx_gas(&block_txs);
        let mut cumulative_gas_used = builder_gas_used;

        let base_fee = parent_header
            .next_block_base_fee(
                self.evm_config
                    .chain_spec()
                    .base_fee_params_at_timestamp(timestamp),
            )
            .unwrap_or(1);

        let mut best_txs = self.pool.best_transactions_with_attributes(
            reth_transaction_pool::BestTransactionsAttributes::base_fee(base_fee),
        );

        // Validate pool tx nonces against canonical state. After a chain rewind
        // (e.g., phantom state detection), the pool's nonce tracking may be stale
        // — returning txs with nonces that don't match the actual chain state.
        // Without this check, the builder includes a stale-nonce tx, the EVM
        // rejects it, and the builder gets stuck in a Sync↔Builder loop.
        let state_for_nonce_check = self.l2_provider.state_by_block_hash(self.head_hash).ok();
        let mut expected_nonces: std::collections::HashMap<alloy_primitives::Address, u64> =
            std::collections::HashMap::new();

        while let Some(pool_tx) = best_txs.next() {
            // Skip transactions from the builder's own address — their nonces
            // conflict with protocol transactions (setContext, deploys, etc.)
            // that are already in block_txs with specific nonces.
            if pool_tx.sender() == self.config.builder_address {
                continue;
            }

            // Check nonce against canonical state to catch stale pool entries.
            if let Some(ref state) = state_for_nonce_check {
                use reth_provider::AccountReader;
                let sender = pool_tx.sender();
                let tx_nonce = pool_tx.nonce();
                let expected = expected_nonces.entry(sender).or_insert_with(|| {
                    state
                        .basic_account(&sender)
                        .ok()
                        .flatten()
                        .map_or(0, |acct| acct.nonce)
                });
                if tx_nonce != *expected {
                    warn!(
                        target: "based_rollup::driver",
                        %sender,
                        tx_nonce,
                        expected = *expected,
                        "skipping pool tx with stale nonce (pool may be stale after rewind)"
                    );
                    best_txs.mark_invalid(
                        &pool_tx,
                        &reth_transaction_pool::error::InvalidPoolTransactionError::ExceedsGasLimit(
                            0, 0,
                        ),
                    );
                    continue;
                }
                *expected = tx_nonce + 1;
            }

            let tx_gas = pool_tx.gas_limit();

            // Skip transactions that don't fit in the remaining gas budget.
            if cumulative_gas_used + tx_gas > block_gas_limit {
                best_txs.mark_invalid(
                    &pool_tx,
                    &reth_transaction_pool::error::InvalidPoolTransactionError::ExceedsGasLimit(
                        tx_gas,
                        block_gas_limit,
                    ),
                );
                continue;
            }

            // Convert pool tx to signed transaction for block inclusion.
            let recovered = pool_tx.to_consensus();
            block_txs.push(recovered.into_inner());
            cumulative_gas_used += tx_gas;
        }

        Ok(encode_block_transactions(&block_txs))
    }

    /// Build a block directly from L1-derived transactions using the EVM config's
    /// `builder_for_next_block` API.
    ///
    /// `parent_block_number` specifies which block to build on top of.
    /// `l1_block_number` is passed via `prev_randao` so the EVM config can read it.
    pub(super) fn build_derived_block(
        &self,
        parent_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        derived_transactions: &Bytes,
    ) -> Result<(BuiltBlock, ExecutionData)> {
        use reth_evm::execute::BlockBuilder;

        // Get parent header
        let parent_header = self
            .l2_provider
            .sealed_header(parent_block_number)
            .wrap_err("failed to get parent header")?
            .ok_or_eyre("parent header not found")?;

        // Get state provider at parent
        let state_provider = self
            .l2_provider
            .state_by_block_hash(parent_header.hash())
            .wrap_err("failed to get state provider")?;

        let state_db = StateProviderDatabase::new(state_provider.as_ref());
        let mut db = State::builder()
            .with_database(state_db)
            .with_bundle_update()
            .build();

        // Encode L1 block number into prev_randao so the EVM config can read it
        let prev_randao = B256::from(alloy_primitives::U256::from(l1_block_number));

        let attributes = NextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: self.config.builder_address,
            prev_randao,
            gas_limit: calc_gas_limit(parent_header.gas_limit(), DESIRED_GAS_LIMIT),
            parent_beacon_block_root: Some(l1_block_hash),
            withdrawals: Some(Default::default()),
            extra_data: Default::default(),
        };

        let mut builder = self
            .evm_config
            .builder_for_next_block(&mut db, &parent_header, attributes)
            .wrap_err("failed to create block builder")?;

        // Apply pre-execution changes (beacon root contract)
        builder
            .apply_pre_execution_changes()
            .wrap_err("pre-execution changes failed")?;

        // Decode and execute L1-derived transactions
        if !derived_transactions.is_empty() {
            let txs: Vec<reth_ethereum_primitives::TransactionSigned> =
                alloy_rlp::Decodable::decode(&mut derived_transactions.as_ref())
                    .wrap_err("failed to RLP-decode derived transactions")?;

            for (tx_idx, tx) in txs.into_iter().enumerate() {
                let tx_hash = *tx.tx_hash();
                let recovered = SignedTransaction::try_into_recovered(tx)
                    .map_err(|_| eyre::eyre!("failed to recover signer for L1-derived tx"))?;

                let signer = recovered.signer();
                builder.execute_transaction(recovered).wrap_err_with(|| {
                    format!(
                        "failed to execute L1-derived tx #{tx_idx} (hash={tx_hash}, signer={signer})"
                    )
                })?;
            }
        }

        // Finish building the block (computes state root, assembles sealed block)
        let outcome = builder
            .finish(state_provider.as_ref())
            .wrap_err("block builder finish failed")?;

        let sealed_block = outcome.block.sealed_block().clone();
        let block_hash = sealed_block.sealed_header().hash();
        let state_root = sealed_block.sealed_header().state_root();
        let tx_count = sealed_block.body().transactions.len();
        let encoded_transactions = encode_block_transactions(&sealed_block.body().transactions);

        let execution_data = <EthEngineTypes as PayloadTypes>::block_to_payload(sealed_block);

        let built = BuiltBlock {
            hash: block_hash,
            pre_state_root: parent_header.state_root(),
            state_root,
            tx_count,
            encoded_transactions,
        };

        Ok((built, execution_data))
    }
}
