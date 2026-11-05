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
pub mod ledger;
pub mod managed;
pub mod presets;
pub mod process;
pub mod redact;
pub mod run;
pub mod selfupdate;
pub mod snapshot;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use heartbeat::{Client, SendError};
use managed::{Live, Offered};

/// Longest wait between heartbeats when Wren can't be reached.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The wait after Wren refused the token (it may be fixed on the server
/// without restarting the agent: a revoked token stays refused, though).
pub const REFUSED_WAIT: Duration = Duration::from_secs(5 * 60);

/// The least time between two scans that Wren asks for ("Check now", or
/// new managed settings). A server that sets the flag with every heartbeat
/// can't keep the agent scanning without pause (each scan reads Docker and
/// runs every version command); a request that arrives sooner waits.
pub const MIN_ASKED_SCAN_GAP: Duration = Duration::from_secs(30);

/// Whether a scan Wren asked for may start now, `last` being when the last
/// scan started.
fn asked_scan_allowed(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= MIN_ASKED_SCAN_GAP)
}

/// Waits for a background task. `false` when it panicked (said in the log,
/// since nobody else would notice) or was cancelled.
async fn finished(name: &str, task: tokio::task::JoinHandle<()>) -> bool {
    match task.await {
        Ok(()) => true,
        Err(err) if err.is_panic() => {
            tracing::error!(task = name, "background task panicked: {err}");
            false
        }
        Err(_) => false,
    }
}

/// Runs `make()` as a task and runs it again, after a pause, whenever it
/// panics. A task that ends by itself ends for good.
fn supervise<F, Fut>(name: &'static str, pause: Duration, make: F) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            if finished(name, tokio::spawn(make())).await {
                return;
            }
            tokio::time::sleep(pause).await;
        }
    })
}

/// The inventory runs beside the heartbeats (version commands may take a
/// while), one at a time.
#[derive(Default, Clone)]
struct Scans {
    running: Arc<AtomicBool>,
    /// The last one didn't reach Wren: try again with the next heartbeat.
    retry: Arc<AtomicBool>,
}

