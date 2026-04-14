//! Block building + fork-choice engine API layer.
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1h. This module
//! owns the three methods that sit between the driver's orchestration
//! loop and reth's consensus engine:
//!
//! - [`Driver::build_and_insert_block`] — build a new block via the
//!   custom EVM path, submit it via `new_payload`, then update the
//!   fork choice. The canonical entry point used by `step_*` to
//!   advance the L2 head.
//! - [`Driver::fork_choice_updated_with_retry`] — send a fork-choice
//!   update with exponential backoff on SYNCING. SYNCING is transient
//!   and needs retry, not bail.
//! - [`Driver::update_fork_choice`] — happy-path fork-choice update
//!   that mutates driver state only AFTER the engine confirms.
//!
//! The `build_derived_block` helper stays in mod.rs for now — it's
//! the payload-building half that cross-references state-root /
//! protocol-tx paths which have not yet been extracted. 2.1i or later
//! will move it out to `driver/protocol_txs.rs`.

use super::Driver;
use super::types::{
    BuiltBlock, FCU_SYNCING_INITIAL_BACKOFF_MS, FCU_SYNCING_MAX_RETRIES, FORK_CHOICE_DEPTH,
    compute_forkchoice_state,
};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadAttributes};
use eyre::{Result, WrapErr};
use reth_payload_primitives::EngineApiMessageVersion;
use reth_provider::{
    BlockHashReader, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use std::time::Duration;
use tracing::warn;

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
    /// Build the next sequential L2 block from derived transactions, submit it
    /// via `engine_newPayload`, and promote it via a fork-choice update.
    pub(super) async fn build_and_insert_block(
        &mut self,
        l2_block_number: u64,
        timestamp: u64,
        l1_block_hash: B256,
        l1_block_number: u64,
        derived_transactions: &Bytes,
    ) -> Result<BuiltBlock> {
        // Sanity check: we should be building the next sequential block
        let expected = self.l2_head_number.saturating_add(1);
        if l2_block_number != expected {
            return Err(eyre::eyre!(
                "expected sequential block {expected}, got {l2_block_number}",
            ));
        }

        let (built, execution_data) = self.build_derived_block(
            self.l2_head_number,
            timestamp,
            l1_block_hash,
            l1_block_number,
            derived_transactions,
        )?;

        // Submit to the engine via newPayload — reth re-executes the block.
        let status = self.engine.new_payload(execution_data).await?;

        if !status.is_valid() {
            eyre::bail!("newPayload rejected: {:?}", status);
        }

        // Update fork choice to accept the new head
        self.update_fork_choice(built.hash).await?;

        Ok(built)
    }

    /// Send a fork choice update with exponential-backoff retry on SYNCING.
    ///
    /// SYNCING is transient — the engine needs time to reconcile its state tree
    /// after blocks are unwound or rebuilt. Without retry, SYNCING causes the
    /// driver to bail and enter exponential backoff in the main loop.
    pub(super) async fn fork_choice_updated_with_retry(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<PayloadAttributes>,
    ) -> Result<ForkchoiceUpdated> {
        let mut backoff_ms = FCU_SYNCING_INITIAL_BACKOFF_MS;
        for attempt in 0..FCU_SYNCING_MAX_RETRIES {
            let fcu = self
                .engine
                .fork_choice_updated(
                    state,
                    payload_attrs.clone(),
                    EngineApiMessageVersion::default(),
                )
                .await
                .wrap_err("fork choice update failed")?;

            if fcu.is_valid() || fcu.is_invalid() {
                return Ok(fcu);
            }

            // SYNCING — retry with exponential backoff
            if attempt + 1 < FCU_SYNCING_MAX_RETRIES {
                warn!(
                    target: "based_rollup::driver",
                    attempt = attempt + 1,
                    max_retries = FCU_SYNCING_MAX_RETRIES,
                    backoff_ms,
                    "FCU returned SYNCING, retrying"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms *= 2;
            }
        }

        eyre::bail!(
            "engine stuck in SYNCING after {} retries",
            FCU_SYNCING_MAX_RETRIES
        );
    }

    /// Update fork choice state after inserting a new block.
    ///
    /// IMPORTANT: State mutations happen AFTER the engine confirms the fork choice
    /// update, not before. This prevents driver/engine desync if the engine rejects.
    pub(super) async fn update_fork_choice(&mut self, block_hash: B256) -> Result<()> {
        // Temporarily compute the forkchoice state with the new block hash
        // without mutating self yet.
        let mut tentative_hashes = self.block_hashes.clone();
        tentative_hashes.push_back(block_hash);
        if tentative_hashes.len() > FORK_CHOICE_DEPTH {
            tentative_hashes.pop_front();
        }
        let fcs = compute_forkchoice_state(block_hash, &tentative_hashes);

        let fcu = self.fork_choice_updated_with_retry(fcs, None).await?;

        if fcu.is_invalid() {
            eyre::bail!(
                "fork choice finalization rejected: {:?}",
                fcu.payload_status
            );
        }

        // Only mutate driver state after engine confirms success
        self.block_hashes = tentative_hashes;
        self.head_hash = block_hash;
        self.l2_head_number = self.l2_head_number.saturating_add(1);

        Ok(())
    }
}
