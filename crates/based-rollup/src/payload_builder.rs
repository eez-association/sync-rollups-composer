//! L1 block info types and builder protocol transaction encoding helpers.
//!
//! Provides the ABI encoding for L2Context.setContext() and the L1BlockInfo carrier struct.

use alloy_primitives::{B256, Bytes, U256};
use alloy_sol_types::{SolCall, sol};

sol! {
    /// L2Context.setContext(uint256,bytes32)
    function setContext(
        uint256 l1ParentBlockNumber,
        bytes32 l1ParentBlockHash
    );
}

/// Context from L1 needed to build an L2 block.
#[derive(Debug, Clone)]
pub struct L1BlockInfo {
    pub l1_block_number: u64,
    pub l1_block_hash: B256,
}

/// Encodes the calldata for the builder's L2Context.setContext() protocol transaction.
pub fn encode_set_context_calldata(l1_info: &L1BlockInfo) -> Bytes {
    let call = setContextCall {
        l1ParentBlockNumber: U256::from(l1_info.l1_block_number),
        l1ParentBlockHash: l1_info.l1_block_hash,
    };
    Bytes::from(call.abi_encode())
}

#[cfg(test)]
#[path = "payload_builder_tests.rs"]
mod tests;
