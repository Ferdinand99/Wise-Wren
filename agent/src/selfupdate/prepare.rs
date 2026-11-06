//! The old agent's part of a self-update: the preflight, the preparation and
//! the hand-over to the helper container (design §6.2, §8.2). Nothing here
//! stops or renames anything: the helper does that once it runs, so every
//! failure before the hand-over leaves the agent as it was.
//!
//! `helper_id` in the state file is set here, between creating the helper
//! and starting it, so the file always has one writer: once the helper
//! runs, only the helper writes it. The helper never sets `result` or
//! `helper_id`.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::helper::{DockerEngine, Engine as _};
use super::report::{Own, blocked, config_mount, own as read_own};
use super::state::{self, FORMAT, Phase, StateFile, break_glass};
use super::version::{Semver, parse, tag_rule, target_ok};
use super::{
    BACKUP_DIR, HARD, HELPER_LABEL, HELPER_START, RUN_LABEL, SELF_UPDATE_REPO, STATE_FILE,
    config_dir, helper_name, scaled, stable_seconds,
};
use crate::config::Config;
use crate::docker::{cli, get_json, post_json};
use crate::heartbeat::VERSION;
use crate::jobs::{Job, JobClient, JobKind, Note, TargetRef, valid_digest};
use crate::ledger::{self, Ledger};
use crate::run::{ClientReport, PREFLIGHT_LIMIT, Report, admit, rollback_tag};

/// What the job loop does after a self-update job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    /// The job is over (it was refused, failed or cancelled): carry on.
    Done,
    /// The helper runs and will stop this agent: take no more jobs.
    Stopping,
}

/// The label on an image that says which release it is.
const VERSION_LABEL: &str = "org.opencontainers.image.version";
/// The folders and files the helper's rollback restores come from here.
const BACKED_UP: [&str; 2] = ["managed.json", "runs.json"];
/// How often the old agent looks at the state file while the helper starts.
const POLL: Duration = Duration::from_millis(500);

/// The job's digest and version are well formed and the version is newer
/// than `current`; the version parsed.
pub fn check_job(job: &Job, current: &str) -> Result<Semver, Note> {
    if !valid_digest(&job.to_digest) {
        return Err(Note::new("selfupdate.bad_digest", &[]));
    }
    target_ok(&job.to_version, current).map_err(|_| {
        Note::new(
            "selfupdate.bad_version",
            &[("version", &job.to_version), ("current", current)],
        )
    })
}

/// No earlier self-update is still going on or waiting to be reported.
/// `run` is the new run's id; the ledger refuses a run that isn't newer.
pub fn may_start(existing: Option<&StateFile>, _run: i64) -> Result<(), Note> {
    match existing {
        Some(s) if !(s.phase.terminal() && s.reported) => Err(Note::new(
            "selfupdate.busy",
            &[("run", &s.run_id.to_string())],
        )),
        _ => Ok(()),
    }
}

/// The image's `org.opencontainers.image.version` label is `to_version`.
pub fn label_ok(image_inspect: &Value, to_version: &str) -> bool {
    image_inspect
        .pointer("/Config/Labels")
        .and_then(|labels| labels.get(VERSION_LABEL))
        .and_then(Value::as_str)
        == Some(to_version)
}

/// The release tag's image is the one with `digest` in `repo`.
pub fn release_ok(repo_digests: &[String], repo: &str, digest: &str) -> bool {
    let want = format!("{repo}@{digest}");
    repo_digests.contains(&want)
}

/// The environment entries the helper gets from the agent's: only the time
/// zone and the log level, never a token or a server address. A test build
/// also passes the test time scale on, or the helper's deadlines would run
/// unscaled beside the agent's scaled ones.
fn helper_env(inspect: &Value) -> Vec<String> {
    const KEEP: &[&str] = &[
        "TZ=",
        "RUST_LOG=",
        #[cfg(feature = "selfupdate-test")]
        "WW_SU_TEST_TIME_PCT=",
    ];
    inspect
        .pointer("/Config/Env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|e| KEEP.iter().any(|k| e.starts_with(k)))
        .map(str::to_string)
        .collect()
}

/// The body that creates the helper container (design §8.3): the old,
/// working image by id, no network, no token, the Docker socket and the
/// config folder, and on Unraid `/var/lib/docker` for the update status.
pub fn helper_body(
    own: &Own,
    run: i64,
    unraid: bool,
    socket: &str,
    config_dir: &Path,
) -> Result<Value, Note> {
    let config = config_mount(&own.inspect, config_dir).ok_or_else(|| {
        Note::new(
            "selfupdate.config_ro",
            &[("folder", &config_dir.display().to_string())],
        )
    })?;
    // The socket comes from where the agent gets it, else from its path.
    let socket_source = own
        .inspect
        .get("Mounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|m| {
            m.get("Destination").and_then(Value::as_str) == Some(socket)
                && m.get("Type").and_then(Value::as_str) == Some("bind")
        })
        .and_then(|m| m.get("Source").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .unwrap_or(socket);
    let mut mounts = vec![
        json!({"Type": "bind", "Source": socket_source, "Target": socket}),
        config,
    ];
    if unraid {
        mounts.push(
            json!({"Type": "bind", "Source": "/var/lib/docker", "Target": "/var/lib/docker"}),
        );
    }
    let dir = config_dir.display().to_string();
    let state = format!("{}/{STATE_FILE}", dir.trim_end_matches('/'));
    Ok(json!({
        "Image": own.image_id,
        "Cmd": ["--self-update-helper", "--state", state, "--socket", socket],
        "Env": helper_env(&own.inspect),
        "Labels": {(HELPER_LABEL): "helper", (RUN_LABEL): run.to_string()},
        "HostConfig": {
            "NetworkMode": "none",
            "RestartPolicy": {"Name": "unless-stopped"},
            "Mounts": mounts,
        },
    }))
}

/// What [`prepared_state`] needs to know.
pub struct Prep<'a> {
    pub own: &'a Own,
    pub job: &'a Job,
    /// Wren's id for this server, as the heartbeat reply says.
    pub server_id: i64,
    /// The old image's digest (its id when it has none).
    pub from_digest: &'a str,
    pub unraid: bool,
    pub confirm_minutes: u64,
    pub stable_seconds: u64,
    pub now: u64,
    pub host_boot: &'a str,
    pub config_dir: &'a Path,
}