impl Scans {
    fn start(&self, live: &Arc<Live>, client: &Arc<Client>) {
        if self.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let (live, client, scans) = (live.clone(), client.clone(), self.clone());
        tokio::spawn(async move {
            let work = tokio::spawn(async move {
                // The settings in force when the scan starts.
                let config = live.config();
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
                        return false;
                    }
                }
                true
            });
            // A scan that panicked must not leave `running` set (no scan
            // would ever start again) and is tried again like a failed one.
            let sent = match work.await {
                Ok(sent) => sent,
                Err(err) => {
                    tracing::error!("inventory scan stopped: {err}");
                    false
                }
            };
            if !sent {
                scans.retry.store(true, Ordering::Relaxed);
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
    config: config::Config,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    // The process's boot id and start, before anything is sent.
    let boot = heartbeat::boot();
    let client = Arc::new(Client::new(&config.server, &config.token)?);
    // `agent.toml` with the settings Wren sent on top (unless locked).
    let locked = managed::locked_by(|k| std::env::var(k).ok());
    let live = Arc::new(Live::new(config, locked));
    let config = live.config();
    let scans = Scans::default();
    let mut next_scan = Instant::now();
    // When the last scan started, and whether Wren asked for one that had
    // to wait (see `MIN_ASKED_SCAN_GAP`).
    let mut last_scan: Option<Instant> = None;
    let mut asked = false;
    tracing::info!(
        version = heartbeat::VERSION,
        server = %config.server,
        roots = config.roots.len(),
        script_stacks = config.script_stacks.len(),
        boot = %boot.id,
        "wren-agent started"
    );
    // Said at start, so a missing or damaged ledger isn't a surprise when a
    // rollback is refused later.
    if let (_, Some(warning)) = ledger::Ledger::load(&config.ledger) {
        tracing::warn!("{warning}");
    }
    if locked {
        tracing::info!(
            "{} is set: settings from Wren are ignored, agent.toml is in force",
            managed::LOCK_ENV
        );
    } else if let Some(error) = live.error() {
        tracing::warn!("managed settings not in force: {error}");
    } else if let Some(version) = live.version() {
        tracing::info!(version, "managed settings from Wren are in force");
    }
    if !config.updates.configured() {
        tracing::warn!(
            "agent.toml has no [updates] allow list: every update and rollback is refused; add [updates] allow = [\"*\"] (or a list of target names)"
        );
    }
    tokio::pin!(stop);
    // What this process is in a self-update (design §9). A run prepared but
    // never handed to a helper is settled first: nothing is on its way now.
    let dir = selfupdate::config_dir(&config).to_path_buf();
    let self_jobs = Arc::new(jobs::JobClient::new(&config.server, &config.token)?);
    let engine = selfupdate::helper::DockerEngine {
        socket: config.docker_socket.clone(),
    };
    selfupdate::probation::settle_prepared(&engine, &dir, 0, selfupdate::state::now()).await;
    let state_path = dir.join(selfupdate::STATE_FILE);
    match selfupdate::probation::role(
        selfupdate::state::load(&state_path).ok().flatten(),
        heartbeat::VERSION,
    ) {
        selfupdate::probation::Role::Probation(st) => {
            tracing::info!(
                run = st.run_id,
                from = %st.from_version,
                "self-update: this agent is on probation"
            );
            if selfupdate::probation::fault("exit2") {
                tracing::error!("fault injected: exit 2 before the first heartbeat");
                std::process::exit(2);
            }
            tokio::select! {
                () = selfupdate::probation::probation(&config, &client, &self_jobs, &st, &dir) => {}
                () = &mut stop => {
                    tracing::info!("stopped");
                    return Ok(());
                }
            }
        }
        selfupdate::probation::Role::Report(st) => tracing::info!(
            run = st.run_id,
            phase = ?st.phase,
            "self-update: the result goes to Wren with the next heartbeats"
        ),
        selfupdate::probation::Role::Normal => {}
    }
    // Kept images beyond `[retention] rollback_images`, from before this
    // setting or a run that never finished, go once at start.
    let (socket, keep) = (
        config.docker_socket.clone(),
        config.retention.rollback_images,
    );
    tokio::spawn(async move {
        let gone = run::prune_rollback_images(&socket, None, keep).await;
        if !gone.is_empty() {
            tracing::info!(images = ?gone, "kept images beyond [retention] removed");
        }
    });
    // A panic in the job loop would leave an agent that sends heartbeats but
    // never takes a job: it is restarted.
    // Wren's id for this server, from the last heartbeat reply (0 before the
    // first): an agent update records it for the new agent to check.
    let server_id = Arc::new(AtomicI64::new(0));
    // The id of a self-update run that has ended (see `selfupdate::probation::round`).
    let ended = Arc::new(AtomicI64::new(0));
    let jobs_live = live.clone();
    let jobs_id = server_id.clone();
    let jobs_ended = ended.clone();
    let job_task = supervise("jobs", Duration::from_secs(5), move || {
        jobs_loop(jobs_live.clone(), jobs_id.clone(), jobs_ended.clone())
    });
    // The self-update chores run beside the heartbeats, one at a time.
    let chores = Arc::new(AtomicBool::new(false));
    let mut next_sweep = Instant::now();
    let mut connected = false;
    let mut failures = 0u32;
    let mut last_refusal = String::new();
    loop {
        // Settings from Wren may have changed these since the last round.
        let config = live.config();
        let mut beat = heartbeat::collect(&config.docker_socket, Path::new("/")).await;
        let mounts = managed::own_mounts(&config.docker_socket, Path::new("/")).await;
        beat.settings = Some(live.report(&mounts));
        beat.self_update = Some(selfupdate::report::report(&config).await);
        if !beat.docker_error.is_empty() {
            tracing::warn!(error = %beat.docker_error, "can't read Docker");
        }
        let wait = match client.send(&beat).await {
            Ok(reply) => {
                if !connected {
                    // Escaped (`?`): the name is Wren's to choose.
                    tracing::info!(server = ?reply.server.name, "connected to Wren");
                    connected = true;
                }
                failures = 0;
                server_id.store(reply.server.id, Ordering::Relaxed);
                // A scan on start, on the agent's own schedule, when Wren asks
                // ("Check now") and again after a failed one.
                let mut scan = reply.scan;
                if let Some(offered) = &reply.settings {
                    match live.offer(offered, &mounts) {
                        Ok(Offered::Applied(version)) => {
                            tracing::info!(version, "managed settings from Wren applied");
                            last_refusal.clear();
                            // Wren should see what the agent allows now.
                            scan = true;
                        }
                        Ok(Offered::Same(_)) => {}
                        // Said once per reason, not with every heartbeat.
                        Err(why) => {
                            if why != last_refusal {
                                tracing::warn!("managed settings from Wren refused: {why}");
                                last_refusal = why;
                            }
                        }
                    }
                }
                asked |= scan;
                let now = Instant::now();
                let may_ask = asked && asked_scan_allowed(last_scan, now);
                if may_ask || now >= next_scan || scans.retry.swap(false, Ordering::Relaxed) {
                    asked = false;
                    last_scan = Some(now);
                    next_scan = now + live.config().scan_interval;
                    scans.start(&live, &client);
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
        // Tells Wren how a finished self-update ended (every round until it
        // has it), settles a half-made one and sweeps leftovers now and then.
        if !chores.swap(true, Ordering::AcqRel) {
            let now = Instant::now();
            let sweep_due = now >= next_sweep;
            if sweep_due {
                next_sweep = now + Duration::from_secs(selfupdate::scaled(selfupdate::SWEEP));
            }
            let (config, jobs, ended, busy) = (
                live.config(),
                self_jobs.clone(),
                ended.clone(),
                chores.clone(),
            );
            tokio::spawn(async move {
                let work = tokio::spawn(async move {
                    selfupdate::probation::round(&config, &jobs, &ended, sweep_due).await;
                });
                finished("self-update chores", work).await;
                busy.store(false, Ordering::Release);
            });
        }
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

/// Seconds an agent update waits for a first heartbeat reply (one round).
const SERVER_ID_WAIT: u64 = 35;

/// Wren's id for this server: `id` as soon as a heartbeat reply has set it,
/// waiting up to `wait` for the first one; 0 when none came.
async fn known_server_id(id: &AtomicI64, wait: Duration) -> i64 {
    let end = Instant::now() + wait;
    loop {
        let known = id.load(Ordering::Relaxed);
        if known != 0 || Instant::now() >= end {
            return known;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Waits until self-update run `run` has ended (`ended` says so).
async fn until_ended(ended: &AtomicI64, run: i64) {
    while ended.load(Ordering::Relaxed) != run {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Waits for jobs and runs them, one at a time.
async fn jobs_loop(live: Arc<Live>, server_id: Arc<AtomicI64>, ended: Arc<AtomicI64>) {
    let config = live.config();
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
                // The settings in force now; a run keeps them to its end.
                let config = live.config();
                // A job that comes before the first heartbeat reply waits for
                // it (up to one round); an agent update needs the id.
                let id = if job.kind == jobs::JobKind::AgentUpdate {
                    let wait = Duration::from_secs(selfupdate::scaled(SERVER_ID_WAIT));
                    known_server_id(&server_id, wait).await
                } else {
                    server_id.load(Ordering::Relaxed)
                };
                let run_id = job.run_id;
                if run::handle(&config, &client, job, id).await
                    == selfupdate::prepare::Handoff::Stopping
                {
                    // The helper stops this agent (SIGTERM ends the process);
                    // no job may start meanwhile. Heartbeats go on. If the
                    // helper rolls back without ever stopping it, the run
                    // ends under this process: then jobs go on.
                    tracing::info!(
                        "self-update handed over; no more jobs until the run has ended or the agent restarts"
                    );
                    until_ended(&ended, run_id).await;
                    tracing::info!("the self-update has ended; jobs go on");
                }
            }
            Ok(jobs::Polled::Job(Err((run, why)))) => {
                tracing::warn!(run, reason = ?why, "job refused");
                let _ = client.finish(run, "refused", &jobs::Note::plain(why)).await;
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
    fn scans_wren_asks_for_keep_a_gap() {
        let now = Instant::now();
        assert!(asked_scan_allowed(None, now));
        let recent = now.checked_sub(Duration::from_secs(5));
        assert!(!asked_scan_allowed(recent, now));
        let old = now.checked_sub(MIN_ASKED_SCAN_GAP);
        assert!(asked_scan_allowed(old, now));
    }

    #[tokio::test]
    async fn a_panicking_task_is_noticed_and_restarted() {
        assert!(finished("ok", tokio::spawn(async {})).await);
        assert!(!finished("boom", tokio::spawn(async { panic!("boom") })).await);

        let runs = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let seen = runs.clone();
        let task = supervise("flaky", Duration::from_millis(10), move || {
            let seen = seen.clone();
            async move {
                // The first two runs panic, the third ends normally.
                if seen.fetch_add(1, Ordering::SeqCst) < 2 {
                    panic!("boom");
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the supervisor ends once the task does")
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn an_agent_update_waits_for_a_known_server_id() {
        let id = Arc::new(AtomicI64::new(0));
        let none = known_server_id(&id, Duration::from_millis(50)).await;
        assert_eq!(none, 0, "no heartbeat reply yet");
        let late = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            late.store(3, Ordering::Relaxed);
        });
        let known = known_server_id(&id, Duration::from_secs(10)).await;
        assert_eq!(known, 3);
    }

    #[tokio::test]
    async fn a_parked_job_loop_goes_on_when_its_run_has_ended() {
        let ended = Arc::new(AtomicI64::new(0));
        // Another run's end (or none) does not release it.
        let wait = tokio::time::timeout(Duration::from_millis(1500), until_ended(&ended, 57)).await;
        assert!(wait.is_err());
        ended.store(56, Ordering::Relaxed);
        let wait = tokio::time::timeout(Duration::from_millis(1500), until_ended(&ended, 57)).await;
        assert!(wait.is_err());
        let late = ended.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            late.store(57, Ordering::Relaxed);
        });
        tokio::time::timeout(Duration::from_secs(10), until_ended(&ended, 57))
            .await
            .expect("the loop goes on once its run has ended");
    }

    #[test]
    fn backoff_grows_to_a_minute() {
        assert_eq!(backoff(1), Duration::from_secs(10));
        assert_eq!(backoff(3), Duration::from_secs(40));
        assert_eq!(backoff(9), MAX_BACKOFF);
    }
}
