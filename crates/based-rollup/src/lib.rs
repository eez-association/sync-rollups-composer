//! A minimal based rollup built on reth.
//!
//! This crate provides custom consensus, EVM execution with builder protocol transactions,
//! L1 derivation, block building, and proposer logic for a based rollup architecture.

pub mod builder_sync;
pub mod config;
pub mod consensus;
pub mod cross_chain;
pub mod derivation;
pub mod driver;
pub mod evm_config;
pub mod execution_planner;
pub mod health;
pub mod composer_rpc;
pub mod payload_builder;
pub mod proposer;
pub mod rpc;
pub mod table_builder;

use crate::config::RollupConfig;
use crate::consensus::RollupConsensus;
use reth_node_builder::{BuilderContext, components::ConsensusBuilder, node::FullNodeTypes};
use std::sync::Arc;

/// Consensus builder that creates a [`RollupConsensus`] instance.
#[derive(Debug, Clone)]
pub struct RollupConsensusBuilder {
    config: Arc<RollupConfig>,
}

impl RollupConsensusBuilder {
    pub fn new(config: Arc<RollupConfig>) -> Self {
        Self { config }
    }
}

impl<Node> ConsensusBuilder<Node> for RollupConsensusBuilder
where
    Node: FullNodeTypes,
{
    type Consensus = Arc<RollupConsensus>;

    async fn build_consensus(self, _ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(RollupConsensus::new(self.config)))
    }
}
