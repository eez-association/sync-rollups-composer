//! Execution planner for synchronous composability.
//!
//! Takes a signed L2 transaction, simulates it against the current state,
//! and produces a [`CrossChainExecutionEntry`] ready for L1 submission via
//! `Rollups.postBatch()`.

use crate::config::RollupConfig;
use crate::cross_chain::{
    CrossChainAction, CrossChainActionType, CrossChainExecutionEntry, CrossChainStateDelta,
    ICrossChainManagerL2, RollupId,
};
use crate::evm_config::RollupEvmConfig;
use crate::rpc::{SimulationResult, entry_to_serializable};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, Bytes, I256, U256, keccak256};
use alloy_sol_types::SolType;
use eyre::{Result, WrapErr};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_primitives_traits::SignedTransaction;
use reth_provider::{BlockNumReader, HeaderProvider, StateProviderFactory};
use reth_revm::database::StateProviderDatabase;
use revm::database::State;
use tracing::debug;

/// Simulate a signed transaction and produce a [`SimulationResult`] with
/// state deltas and a pre-built execution entry.
///
/// This uses a read-only state snapshot — no canonical state is mutated.
pub fn simulate_transaction<P>(
    provider: &P,
    evm_config: &RollupEvmConfig,
    config: &RollupConfig,
    signed_tx_rlp: Bytes,
) -> Result<SimulationResult>
where
    P: StateProviderFactory + HeaderProvider<Header = alloy_consensus::Header> + BlockNumReader,
{
    use reth_evm::execute::BlockBuilder;

    // Decode the signed transaction from RLP
    let tx: reth_ethereum_primitives::TransactionSigned =
        alloy_rlp::Decodable::decode(&mut signed_tx_rlp.as_ref())
            .wrap_err("failed to RLP-decode signed transaction")?;

    let recovered = tx
        .clone()
        .try_into_recovered()
        .map_err(|_| eyre::eyre!("failed to recover signer from transaction"))?;

    // Get latest block header for state snapshot
    let best = provider
        .best_block_number()
        .wrap_err("failed to get latest block number")?;

    let parent_header = provider
        .sealed_header(best)
        .wrap_err("failed to get latest header")?
        .ok_or_else(|| eyre::eyre!("latest header not found"))?;

    let pre_state_root = parent_header.state_root();

    // Create read-only state snapshot
    let state_provider = provider
        .state_by_block_hash(parent_header.hash())
        .wrap_err("failed to get state provider")?;

    let state_db = StateProviderDatabase::new(state_provider.as_ref());
    let mut db = State::builder()
        .with_database(state_db)
        .with_bundle_update()
        .build();

    // Build a synthetic block containing only this transaction.
    // We use a timestamp one block_time ahead of the parent.
    let timestamp = parent_header.timestamp().saturating_add(config.block_time);

    // Use zero L1 context (simulation only — not canonical)
    let prev_randao = B256::ZERO;

    let attributes = NextBlockEnvAttributes {
        timestamp,
        suggested_fee_recipient: config.builder_address,
        prev_randao,
        gas_limit: parent_header.gas_limit(),
        parent_beacon_block_root: Some(B256::ZERO),
        withdrawals: Some(Default::default()),
        extra_data: Default::default(),
    };

    // Create an isolated evm_config for simulation.
    let sim_evm_config = evm_config.isolated_clone();

    let mut builder = sim_evm_config
        .builder_for_next_block(&mut db, &parent_header, attributes)
        .wrap_err("failed to create block builder")?;

    // Apply pre-execution changes (only beacon root EIP-4788 — no system calls)
    builder
        .apply_pre_execution_changes()
        .wrap_err("pre-execution changes failed")?;

    // Execute the transaction
    let exec_result = builder.execute_transaction(recovered);

    let (success, gas_used, return_data) = match exec_result {
        Ok(gas) => (true, gas, Bytes::default()),
        Err(err) => {
            debug!(
                target: "based_rollup::execution_planner",
                %err,
                "transaction simulation reverted"
            );
            // Transaction reverted — still produce a result
            (false, 0, Bytes::from(format!("{err}").into_bytes()))
        }
    };

    // Finish the block to compute the post-execution state root
    let outcome = builder
        .finish(state_provider.as_ref())
        .wrap_err("block builder finish failed")?;

    let post_state_root = outcome.block.sealed_block().sealed_header().state_root();

    // Compute the action hash (L2TX action wrapping the raw transaction).
    // Use encode_list for consistency with build_entries_for_block, which encodes
    // the full transaction list. For a single-tx simulation, this wraps the tx in a list.
    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(&[tx], &mut rlp_buf);
    let action_hash = compute_l2tx_action_hash(config.rollup_id, &rlp_buf);

    // Build the execution entry
    let entry = CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: RollupId::new(U256::from(config.rollup_id)),
            current_state: pre_state_root,
            new_state: post_state_root,
            ether_delta: I256::ZERO,
        }],
        action_hash,
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: RollupId::new(U256::from(config.rollup_id)),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: return_data.to_vec(),
            failed: !success,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: vec![],
        },
    };

    Ok(SimulationResult {
        success,
        gas_used,
        return_data: Bytes::from(entry.next_action.data.clone()),
        pre_state_root,
        post_state_root,
        action_hash,
        execution_entry: entry_to_serializable(&entry),
    })
}

