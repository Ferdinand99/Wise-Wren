//! The new agent's probation and the report of a self-update's result
//! (design §9). Which part this process plays follows from the state file and
//! this agent's own version ([`role`]):
//!
//! - **Probation**: the new agent, started by the helper. It talks to Wren
//!   (heartbeats only, no settings, no jobs), checks itself ([`Checks`]) and,
//!   once everything has held for the stable period, writes the confirm
//!   marker. The helper then does the rest.
//! - **Report**: the agent that tells Wren how the run ended: the new agent
//!   after `done`, the old one after `rolled_back` / `rollback_failed`. A
//!   run that is still `rolling_back` waits: the helper starts the old agent
//!   before it writes the result.
//! - **Normal**: nothing to do with a self-update.
//!
//! The state file's `result` field is not used: the phase says how the run
//! ended, and `reported` says whether Wren has heard it.
//!
//! Leftovers (a finished run's helper and renamed old container, a run
//! prepared but never handed to a helper) are cleared by [`sweep`] and
//! [`settle_prepared`].

use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::helper::{DockerEngine, Engine};
use super::state::{self, Phase, StateFile};
use super::{
    BACKUP_DIR, HELPER_LABEL, HELPER_START, RUN_LABEL, STATE_FILE, SWEEP, config_dir, helper_name,
    old_name, scaled,
};
use crate::config::Config;
use crate::docker::get_json;
use crate::heartbeat::{self, VERSION};
use crate::jobs::{Job, JobClient, JobKind, Note, StepReport, TargetRef, rejected};
use crate::ledger::Ledger;
use crate::run::{prune_rollback_images, rollback_tag, write_unraid_status};

/// What this process is in a self-update.
#[derive(Debug, PartialEq)]
pub enum Role {
    Normal,
    /// The new agent, until the helper has finished the run.
    Probation(StateFile),
    /// Tells Wren how the run ended (waits while it is `rolling_back`).
    Report(StateFile),
}

/// The part of this process (version `version`) in the run in `state`.
pub fn role(state: Option<StateFile>, version: &str) -> Role {
    let Some(s) = state else {
        return Role::Normal;
    };
    match s.phase {
        // `new_created`: the helper has started the new agent but not yet
        // written down that it did.
        Phase::NewCreated | Phase::NewStarted | Phase::Confirmed if s.to_version == version => {
            Role::Probation(s)
        }
        Phase::Done if !s.reported && s.to_version == version => Role::Report(s),
        Phase::RollingBack | Phase::RolledBack | Phase::RollbackFailed
            if !s.reported && s.from_version == version =>
        {
            Role::Report(s)
        }
        _ => Role::Normal,
    }
}

/// What the new agent has found out about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Checks {
    /// Wren has answered with the server id the state file names.
    pub server_id_ok: bool,
    /// Wren answered with another server's id: never ready again.
    pub wrong_server: bool,
    /// The latest heartbeat reached Wren with the right id.
    pub last_ok: bool,
    pub docker_ok: bool,
    /// Wren answered the job channel.
    pub probe_ok: bool,
    /// Seconds since the container's own start.
    pub up_seconds: u64,
    /// The container's own restart count.
    pub restarts: i64,
}

impl Checks {
    /// Takes in a heartbeat: the server id Wren answered with, or `Err` when
    /// it didn't answer. `want` is the id in the state file.
    pub fn beat(&mut self, reply: Result<i64, ()>, want: i64) {
        match reply {
            Ok(id) if id == want => {
                self.server_id_ok = true;
                self.last_ok = true;
            }
            Ok(_) => {
                self.wrong_server = true;
                self.last_ok = false;
            }
            Err(()) => self.last_ok = false,
        }
    }
}

/// Everything holds, and has held for `stable_seconds`.
pub fn ready(c: &Checks, stable_seconds: u64) -> bool {
    c.server_id_ok
        && c.last_ok
        && !c.wrong_server
        && c.docker_ok
        && c.probe_ok
        && c.restarts == 0
        && c.up_seconds >= stable_seconds
}

/// How a finished run ended, as Wren is told: the outcome and its note.
pub fn outcome(s: &StateFile) -> (&'static str, Note) {
    match s.phase {
        Phase::Done => (
            "succeeded",
            Note::new("selfupdate.done", &[("version", &s.to_version)]),
        ),
        Phase::RolledBack => (
            "rolled_back",
            Note::new(
                "selfupdate.rolled_back",
                &[("version", &s.from_version), ("reason", &s.reason)],
            ),
        ),
        Phase::RollbackFailed => (
            "rollback_failed",
            Note::new(
                "selfupdate.rollback_failed",
                &[("reason", &s.reason), ("break_glass", &s.break_glass)],
            ),
        ),
        _ => (
            "failed",
            Note::plain(format!("the self-update stopped in phase {:?}", s.phase)),
        ),
    }
}

/// The containers of a finished run that are still there: its helper, and
/// the renamed old agent once the new one is `done`. A helper is the run's
/// when its labels say so; with no state file any helper counts (the caller
/// leaves out those that run). `containers` is `(name, labels)`.
pub fn leftovers(state: Option<&StateFile>, containers: &[(String, Value)]) -> Vec<String> {
    let over = state.is_none_or(|s| s.phase.terminal());
    let run = state.map(|s| s.run_id.to_string());
    let old = state
        .filter(|s| s.phase == Phase::Done)
        .map(|s| old_name(&s.container, s.run_id));
    let label =
        |labels: &Value, key: &str| labels.get(key).and_then(Value::as_str).map(str::to_string);
    containers
        .iter()
        .filter(|(name, labels)| {
            let helper = label(labels, HELPER_LABEL).as_deref() == Some("helper")
                && run
                    .as_ref()
                    .is_none_or(|run| label(labels, RUN_LABEL).as_ref() == Some(run));
            (over && helper) || old.as_deref() == Some(name.as_str())
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// A run that was prepared but whose helper never came: the old agent
/// stopped between saving the state and starting the helper. `grace` seconds
/// give an old agent that is still on its way the time to get there.
pub fn stale_prepared(s: &StateFile, now: u64, grace: u64, helper_exists: bool) -> bool {
    s.phase == Phase::Prepared && !helper_exists && now >= s.updated_at.saturating_add(grace)
}

/// A container's name in Docker's list answer (without the slash).
fn name_of(c: &Value) -> Option<String> {
    Some(
        c.get("Names")?
            .as_array()?
            .first()?
            .as_str()?
            .trim_start_matches('/')
            .to_string(),
    )
}

/// `(name, labels)` of every container in Docker's list answer.
pub fn containers_of(list: &Value) -> Vec<(String, Value)> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let labels = c
                .get("Labels")
                .filter(|l| l.is_object())
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            Some((name_of(c)?, labels))
        })
        .collect()
}

