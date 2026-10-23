//! `wren-agent`: runs on each Docker host and connects **out** to Wise Wren
//! (docs/design.md §3, §8). It checks its `agent.toml`, reads
//! Docker's `/version` and `/info` and sends a heartbeat every 30 seconds
//! and an inventory now and then. Beside them it waits for jobs
//! ([`jobs`]): updates and rollbacks someone approved in Wren, which it
//! checks against its own `agent.toml` and Docker before it runs them
//! ([`run`]).

#![forbid(unsafe_code)]

pub mod config;
pub mod discover;
pub mod docker;
pub mod heartbeat;
pub mod jobs;
pub mod run;
pub mod snapshot;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use config::Config;
use heartbeat::{Client, SendError};

/// Longest wait between heartbeats when Wren can't be reached.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The wait after Wren refused the token (it may be fixed on the server
/// without restarting the agent: a revoked token stays refused, though).
pub const REFUSED_WAIT: Duration = Duration::from_secs(5 * 60);

/// The inventory runs beside the heartbeats (version commands may take a
/// while), one at a time.
#[derive(Default, Clone)]
struct Scans {
    running: Arc<AtomicBool>,
    /// The last one didn't reach Wren: try again with the next heartbeat.
    retry: Arc<AtomicBool>,
}

impl Scans {
    fn start(&self, config: &Arc<Config>, client: &Arc<Client>) {
        if self.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let (config, client, scans) = (config.clone(), client.clone(), self.clone());
        tokio::spawn(async move {
            let inv = discover::scan(&config, Path::new("/")).await;
            if !inv.error.is_empty() {
                tracing::warn!(error = %inv.error, "can't read Docker for the inventory");
            }
            match client.inventory(&inv).await {
                Ok(()) => tracing::info!(
                    containers = inv.containers.len(),
                    script_stacks = inv.script_stacks.len(),
                    "inventory sent"
                ),
                Err(err) => {
                    tracing::warn!("inventory not sent: {err}");
                    scans.retry.store(true, Ordering::Relaxed);
                }
            }
            scans.running.store(false, Ordering::Release);
        });
    }
}

fn backoff(failures: u32) -> Duration {
    let secs = 5u64.saturating_mul(1 << failures.min(4));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// Sends heartbeats until `stop` resolves.
pub async fn run(
    config: Config,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    // The process's boot id and start, before anything is sent.
    let boot = heartbeat::boot();
    let client = Arc::new(Client::new(&config.server, &config.token)?);
    let config = Arc::new(config);
    let scans = Scans::default();
    let mut next_scan = Instant::now();
    tracing::info!(
        version = heartbeat::VERSION,
        server = %config.server,
        roots = config.roots.len(),
        script_stacks = config.script_stacks.len(),
        boot = %boot.id,
        "wren-agent started"
    );
    tokio::pin!(stop);
    let job_task = tokio::spawn(jobs_loop(config.clone()));
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
                // A scan on start, on the agent's own schedule, when Wren asks
                // ("Check now") and again after a failed one.
                if reply.scan
                    || Instant::now() >= next_scan
                    || scans.retry.swap(false, Ordering::Relaxed)
                {
                    next_scan = Instant::now() + config.scan_interval;
                    scans.start(&config, &client);
                }
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
                job_task.abort();
                tracing::info!("stopped");
                return Ok(());
            }
        }
    }
}

/// Waits for jobs and runs them, one at a time.
async fn jobs_loop(config: Arc<Config>) {
    let client = match jobs::JobClient::new(&config.server, &config.token) {
        Ok(c) => c,
        Err(err) => {
            tracing::error!("no job channel: {err}");
            return;
        }
    };
    let mut failures = 0u32;
    loop {
        match client.poll().await {
            Ok(jobs::Polled::Nothing) => failures = 0,
            Ok(jobs::Polled::Job(Ok(job))) => {
                failures = 0;
                run::handle(&config, &client, job).await;
            }
            Ok(jobs::Polled::Job(Err((run, why)))) => {
                tracing::warn!(run, "{why}");
                let _ = client.finish(run, "refused", &why).await;
            }
            Err(SendError::Refused(_)) => tokio::time::sleep(REFUSED_WAIT).await,
            Err(SendError::Failed(msg)) => {
                failures += 1;
                let wait = backoff(failures);
                tracing::debug!(retry_in = ?wait, "job poll: {msg}");
                tokio::time::sleep(wait).await;
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