/// Compute the action hash for an L2TX action wrapping the given RLP-encoded transaction.
///
/// Matches Solidity: `keccak256(abi.encode(Action({ actionType: L2TX, data: rlpTx, ... })))`
pub fn compute_l2tx_action_hash(rollup_id: u64, rlp_tx: &[u8]) -> B256 {
    let sol_action = ICrossChainManagerL2::Action {
        actionType: ICrossChainManagerL2::ActionType::L2TX,
        rollupId: U256::from(rollup_id),
        destination: Address::ZERO,
        value: U256::ZERO,
        data: rlp_tx.to_vec().into(),
        failed: false,
        sourceAddress: Address::ZERO,
        sourceRollup: U256::ZERO,
        scope: vec![],
    };

    keccak256(ICrossChainManagerL2::Action::abi_encode(&sol_action))
}

/// Simulate a simple contract call against current L2 state and return
/// `(success, return_data)`.
///
/// This is used by `initiate_cross_chain_call` to predict the return data
/// from a cross-chain call so that the RESULT entry's action hash matches
/// what `CrossChainManagerL2._processCallAtScope()` will compute after
/// actually executing the call.
///
/// The simulation is a read-only `eth_call`-style execution — no state is
/// committed.
pub fn simulate_call<P>(
    provider: &P,
    evm_config: &RollupEvmConfig,
    destination: Address,
    data: Vec<u8>,
) -> Result<(bool, Vec<u8>)>
where
    P: StateProviderFactory + HeaderProvider<Header = alloy_consensus::Header> + BlockNumReader,
{
    use alloy_evm::EvmFactory;
    use reth_evm::ConfigureEvm;

    // Get latest block header for state snapshot
    let best = provider
        .best_block_number()
        .wrap_err("failed to get latest block number")?;

    let parent_header = provider
        .sealed_header(best)
        .wrap_err("failed to get latest header")?
        .ok_or_else(|| eyre::eyre!("latest header not found"))?;

    // Create read-only state snapshot
    let state_provider = provider
        .state_by_block_hash(parent_header.hash())
        .wrap_err("failed to get state provider")?;

    let state_db = StateProviderDatabase::new(state_provider.as_ref());

    // Get the EVM environment from the current block header
    let evm_env = evm_config
        .evm_env(&parent_header)
        .wrap_err("failed to create EVM environment")?;

    // Create an EVM using the factory
    let evm_factory = evm_config.evm_factory();
    let mut evm = evm_factory.create_evm(state_db, evm_env);

    // Use transact_system_call to bypass balance/nonce/gas-price validation.
    // This is a read-only simulation — we only care about the return data.
    use alloy_evm::Evm;

    let calldata = alloy_primitives::Bytes::from(data);
    let result = evm
        .transact_system_call(Address::ZERO, destination, calldata)
        .map_err(|e| eyre::eyre!("EVM call simulation failed: {e}"))?;

    use revm::context_interface::result::{ExecutionResult, Output};

    match result.result {
        ExecutionResult::Success { output, .. } => {
            let return_data = match output {
                Output::Call(data) => data.to_vec(),
                Output::Create(data, _) => data.to_vec(),
            };
            Ok((true, return_data))
        }
        ExecutionResult::Revert { output, .. } => Ok((false, output.to_vec())),
        ExecutionResult::Halt { .. } => Ok((false, vec![])),
    }
}

