//! Custom consensus implementation validating deterministic L2 timestamps.
//!
//! L1 is the real consensus layer; this module only enforces the timestamp formula.

use crate::config::RollupConfig;
use reth_consensus::{
    Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom, TransactionRoot,
};
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{
    Block, BlockHeader, NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader,
};
// AlloyBlockHeader provides number(), timestamp() etc. on BlockHeader implementors
use alloy_consensus::BlockHeader as _;

use std::sync::Arc;

/// Consensus implementation for the based rollup.
///
/// Validates that block timestamps match the deterministic formula:
///   timestamp = deployment_timestamp + ((block_number + 1) * block_time)
#[derive(Debug, Clone)]
pub struct RollupConsensus {
    config: Arc<RollupConfig>,
}

impl RollupConsensus {
    pub fn new(config: Arc<RollupConfig>) -> Self {
        Self { config }
    }

    fn expected_timestamp(&self, block_number: u64) -> Result<u64, ConsensusError> {
        self.config
            .l2_timestamp_checked(block_number)
            .ok_or(ConsensusError::TimestampIsInFuture {
                timestamp: u64::MAX,
                present_timestamp: 0,
            })
    }
}

impl<H: BlockHeader> HeaderValidator<H> for RollupConsensus {
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        let expected_ts = self.expected_timestamp(header.number())?;
        if header.timestamp() != expected_ts {
            return Err(ConsensusError::TimestampIsInFuture {
                timestamp: header.timestamp(),
                present_timestamp: expected_ts,
            });
        }
        Ok(())
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        if header.parent_hash() != parent.hash() {
            return Err(ConsensusError::ParentBlockNumberMismatch {
                parent_block_number: parent.number(),
                block_number: header.number(),
            });
        }

        let expected_child =
            parent
                .number()
                .checked_add(1)
                .ok_or(ConsensusError::ParentBlockNumberMismatch {
                    parent_block_number: parent.number(),
                    block_number: header.number(),
                })?;
        if header.number() != expected_child {
            return Err(ConsensusError::ParentBlockNumberMismatch {
                parent_block_number: parent.number(),
                block_number: header.number(),
            });
        }

        let expected_ts = self.expected_timestamp(header.number())?;
        if header.timestamp() != expected_ts {
            return Err(ConsensusError::TimestampIsInFuture {
                timestamp: header.timestamp(),
                present_timestamp: expected_ts,
            });
        }

        Ok(())
    }
}

impl<B: Block<Header: BlockHeader>> Consensus<B> for RollupConsensus {
    fn validate_body_against_header(
        &self,
        _body: &B::Body,
        _header: &SealedHeader<B::Header>,
    ) -> Result<(), ConsensusError> {
        Ok(())
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<B>) -> Result<(), ConsensusError> {
        let header = block.header();

        // Validate deterministic timestamp
        let expected_ts = self.expected_timestamp(header.number())?;
        if header.timestamp() != expected_ts {
            return Err(ConsensusError::TimestampIsInFuture {
                timestamp: header.timestamp(),
                present_timestamp: expected_ts,
            });
        }

        // Post-merge: difficulty must be 0
        if !header.difficulty().is_zero() {
            return Err(ConsensusError::TheMergeDifficultyIsNotZero);
        }

        // Post-merge: nonce must be 0
        if let Some(nonce) = header.nonce() {
            if !nonce.is_zero() {
                return Err(ConsensusError::TheMergeNonceIsNotZero);
            }
        }

        // Extra data must be empty for deterministic block building.
        // All nodes must produce identical blocks, so no node-specific data is allowed.
        if !header.extra_data().is_empty() {
            return Err(ConsensusError::ExtraDataExceedsMax {
                len: header.extra_data().len(),
            });
        }

        // Gas used must not exceed gas limit
        if header.gas_used() > header.gas_limit() {
            return Err(ConsensusError::HeaderGasUsedExceedsGasLimit {
                gas_used: header.gas_used(),
                gas_limit: header.gas_limit(),
            });
        }

        Ok(())
    }

    fn validate_block_pre_execution_with_tx_root(
        &self,
        block: &SealedBlock<B>,
        _transaction_root: Option<TransactionRoot>,
    ) -> Result<(), ConsensusError> {
        self.validate_block_pre_execution(block)
    }
}

impl<N: NodePrimitives> FullConsensus<N> for RollupConsensus {
    fn validate_block_post_execution(
        &self,
        _block: &RecoveredBlock<N::Block>,
        _result: &BlockExecutionResult<N::Receipt>,
        _receipt_root_bloom: Option<ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "consensus_tests.rs"]
mod tests;
