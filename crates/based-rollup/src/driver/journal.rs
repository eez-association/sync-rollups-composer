//! Transaction replay journal + L1-confirmed anchor persistence.
//!
//! Extracted from `driver/mod.rs` in refactor step 2.1f. This module
//! owns the six `impl Driver` methods that read and write the two
//! driver-local stage checkpoints:
//!
//! - `tx_journal` — the persistent transaction replay journal used
//!   for crash recovery.
//! - `l1_confirmed_anchor` — the efficient rewind anchor that records
//!   the last `(l2_block_number, l1_block_number)` pair confirmed on
//!   L1.
//!
//! The methods are declared `pub(super)` so they can be called from
//! the `impl Driver` blocks in other sibling modules (currently just
//! mod.rs).

use super::Driver;
use super::types::{L1ConfirmedAnchor, TX_JOURNAL_STAGE_ID, TxJournalEntry};
use crate::derivation::{L1_CONFIRMED_L1_STAGE_ID, L1_CONFIRMED_L2_STAGE_ID};
use alloy_primitives::Bytes;
use reth_primitives_traits::SignerRecoverable;
use reth_provider::{
    BlockHashReader, BlockNumReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StageCheckpointWriter, StateProviderFactory, TransactionsProvider,
};
use reth_stages_types::StageCheckpoint;
use tracing::{debug, info, warn};

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
    /// Append the block's transactions to the replay journal and persist.
    pub(super) fn journal_block_transactions(
        &mut self,
        l2_block_number: u64,
        encoded_transactions: &Bytes,
    ) {
        self.tx_journal.push(TxJournalEntry {
            l2_block_number,
            block_txs: encoded_transactions.to_vec(),
        });
        self.save_tx_journal();
    }

    /// Persist the transaction journal to the L2 database.
    pub(super) fn save_tx_journal(&self) {
        let data = TxJournalEntry::encode_all(&self.tx_journal);
        let rw = match self.l2_provider.database_provider_rw() {
            Ok(rw) => rw,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    "failed to open DB for tx journal save"
                );
                return;
            }
        };
        if let Err(err) = rw.save_stage_checkpoint_progress(TX_JOURNAL_STAGE_ID, data) {
            warn!(
                target: "based_rollup::driver",
                %err,
                "failed to save tx journal"
            );
            return;
        }
        if let Err(err) = rw.commit() {
            warn!(
                target: "based_rollup::driver",
                %err,
                "failed to commit tx journal"
            );
        }
    }

    /// Load the transaction journal from the L2 database (crash recovery).
    ///
    /// Entries for blocks above the canonical head represent transactions from
    /// blocks that were being reverted when a crash occurred. These are decoded
    /// and placed in `pending_reinjection` for deferred re-injection.
    pub(super) fn load_tx_journal(&mut self) {
        let data = match self
            .l2_provider
            .get_stage_checkpoint_progress(TX_JOURNAL_STAGE_ID)
        {
            Ok(Some(data)) => data,
            Ok(None) => return,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    "failed to load tx journal"
                );
                return;
            }
        };

        let entries = TxJournalEntry::decode_all(&data);
        if entries.is_empty() {
            return;
        }

        // Entries for blocks > canonical head need re-injection (crash recovery).
        let mut recovered = 0usize;
        for entry in &entries {
            if entry.l2_block_number > self.l2_head_number {
                let txs: Vec<reth_ethereum_primitives::TransactionSigned> =
                    match alloy_rlp::Decodable::decode(&mut entry.block_txs.as_slice()) {
                        Ok(txs) => txs,
                        Err(_) => continue,
                    };
                for tx in txs {
                    match tx.recover_signer() {
                        Ok(sender) => {
                            // Skip builder's protocol transactions.
                            if sender == self.config.builder_address {
                                continue;
                            }
                            self.pending_reinjection.push((sender, tx));
                            recovered += 1;
                        }
                        Err(_) => continue,
                    }
                }
            }
        }

        // Keep only entries for blocks <= canonical head.
        self.tx_journal = entries
            .into_iter()
            .filter(|e| e.l2_block_number <= self.l2_head_number)
            .collect();

        if recovered > 0 {
            info!(
                target: "based_rollup::driver",
                recovered,
                journal_size = self.tx_journal.len(),
                "recovered transactions from journal for re-injection (crash recovery)"
            );
            // Persist the cleaned journal (without the crash-recovery entries).
            self.save_tx_journal();
        }
    }

    /// Prune journal entries for L1-confirmed blocks.
    pub(super) fn prune_tx_journal(&mut self, confirmed_l2_block: u64) {
        let before = self.tx_journal.len();
        self.tx_journal
            .retain(|e| e.l2_block_number > confirmed_l2_block);
        let pruned = before - self.tx_journal.len();
        if pruned > 0 {
            self.save_tx_journal();
            debug!(
                target: "based_rollup::driver",
                pruned,
                remaining = self.tx_journal.len(),
                confirmed_l2_block,
                "pruned confirmed entries from tx journal"
            );
        }
    }

    /// Persist the most recent L1-confirmed anchor to the L2 database.
    pub(super) fn save_l1_confirmed_anchor(&self) {
        let Some(anchor) = self.l1_confirmed_anchor else {
            return;
        };
        let rw = match self.l2_provider.database_provider_rw() {
            Ok(rw) => rw,
            Err(err) => {
                warn!(
                    target: "based_rollup::driver",
                    %err,
                    "failed to open DB for L1-confirmed anchor save"
                );
                return;
            }
        };
        if let Err(err) = rw.save_stage_checkpoint(
            L1_CONFIRMED_L2_STAGE_ID,
            StageCheckpoint::new(anchor.l2_block_number),
        ) {
            warn!(target: "based_rollup::driver", %err, "failed to save L1-confirmed L2 anchor");
            return;
        }
        if let Err(err) = rw.save_stage_checkpoint(
            L1_CONFIRMED_L1_STAGE_ID,
            StageCheckpoint::new(anchor.l1_block_number),
        ) {
            warn!(target: "based_rollup::driver", %err, "failed to save L1-confirmed L1 anchor");
            return;
        }
        if let Err(err) = rw.commit() {
            warn!(target: "based_rollup::driver", %err, "failed to commit L1-confirmed anchor");
            return;
        }
        info!(
            target: "based_rollup::driver",
            l2_block = anchor.l2_block_number,
            l1_block = anchor.l1_block_number,
            "recorded L1-confirmed anchor"
        );
    }

    /// Load the L1-confirmed anchor from the L2 database.
    pub(super) fn load_l1_confirmed_anchor(&mut self) {
        let l2_cp = match self
            .l2_provider
            .get_stage_checkpoint(L1_CONFIRMED_L2_STAGE_ID)
        {
            Ok(Some(cp)) => cp.block_number,
            _ => return,
        };
        let l1_cp = match self
            .l2_provider
            .get_stage_checkpoint(L1_CONFIRMED_L1_STAGE_ID)
        {
            Ok(Some(cp)) => cp.block_number,
            _ => return,
        };
        self.l1_confirmed_anchor = Some(L1ConfirmedAnchor {
            l2_block_number: l2_cp,
            l1_block_number: l1_cp,
        });
        info!(
            target: "based_rollup::driver",
            l2_block = l2_cp,
            l1_block = l1_cp,
            "loaded L1-confirmed anchor from DB"
        );
    }
}