/// Build execution entries for a set of transactions in a built block.
///
/// This is used by the builder to produce entries for L1 submission after
/// building a block. Unlike `simulate_transaction`, this uses the actual
/// pre/post state roots from the canonical block.
pub fn build_entries_for_block(
    rollup_id: u64,
    pre_state_root: B256,
    post_state_root: B256,
    transactions: &[reth_ethereum_primitives::TransactionSigned],
) -> Vec<CrossChainExecutionEntry> {
    if transactions.is_empty() {
        return vec![];
    }

    // One entry per block covering all transactions.
    // The action_hash covers the entire block's transaction list.
    let mut rlp_buf = Vec::new();
    alloy_rlp::encode_list(transactions, &mut rlp_buf);
    let action_hash = compute_l2tx_action_hash(rollup_id, &rlp_buf);

    vec![CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: RollupId::new(U256::from(rollup_id)),
            current_state: pre_state_root,
            new_state: post_state_root,
            ether_delta: I256::ZERO,
        }],
        action_hash,
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: RollupId::new(U256::from(rollup_id)),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: vec![],
        },
    }]
}

/// Build execution entries for a block using pre-encoded RLP transaction bytes.
///
/// This avoids re-encoding when the caller already has the RLP bytes (e.g.,
/// the driver stores `encoded_transactions` on `BuiltBlock`). Returns an
/// empty vec if `encoded_transactions` is empty.
pub fn build_entries_from_encoded(
    rollup_id: u64,
    pre_state_root: B256,
    post_state_root: B256,
    encoded_transactions: &[u8],
) -> Vec<CrossChainExecutionEntry> {
    if encoded_transactions.is_empty() {
        return vec![];
    }

    let action_hash = compute_l2tx_action_hash(rollup_id, encoded_transactions);

    vec![CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: RollupId::new(U256::from(rollup_id)),
            current_state: pre_state_root,
            new_state: post_state_root,
            ether_delta: I256::ZERO,
        }],
        action_hash,
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: RollupId::new(U256::from(rollup_id)),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: vec![],
        },
    }]
}

/// Build a state-transition-only entry for an empty block (no user transactions).
///
/// Empty blocks still change the state root due to builder protocol transactions (L2Context,
/// deposits, etc.). To keep the on-chain state root chain unbroken, we submit
/// an entry with `actionHash = bytes32(0)`, which the Rollups contract applies
/// **immediately** (no deferred matching required). This ensures the on-chain
/// stateRoot advances even when there are no user transactions.
///
/// Returns empty vec if pre and post state roots are equal (truly no-op).
pub fn build_state_only_entry(
    rollup_id: u64,
    pre_state_root: B256,
    post_state_root: B256,
) -> Vec<CrossChainExecutionEntry> {
    if pre_state_root == post_state_root {
        return vec![];
    }

    // actionHash = 0 triggers immediate state delta application in Rollups.sol
    // (see postBatch line 217: `if (entries[i].actionHash == bytes32(0))`)
    vec![CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: RollupId::new(U256::from(rollup_id)),
            current_state: pre_state_root,
            new_state: post_state_root,
            ether_delta: I256::ZERO,
        }],
        action_hash: B256::ZERO,
        next_action: CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: RollupId::new(U256::from(rollup_id)),
            destination: Address::ZERO,
            value: U256::ZERO,
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: vec![],
        },
    }]
}

#[cfg(test)]
#[path = "execution_planner_tests.rs"]
mod tests;
