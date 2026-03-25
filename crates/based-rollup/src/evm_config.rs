//! Custom EVM configuration for the based rollup.
//!
//! Wraps the standard Ethereum block executor. System calls have been replaced
//! by builder-signed transactions — all protocol operations (setContext,
//! loadExecutionTable, executeIncomingCrossChainCall) are now normal
//! transactions in the block body. Only Ethereum's beacon root contract
//! (EIP-4788) remains as an implicit pre-execution call.
//!
//! The CCM receives a large ETH pre-mint in genesis, so no runtime deposit
//! minting is needed — the executor is a thin delegation wrapper.

use crate::config::RollupConfig;
use alloy_evm::{
    EthEvm, EthEvmFactory,
    block::{BlockExecutorFactory, BlockExecutorFor, ExecutableTx},
    eth::{EthBlockExecutionCtx, EthBlockExecutor, EthTxResult},
    precompiles::PrecompilesMap,
};
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned, TxType};
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, Evm, EvmEnv, EvmEnvFor, ExecutionCtxFor, InspectorFor,
    NextBlockEnvAttributes, OnStateHook,
    block::StateDB,
    execute::{BlockExecutionError, BlockExecutor},
};
use reth_evm_ethereum::{EthBlockAssembler, EthEvmConfig, RethReceiptBuilder};
use reth_node_builder::{BuilderContext, components::ExecutorBuilder, node::FullNodeTypes};
use reth_primitives_traits::SealedHeader;
use reth_provider::BlockExecutionResult;
use revm::context::TxEnv;
use std::sync::Arc;

use alloy_rpc_types_engine::ExecutionData;
use reth_evm::ExecutableTxIterator;
use reth_node_api::NodeTypes;
use revm::primitives::hardfork::SpecId;

/// Custom EVM config that wraps Ethereum's.
///
/// Protocol operations (setContext, loadExecutionTable, executeIncomingCrossChainCall)
/// are builder-signed transactions in the block body. Only Ethereum's beacon root
/// contract (EIP-4788) runs as a pre-execution change. The CCM receives a large
/// ETH pre-mint in genesis, so no runtime deposit minting is needed.
#[derive(Debug, Clone)]
pub struct RollupEvmConfig {
    inner: EthEvmConfig,
    /// Retained for future use (e.g., custom gas pricing, EIP overrides).
    #[allow(dead_code)]
    rollup_config: Arc<RollupConfig>,
}

impl RollupEvmConfig {
    pub fn new(chain_spec: Arc<ChainSpec>, rollup_config: Arc<RollupConfig>) -> Self {
        Self {
            inner: EthEvmConfig::new(chain_spec),
            rollup_config,
        }
    }

    /// Create an isolated copy of this config with fresh (unshared) state.
    /// Used by the execution planner for simulations.
    pub fn isolated_clone(&self) -> Self {
        Self {
            inner: EthEvmConfig::new(self.inner.chain_spec().clone()),
            rollup_config: self.rollup_config.clone(),
        }
    }

    /// Access the chain spec for chain_id lookup etc.
    pub fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.inner.chain_spec()
    }
}

/// Custom executor builder that creates a [`RollupEvmConfig`].
#[derive(Debug, Clone)]
pub struct RollupExecutorBuilder {
    rollup_config: Arc<RollupConfig>,
}

impl RollupExecutorBuilder {
    pub fn new(rollup_config: Arc<RollupConfig>) -> Self {
        Self { rollup_config }
    }
}

impl<Types, Node> ExecutorBuilder<Node> for RollupExecutorBuilder
where
    Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = RollupEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(RollupEvmConfig::new(ctx.chain_spec(), self.rollup_config))
    }
}

// --- BlockExecutorFactory ---

impl BlockExecutorFactory for RollupEvmConfig {
    type EvmFactory = EthEvmFactory;
    type ExecutionCtx<'a> = EthBlockExecutionCtx<'a>;
    type Transaction = TransactionSigned;
    type Receipt = reth_ethereum_primitives::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: EthEvm<DB, I, PrecompilesMap>,
        ctx: EthBlockExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: StateDB + 'a,
        I: InspectorFor<Self, DB> + 'a,
    {
        RollupBlockExecutor {
            inner: EthBlockExecutor::new(
                evm,
                ctx,
                self.inner.chain_spec(),
                self.inner.executor_factory.receipt_builder(),
            ),
        }
    }
}

// --- ConfigureEvm ---

impl ConfigureEvm for RollupEvmConfig {
    type Primitives = <EthEvmConfig as ConfigureEvm>::Primitives;
    type Error = <EthEvmConfig as ConfigureEvm>::Error;
    type NextBlockEnvCtx = <EthEvmConfig as ConfigureEvm>::NextBlockEnvCtx;
    type BlockExecutorFactory = Self;
    type BlockAssembler = EthBlockAssembler<ChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(&self, header: &alloy_consensus::Header) -> Result<EvmEnv<SpecId>, Self::Error> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &alloy_consensus::Header,
        attributes: &NextBlockEnvAttributes,
    ) -> Result<EvmEnv<SpecId>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a reth_primitives_traits::SealedBlock<reth_ethereum_primitives::Block>,
    ) -> Result<EthBlockExecutionCtx<'a>, Self::Error> {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<EthBlockExecutionCtx<'_>, Self::Error> {
        self.inner.context_for_next_block(parent, attributes)
    }
}

// --- ConfigureEngineEvm ---

impl ConfigureEngineEvm<ExecutionData> for RollupEvmConfig {
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        self.inner.tx_iterator_for_payload(payload)
    }
}

// --- Custom Block Executor ---

/// Block executor that wraps [`EthBlockExecutor`].
///
/// Delegates all operations to the inner `EthBlockExecutor`. The CCM receives
/// a large ETH pre-mint in genesis, so no runtime deposit minting is needed.
/// All protocol operations (setContext, loadExecutionTable, executeIncomingCrossChainCall)
/// are builder-signed transactions in the block body.
pub struct RollupBlockExecutor<'a, Evm> {
    inner: EthBlockExecutor<'a, Evm, &'a Arc<ChainSpec>, &'a RethReceiptBuilder>,
}

impl<E> BlockExecutor for RollupBlockExecutor<'_, E>
where
    E: Evm<DB: StateDB, Tx = TxEnv>,
{
    type Transaction = TransactionSigned;
    type Receipt = reth_ethereum_primitives::Receipt;
    type Evm = E;
    type Result = EthTxResult<E::HaltReason, TxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        self.inner.execute_transaction_without_commit(tx)
    }

    fn commit_transaction(&mut self, output: Self::Result) -> Result<u64, BlockExecutionError> {
        self.inner.commit_transaction(output)
    }

    fn finish(
        self,
    ) -> Result<
        (
            Self::Evm,
            BlockExecutionResult<reth_ethereum_primitives::Receipt>,
        ),
        BlockExecutionError,
    > {
        self.inner.finish()
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.inner.set_state_hook(hook)
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }
}

#[cfg(test)]
#[path = "evm_config_tests.rs"]
mod tests;