/// What the sweep removes, given Docker's list answer. With no state file a
/// helper that runs is left alone: it may belong to a run being prepared.
pub fn sweepable(state: Option<&StateFile>, list: &Value) -> Vec<String> {
    let mut all = containers_of(list);
    if state.is_none() {
        let live: HashSet<String> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| {
                matches!(
                    c.get("State").and_then(Value::as_str),
                    Some("running" | "restarting" | "created")
                )
            })
            .filter_map(name_of)
            .collect();
        all.retain(|(name, _)| !live.contains(name));
    }
    leftovers(state, &all)
}

/// Seconds since the epoch of a Docker time (`2026-11-05T10:20:30.123Z`, UTC);
/// `None` for anything else, Docker's zero time included.
fn epoch(stamp: &str) -> Option<u64> {
    let (date, time) = stamp.split_once('T')?;
    let time = time.strip_suffix('Z')?;
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split('.').next()?.splitn(3, ':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days since 1970-01-01 (the civil-calendar algorithm: March first).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

/// The repository the old images of this agent's updates are kept under
/// (`wren-rollback/agent-<container>`).
pub fn rollback_repo(container: &str) -> String {
    rollback_tag(&format!("agent-{container}"), 0, container)
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Whether a test build was told to fall over at `name` (`WW_SU_FAULT`).
/// A release never does.
#[cfg(feature = "selfupdate-test")]
pub fn fault(name: &str) -> bool {
    std::env::var("WW_SU_FAULT").is_ok_and(|f| f == name)
}

/// See the test build above.
#[cfg(not(feature = "selfupdate-test"))]
pub fn fault(_name: &str) -> bool {
    false
}

/// What the result's report needs of Wren.
trait Wren: Sync {
    fn step(
        &self,
        run: i64,
        step: &str,
        status: &str,
        log: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn finish(
        &self,
        run: i64,
        outcome: &str,
        note: &Note,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

impl Wren for JobClient {
    async fn step(&self, run: i64, step: &str, status: &str, log: &str) -> Result<(), String> {
        JobClient::step(
            self,
            run,
            &StepReport {
                step,
                status,
                log,
                duration_ms: 0,
                notes: &[],
            },
        )
        .await
        .map(|_| ())
    }

    async fn finish(&self, run: i64, outcome: &str, note: &Note) -> Result<(), String> {
        JobClient::finish(self, run, outcome, note).await
    }
}

/// Tells Wren how the run ended: the steps, then the outcome; then the ledger
/// entry is closed and `reported` saved. An error means Wren was not reached
/// (try again); a run Wren has closed already counts as told.
async fn report_to<W: Wren>(
    wren: &W,
    st: &mut StateFile,
    path: &Path,
    ledger: &Path,
) -> Result<(), String> {
    if !st.phase.terminal() {
        return Err(format!(
            "run {} is not over yet (phase {:?})",
            st.run_id, st.phase
        ));
    }
    let run = st.run_id;
    let (result, note) = outcome(st);
    let steps = match st.phase {
        Phase::Done => [
            ("update", "ok", "the new agent took over".to_string()),
            (
                "health",
                "ok",
                format!(
                    "back as {}, stable for {} s",
                    st.to_version, st.stable_seconds
                ),
            ),
        ],
        Phase::RolledBack => [
            ("update", "failed", st.reason.clone()),
            ("rollback", "ok", st.reason.clone()),
        ],
        _ => [
            ("update", "failed", st.reason.clone()),
            ("rollback", "failed", st.reason.clone()),
        ],
    };
    for (step, status, log) in &steps {
        match wren.step(run, step, status, log).await {
            Ok(()) => {}
            // A step Wren won't take doesn't stop the outcome from being told.
            Err(e) if rejected(&e) => tracing::debug!(run, step, "{e}"),
            Err(e) => return Err(e),
        }
    }
    match wren.finish(run, result, &note).await {
        Ok(()) => {}
        // The run is closed (Wren gave up on it, or it is not this server's):
        // asking again changes nothing.
        Err(e) if rejected(&e) => {
            tracing::warn!(
                run,
                "Wren has no open run for this self-update ({e}); counting it as reported"
            );
        }
        Err(e) => return Err(e),
    }
    // The ledger knows an agent update by the container it replaced.
    let job = Job {
        run_id: run,
        kind: JobKind::AgentUpdate,
        target: TargetRef {
            kind: "agent".into(),
            name: st.container.clone(),
        },
        from_key: String::new(),
        to_key: String::new(),
        images: Vec::new(),
        health_seconds: 0,
        keep_snapshots: 0,
        rollback_of: None,
        require_snapshot: false,
        to_version: st.to_version.clone(),
        to_digest: st.to_digest.clone(),
    };
    let (mut ledger, warning) = Ledger::load(ledger);
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
    if let Err(e) = ledger.finish(&job, result) {
        tracing::warn!(run, "{e}");
    }
    let mut done = st.clone();
    done.reported = true;
    state::save(path, &done)?;
    *st = done;
    Ok(())
}

/// Reports a finished run to Wren (see [`report_to`]). After a successful
/// update the old images beyond `keep` are pruned.
pub async fn report(
    jobs: &JobClient,
    st: &mut StateFile,
    path: &Path,
    ledger: &Path,
    socket: &str,
    keep: usize,
) -> Result<(), String> {
    report_to(jobs, st, path, ledger).await?;
    if st.phase == Phase::Done {
        let gone = prune_rollback_images(socket, Some(&rollback_repo(&st.container)), keep).await;
        if !gone.is_empty() {
            tracing::info!(images = ?gone, "old agent images beyond [retention] removed");
        }
    }
    Ok(())
}

/// Removes the helper of run `run` if it is still there.
pub async fn remove_helper<E: Engine>(engine: &E, run: i64) {
    let name = helper_name(run);
    match engine.inspect(&name).await {
        Ok(Some(_)) => {
            if let Err(e) = engine.remove(&name, true).await {
                tracing::warn!("couldn't remove the helper {name}: {e}");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("couldn't look for the helper {name}: {e}"),
    }
}

/// How a helper container looks to the new agent.
#[derive(Debug, Clone, PartialEq)]
pub struct HelperView {
    /// Docker's status word.
    pub status: String,
    pub restarts: i64,
    /// Seconds since it last stopped (0 when that isn't known).
    pub idle: u64,
}

impl HelperView {
    pub fn from_inspect(v: &Value, now: u64) -> HelperView {
        HelperView {
            status: v
                .pointer("/State/Status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            restarts: v.get("RestartCount").and_then(Value::as_i64).unwrap_or(0),
            idle: v
                .pointer("/State/FinishedAt")
                .and_then(Value::as_str)
                .and_then(epoch)
                .map_or(0, |at| now.saturating_sub(at)),
        }
    }
}

/// Restarts after which a helper that keeps falling over counts as broken.
const STUCK_RESTARTS: i64 = 5;

/// The helper is gone, or so broken it will not finish the run: no
/// container, one that keeps restarting, or one that has stayed down. A
/// helper that was killed comes back by itself and is not counted.
pub fn helper_gone(helper: Option<&HelperView>) -> bool {
    match helper {
        None => true,
        Some(h) => match h.status.as_str() {
            "restarting" => h.restarts >= STUCK_RESTARTS,
            "exited" | "dead" => h.idle >= scaled(HELPER_START),
            _ => false,
        },
    }
}

/// What the new agent does about a run whose helper may be gone.
#[derive(Debug, PartialEq)]
pub enum Orphan {
    Wait,
    /// Do the helper's clean-up and mark the run `done`.
    Finish,
    /// Never confirmed: say how to go back by hand.
    BreakGlass,
}

/// The new agent's decision for run `s` (it is on probation or confirmed):
/// the helper is `gone`, and `marker` says whether this agent confirmed.
pub fn orphan(s: &StateFile, gone: bool, marker: bool) -> Orphan {
    if !gone || !matches!(s.phase, Phase::NewStarted | Phase::Confirmed) {
        return Orphan::Wait;
    }
    if marker {
        Orphan::Finish
    } else {
        Orphan::BreakGlass
    }
}

/// The helper of the run the new agent confirmed is gone: the new agent does
/// the helper's clean-up itself (the helper is gone, so the file still has
/// one writer) and sets `done`; the ordinary report path tells Wren. `true`
/// when it did. Without the confirmation nothing is decided: the break-glass
/// command is logged (`loud`) and Wren's own time-out closes the run.
pub async fn adopt<E: Engine>(engine: &E, dir: &Path, now: u64, loud: bool) -> bool {
    let path = dir.join(STATE_FILE);
    let Ok(Some(mut s)) = state::load(&path) else {
        return false;
    };
    if s.to_version != VERSION {
        return false;
    }
    let helper = helper_name(s.run_id);
    let view = match helper_view(engine, &helper, now).await {
        Ok(view) => view,
        Err(e) => {
            tracing::warn!("can't look for the helper {helper}: {e}");
            return false;
        }
    };
    match orphan(
        &s,
        helper_gone(view.as_ref()),
        state::confirmed(dir, s.run_id),
    ) {
        Orphan::Wait => return false,
        Orphan::BreakGlass => {
            if loud {
                tracing::warn!(
                    run = s.run_id,
                    "the self-update helper is gone and this agent never confirmed; to go back by hand: {}",
                    s.break_glass
                );
            }
            return false;
        }
        Orphan::Finish => {}
    }
    tracing::warn!(
        run = s.run_id,
        "the self-update helper is gone; finishing the update here"
    );
    // A helper that is stuck must not write the file later.
    if view.is_some()
        && let Err(e) = engine.remove(&helper, true).await
    {
        tracing::warn!("can't remove the broken helper {helper}: {e}");
        return false;
    }
    let old = old_name(&s.container, s.run_id);
    match engine.inspect(&old).await {
        Ok(Some(_)) => {
            if let Err(e) = engine.remove(&old, false).await {
                tracing::warn!("couldn't remove the old container {old}: {e}");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("couldn't look for the old container {old}: {e}"),
    }
    if s.unraid
        && let Err(e) = write_unraid_status(
            &engine.unraid_status_file(),
            &s.reference,
            &s.to_digest,
            &s.to_digest,
        )
    {
        tracing::warn!("couldn't set Unraid's update status: {e}");
    }
    let _ = std::fs::remove_dir_all(dir.join(BACKUP_DIR).join(s.run_id.to_string()));
    s.phase = Phase::Done;
    s.updated_at = now;
    match state::save(&path, &s) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("can't save {}: {e}", path.display());
            false
        }
    }
}

/// The helper container `name`, as the new agent sees it.
async fn helper_view<E: Engine>(
    engine: &E,
    name: &str,
    now: u64,
) -> Result<Option<HelperView>, String> {
    Ok(engine
        .inspect(name)
        .await?
        .map(|v| HelperView::from_inspect(&v, now)))
}

/// A run that was prepared but never handed to a helper (the old agent
/// stopped in between) is put back as it was and marked `rolled_back`, so the
/// agent that reports tells Wren and the next update may start. `true` when
/// it settled one. Nothing is decided when Docker can't be looked at.
pub async fn settle_prepared<E: Engine>(engine: &E, dir: &Path, grace: u64, now: u64) -> bool {
    let path = dir.join(STATE_FILE);
    let Ok(Some(mut s)) = state::load(&path) else {
        return false;
    };
    if s.phase != Phase::Prepared {
        return false;
    }
    let helper = helper_name(s.run_id);
    let found = match engine.inspect(&helper).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("can't look for the helper of run {}: {e}", s.run_id);
            return false;
        }
    };
    // A helper that was created but never started (the old agent stopped
    // between the two) is no helper.
    let never_started = found
        .as_ref()
        .is_some_and(|v| v.pointer("/State/Status").and_then(Value::as_str) == Some("created"));
    let alive = found.is_some() && !never_started;
    if !stale_prepared(&s, now, grace, alive) {
        return false;
    }
    if never_started && let Err(e) = engine.remove(&helper, true).await {
        tracing::warn!("can't remove the helper {helper} that never started: {e}");
        return false;
    }
    tracing::warn!(
        run = s.run_id,
        "a prepared self-update never got a helper; putting it back"
    );
    if !s.old_image_id.is_empty()
        && !s.reference.is_empty()
        && let Err(e) = engine.tag(&s.old_image_id, &s.reference).await
    {
        tracing::warn!("can't put the image tag back: {e}");
    }
    let _ = std::fs::remove_dir_all(dir.join(BACKUP_DIR).join(s.run_id.to_string()));
    s.phase = Phase::RolledBack;
    s.reason = "the helper was never started (the agent stopped while handing over)".into();
    s.updated_at = now;
    // Only the agent that prepared the run (this version) tells Wren.
    s.reported = s.from_version != VERSION;
    match state::save(&path, &s) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("can't save {}: {e}", path.display());
            false
        }
    }
}

/// Removes what a finished run left (see [`leftovers`]); the names removed.
/// A state file that can't be read leaves everything alone.
pub async fn sweep(socket: &str, dir: &Path) -> Vec<String> {
    let state = match state::load(&dir.join(STATE_FILE)) {
        Ok(state) => state,
        Err(e) => {
            tracing::warn!("self-update leftovers not swept: {e}");
            return Vec::new();
        }
    };
    let Ok(list) = get_json(socket, "/containers/json?all=1").await else {
        return Vec::new();
    };
    let engine = DockerEngine {
        socket: socket.to_string(),
    };
    let mut gone = Vec::new();
    for name in sweepable(state.as_ref(), &list) {
        match engine.remove(&name, true).await {
            Ok(()) => {
                tracing::info!(container = %name, "removed a self-update leftover");
                gone.push(name);
            }
            Err(e) => tracing::warn!("couldn't remove {name}: {e}"),
        }
    }
    gone
}

/// One round of the chores a running agent does for self-updates: settles a
/// prepared run that has no helper, tells Wren how a finished run ended
/// (again every round until Wren has it) and, when `sweep_due`, sweeps the
/// leftovers. `ended` gets the id of a finished run, which lets a parked job
/// loop go on (the helper rolled back without ever stopping this agent).
pub async fn round(config: &Config, jobs: &JobClient, ended: &AtomicI64, sweep_due: bool) {
    let dir = config_dir(config);
    let socket = config.docker_socket.as_str();
    let engine = DockerEngine {
        socket: socket.to_string(),
    };
    settle_prepared(&engine, dir, scaled(SWEEP), state::now()).await;
    adopt(&engine, dir, state::now(), sweep_due).await;
    let path = dir.join(STATE_FILE);
    let found = state::load(&path).ok().flatten();
    if let Some(s) = found.as_ref().filter(|s| s.phase.terminal()) {
        ended.store(s.run_id, Ordering::Relaxed);
    }
    if let Role::Report(mut s) = role(found, VERSION)
        && s.phase.terminal()
    {
        let keep = config.retention.rollback_images;
        match report(jobs, &mut s, &path, &config.ledger, socket, keep).await {
            Ok(()) => {
                tracing::info!(run = s.run_id, phase = ?s.phase, "self-update reported to Wren");
                remove_helper(&engine, s.run_id).await;
            }
            Err(e) => tracing::warn!(
                run = s.run_id,
                "the result of the self-update didn't reach Wren, trying again: {e}"
            ),
        }
    }
    if sweep_due {
        sweep(socket, dir).await;
    }
}

/// The new agent's probation: heartbeats (only; no settings, no jobs, no
/// inventory) every [`scaled`]`(10)` s whatever the answer, its own checks,
/// and the confirm marker once [`ready`]. Then it waits until the run is over
/// or the helper is gone (or broken). Never touches `managed.json` or `runs.json`.
pub async fn probation(
    config: &Config,
    client: &heartbeat::Client,
    jobs: &JobClient,
    st: &StateFile,
    dir: &Path,
) {
    let socket = config.docker_socket.as_str();
    let state_path = dir.join(STATE_FILE);
    let engine = DockerEngine {
        socket: socket.to_string(),
    };
    let helper = helper_name(st.run_id);
    let stable = scaled(st.stable_seconds);
    let pause = Duration::from_secs(scaled(10));
    let mut checks = Checks::default();
    let mut confirmed = false;
    loop {
        let mut beat = heartbeat::collect(socket, Path::new("/")).await;
        beat.self_update = Some(super::report::report(config).await);
        checks.docker_ok = beat.docker_error.is_empty();
        match client.send(&beat).await {
            Ok(reply) => checks.beat(Ok(reply.server.id), st.server_id),
            Err(e) => {
                tracing::warn!("probation: {e}");
                checks.beat(Err(()), st.server_id);
            }
        }
        if !checks.probe_ok {
            checks.probe_ok = jobs.probe().await.is_ok();
        }
        match super::report::own(socket).await {
            Some(own) => {
                checks.restarts = own
                    .inspect
                    .get("RestartCount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                checks.up_seconds = own
                    .inspect
                    .pointer("/State/StartedAt")
                    .and_then(Value::as_str)
                    .and_then(epoch)
                    .map_or(0, |at| state::now().saturating_sub(at));
            }
            None => checks.up_seconds = 0,
        }
        if !confirmed && ready(&checks, stable) {
            match state::write_confirm(dir, st.run_id) {
                Ok(()) => {
                    confirmed = true;
                    tracing::info!(
                        run = st.run_id,
                        "self-update: confirmed; waiting for the helper to finish"
                    );
                    if fault("crash_after_confirm") {
                        tracing::error!("fault injected: crash after the confirmation");
                        std::process::exit(1);
                    }
                }
                Err(e) => tracing::warn!("can't write the confirmation: {e}"),
            }
        }
        match state::load(&state_path) {
            Ok(Some(s)) if s.run_id == st.run_id && !s.phase.terminal() => {}
            // A file that can't be read this round may be read the next.
            Err(_) => {}
            _ => return,
        }
        if let Ok(view) = helper_view(&engine, &helper, state::now()).await
            && helper_gone(view.as_ref())
        {
            // `round` finishes the run if this agent confirmed.
            tracing::info!(run = st.run_id, "self-update: the helper is gone");
            return;
        }
        tokio::time::sleep(pause).await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use serde_json::{Value, json};

    use super::*;
    use crate::jobs::{Job, JobKind, Note, TargetRef};
    use crate::ledger::Ledger;
    use crate::selfupdate::helper::Engine;
    use crate::selfupdate::state::{self, Phase, StateFile};

    fn at(phase: Phase, from: &str, to: &str, reported: bool) -> StateFile {
        StateFile {
            format: 1,
            run_id: 57,
            container: "wren-agent".into(),
            phase,
            reported,
            from_version: from.into(),
            to_version: to.into(),
            reason: "why".into(),
            break_glass: crate::selfupdate::state::break_glass("wren-agent", 57),
            ..StateFile::default()
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wren-su-prob-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roles_from_the_state_file() {
        use Phase::*;
        assert_eq!(role(None, "0.10.1"), Role::Normal);
        assert!(matches!(
            role(Some(at(NewStarted, "0.10.0", "0.10.1", false)), "0.10.1"),
            Role::Probation(_)
        ));
        assert!(matches!(
            role(Some(at(Confirmed, "0.10.0", "0.10.1", false)), "0.10.1"),
            Role::Probation(_)
        ));
        // The helper may have started the new agent before it recorded it.
        assert!(matches!(
            role(Some(at(NewCreated, "0.10.0", "0.10.1", false)), "0.10.1"),
            Role::Probation(_)
        ));
        assert_eq!(
            role(Some(at(NewStarted, "0.10.0", "0.10.1", false)), "0.10.0"),
            Role::Normal
        );
        assert!(matches!(
            role(Some(at(Done, "0.10.0", "0.10.1", false)), "0.10.1"),
            Role::Report(_)
        ));
        assert!(matches!(
            role(Some(at(RolledBack, "0.10.0", "0.10.1", false)), "0.10.0"),
            Role::Report(_)
        ));
        assert!(matches!(
            role(
                Some(at(RollbackFailed, "0.10.0", "0.10.1", false)),
                "0.10.0"
            ),
            Role::Report(_)
        ));
        // The helper puts the old agent back before it writes `rolled_back`:
        // the old agent waits for it.
        assert!(matches!(
            role(Some(at(RollingBack, "0.10.0", "0.10.1", false)), "0.10.0"),
            Role::Report(_)
        ));
        assert_eq!(
            role(Some(at(RollingBack, "0.10.0", "0.10.1", false)), "0.10.1"),
            Role::Normal
        );
        assert_eq!(
            role(Some(at(Done, "0.10.0", "0.10.1", true)), "0.10.1"),
            Role::Normal
        );
        assert_eq!(
            role(Some(at(RolledBack, "0.10.0", "0.10.1", true)), "0.10.0"),
            Role::Normal
        );
        assert_eq!(
            role(Some(at(OldStopped, "0.10.0", "0.10.1", false)), "0.10.0"),
            Role::Normal
        );
    }

    fn good() -> Checks {
        Checks {
            server_id_ok: true,
            last_ok: true,
            docker_ok: true,
            probe_ok: true,
            up_seconds: 60,
            ..Checks::default()
        }
    }

    #[test]
    fn confirmation_needs_every_condition() {
        let ok = good();
        assert!(ready(&ok, 60));
        assert!(!ready(
            &Checks {
                up_seconds: 59,
                ..ok.clone()
            },
            60
        ));
        assert!(!ready(&ok, 180));
        assert!(!ready(
            &Checks {
                docker_ok: false,
                ..ok.clone()
            },
            60
        ));
        assert!(!ready(
            &Checks {
                probe_ok: false,
                ..ok.clone()
            },
            60
        ));
        assert!(!ready(
            &Checks {
                restarts: 1,
                ..ok.clone()
            },
            60
        ));
        assert!(!ready(
            &Checks {
                server_id_ok: false,
                ..ok.clone()
            },
            60
        ));
        assert!(!ready(
            &Checks {
                last_ok: false,
                ..ok.clone()
            },
            60
        ));
        assert!(!ready(
            &Checks {
                wrong_server: true,
                ..ok.clone()
            },
            60
        ));
    }

    #[test]
    fn wren_going_down_during_probation_means_not_ready() {
        let rest = |c: &Checks| Checks {
            docker_ok: true,
            probe_ok: true,
            up_seconds: 99,
            ..c.clone()
        };
        let mut c = Checks::default();
        c.beat(Err(()), 3);
        assert!(!c.server_id_ok && !c.last_ok && !c.wrong_server);
        c.beat(Ok(3), 3);
        assert!(c.server_id_ok && c.last_ok);
        assert!(ready(&rest(&c), 60));
        // A beat that fails after a good one: the id stays known, but the
        // check is not passed until Wren answers again.
        c.beat(Err(()), 3);
        assert!(c.server_id_ok && !c.last_ok && !c.wrong_server);
        assert!(!ready(&rest(&c), 60));
        c.beat(Ok(3), 3);
        assert!(ready(&rest(&c), 60));
        // Another Wren's id is permanent.
        c.beat(Ok(4), 3);
        assert!(c.wrong_server && !c.last_ok);
        c.beat(Ok(3), 3);
        assert!(c.wrong_server);
        assert!(!ready(&rest(&c), 60));
    }

    #[test]
    fn outcomes_for_wren() {
        let (o, n) = outcome(&at(Phase::Done, "0.10.0", "0.10.1", false));
        assert_eq!(
            (o, n.code, n.params["version"].as_str()),
            ("succeeded", "selfupdate.done", "0.10.1")
        );
        let (o, n) = outcome(&at(Phase::RolledBack, "0.10.0", "0.10.1", false));
        assert_eq!(
            (
                o,
                n.code,
                n.params["reason"].as_str(),
                n.params["version"].as_str()
            ),
            ("rolled_back", "selfupdate.rolled_back", "why", "0.10.0")
        );
        let (o, n) = outcome(&at(Phase::RollbackFailed, "0.10.0", "0.10.1", false));
        assert_eq!(
            (o, n.code),
            ("rollback_failed", "selfupdate.rollback_failed")
        );
        assert!(
            n.params["break_glass"].contains("docker rename wren-agent-wren-old-57 wren-agent")
        );
    }

    #[test]
    fn leftovers_go_only_when_the_run_is_over() {
        let helper = (
            "wren-agent-selfupdate-57".to_string(),
            json!({"tools.hedgerow.wren.selfupdate": "helper", "tools.hedgerow.wren.run": "57"}),
        );
        let old = ("wren-agent-wren-old-57".to_string(), json!({}));
        let all = [
            helper.clone(),
            old.clone(),
            ("postiz".to_string(), json!({})),
        ];
        assert_eq!(
            leftovers(Some(&at(Phase::Done, "0.10.0", "0.10.1", true)), &all),
            [helper.0.clone(), old.0.clone()]
        );
        assert_eq!(
            leftovers(Some(&at(Phase::RolledBack, "0.10.0", "0.10.1", true)), &all),
            vec![helper.0.clone()]
        );
        assert!(
            leftovers(
                Some(&at(Phase::NewStarted, "0.10.0", "0.10.1", false)),
                &all
            )
            .is_empty()
        );
        // No state at all: a helper has nothing to do.
        assert_eq!(leftovers(None, &all), vec![helper.0.clone()]);
    }

    #[test]
    fn a_prepared_run_without_a_helper_is_a_leftover_once_it_is_old_enough() {
        let mut s = at(Phase::Prepared, "0.10.0", "0.10.1", false);
        s.updated_at = 1000;
        // The old agent may be on its way to create the helper: wait the
        // grace out.
        assert!(!stale_prepared(&s, 1500, 600, false));
        assert!(stale_prepared(&s, 1600, 600, false));
        // At start (grace 0) nothing can be on its way.
        assert!(stale_prepared(&s, 1000, 0, false));
        // A helper that exists decides for itself.
        assert!(!stale_prepared(&s, 99_999, 0, true));
        // Other phases belong to the helper.
        s.phase = Phase::HelperStarted;
        assert!(!stale_prepared(&s, 99_999, 0, false));
    }

    #[test]
    fn the_container_list_gives_names_and_labels() {
        let list = json!([
            {"Names": ["/wren-agent-selfupdate-57"], "Labels": {"tools.hedgerow.wren.selfupdate": "helper"}},
            {"Names": ["/postiz"], "Labels": null},
            {"Names": [], "Labels": {}},
        ]);
        let got = containers_of(&list);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "wren-agent-selfupdate-57");
        assert_eq!(got[0].1["tools.hedgerow.wren.selfupdate"], "helper");
        assert_eq!(got[1].0, "postiz");
    }

    #[test]
    fn docker_start_times_become_seconds() {
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2026-11-05T10:20:30.123456789Z"), Some(1_793_874_030));
        assert_eq!(epoch("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        // Docker's zero time and junk are no start time.
        assert_eq!(epoch("0001-01-01T00:00:00Z"), None);
        assert_eq!(epoch(""), None);
        assert_eq!(epoch("yesterday"), None);
    }

    #[test]
    fn the_rollback_images_of_the_agent_are_pruned_by_their_repo() {
        assert_eq!(
            rollback_repo("Wise-Wren-Agent"),
            "wren-rollback/agent-wise-wren-agent"
        );
    }

    #[test]
    fn rejected_answers_are_wrens_no() {
        use crate::jobs::rejected;
        assert!(rejected("Wren answered 404 Not Found"));
        assert!(rejected("Wren answered 409 Conflict"));
        assert!(!rejected("Wren answered 429 Too Many Requests"));
        assert!(!rejected("Wren answered 408 Request Timeout"));
        // A refused token never reached the run.
        assert!(!rejected("Wren answered 401 Unauthorized"));
        assert!(!rejected("Wren answered 403 Forbidden"));
        assert!(!rejected("Wren answered 502 Bad Gateway"));
        assert!(!rejected("can't reach Wren: connection refused"));
    }

    /// Wren in memory: what was posted, and what it answers.
    #[derive(Default)]
    struct FakeWren {
        posts: Mutex<Vec<String>>,
        step_err: Option<&'static str>,
        finish_err: Option<&'static str>,
    }

    impl Wren for FakeWren {
        async fn step(&self, run: i64, step: &str, status: &str, log: &str) -> Result<(), String> {
            self.posts
                .lock()
                .unwrap()
                .push(format!("step {run} {step} {status} {log}"));
            self.step_err.map_or(Ok(()), |e| Err(e.to_string()))
        }
        async fn finish(&self, run: i64, outcome: &str, note: &Note) -> Result<(), String> {
            self.posts
                .lock()
                .unwrap()
                .push(format!("finish {run} {outcome} {}", note.code));
            self.finish_err.map_or(Ok(()), |e| Err(e.to_string()))
        }
    }

    impl FakeWren {
        fn posts(&self) -> Vec<String> {
            self.posts.lock().unwrap().clone()
        }
    }

    fn agent_job(s: &StateFile) -> Job {
        Job {
            run_id: s.run_id,
            kind: JobKind::AgentUpdate,
            target: TargetRef {
                kind: "agent".into(),
                name: s.container.clone(),
            },
            from_key: String::new(),
            to_key: String::new(),
            images: Vec::new(),
            health_seconds: 0,
            keep_snapshots: 0,
            rollback_of: None,
            require_snapshot: false,
            to_version: s.to_version.clone(),
            to_digest: s.to_digest.clone(),
        }
    }

    /// A config folder with the state file and a ledger that has run 57 running.
    fn folder(name: &str, s: &StateFile) -> (PathBuf, PathBuf, PathBuf) {
        let dir = temp(name);
        let path = dir.join(crate::selfupdate::STATE_FILE);
        state::save(&path, s).unwrap();
        let ledger = dir.join("runs.json");
        let (mut l, _) = Ledger::load(&ledger);
        l.begin(&agent_job(s), 100).unwrap();
        (dir, path, ledger)
    }

    fn ledger_outcome(ledger: &Path) -> String {
        let (l, _) = Ledger::load(ledger);
        l.entries("agent", "wren-agent")[0].outcome.clone()
    }

    #[tokio::test]
    async fn a_finished_update_is_reported_step_by_step_then_finished() {
        let mut s = at(Phase::Done, "0.10.0", "0.10.1", false);
        s.stable_seconds = 60;
        let (dir, path, ledger) = folder("done", &s);
        let wren = FakeWren::default();
        report_to(&wren, &mut s, &path, &ledger).await.unwrap();
        assert_eq!(
            wren.posts(),
            [
                "step 57 update ok the new agent took over",
                "step 57 health ok back as 0.10.1, stable for 60 s",
                "finish 57 succeeded selfupdate.done",
            ]
        );
        assert_eq!(ledger_outcome(&ledger), "succeeded");
        assert!(s.reported);
        assert!(state::load(&path).unwrap().unwrap().reported);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_rollback_is_reported_with_its_reason() {
        let mut s = at(Phase::RolledBack, "0.10.0", "0.10.1", false);
        let (dir, path, ledger) = folder("rb", &s);
        let wren = FakeWren::default();
        report_to(&wren, &mut s, &path, &ledger).await.unwrap();
        assert_eq!(
            wren.posts(),
            [
                "step 57 update failed why",
                "step 57 rollback ok why",
                "finish 57 rolled_back selfupdate.rolled_back",
            ]
        );
        assert_eq!(ledger_outcome(&ledger), "rolled_back");
        let _ = std::fs::remove_dir_all(&dir);

        let mut s = at(Phase::RollbackFailed, "0.10.0", "0.10.1", false);
        let (dir, path, ledger) = folder("rbf", &s);
        let wren = FakeWren::default();
        report_to(&wren, &mut s, &path, &ledger).await.unwrap();
        assert_eq!(wren.posts()[1], "step 57 rollback failed why");
        assert_eq!(
            wren.posts()[2],
            "finish 57 rollback_failed selfupdate.rollback_failed"
        );
        assert_eq!(ledger_outcome(&ledger), "rollback_failed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_report_that_didnt_reach_wren_is_tried_again() {
        let mut s = at(Phase::Done, "0.10.0", "0.10.1", false);
        let (dir, path, ledger) = folder("retry", &s);
        let wren = FakeWren {
            finish_err: Some("can't reach Wren: down"),
            ..FakeWren::default()
        };
        assert!(report_to(&wren, &mut s, &path, &ledger).await.is_err());
        assert!(!s.reported && !state::load(&path).unwrap().unwrap().reported);
        assert_eq!(ledger_outcome(&ledger), "running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_run_wren_has_closed_counts_as_reported() {
        let mut s = at(Phase::Done, "0.10.0", "0.10.1", false);
        let (dir, path, ledger) = folder("gone", &s);
        let wren = FakeWren {
            step_err: Some("Wren answered 404 Not Found"),
            finish_err: Some("Wren answered 404 Not Found"),
            ..FakeWren::default()
        };
        report_to(&wren, &mut s, &path, &ledger).await.unwrap();
        // The steps were tried and the finish too: a refused step doesn't
        // stop it.
        assert_eq!(wren.posts().len(), 3);
        assert!(s.reported && state::load(&path).unwrap().unwrap().reported);
        assert_eq!(ledger_outcome(&ledger), "succeeded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_run_that_is_not_over_is_not_reported() {
        let mut s = at(Phase::RollingBack, "0.10.0", "0.10.1", false);
        let (dir, path, ledger) = folder("rolling", &s);
        let wren = FakeWren::default();
        assert!(report_to(&wren, &mut s, &path, &ledger).await.is_err());
        assert!(wren.posts().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Docker in memory for the leftover sweep: which containers exist (with
    /// what inspect says), what was tagged and what was removed.
    struct FakeDocker {
        existing: Vec<(String, Value)>,
        fail: bool,
        tags: Mutex<Vec<String>>,
        removed: Mutex<Vec<String>>,
    }

    impl FakeDocker {
        fn new(existing: &[&str], fail: bool) -> FakeDocker {
            FakeDocker {
                existing: existing
                    .iter()
                    .map(|n| ((*n).to_string(), json!({})))
                    .collect(),
                fail,
                tags: Mutex::new(Vec::new()),
                removed: Mutex::new(Vec::new()),
            }
        }
        fn with(mut self, name: &str, inspect: Value) -> FakeDocker {
            self.existing.retain(|(n, _)| n != name);
            self.existing.push((name.to_string(), inspect));
            self
        }
        fn removed(&self) -> Vec<String> {
            self.removed.lock().unwrap().clone()
        }
    }

    impl Engine for FakeDocker {
        async fn inspect(&self, name: &str) -> Result<Option<Value>, String> {
            if self.fail {
                return Err("Docker didn't answer".into());
            }
            let removed = self.removed.lock().unwrap();
            Ok(self
                .existing
                .iter()
                .find(|(n, _)| n == name && !removed.contains(n))
                .map(|(_, v)| v.clone()))
        }
        async fn image_config(&self, _: &str) -> Value {
            Value::Null
        }
        async fn stop(&self, _: &str, _: u64) -> Result<(), String> {
            unreachable!()
        }
        async fn rename(&self, _: &str, _: &str) -> Result<(), String> {
            unreachable!()
        }
        async fn create(&self, _: &str, _: &Value) -> Result<String, String> {
            unreachable!()
        }
        async fn start(&self, _: &str) -> Result<(), String> {
            unreachable!()
        }
        async fn remove(&self, name: &str, _: bool) -> Result<(), String> {
            self.removed.lock().unwrap().push(name.to_string());
            Ok(())
        }
        async fn tag(&self, source: &str, target: &str) -> Result<(), String> {
            self.tags.lock().unwrap().push(format!("{source} {target}"));
            Ok(())
        }
        fn host_boot(&self) -> String {
            String::new()
        }
        fn unraid_status_file(&self) -> PathBuf {
            PathBuf::new()
        }
        fn now(&self) -> u64 {
            0
        }
        async fn sleep(&self, _: std::time::Duration) {}
    }

    fn crashed_prepare(name: &str) -> (PathBuf, PathBuf) {
        let mut s = at(Phase::Prepared, "0.10.0", "0.10.1", false);
        s.updated_at = 1000;
        s.old_image_id = "sha256:old".into();
        s.reference = "ghcr.io/ferdinand99/wise-wren-agent:latest".into();
        let (dir, path, _) = folder(name, &s);
        std::fs::create_dir_all(dir.join("selfupdate").join("57")).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn a_prepared_run_whose_helper_never_came_is_settled_as_rolled_back() {
        let (dir, path) = crashed_prepare("settle");
        let docker = FakeDocker::new(&[], false);
        assert!(settle_prepared(&docker, &dir, 0, 5000).await);
        let s = state::load(&path).unwrap().unwrap();
        assert_eq!(s.phase, Phase::RolledBack);
        assert!(s.reason.contains("helper"), "{}", s.reason);
        assert_eq!(s.updated_at, 5000);
        assert_eq!(
            *docker.tags.lock().unwrap(),
            ["sha256:old ghcr.io/ferdinand99/wise-wren-agent:latest"]
        );
        assert!(!dir.join("selfupdate").join("57").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_helper_that_exists_or_a_docker_that_didnt_answer_settles_nothing() {
        let (dir, path) = crashed_prepare("nosettle");
        let docker = FakeDocker::new(&["wren-agent-selfupdate-57"], false);
        assert!(!settle_prepared(&docker, &dir, 0, 5000).await);
        let docker = FakeDocker::new(&[], true);
        assert!(!settle_prepared(&docker, &dir, 0, 5000).await);
        // Young, with a grace to wait out.
        let docker = FakeDocker::new(&[], false);
        assert!(!settle_prepared(&docker, &dir, 600, 1100).await);
        assert_eq!(state::load(&path).unwrap().unwrap().phase, Phase::Prepared);
        assert!(docker.tags.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_helper_of_the_finished_run_is_swept() {
        let label = |run: &str| json!({"tools.hedgerow.wren.selfupdate": "helper", "tools.hedgerow.wren.run": run});
        let all = [
            ("wren-agent-selfupdate-57".to_string(), label("57")),
            // The next run's helper, just created: not this state's to remove.
            ("wren-agent-selfupdate-58".to_string(), label("58")),
            // Carries the label under another value, or no run at all.
            (
                "other".to_string(),
                json!({"tools.hedgerow.wren.selfupdate": "x", "tools.hedgerow.wren.run": "57"}),
            ),
            (
                "norun".to_string(),
                json!({"tools.hedgerow.wren.selfupdate": "helper"}),
            ),
        ];
        let s = at(Phase::RolledBack, "0.10.0", "0.10.1", true);
        assert_eq!(leftovers(Some(&s), &all), vec!["wren-agent-selfupdate-57"]);
    }

    #[test]
    fn with_no_state_only_helpers_that_do_not_run_are_swept() {
        let list = json!([
            {"Names": ["/wren-agent-selfupdate-61"], "State": "running",
             "Labels": {"tools.hedgerow.wren.selfupdate": "helper", "tools.hedgerow.wren.run": "61"}},
            {"Names": ["/wren-agent-selfupdate-60"], "State": "exited",
             "Labels": {"tools.hedgerow.wren.selfupdate": "helper", "tools.hedgerow.wren.run": "60"}},
            {"Names": ["/postiz"], "State": "exited", "Labels": {}},
        ]);
        assert_eq!(sweepable(None, &list), vec!["wren-agent-selfupdate-60"]);
        // With a finished run the label decides, running or not.
        let s = at(Phase::RolledBack, "0.10.0", "0.10.1", true);
        assert!(sweepable(Some(&s), &list).is_empty());
    }

    #[test]
    fn a_helper_that_is_gone_or_stuck_is_not_waited_for() {
        let view = |status: &str, restarts: i64, idle: u64| HelperView {
            status: status.into(),
            restarts,
            idle,
        };
        assert!(helper_gone(None));
        assert!(!helper_gone(Some(&view("running", 0, 0))));
        // A helper that was killed comes back by itself.
        assert!(!helper_gone(Some(&view("restarting", 1, 0))));
        assert!(!helper_gone(Some(&view("exited", 1, 5))));
        // One that keeps falling over, or stays down, does not.
        assert!(helper_gone(Some(&view("restarting", 5, 0))));
        assert!(helper_gone(Some(&view("exited", 0, 100_000))));
        assert!(helper_gone(Some(&view("dead", 0, 100_000))));
    }

    #[test]
    fn a_confirmed_run_without_a_helper_is_finished_by_the_new_agent() {
        use Phase::*;
        let s = |p| at(p, "0.10.0", "0.10.1", false);
        assert_eq!(orphan(&s(NewStarted), true, true), Orphan::Finish);
        assert_eq!(orphan(&s(Confirmed), true, true), Orphan::Finish);
        // Never confirmed: nobody may call it done; the way back is by hand.
        assert_eq!(orphan(&s(NewStarted), true, false), Orphan::BreakGlass);
        // A helper that lives decides for itself.
        assert_eq!(orphan(&s(NewStarted), false, true), Orphan::Wait);
        for p in [NewCreated, OldStopped, RollingBack, Done, RolledBack] {
            assert_eq!(orphan(&s(p), true, true), Orphan::Wait, "{p:?}");
        }
    }

    fn adoptable(name: &str, phase: Phase, marker: bool) -> (PathBuf, PathBuf) {
        let s = at(phase, "0.10.0", crate::heartbeat::VERSION, false);
        let (dir, path, _) = folder(name, &s);
        std::fs::create_dir_all(dir.join("selfupdate").join("57")).unwrap();
        if marker {
            state::write_confirm(&dir, 57).unwrap();
        }
        (dir, path)
    }

    #[tokio::test]
    async fn the_new_agent_finishes_a_confirmed_run_whose_helper_is_gone() {
        let (dir, path) = adoptable("adopt", Phase::Confirmed, true);
        let docker = FakeDocker::new(&["wren-agent-wren-old-57"], false);
        assert!(adopt(&docker, &dir, 7000, true).await);
        let s = state::load(&path).unwrap().unwrap();
        assert_eq!((s.phase, s.updated_at), (Phase::Done, 7000));
        assert_eq!(docker.removed(), ["wren-agent-wren-old-57"]);
        assert!(!dir.join("selfupdate").join("57").exists());
        // The ordinary report path now tells Wren.
        assert!(matches!(
            role(Some(s), crate::heartbeat::VERSION),
            Role::Report(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_helper_stuck_restarting_is_removed_before_the_new_agent_finishes() {
        let (dir, path) = adoptable("stuck", Phase::NewStarted, true);
        let docker = FakeDocker::new(&["wren-agent-wren-old-57"], false).with(
            "wren-agent-selfupdate-57",
            json!({"State": {"Status": "restarting"}, "RestartCount": 9}),
        );
        assert!(adopt(&docker, &dir, 7000, true).await);
        assert_eq!(
            docker.removed(),
            ["wren-agent-selfupdate-57", "wren-agent-wren-old-57"]
        );
        assert_eq!(state::load(&path).unwrap().unwrap().phase, Phase::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn nothing_is_finished_while_the_helper_lives_or_without_a_confirmation() {
        // A running helper.
        let (dir, path) = adoptable("alive", Phase::Confirmed, true);
        let docker = FakeDocker::new(&[], false).with(
            "wren-agent-selfupdate-57",
            json!({"State": {"Status": "running"}, "RestartCount": 0}),
        );
        assert!(!adopt(&docker, &dir, 7000, true).await);
        // Docker not answering.
        let broken = FakeDocker::new(&[], true);
        assert!(!adopt(&broken, &dir, 7000, true).await);
        assert_eq!(state::load(&path).unwrap().unwrap().phase, Phase::Confirmed);
        assert!(docker.removed().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        // No marker: the new agent never confirmed.
        let (dir, path) = adoptable("nomark", Phase::NewStarted, false);
        let docker = FakeDocker::new(&["wren-agent-wren-old-57"], false);
        assert!(!adopt(&docker, &dir, 7000, true).await);
        assert_eq!(
            state::load(&path).unwrap().unwrap().phase,
            Phase::NewStarted
        );
        assert!(docker.removed().is_empty());
        assert!(dir.join("selfupdate").join("57").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_helper_that_was_created_but_never_started_is_settled_after_the_grace() {
        let created = json!({"State": {"Status": "created"}});
        let (dir, path) = crashed_prepare("created");
        let docker = FakeDocker::new(&[], false).with("wren-agent-selfupdate-57", created);
        // Young: the old agent may be about to start it.
        assert!(!settle_prepared(&docker, &dir, 600, 1100).await);
        assert!(docker.removed().is_empty());
        // Old: removed, and the run is settled.
        assert!(settle_prepared(&docker, &dir, 600, 1700).await);
        assert_eq!(docker.removed(), ["wren-agent-selfupdate-57"]);
        assert_eq!(
            state::load(&path).unwrap().unwrap().phase,
            Phase::RolledBack
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