/// The state file of a prepared run. It holds ids, names, versions and
/// digests; never the token or any other environment of the container.
pub fn prepared_state(p: &Prep<'_>) -> StateFile {
    StateFile {
        format: FORMAT,
        run_id: p.job.run_id,
        server_id: p.server_id,
        container: p.own.name.clone(),
        reference: p.own.reference.clone(),
        from_version: VERSION.to_string(),
        to_version: p.job.to_version.clone(),
        to_digest: p.job.to_digest.clone(),
        old_container_id: p.own.id.clone(),
        old_image_id: p.own.image_id.clone(),
        unraid: p.unraid,
        phase: Phase::Prepared,
        updated_at: p.now,
        hard_deadline: p.now + scaled(HARD),
        break_glass: break_glass(&p.own.name, p.job.run_id),
        from_digest: p.from_digest.to_string(),
        confirm_minutes: p.confirm_minutes,
        stable_seconds: p.stable_seconds,
        host_boot: p.host_boot.to_string(),
        config_dir: p.config_dir.display().to_string(),
        ..StateFile::default()
    }
}

fn pinned(image: &str, major: u64) -> Note {
    Note::new(
        "selfupdate.pinned",
        &[("image", image), ("major", &major.to_string())],
    )
}

/// The note for a reason [`blocked`] gave.
fn blocked_note(code: &str, own: Option<&Own>, config_dir: &Path, current: &str) -> Note {
    let image = own.map_or("", |o| o.reference.as_str());
    match code {
        "off" => Note::new("selfupdate.off", &[]),
        "no_own_id" => Note::new("selfupdate.no_own_id", &[]),
        "foreign_repo" => Note::new("selfupdate.foreign_repo", &[("image", image)]),
        "pinned" => pinned(image, parse(current).map_or(0, |v| v.major)),
        "autoremove" => Note::new("selfupdate.autoremove", &[]),
        "network_mode" => {
            let mode = own
                .and_then(|o| o.inspect.pointer("/HostConfig/NetworkMode"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Note::new("selfupdate.network_mode", &[("mode", mode)])
        }
        "config_ro" => Note::new(
            "selfupdate.config_ro",
            &[("folder", &config_dir.display().to_string())],
        ),
        _ => Note::new("selfupdate.not_container", &[]),
    }
}

/// The preflight's checks that need no Docker beyond the agent's own
/// container (design §6.2, 1-5 and 9-10): whatever [`blocked`] says, the
/// job's digest and version, and that the tag keeps up with the version the
/// job moves to (`:0.10` must not be moved to 0.11).
pub fn preflight(
    enabled: bool,
    own_id: &str,
    own: Option<&Own>,
    config_dir: &Path,
    job: &Job,
    current: &str,
) -> Result<Semver, Note> {
    let code = blocked(enabled, own_id, own, config_dir, current);
    if !code.is_empty() {
        return Err(blocked_note(code, own, config_dir, current));
    }
    let to = check_job(job, current)?;
    let own = own.ok_or_else(|| Note::new("selfupdate.not_container", &[]))?;
    if tag_rule(&own.reference, &to).is_err() {
        return Err(pinned(&own.reference, to.major));
    }
    Ok(to)
}

/// How a prepared run ended before the hand-over, or that it was handed over.
enum End {
    Refused(Note),
    Failed(Note),
    Cancelled,
    Handed,
}

/// Posts a failed preflight step and ends the run as failed.
async fn failed<R: Report>(report: &R, log: &str, note: Note) -> End {
    let _ = report.step("preflight", "failed", log, 0, &[]).await;
    End::Failed(note)
}

/// What the run changed before the helper started, put back: the reference
/// tag on the old image again, the state file and the backups gone.
async fn undo(config: &Config, own: &Own, run: i64) {
    let dir = config_dir(config);
    if let Err(e) = cli(
        &config.docker_socket,
        &["tag", &own.image_id, &own.reference],
        Duration::from_secs(30),
    )
    .await
    {
        tracing::warn!(run, "can't put the image tag back: {e}");
    }
    let _ = std::fs::remove_file(dir.join(STATE_FILE));
    let backup = dir.join(BACKUP_DIR);
    let _ = std::fs::remove_dir_all(backup.join(run.to_string()));
    let _ = std::fs::remove_dir(&backup);
}

/// The digest of `repo` the image id has locally, else the id itself.
async fn digest_of(socket: &str, image_id: &str) -> String {
    let prefix = format!("{SELF_UPDATE_REPO}@");
    get_json(socket, &format!("/images/{image_id}/json"))
        .await
        .ok()
        .and_then(|image| {
            image
                .get("RepoDigests")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .find_map(|d| d.strip_prefix(&prefix).map(str::to_string))
        })
        .unwrap_or_else(|| image_id.to_string())
}

/// The strings of an image's `RepoDigests`.
fn repo_digests(image: &Value) -> Vec<String> {
    image
        .get("RepoDigests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Copies the files a rollback restores to `<config>/selfupdate/<run>/`
/// (those that exist).
fn back_up(dir: &Path, run: i64) -> Result<(), String> {
    let to = dir.join(BACKUP_DIR).join(run.to_string());
    for name in BACKED_UP {
        match std::fs::read(dir.join(name)) {
            Ok(bytes) => crate::managed::write_private(&to.join(name), &bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("can't read {name}: {e}")),
        }
    }
    Ok(())
}

/// The helper has taken over: the state file is this run's and its phase is
/// `helper_started` or later.
fn started(path: &Path, run: i64) -> bool {
    matches!(state::load(path), Ok(Some(s)) if s.run_id == run && s.phase != Phase::Prepared)
}

/// Waits up to `limit` for [`started`].
async fn wait_started(path: &Path, run: i64, limit: Duration, poll: Duration) -> bool {
    let end = Instant::now() + limit;
    loop {
        if started(path, run) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Saves `s` and reads it back: a helper must never start on a file it
/// can't load (it would exit and restart forever).
fn save_checked(path: &Path, s: &StateFile) -> Result<(), String> {
    state::save(path, s)?;
    match state::load(path)? {
        Some(back) if back == *s => Ok(()),
        _ => Err(format!("{} didn't read back as written", path.display())),
    }
}

/// Writes the run down, then moves the reference onto `release` with `tag`
/// (source, target). The backups and the `prepared` state, which names the
/// old image, are saved before the tag moves: an agent that dies in between
/// leaves a run that [`settle_prepared`](super::probation::settle_prepared)
/// puts back. On an error the caller [`undo`]es (which also puts back a tag
/// that did move).
async fn record_then_retag<T, F>(
    tag: T,
    dir: &Path,
    prepared: &StateFile,
    release: &str,
    log: &mut Vec<String>,
) -> Result<(), String>
where
    T: Fn(String, String) -> F,
    F: std::future::Future<Output = Result<(), String>>,
{
    back_up(dir, prepared.run_id)?;
    save_checked(&dir.join(STATE_FILE), prepared)?;
    log.push(format!("state saved, hard limit in {} s", scaled(HARD)));
    tag(release.to_string(), prepared.reference.clone()).await?;
    log.push(format!("{} now names {release}", prepared.reference));
    Ok(())
}

/// Everything from the pulls to the hand-over. Runs after the ledger has
/// admitted the job.
async fn prepare_and_hand_over<R: Report>(
    config: &Config,
    report: &R,
    job: &Job,
    own: &Own,
    server_id: i64,
) -> End {
    let run = job.run_id;
    let socket = config.docker_socket.as_str();
    let dir = config_dir(config);
    let began = Instant::now();
    let unraid = crate::docker::is_unraid(Path::new("/"));
    let release = format!("{SELF_UPDATE_REPO}@{}", job.to_digest);
    let by_tag = format!("{SELF_UPDATE_REPO}:{}", job.to_version);
    let mut log = Vec::new();

    // The helper's body is built first: a config folder it can't mount ends
    // the run before the first pull.
    let body = match helper_body(own, run, unraid, socket, dir) {
        Ok(body) => body,
        Err(note) => return End::Refused(note),
    };

    // The pinned digest, and that it is a release of this version.
    if let Err(e) = cli(socket, &["pull", "--quiet", &release], PREFLIGHT_LIMIT).await {
        return failed(
            report,
            &e,
            Note::plain(format!("failed before anything changed: {e}")),
        )
        .await;
    }
    let image = match get_json(socket, &format!("/images/{release}/json")).await {
        Ok(image) => image,
        Err(e) => {
            return failed(
                report,
                &e,
                Note::plain(format!("failed before anything changed: {e}")),
            )
            .await;
        }
    };
    if !label_ok(&image, &job.to_version) {
        let label = image
            .pointer("/Config/Labels")
            .and_then(|l| l.get(VERSION_LABEL))
            .and_then(Value::as_str)
            .unwrap_or("none");
        let note = Note::new(
            "selfupdate.label_mismatch",
            &[("label", label), ("version", &job.to_version)],
        );
        return failed(report, &note.text(), note).await;
    }
    log.push(format!("pulled {release}"));
    if let Err(e) = cli(socket, &["pull", "--quiet", &by_tag], PREFLIGHT_LIMIT).await {
        return failed(
            report,
            &e,
            Note::plain(format!("failed before anything changed: {e}")),
        )
        .await;
    }
    let tagged = match get_json(socket, &format!("/images/{by_tag}/json")).await {
        Ok(image) => image,
        Err(e) => {
            return failed(
                report,
                &e,
                Note::plain(format!("failed before anything changed: {e}")),
            )
            .await;
        }
    };
    if !release_ok(&repo_digests(&tagged), SELF_UPDATE_REPO, &job.to_digest) {
        let note = Note::new(
            "selfupdate.not_release",
            &[("digest", &job.to_digest), ("version", &job.to_version)],
        );
        return failed(report, &note.text(), note).await;
    }
    log.push(format!("{by_tag} is {}", job.to_digest));

    // The old image is kept for a rollback; the new one becomes what the
    // container's own reference names.
    let from_digest = digest_of(socket, &own.image_id).await;
    let keep = rollback_tag(&format!("agent-{}", own.name), run, &own.name);
    let tag = |from: String, to: String| async move {
        cli(socket, &["tag", &from, &to], Duration::from_secs(30)).await
    };
    if let Err(e) = tag(own.image_id.clone(), keep.clone()).await {
        return failed(
            report,
            &e,
            Note::plain(format!("failed before anything changed: {e}")),
        )
        .await;
    }
    log.push(format!("old image kept as {keep}"));

    // The run is written down before the reference moves; from here every
    // way out puts the tag back.
    let engine = DockerEngine {
        socket: socket.to_string(),
    };
    let prepared = prepared_state(&Prep {
        own,
        job,
        server_id,
        from_digest: &from_digest,
        unraid,
        confirm_minutes: config.self_update.confirm_minutes,
        stable_seconds: stable_seconds(config.updates.min_health_seconds),
        now: state::now(),
        host_boot: &engine.host_boot(),
        config_dir: dir,
    });
    let path = dir.join(STATE_FILE);
    let retag = |from: String, to: String| async move { tag(from, to).await.map(|_| ()) };
    if let Err(e) = record_then_retag(retag, dir, &prepared, &release, &mut log).await {
        undo(config, own, run).await;
        return failed(
            report,
            &e,
            Note::plain(format!("failed before anything changed: {e}")),
        )
        .await;
    }

    let ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    let posted = report
        .step("preflight", "ok", &log.join("\n"), ms, &[])
        .await;
    match posted {
        // Wren can't be reached: nothing has changed, so nothing starts.
        Err(e) => {
            undo(config, own, run).await;
            return End::Failed(Note::new("failed.unreachable", &[("error", &e)]));
        }
        Ok(true) => {
            undo(config, own, run).await;
            return End::Cancelled;
        }
        Ok(false) => {}
    }

    // The helper. `update running` goes first: the helper may stop this
    // agent as soon as it runs, and the job task with it.
    let _ = report
        .step(
            "update",
            "running",
            "helper started; the agent restarts",
            0,
            &[],
        )
        .await;
    let name = helper_name(run);
    match hand_over(config, own, run, &name, body, prepared, &path).await {
        Ok(()) => End::Handed,
        Err(e) => {
            // The helper may exist in any state; it must not outlive the run.
            if let Err(rm) = cli(socket, &["rm", "-f", &name], Duration::from_secs(60)).await {
                tracing::warn!(run, "can't remove the helper: {rm}");
            }
            undo(config, own, run).await;
            let _ = report.step("update", "failed", &e, 0, &[]).await;
            End::Failed(Note::new("selfupdate.helper_failed", &[("error", &e)]))
        }
    }
}

/// Creates the helper, records its id, starts it and waits for it to say so.
async fn hand_over(
    config: &Config,
    own: &Own,
    run: i64,
    name: &str,
    body: Value,
    mut prepared: StateFile,
    path: &Path,
) -> Result<(), String> {
    let socket = config.docker_socket.as_str();
    let made = post_json(socket, &format!("/containers/create?name={name}"), &body).await?;
    let id = made
        .get("Id")
        .and_then(Value::as_str)
        .ok_or("Docker created the helper but sent no id")?;
    // Before the helper starts: from then on the helper alone writes the file.
    prepared.helper_id = id.to_string();
    save_checked(path, &prepared)?;
    cli(socket, &["start", id], Duration::from_secs(120)).await?;
    tracing::info!(run, helper = %name, container = %own.name, "helper started");
    if wait_started(path, run, Duration::from_secs(scaled(HELPER_START)), POLL).await {
        Ok(())
    } else {
        Err(format!(
            "the helper didn't report in within {} s",
            scaled(HELPER_START)
        ))
    }
}

/// The self-update job of the old agent, from the first report to the
/// hand-over. `server_id` is Wren's id for this server from the last
/// heartbeat reply (0: not known yet).
pub async fn handle(config: &Config, client: &JobClient, job: Job, server_id: i64) -> Handoff {
    let report = ClientReport {
        client,
        run: job.run_id,
    };
    prepare(config, &report, job, server_id).await
}

async fn prepare<R: Report>(config: &Config, report: &R, job: Job, server_id: i64) -> Handoff {
    let run = job.run_id;
    // Escaped (`?`): what the server sent can't forge log lines.
    tracing::info!(run, target = ?job.target.name, "agent update received");
    let _ = report.step("preflight", "running", "", 0, &[]).await;
    let refuse = |note: Note| async move {
        tracing::warn!(run, reason = ?note.text(), "job refused");
        report.finish("refused", &note).await;
        Handoff::Done
    };
    if job.kind != JobKind::AgentUpdate || job.target.kind != "agent" {
        return refuse(Note::plain("refused: this is not an update of the agent")).await;
    }
    // A job that comes before the first heartbeat reply has no id to
    // record, and the new agent would take its own Wren for another one.
    if server_id <= 0 {
        report
            .finish(
                "failed",
                &Note::plain(
                    "failed before anything changed: the agent doesn't know Wren's id for this server yet; try again",
                ),
            )
            .await;
        return Handoff::Done;
    }
    let dir = config_dir(config);
    let own = read_own(&config.docker_socket).await;
    if let Err(note) = preflight(
        config.self_update.enabled,
        &crate::jobs::own_id(),
        own.as_ref(),
        dir,
        &job,
        VERSION,
    ) {
        return refuse(note).await;
    }
    let Some(own) = own else {
        return refuse(Note::new("selfupdate.not_container", &[])).await;
    };
    match state::load(&dir.join(STATE_FILE)) {
        Ok(existing) => {
            if let Err(note) = may_start(existing.as_ref(), run) {
                return refuse(note).await;
            }
        }
        Err(e) => {
            return refuse(Note::plain(format!(
                "refused: the state file of an earlier self-update can't be read ({e}); remove it by hand if no update is running"
            )))
            .await;
        }
    }
    // The ledger knows an agent update by the container it replaces, not by
    // the name Wren shows.
    let mut recorded = job.clone();
    recorded.target = TargetRef {
        kind: "agent".into(),
        name: own.name.clone(),
    };
    let (mut ledger, warning) = Ledger::load(&config.ledger);
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
    if let Err(note) = admit(&mut ledger, &recorded, None, ledger::now()) {
        return refuse(note).await;
    }
    let (outcome, note) = match prepare_and_hand_over(config, report, &job, &own, server_id).await {
        // The new agent finishes the run and its ledger entry.
        End::Handed => return Handoff::Stopping,
        End::Refused(note) => ("refused", note),
        End::Failed(note) => ("failed", note),
        End::Cancelled => ("cancelled", Note::default()),
    };
    tracing::info!(run, outcome, note = ?note.text(), "self-update ended before the hand-over");
    report.finish(outcome, &note).await;
    if let Err(error) = ledger.finish(&recorded, outcome) {
        tracing::warn!(run, "{error}");
    }
    Handoff::Done
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Value, json};

    use super::*;
    use crate::jobs::{Job, JobKind, TargetRef};
    use crate::selfupdate::report::Own;
    use crate::selfupdate::state::{Phase, StateFile, break_glass};

    fn own(env: &[&str], mounts: Value, host: Value) -> Own {
        Own::from_inspect(json!({
            "Id": "a".repeat(64), "Name": "/wren-agent", "Image": "sha256:old",
            "Config": {"Image": "ghcr.io/ferdinand99/wise-wren-agent:latest", "Env": env},
            "HostConfig": host, "Mounts": mounts
        }))
    }
    fn binds() -> Value {
        json!([{"Type": "bind", "Source": "/var/run/docker.sock", "Destination": "/var/run/docker.sock"},
               {"Type": "bind", "Source": "/etc/wren-agent", "Destination": "/config"}])
    }
    fn d(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }
    fn test_job(run: i64, to_version: &str, to_digest: &str) -> Job {
        Job {
            run_id: run,
            kind: JobKind::AgentUpdate,
            target: TargetRef {
                kind: "agent".into(),
                name: "wren-agent".into(),
            },
            from_key: String::new(),
            to_key: String::new(),
            images: Vec::new(),
            health_seconds: 180,
            keep_snapshots: 2,
            rollback_of: None,
            require_snapshot: false,
            to_version: to_version.into(),
            to_digest: to_digest.into(),
        }
    }

    #[test]
    fn preflight_refusals_have_codes() {
        let mut j = test_job(57, "0.10.1", &d('b'));
        assert!(check_job(&j, "0.10.0").is_ok());
        j.to_digest = "sha256:short".into();
        assert_eq!(
            check_job(&j, "0.10.0").unwrap_err().code,
            "selfupdate.bad_digest"
        );
        let j = test_job(57, "0.10.0", &d('b'));
        assert_eq!(
            check_job(&j, "0.10.0").unwrap_err().code,
            "selfupdate.bad_version"
        );
        assert!(label_ok(
            &json!({"Config": {"Labels": {"org.opencontainers.image.version": "0.10.1"}}}),
            "0.10.1"
        ));
        assert!(!label_ok(
            &json!({"Config": {"Labels": {"org.opencontainers.image.version": "edge"}}}),
            "0.10.1"
        ));
        let repo = "ghcr.io/ferdinand99/wise-wren-agent";
        assert!(release_ok(&[format!("{repo}@{}", d('b'))], repo, &d('b')));
        assert!(!release_ok(&[format!("{repo}@{}", d('c'))], repo, &d('b')));
    }

    #[test]
    fn helper_body_is_minimal() {
        let o = own(
            &[
                "WW_AGENT_TOKEN=wwa_x",
                "WW_SERVER_URL=http://w:3003",
                "TZ=Europe/Oslo",
                "RUST_LOG=info",
            ],
            binds(),
            json!({}),
        );
        let b = helper_body(&o, 57, false, "/var/run/docker.sock", Path::new("/config")).unwrap();
        assert_eq!(b["Image"], "sha256:old");
        assert_eq!(
            b["Cmd"],
            json!([
                "--self-update-helper",
                "--state",
                "/config/selfupdate.json",
                "--socket",
                "/var/run/docker.sock"
            ])
        );
        assert_eq!(b["Env"], json!(["TZ=Europe/Oslo", "RUST_LOG=info"]));
        assert_eq!(b["HostConfig"]["NetworkMode"], "none");
        assert_eq!(b["HostConfig"]["RestartPolicy"]["Name"], "unless-stopped");
        assert_eq!(b["Labels"]["tools.hedgerow.wren.selfupdate"], "helper");
        assert_eq!(b["Labels"]["tools.hedgerow.wren.run"], "57");
        assert_eq!(b["Labels"].as_object().unwrap().len(), 2);
        assert_eq!(b["HostConfig"]["Mounts"].as_array().unwrap().len(), 2);
        let u = helper_body(&o, 57, true, "/var/run/docker.sock", Path::new("/config")).unwrap();
        assert!(
            u["HostConfig"]["Mounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["Target"] == "/var/lib/docker")
        );
        assert_eq!(u["HostConfig"]["Mounts"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn helper_body_mounts_the_socket_from_where_the_agent_gets_it() {
        let mounts = json!([{"Type": "bind", "Source": "/run/user/docker.sock", "Destination": "/var/run/docker.sock"},
                            {"Type": "bind", "Source": "/etc/wren-agent", "Destination": "/config"}]);
        let b = helper_body(
            &own(&[], mounts, json!({})),
            57,
            false,
            "/var/run/docker.sock",
            Path::new("/config"),
        )
        .unwrap();
        assert!(b["HostConfig"]["Mounts"].as_array().unwrap().contains(
            &json!({"Type": "bind", "Source": "/run/user/docker.sock", "Target": "/var/run/docker.sock"})
        ));
        // No mount for the socket (a path the agent reached some other way):
        // the path itself is the source.
        let mounts =
            json!([{"Type": "bind", "Source": "/etc/wren-agent", "Destination": "/config"}]);
        let b = helper_body(
            &own(&[], mounts, json!({})),
            57,
            false,
            "/var/run/docker.sock",
            Path::new("/config"),
        )
        .unwrap();
        assert!(b["HostConfig"]["Mounts"].as_array().unwrap().contains(
            &json!({"Type": "bind", "Source": "/var/run/docker.sock", "Target": "/var/run/docker.sock"})
        ));
    }

    #[test]
    fn helper_body_mounts_a_named_config_volume_at_the_same_path() {
        let mounts = json!([{"Type": "bind", "Source": "/var/run/docker.sock", "Destination": "/var/run/docker.sock"},
                            {"Type": "volume", "Name": "wren-cfg", "Source": "/var/lib/docker/volumes/wren-cfg/_data", "Destination": "/etc/wren"}]);
        let b = helper_body(
            &own(&[], mounts, json!({})),
            57,
            false,
            "/var/run/docker.sock",
            Path::new("/etc/wren"),
        )
        .unwrap();
        assert!(
            b["HostConfig"]["Mounts"]
                .as_array()
                .unwrap()
                .contains(&json!({"Type": "volume", "Source": "wren-cfg", "Target": "/etc/wren"}))
        );
        assert_eq!(b["Cmd"][2], "/etc/wren/selfupdate.json");
        let none = helper_body(
            &own(&[], json!([]), json!({})),
            57,
            false,
            "/var/run/docker.sock",
            Path::new("/config"),
        );
        assert_eq!(none.unwrap_err().code, "selfupdate.config_ro");
    }

    #[cfg(not(feature = "selfupdate-test"))]
    #[test]
    fn a_release_helper_never_gets_the_test_time_scale() {
        let o = own(
            &["TZ=Europe/Oslo", "WW_SU_TEST_TIME_PCT=10"],
            binds(),
            json!({}),
        );
        let b = helper_body(&o, 57, false, "/var/run/docker.sock", Path::new("/config")).unwrap();
        assert_eq!(b["Env"], json!(["TZ=Europe/Oslo"]));
    }

    #[cfg(feature = "selfupdate-test")]
    #[test]
    fn a_test_helper_gets_the_test_time_scale_when_set() {
        let o = own(
            &["TZ=Europe/Oslo", "WW_SU_TEST_TIME_PCT=10", "OTHER=1"],
            binds(),
            json!({}),
        );
        let b = helper_body(&o, 57, false, "/var/run/docker.sock", Path::new("/config")).unwrap();
        assert_eq!(
            b["Env"],
            json!(["TZ=Europe/Oslo", "WW_SU_TEST_TIME_PCT=10"])
        );
        let o = own(&["TZ=Europe/Oslo"], binds(), json!({}));
        let b = helper_body(&o, 57, false, "/var/run/docker.sock", Path::new("/config")).unwrap();
        assert_eq!(b["Env"], json!(["TZ=Europe/Oslo"]));
    }

    #[test]
    fn the_state_file_never_holds_the_token() {
        let o = own(
            &["WW_AGENT_TOKEN=wwa_0123456789abcdefghijABCDEFGHIJ0123456789xyz"],
            binds(),
            json!({}),
        );
        let j = test_job(57, "0.10.1", &d('b'));
        let s = prepared_state(&Prep {
            own: &o,
            job: &j,
            server_id: 3,
            from_digest: &d('a'),
            unraid: false,
            confirm_minutes: 5,
            stable_seconds: 60,
            now: 1000,
            host_boot: "boot",
            config_dir: Path::new("/config"),
        });
        let text = serde_json::to_string(&s).unwrap();
        assert!(!text.contains("wwa_"), "{text}");
        assert_eq!(
            (s.phase, s.server_id, s.container.as_str()),
            (Phase::Prepared, 3, "wren-agent")
        );
        assert_eq!(s.break_glass, break_glass("wren-agent", 57));
        let b = helper_body(&o, 57, false, "/var/run/docker.sock", Path::new("/config")).unwrap();
        assert!(!b.to_string().contains("wwa_"));
    }

    #[test]
    fn a_pending_self_update_refuses_a_second_one() {
        let at = |phase, reported| StateFile {
            run_id: 57,
            phase,
            reported,
            ..StateFile::default()
        };
        assert!(may_start(None, 58).is_ok());
        assert!(may_start(Some(&at(Phase::Done, true)), 58).is_ok());
        for s in [
            at(Phase::NewStarted, false),
            at(Phase::Done, false),
            at(Phase::RolledBack, false),
        ] {
            assert_eq!(may_start(Some(&s), 58).unwrap_err().code, "selfupdate.busy");
        }
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wren-su-prep-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn own_at(reference: &str, dir: &Path) -> Own {
        Own::from_inspect(json!({
            "Id": "a".repeat(64), "Name": "/wren-agent", "Image": "sha256:old",
            "Config": {"Image": reference, "Env": []},
            "HostConfig": {},
            "Mounts": [{"Type": "bind", "Source": "/mnt/cfg", "Destination": dir}]
        }))
    }

    #[test]
    fn a_tag_must_cover_the_version_the_job_moves_to() {
        use crate::selfupdate::SELF_UPDATE_REPO as REPO;
        let dir = temp("tag");
        let id = "a".repeat(64);
        let go = |tag: &str, to: &str| {
            let own = own_at(&format!("{REPO}:{tag}"), &dir);
            let job = test_job(57, to, &d('b'));
            preflight(true, &id, Some(&own), &dir, &job, "0.10.0")
                .err()
                .map(|n| n.code)
        };
        // `:0.10` covers 0.10.1, but not 0.11.0: it would move to 0.11 content.
        assert_eq!(go("0.10", "0.10.1"), None);
        assert_eq!(go("0.10", "0.11.0"), Some("selfupdate.pinned"));
        assert_eq!(go("0", "0.11.0"), None);
        assert_eq!(go("0", "1.0.0"), Some("selfupdate.pinned"));
        assert_eq!(go("latest", "1.0.0"), None);
        // The job's own faults come as themselves.
        assert_eq!(go("latest", "0.10.0"), Some("selfupdate.bad_version"));
        let own = own_at(&format!("{REPO}:latest"), &dir);
        let off = preflight(
            false,
            &id,
            Some(&own),
            &dir,
            &test_job(57, "0.10.1", &d('b')),
            "0.10.0",
        );
        assert_eq!(off.unwrap_err().code, "selfupdate.off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_old_agent_waits_for_the_helper_to_take_over() {
        let dir = temp("wait");
        let path = dir.join(crate::selfupdate::STATE_FILE);
        let short = Duration::from_millis(80);
        let poll = Duration::from_millis(10);
        let mut s = StateFile {
            format: state::FORMAT,
            run_id: 57,
            phase: Phase::Prepared,
            ..StateFile::default()
        };
        assert!(!wait_started(&path, 57, short, poll).await, "no file");
        state::save(&path, &s).unwrap();
        assert!(
            !wait_started(&path, 57, short, poll).await,
            "still prepared"
        );
        // Any phase from helper_started on counts: the helper may be quick.
        for phase in [Phase::HelperStarted, Phase::OldStopped, Phase::Done] {
            s.phase = phase;
            state::save(&path, &s).unwrap();
            assert!(wait_started(&path, 57, short, poll).await, "{phase:?}");
        }
        assert!(!wait_started(&path, 58, short, poll).await, "another run");
        std::fs::write(&path, "nonsense").unwrap();
        assert!(!wait_started(&path, 57, short, poll).await, "unreadable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // An agent that dies right after the reference moved must leave a
    // `prepared` run naming the old image, which `settle_prepared` puts back.
    #[tokio::test]
    async fn the_run_is_written_down_before_the_reference_moves() {
        let dir = temp("retag");
        let path = dir.join(crate::selfupdate::STATE_FILE);
        std::fs::write(dir.join("runs.json"), "{}").unwrap();
        let s = StateFile {
            format: state::FORMAT,
            run_id: 57,
            phase: Phase::Prepared,
            reference: "ghcr.io/ferdinand99/wise-wren-agent:latest".into(),
            old_image_id: "sha256:old".into(),
            ..StateFile::default()
        };
        let release = "ghcr.io/ferdinand99/wise-wren-agent@sha256:new";
        // Each tag, and the state file as it was when the tag moved.
        let seen = std::sync::Mutex::new(Vec::new());
        let (seen_ref, path_ref) = (&seen, path.as_path());
        let tag = move |from: String, to: String| async move {
            let on_disk = state::load(path_ref).ok().flatten();
            seen_ref.lock().unwrap().push((from, to, on_disk));
            Ok::<(), String>(())
        };
        let mut log = Vec::new();
        record_then_retag(tag, &dir, &s, release, &mut log)
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [(release.to_string(), s.reference.clone(), Some(s.clone()))]
        );
        assert!(dir.join("selfupdate").join("57").join("runs.json").exists());
        assert_eq!(log.len(), 2);

        // A run that can't be written down never moves the reference.
        let blocked = dir.join("not-a-folder");
        std::fs::write(&blocked, "x").unwrap();
        assert!(
            record_then_retag(tag, &blocked, &s, release, &mut log)
                .await
                .is_err()
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_state_file_is_only_good_when_it_reads_back() {
        let dir = temp("readback");
        let path = dir.join(crate::selfupdate::STATE_FILE);
        let s = StateFile {
            format: state::FORMAT,
            run_id: 57,
            ..StateFile::default()
        };
        assert!(save_checked(&path, &s).is_ok());
        // A folder where the file belongs can't be replaced: an error, so no
        // helper starts on a file it couldn't load.
        let blocked = dir.join("sub");
        std::fs::create_dir_all(blocked.join(crate::selfupdate::STATE_FILE)).unwrap();
        assert!(save_checked(&blocked.join(crate::selfupdate::STATE_FILE), &s).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What reaches Wren.
    #[derive(Default)]
    struct Capture {
        steps: std::sync::Mutex<Vec<String>>,
        finished: std::sync::Mutex<Option<(String, Note)>>,
    }

    impl Report for Capture {
        async fn step(
            &self,
            step: &str,
            status: &str,
            _: &str,
            _: u64,
            _: &[Note],
        ) -> Result<bool, String> {
            self.steps.lock().unwrap().push(format!("{step}:{status}"));
            Ok(false)
        }
        async fn finish(&self, outcome: &str, note: &Note) {
            *self.finished.lock().unwrap() = Some((outcome.into(), note.clone()));
        }
        async fn monitors(&self) -> Result<(String, String), String> {
            Ok(("skipped".into(), String::new()))
        }
    }

    fn config() -> crate::config::Config {
        crate::config::Config::from_parts(
            Some("server = \"http://x\"\ntoken = \"wwa_0123456789abcdefghijABCDEFGHIJ0123456789xyz\"\n"),
            |_| None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_job_that_cannot_run_ends_before_anything_changes() {
        let run = |job: Job, server_id: i64| async move {
            let log = Capture::default();
            let end = prepare(&config(), &log, job, server_id).await;
            let finished = log.finished.lock().unwrap().clone().unwrap();
            let steps = log.steps.lock().unwrap().clone();
            (end, finished, steps)
        };
        // Not an agent target.
        let mut job = test_job(57, "0.10.1", &d('b'));
        job.target.kind = "container".into();
        let (end, (outcome, _), steps) = run(job, 3).await;
        assert_eq!((end, outcome.as_str()), (Handoff::Done, "refused"));
        assert_eq!(steps, ["preflight:running"]);
        // Wren's id for this server is not known yet: failed, nothing done.
        let (end, (outcome, note), _) = run(test_job(57, "0.10.1", &d('b')), 0).await;
        assert_eq!((end, outcome.as_str()), (Handoff::Done, "failed"));
        assert!(
            note.text().contains("before anything changed"),
            "{}",
            note.text()
        );
        // The host's switch is off (the default): refused with its code.
        let (end, (outcome, note), _) = run(test_job(57, "0.10.1", &d('b')), 3).await;
        assert_eq!(
            (end, outcome.as_str(), note.code),
            (Handoff::Done, "refused", "selfupdate.off")
        );
    }

    #[test]
    fn the_agent_create_body_keeps_what_matters() {
        let inspect = json!({"Id": "a".repeat(64), "Name": "/wren-agent",
            "Config": {"Image": "ghcr.io/ferdinand99/wise-wren-agent:latest", "Hostname": "aaaaaaaaaaaa",
                       "Env": ["WW_AGENT_TOKEN=wwa_x", "WW_AGENT_SELF_UPDATE=1", "PATH=/usr/bin"],
                       "Labels": {"com.docker.compose.project": "wren", "org.opencontainers.image.version": "0.10.0"}},
            "HostConfig": {"Binds": ["/etc/wren-agent:/config", "/var/run/docker.sock:/var/run/docker.sock"],
                           "RestartPolicy": {"Name": "unless-stopped"}, "NetworkMode": "wren_default"},
            "NetworkSettings": {"Networks": {"wren_default": {"Aliases": ["wren-agent", "aaaaaaaaaaaa"]}}}});
        let image = json!({"Env": ["PATH=/usr/bin"], "Labels": {"org.opencontainers.image.version": "0.10.0"}});
        let b = crate::run::create_body(
            &inspect,
            &image,
            "ghcr.io/ferdinand99/wise-wren-agent:latest",
        );
        assert_eq!(
            b["Env"],
            json!(["WW_AGENT_TOKEN=wwa_x", "WW_AGENT_SELF_UPDATE=1"])
        );
        assert_eq!(b["Labels"], json!({"com.docker.compose.project": "wren"}));
        assert_eq!(b["HostConfig"]["RestartPolicy"]["Name"], "unless-stopped");
        assert_eq!(
            b["NetworkingConfig"]["EndpointsConfig"]["wren_default"]["Aliases"],
            json!(["wren-agent"])
        );
        assert!(b.get("Hostname").is_none());
    }
}
