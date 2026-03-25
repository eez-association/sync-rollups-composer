//! WebSocket subscription for receiving preconfirmation blocks from the builder.
//!
//! Fullnodes use this to get low-latency block updates before L1 confirmation.

use alloy_primitives::B256;
use alloy_provider::{Provider, ProviderBuilder};
use eyre::{Result, WrapErr};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// A preconfirmed block received from the builder via WebSocket.
#[derive(Debug, Clone)]
pub struct PreconfirmedBlock {
    pub block_number: u64,
    pub block_hash: B256,
}

/// Connects to the builder's WebSocket endpoint and streams new blocks.
///
/// On each new block header, records the block number and hash and sends
/// them through a channel for the driver to process.
pub struct BuilderSync {
    ws_url: String,
}

impl BuilderSync {
    pub fn new(ws_url: String) -> Self {
        Self { ws_url }
    }

    /// Run the sync loop with automatic reconnection.
    ///
    /// Reconnects with exponential backoff on connection failures or stream
    /// disconnections. Only returns when the channel is closed (driver shutdown).
    pub async fn run(self, tx: mpsc::Sender<PreconfirmedBlock>) -> Result<()> {
        let mut backoff_secs = 1u64;
        const MAX_BACKOFF_SECS: u64 = 60;

        loop {
            match self.run_once(&tx).await {
                Ok(()) => {
                    // Channel closed — driver is shutting down
                    return Ok(());
                }
                Err(err) => {
                    // Reset backoff if the error indicates a stream drop (was connected)
                    // vs a connection failure (never connected).
                    let was_connected = err.to_string().contains("stream ended");
                    if was_connected {
                        backoff_secs = 1;
                    }
                    warn!(
                        target: "based_rollup::builder_sync",
                        %err,
                        backoff_secs,
                        "builder WS connection failed, reconnecting"
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                }
            }
        }
    }

    /// Single connection attempt. Returns Ok(()) if channel is closed,
    /// Err on connection/stream failure.
    async fn run_once(&self, tx: &mpsc::Sender<PreconfirmedBlock>) -> Result<()> {
        info!(
            target: "based_rollup::builder_sync",
            url = %self.ws_url,
            "connecting to builder WebSocket"
        );

        let provider = ProviderBuilder::new()
            .connect_ws(alloy_provider::WsConnect::new(&self.ws_url))
            .await
            .wrap_err("failed to connect to builder WS")?;

        let sub = provider.subscribe_blocks().await?;
        let mut stream = sub.into_stream();

        info!(
            target: "based_rollup::builder_sync",
            "subscribed to builder newHeads"
        );

        while let Some(header) = stream.next().await {
            let block_number = header.inner.number;
            let block_hash = header.hash;

            debug!(
                target: "based_rollup::builder_sync",
                block_number,
                %block_hash,
                "received new head from builder"
            );

            let preconfirmed = PreconfirmedBlock {
                block_number,
                block_hash,
            };

            match tx.try_send(preconfirmed) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        target: "based_rollup::builder_sync",
                        block_number,
                        "preconfirmation channel full, dropping block"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    info!(
                        target: "based_rollup::builder_sync",
                        "channel closed, stopping builder sync"
                    );
                    return Ok(());
                }
            }
        }

        // Stream ended — connection was lost, trigger reconnect
        Err(eyre::eyre!("builder WebSocket stream ended"))
    }
}

#[cfg(test)]
#[path = "builder_sync_tests.rs"]
mod tests;
