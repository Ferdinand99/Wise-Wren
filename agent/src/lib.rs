//! `wren-agent`: runs on each Docker host and connects **out** to Wise Wren
//! (docs/design.md §3, §8). In phase 2 it checks its `agent.toml`, reads
//! Docker's `/version` and `/info` and sends a heartbeat every 30 seconds;
//! it changes nothing and takes no orders.

#![forbid(unsafe_code)]

pub mod config;
pub mod docker;
pub mod heartbeat;

use std::path::Path;
use std::time::Duration;

use config::Config;
use heartbeat::{Client, SendError};

/// Longest wait between heartbeats when Wren can't be reached.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The wait after Wren refused the token (it may be fixed on the server
/// without restarting the agent: a revoked token stays refused, though).
pub const REFUSED_WAIT: Duration = Duration::from_secs(5 * 60);

fn backoff(failures: u32) -> Duration {
    let secs = 5u64.saturating_mul(1 << failures.min(4));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// Sends heartbeats until `stop` resolves.
pub async fn run(
    config: Config,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    let client = Client::new(&config.server, &config.token)?;
    tracing::info!(
        version = heartbeat::VERSION,
        server = %config.server,
        roots = config.roots.len(),
        script_stacks = config.script_stacks.len(),
        "wren-agent started"
    );
    tokio::pin!(stop);
    let mut connected = false;
    let mut failures = 0u32;
    loop {
        let beat = heartbeat::collect(&config.docker_socket, Path::new("/")).await;
        if !beat.docker_error.is_empty() {
            tracing::warn!(error = %beat.docker_error, "can't read Docker");
        }
        let wait = match client.send(&beat).await {
            Ok(reply) => {
                if !connected {
                    tracing::info!(server = %reply.server.name, "connected to Wren");
                    connected = true;
                }
                failures = 0;
                Duration::from_secs(reply.interval_seconds.clamp(5, 300))
            }
            Err(SendError::Refused(msg)) => {
                tracing::error!("{msg}");
                connected = false;
                REFUSED_WAIT
            }
            Err(SendError::Failed(msg)) => {
                failures += 1;
                let wait = backoff(failures);
                tracing::warn!(retry_in = ?wait, "{msg}");
                connected = false;
                wait
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = &mut stop => {
                tracing::info!("stopped");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_to_a_minute() {
        assert_eq!(backoff(1), Duration::from_secs(10));
        assert_eq!(backoff(3), Duration::from_secs(40));
        assert_eq!(backoff(9), MAX_BACKOFF);
    }
}
