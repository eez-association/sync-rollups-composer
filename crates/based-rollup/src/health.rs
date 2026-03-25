//! Lightweight HTTP health endpoint for monitoring and Docker healthchecks.
//!
//! Serves JSON status on a configurable port using raw TCP to avoid extra dependencies.

use std::fmt;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Git commit hash baked in at build time. Falls back to `"unknown"` when git is unavailable.
const GIT_COMMIT: &str = match option_env!("GIT_COMMIT_HASH") {
    Some(hash) => hash,
    None => "unknown",
};

/// Maximum number of consecutive rewind cycles before reporting unhealthy.
const MAX_REWIND_CYCLES: u32 = 10;

/// Maximum duration without L2 head advancement before reporting unhealthy (120s = 10 L2 blocks).
const STALENESS_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(120);

/// Snapshot of the rollup node's health status.
#[derive(Debug, Clone)]
pub struct HealthStatus {
    /// Current driver operating mode (Sync, Builder, Fullnode).
    pub mode: String,
    /// Latest L2 block number at the tip of the chain.
    pub l2_head: u64,
    /// Latest L1 block processed by the derivation pipeline.
    pub l1_derivation_head: u64,
    /// Number of blocks built locally but not yet submitted to L1.
    pub pending_submissions: usize,
    /// Number of consecutive Builder→Sync rewind cycles (0 = healthy).
    pub consecutive_rewind_cycles: u32,
    /// Timestamp of the last time `l2_head` advanced. `None` means just started.
    pub last_l2_head_advance: Option<Instant>,
}

impl HealthStatus {
    /// Returns `true` when the node is operating normally.
    ///
    /// Unhealthy when:
    /// - `consecutive_rewind_cycles` exceeds [`MAX_REWIND_CYCLES`], or
    /// - `l2_head` has not advanced for longer than [`STALENESS_THRESHOLD`] (and we have
    ///   observed at least one advance, so fresh starts are not penalised).
    pub fn is_healthy(&self) -> bool {
        if self.consecutive_rewind_cycles > MAX_REWIND_CYCLES {
            return false;
        }
        if let Some(last_advance) = self.last_l2_head_advance {
            if last_advance.elapsed() > STALENESS_THRESHOLD {
                return false;
            }
        }
        true
    }
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self {
            mode: "Sync".to_string(),
            l2_head: 0,
            l1_derivation_head: 0,
            pending_submissions: 0,
            consecutive_rewind_cycles: 0,
            last_l2_head_advance: None,
        }
    }
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Escape backslashes first, then double-quotes and control characters,
        // to produce valid JSON even with unexpected mode strings.
        let escaped_mode = self
            .mode
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        let healthy = self.is_healthy();
        write!(
            f,
            r#"{{"healthy":{},"mode":"{}","l2_head":{},"l1_derivation_head":{},"pending_submissions":{},"consecutive_rewind_cycles":{},"commit":"{}"}}"#,
            healthy,
            escaped_mode,
            self.l2_head,
            self.l1_derivation_head,
            self.pending_submissions,
            self.consecutive_rewind_cycles,
            GIT_COMMIT
        )
    }
}

/// Run the health HTTP server, responding to every request with the latest status.
///
/// This uses raw TCP + manual HTTP response formatting to avoid adding new
/// crate dependencies (no hyper/axum needed).
pub async fn run_health_server(
    port: u16,
    status_rx: watch::Receiver<HealthStatus>,
) -> eyre::Result<()> {
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).await?;

    tracing::info!(
        target: "based_rollup::health",
        %addr,
        "health server listening"
    );

    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                tracing::warn!(
                    target: "based_rollup::health",
                    %err,
                    "accept error — continuing"
                );
                // Brief backoff to prevent CPU-saturating spin on persistent errors
                // (e.g., file descriptor exhaustion).
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        // Read (and discard) the HTTP request before responding
        let mut buf = [0u8; 1024];
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), socket.read(&mut buf)).await;
        let status = status_rx.borrow().clone();
        let json = status.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            json.len(),
            json
        );
        // Best-effort write with timeout; don't crash if a client disconnects or stalls
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.write_all(response.as_bytes()),
        )
        .await;
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
