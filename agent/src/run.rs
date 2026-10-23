//! One run on the agent (design §6): preflight, stop, snapshot, update,
//! health check, and on failure the rollback. The agent runs every step
//! itself, so a lost connection to Wren can't leave a target stopped
//! half-way; it reports each step as it goes. The answer to the preflight
//! report is the last moment Wren can cancel: nothing has stopped yet.
//!
//! The steps are behind [`Host`] so the order and the rollback rules are
//! tested without Docker; [`DockerHost`] does them on this server.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::docker::{cli, get_json, post_json};
use crate::jobs::{self, Ctr, Job, JobClient, JobKind, Kind, Plan, StepReport, repo_of};
use crate::snapshot::{self, DataPath, Method, exec, tail};

/// Step time limits; the update of a script stack uses its own `timeout`.
pub const PREFLIGHT_LIMIT: Duration = Duration::from_secs(30 * 60);
pub const STOP_LIMIT: Duration = Duration::from_secs(10 * 60);
pub const SNAPSHOT_LIMIT: Duration = Duration::from_secs(2 * 60 * 60);
pub const UPDATE_LIMIT: Duration = Duration::from_secs(30 * 60);
pub const ROLLBACK_LIMIT: Duration = Duration::from_secs(2 * 60 * 60);
/// How long a container may take to become healthy, on top of the stable
/// period.
pub const START_GRACE: Duration = Duration::from_secs(300);
/// The longest log kept per step.
pub const LOG_CHARS: usize = 4000;
/// Unraid's own update script (spike Q2).
pub const UPDATE_CONTAINER: &str =
    "/usr/local/emhttp/plugins/dynamix.docker.manager/scripts/update_container";
/// Unraid's update status file (spike Q2).
pub const UNRAID_STATUS: &str = "/var/lib/docker/unraid-update-status.json";

/// What one run changed so far (the rollback undoes exactly that).
#[derive(Debug, Default)]
pub struct State {
    /// Containers Wren stopped.
    pub stopped: Vec<String>,
    /// Data paths with their method, from the preflight.
    pub paths: Vec<(DataPath, Method)>,
    /// Snapshots taken (an update) or to restore (a rollback job).
    pub snapshots: Vec<(DataPath, PathBuf)>,
    pub update_began: bool,
    /// The old container's name while the new one runs.
    pub renamed: Option<String>,
    /// Unraid's own `update_container` works here.
    pub unraid_script: bool,
    /// `(container, reference, image id)` before the update.
    pub old: Vec<(String, String, String)>,
    /// The pulled image id per container.
    pub new_ids: HashMap<String, String>,
    /// A script stack's Compose files before its script ran.
    pub saved_files: Vec<(PathBuf, Vec<u8>)>,
}

/// How a run ends, for clean-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Close {
    Succeeded,
    RolledBack,
    /// Nothing changed (preflight failed or cancelled).
    Abandoned,
}

type StepResult = Result<String, String>;

/// The steps on a server.
pub trait Host: Sync {
    fn preflight(&self, p: &Plan, s: &mut State) -> impl Future<Output = StepResult> + Send;
    fn stop(&self, p: &Plan, s: &mut State) -> impl Future<Output = StepResult> + Send;
    fn snapshot(&self, p: &Plan, s: &mut State) -> impl Future<Output = StepResult> + Send;
    fn update(&self, p: &Plan, s: &mut State) -> impl Future<Output = StepResult> + Send;
    fn health(&self, p: &Plan, s: &mut State) -> impl Future<Output = StepResult> + Send;
    fn rollback(&self, p: &Plan, s: &mut State) -> impl Future<Output = StepResult> + Send;
    fn close(&self, p: &Plan, s: &mut State, how: Close) -> impl Future<Output = ()> + Send;
}

/// Where step results go.
pub trait Report: Sync {
    /// Posts a step's state; whether Wren asks to cancel.
    fn step(
        &self,
        step: &str,
        status: &str,
        log: &str,
        ms: u64,
    ) -> impl Future<Output = Result<bool, String>> + Send;
    fn finish(&self, outcome: &str, detail: &str) -> impl Future<Output = ()> + Send;
    /// Asks Wren whether the target's Vigilant Vole monitors are up:
    /// `(verdict, log)` with `ok`, `wait`, `failed` or `skipped`.
    fn monitors(&self) -> impl Future<Output = Result<(String, String), String>> + Send;
    /// How long to wait before asking again.
    fn monitor_pause(&self) -> Duration {
        MONITOR_PAUSE
    }
}

/// How often the agent asks Wren about the monitors.
pub const MONITOR_PAUSE: Duration = Duration::from_secs(10);
/// Failed questions in a row after which the container checks decide.
pub const MONITOR_TRIES: u32 = 3;

/// Waits for Wren's monitor verdict (design §6 step 6). `Err`: the
/// monitors say the update is broken. When Wren can't be reached the
/// container checks, which passed, decide.
pub async fn wait_monitors<R: Report>(report: &R) -> Result<String, String> {
    let mut errors = 0;
    loop {
        match report.monitors().await {
            Ok((verdict, log)) => {
                errors = 0;
                match verdict.as_str() {
                    "ok" | "skipped" => return Ok(log),
                    "failed" => return Err(log),
                    _ => {}
                }
            }
            Err(e) => {
                errors += 1;
                if errors >= MONITOR_TRIES {
                    return Ok(format!(
                        "Wren couldn't be asked about the monitors ({e}); the container checks decide"
                    ));
                }
            }
        }
        tokio::time::sleep(report.monitor_pause()).await;
    }
}

/// Runs a step with its time limit and reports it; the step's result and
/// what posting the result brought (cancel or not).
async fn step<R: Report>(
    report: &R,
    name: &str,
    limit: Duration,
    fut: impl Future<Output = StepResult>,
) -> (StepResult, Result<bool, String>) {
    let _ = report.step(name, "running", "", 0).await;
    let t = Instant::now();
    let res = match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!("{name} took longer than {} s", limit.as_secs())),
    };
    let ms = u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (status, log) = match &res {
        Ok(l) => ("ok", l.as_str()),
        Err(e) => ("failed", e.as_str()),
    };
    let posted = report.step(name, status, &tail(log, LOG_CHARS), ms).await;
    (res, posted)
}

/// Rolls back after `err`, then reports the outcome.
async fn roll_back<H: Host, R: Report>(
    host: &H,
    report: &R,
    plan: &Plan,
    st: &mut State,
    err: String,
) -> &'static str {
    let (res, _) = step(report, "rollback", ROLLBACK_LIMIT, host.rollback(plan, st)).await;
    match res {
        Ok(_) => {
            host.close(plan, st, Close::RolledBack).await;
            let outcome = if st.update_began {
                "rolled_back"
            } else {
                "failed"
            };
            report.finish(outcome, &err).await;
            outcome
        }
        Err(e) => {
            // Stop here and keep everything as it is: a person decides.
            report
                .finish(
                    "rollback_failed",
                    &format!("{err}; the rollback failed too: {e}"),
                )
                .await;
            "rollback_failed"
        }
    }
}

/// Runs a checked job; the outcome reported.
pub async fn execute<H: Host, R: Report>(host: &H, report: &R, plan: &Plan) -> &'static str {
    let mut st = State::default();
    let job = plan.job();
    let manual = job.kind == JobKind::Rollback;

    let (res, posted) = step(
        report,
        "preflight",
        PREFLIGHT_LIMIT,
        host.preflight(plan, &mut st),
    )
    .await;
    if let Err(e) = res {
        host.close(plan, &mut st, Close::Abandoned).await;
        report.finish("failed", &e).await;
        return "failed";
    }
    // Nothing has stopped yet: Wren must say go.
    match posted {
        Err(e) => {
            host.close(plan, &mut st, Close::Abandoned).await;
            report
                .finish(
                    "failed",
                    &format!("Wren couldn't be reached before anything stopped: {e}"),
                )
                .await;
            return "failed";
        }
        Ok(true) => {
            host.close(plan, &mut st, Close::Abandoned).await;
            report.finish("cancelled", "").await;
            return "cancelled";
        }
        Ok(false) => {}
    }

    if plan.own_backup() {
        let why = "the script stops, backs up and verifies itself";
        let _ = report.step("stop", "skipped", why, 0).await;
        let _ = report.step("snapshot", "skipped", why, 0).await;
    } else {
        let (res, _) = step(report, "stop", STOP_LIMIT, host.stop(plan, &mut st)).await;
        if let Err(e) = res {
            if manual {
                report.finish("rollback_failed", &e).await;
                return "rollback_failed";
            }
            return roll_back(host, report, plan, &mut st, e).await;
        }
        if manual {
            let _ = report
                .step(
                    "snapshot",
                    "skipped",
                    "a rollback restores an earlier snapshot",
                    0,
                )
                .await;
        } else {
            let (res, _) = step(
                report,
                "snapshot",
                SNAPSHOT_LIMIT,
                host.snapshot(plan, &mut st),
            )
            .await;
            if let Err(e) = res {
                return roll_back(host, report, plan, &mut st, e).await;
            }
        }
    }

    let limit = plan
        .stack
        .as_ref()
        .map_or(UPDATE_LIMIT, |s| s.timeout + STOP_LIMIT);
    let (res, _) = step(report, "update", limit, host.update(plan, &mut st)).await;
    let res = match res {
        Ok(_) => {
            // The container checks, then the monitors (their own stable
            // period, within twice it and the grace).
            let health = Duration::from_secs(job.health_seconds) * 3 + START_GRACE * 2 + STOP_LIMIT;
            let st = &mut st;
            step(report, "health", health, async move {
                let mut log = host.health(plan, st).await?;
                let m = wait_monitors(report).await?;
                if !m.is_empty() {
                    log.push('\n');
                    log.push_str(&m);
                }
                Ok(log)
            })
            .await
            .0
        }
        Err(e) => Err(e),
    };
    match res {
        Ok(_) => {
            host.close(plan, &mut st, Close::Succeeded).await;
            report.finish("succeeded", "").await;
            "succeeded"
        }
        // A rollback that fails is not rolled back again: a person decides.
        Err(e) if manual => {
            report.finish("rollback_failed", &e).await;
            "rollback_failed"
        }
        Err(e) => roll_back(host, report, plan, &mut st, e).await,
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// One look at one container.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sample {
    pub name: String,
    pub exists: bool,
    pub status: String,
    /// `None` without a healthcheck.
    pub health: Option<String>,
    pub started_at: String,
    pub restarts: i64,
}

impl Sample {
    pub fn from_inspect(name: &str, v: Option<&Value>) -> Sample {
        let Some(v) = v else {
            return Sample {
                name: name.into(),
                ..Sample::default()
            };
        };
        let s = |p: &str| {
            v.pointer(p)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Sample {
            name: name.into(),
            exists: true,
            status: s("/State/Status"),
            health: v
                .pointer("/State/Health/Status")
                .and_then(Value::as_str)
                .map(str::to_string),
            started_at: s("/State/StartedAt"),
            restarts: v.get("RestartCount").and_then(Value::as_i64).unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Wait,
    Ok,
    Fail(String),
}

/// The health rules: every container running, its Docker healthcheck (if
/// any) healthy, no restart, for `need` without a break; all within
/// `need + START_GRACE`.
pub struct Watch {
    need: Duration,
    first: HashMap<String, (String, i64)>,
    stable_since: Option<Duration>,
}

impl Watch {
    pub fn new(need: Duration) -> Watch {
        Watch {
            need,
            first: HashMap::new(),
            stable_since: None,
        }
    }

    /// Judges the samples taken `at` after the start.
    pub fn observe(&mut self, samples: &[Sample], at: Duration) -> Verdict {
        let mut ready = true;
        for s in samples {
            if !s.exists {
                return Verdict::Fail(format!("{} is gone", s.name));
            }
            if matches!(s.status.as_str(), "exited" | "dead") {
                return Verdict::Fail(format!("{} stopped ({})", s.name, s.status));
            }
            let first = self
                .first
                .entry(s.name.clone())
                .or_insert_with(|| (s.started_at.clone(), s.restarts));
            if s.restarts > first.1 || (s.started_at != first.0 && !first.0.is_empty()) {
                return Verdict::Fail(format!("{} restarted", s.name));
            }
            if s.health.as_deref() == Some("unhealthy") {
                return Verdict::Fail(format!("{} is unhealthy", s.name));
            }
            if s.status != "running" || !matches!(s.health.as_deref(), None | Some("healthy")) {
                ready = false;
            }
        }
        if ready {
            let since = *self.stable_since.get_or_insert(at);
            if at.saturating_sub(since) >= self.need {
                return Verdict::Ok;
            }
        } else {
            self.stable_since = None;
        }
        if at >= self.need + START_GRACE {
            return Verdict::Fail(format!(
                "not running and healthy for {} s within {} s",
                self.need.as_secs(),
                (self.need + START_GRACE).as_secs()
            ));
        }
        Verdict::Wait
    }
}

// ---------------------------------------------------------------------------
// Recreating a container
// ---------------------------------------------------------------------------

/// The create body for a container like `inspect`, running `image`. Values
/// that were only the old image's defaults are left out so the new image's
/// defaults apply (spike Q2, what Watchtower does).
pub fn create_body(inspect: &Value, old_image_config: &Value, image: &str) -> Value {
    let mut config = inspect
        .get("Config")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let img = old_image_config.as_object().cloned().unwrap_or_default();
    for key in [
        "Cmd",
        "Entrypoint",
        "WorkingDir",
        "User",
        "Healthcheck",
        "StopSignal",
    ] {
        if config.get(key).is_some() && config.get(key) == img.get(key) {
            config.remove(key);
        }
    }
    if let (Some(Value::Array(env)), Some(Value::Array(defaults))) =
        (config.get("Env").cloned(), img.get("Env"))
    {
        let kept: Vec<Value> = env.into_iter().filter(|e| !defaults.contains(e)).collect();
        config.insert("Env".into(), Value::Array(kept));
    }
    if let (Some(Value::Object(labels)), Some(Value::Object(defaults))) =
        (config.get("Labels").cloned(), img.get("Labels"))
    {
        let kept: Map<String, Value> = labels
            .into_iter()
            .filter(|(k, v)| defaults.get(k) != Some(v))
            .collect();
        config.insert("Labels".into(), Value::Object(kept));
    }
    // A generated hostname is the old container's id.
    let id = inspect.get("Id").and_then(Value::as_str).unwrap_or("");
    if let Some(h) = config.get("Hostname").and_then(Value::as_str)
        && !h.is_empty()
        && id.starts_with(h)
    {
        config.remove("Hostname");
    }
    config.insert("Image".into(), Value::String(image.into()));
    let host = inspect.get("HostConfig").cloned().unwrap_or(json!({}));
    let mode = host
        .get("NetworkMode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut body = Value::Object(config);
    body["HostConfig"] = host;
    if !matches!(mode.as_str(), "host" | "none") && !mode.starts_with("container:") {
        let mut endpoints = Map::new();
        if let Some(nets) = inspect
            .pointer("/NetworkSettings/Networks")
            .and_then(Value::as_object)
        {
            let short: String = id.chars().take(12).collect();
            for (net, ep) in nets {
                let mut e = Map::new();
                for key in ["IPAMConfig", "Links", "DriverOpts"] {
                    if let Some(v) = ep.get(key).filter(|v| !v.is_null()) {
                        e.insert(key.into(), v.clone());
                    }
                }
                if let Some(Value::Array(aliases)) = ep.get("Aliases") {
                    let kept: Vec<Value> = aliases
                        .iter()
                        .filter(|a| a.as_str() != Some(short.as_str()))
                        .cloned()
                        .collect();
                    e.insert("Aliases".into(), Value::Array(kept));
                }
                endpoints.insert(net.clone(), Value::Object(e));
            }
        }
        body["NetworkingConfig"] = json!({ "EndpointsConfig": endpoints });
    }
    body
}

/// The image tag that keeps a container's old image for a rollback:
/// `wren-rollback/<target>:<run>-<container>`.
pub fn rollback_tag(target: &str, run: i64, container: &str) -> String {
    let repo: String = target
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let tag: String = format!("{run}-{container}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect();
    format!(
        "wren-rollback/{}:{tag}",
        if repo.is_empty() { "x" } else { &repo }
    )
}

/// Sets Unraid's update status for `image` the way `setUpdateStatus` does
/// (spike Q2), when the file exists here.
pub fn write_unraid_status(
    file: &Path,
    image: &str,
    local: &str,
    remote: &str,
) -> Result<bool, String> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Ok(false);
    };
    let mut all: Map<String, Value> = serde_json::from_str(&text).unwrap_or_default();
    let key = match repo_of(image) {
        Some(repo) if repo == image => format!("{image}:latest"),
        _ => image.to_string(),
    };
    all.insert(
        key,
        json!({ "local": local, "remote": remote, "status": if local == remote { "true" } else { "false" } }),
    );
    let tmp = file.with_extension("json.wren");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&Value::Object(all)).unwrap_or_default(),
    )
    .map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, file).map_err(|e| format!("can't replace {}: {e}", file.display()))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// On this server
// ---------------------------------------------------------------------------

/// Every container with its full inspect and its image's `RepoDigests`.
pub async fn read_all(socket: &str) -> Result<Vec<Ctr>, String> {
    let list = get_json(socket, "/containers/json?all=1").await?;
    let mut digests: HashMap<String, Vec<String>> = HashMap::new();
    let mut out = Vec::new();
    for c in list.as_array().into_iter().flatten() {
        let id = c.get("Id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(inspect) = get_json(socket, &format!("/containers/{id}/json")).await else {
            continue;
        };
        let image_id = inspect
            .get("Image")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !digests.contains_key(&image_id) {
            let d = if image_id.starts_with("sha256:") {
                get_json(socket, &format!("/images/{image_id}/json"))
                    .await
                    .ok()
                    .and_then(|i| i.get("RepoDigests").cloned())
                    .and_then(|d| serde_json::from_value(d).ok())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            digests.insert(image_id.clone(), d);
        }
        out.push(Ctr::from_inspect(inspect, digests[&image_id].clone()));
    }
    Ok(out)
}

/// Runs a script from `agent.toml` (`sh -c`) in its folder with its
/// environment; the end of what it printed, or why it failed.
pub async fn run_script(
    command: &str,
    dir: &Path,
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
) -> StepResult {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, child)
        .await
        .map_err(|_| format!("the script took longer than {} s", timeout.as_secs()))?
        .map_err(|e| format!("can't start sh: {e}"))?;
    let text = tail(
        &format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        LOG_CHARS - 200,
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("the script failed ({}):\n{text}", out.status))
    }
}

/// The steps against this server's Docker.
pub struct DockerHost<'a> {
    pub config: &'a Config,
}

impl DockerHost<'_> {
    fn socket(&self) -> &str {
        &self.config.docker_socket
    }

    async fn docker(&self, args: &[&str], limit: Duration) -> StepResult {
        cli(self.socket(), args, limit).await
    }

    /// `docker compose` for the plan's project, in its folder.
    async fn compose(&self, p: &Plan, args: &[&str]) -> StepResult {
        let project = match &p.stack {
            Some(s) => s.compose_project.clone(),
            None => p.job().target.name.clone(),
        };
        let dir = p.working_dir.to_string_lossy().to_string();
        let files: Vec<String> = p
            .files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect();
        let mut all = vec![
            "compose",
            "-p",
            project.as_str(),
            "--project-directory",
            dir.as_str(),
        ];
        for f in &files {
            all.push("-f");
            all.push(f);
        }
        all.extend_from_slice(args);
        self.docker(&all, UPDATE_LIMIT).await
    }

    async fn inspect(&self, name: &str) -> Option<Value> {
        get_json(self.socket(), &format!("/containers/{name}/json"))
            .await
            .ok()
    }

    /// Containers of the target that ran before the update.
    fn watched(p: &Plan) -> Vec<String> {
        p.ctrs
            .iter()
            .filter(|c| c.running)
            .map(|c| c.name.clone())
            .collect()
    }

    async fn image_id(&self, reference: &str) -> StepResult {
        self.docker(
            &["image", "inspect", "--format", "{{.Id}}", reference],
            Duration::from_secs(30),
        )
        .await
    }

    /// Replaces the container `c` with one of the same configuration running
    /// `image` (the old one is kept, renamed, for the rollback).
    async fn recreate(&self, p: &Plan, st: &mut State, c: &Ctr, image: &str) -> StepResult {
        let run = p.job().run_id;
        let old_image = get_json(self.socket(), &format!("/images/{}/json", c.image_id))
            .await
            .ok()
            .and_then(|i| i.get("Config").cloned())
            .unwrap_or(Value::Null);
        let body = create_body(&c.inspect, &old_image, image);
        let temp = format!("{}-wren-old-{run}", c.name);
        self.docker(&["rename", &c.name, &temp], Duration::from_secs(30))
            .await?;
        st.renamed = Some(temp);
        post_json(
            self.socket(),
            &format!("/containers/create?name={}", c.name),
            &body,
        )
        .await?;
        if c.running {
            self.docker(&["start", &c.name], Duration::from_secs(120))
                .await?;
        }
        Ok(format!("recreated {} with {image}", c.name))
    }

    /// The container now runs the pulled image.
    async fn verify(&self, st: &State, name: &str) -> StepResult {
        let Some(want) = st.new_ids.get(name) else {
            return Ok(String::new());
        };
        let got = self
            .inspect(name)
            .await
            .and_then(|v| v.get("Image").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        if &got == want {
            Ok(format!("{name} runs {}", &want[..want.len().min(19)]))
        } else {
            Err(format!(
                "{name} runs {got}, not the approved image {want} (the registry moved on?)"
            ))
        }
    }

    async fn restore_data(
        &self,
        p: &Plan,
        st: &State,
        log: &mut Vec<String>,
    ) -> Result<(), String> {
        for (dp, snap) in &st.snapshots {
            log.push(snapshot::restore(dp, snap, p.job().run_id).await?);
        }
        Ok(())
    }

    async fn start_stopped(&self, st: &State, log: &mut Vec<String>) -> Result<(), String> {
        for name in &st.stopped {
            let running = self
                .inspect(name)
                .await
                .and_then(|v| v.pointer("/State/Running").and_then(Value::as_bool))
                .unwrap_or(false);
            if !running {
                self.docker(&["start", name], Duration::from_secs(120))
                    .await?;
                log.push(format!("started {name}"));
            }
        }
        Ok(())
    }

    /// Puts the old images back under their references and recreates the
    /// target with them (manual rollback; Compose and scripts also after a
    /// failed update).
    async fn back_to_old_images(
        &self,
        p: &Plan,
        st: &mut State,
        log: &mut Vec<String>,
    ) -> Result<(), String> {
        for (_, reference, id) in &st.old {
            self.docker(&["tag", id, reference], Duration::from_secs(30))
                .await?;
        }
        match p.kind() {
            Kind::Container | Kind::Unraid => {
                let c = p.ctrs[0].clone();
                // The container now runs the newer image; recreate it on the old one.
                let current = read_all(self.socket())
                    .await?
                    .into_iter()
                    .find(|x| x.name == c.name)
                    .unwrap_or(c);
                log.push(
                    self.recreate(p, st, &current, &current.image.clone())
                        .await?,
                );
                if p.kind() == Kind::Unraid {
                    for i in &p.job().images {
                        write_unraid_status(Path::new(UNRAID_STATUS), &i.image, &i.from, &i.to)?;
                    }
                }
            }
            Kind::Compose | Kind::Script => {
                log.push(
                    self.compose(p, &["up", "-d", "--pull", "never", "--no-build"])
                        .await?,
                );
            }
        }
        Ok(())
    }
}

impl Host for DockerHost<'_> {
    async fn preflight(&self, p: &Plan, st: &mut State) -> StepResult {
        let job = p.job();
        let target = job.target.name.clone();
        let mut log: Vec<String> = p.notes.clone();
        if job.kind == JobKind::Rollback {
            let of = job.rollback_of.unwrap_or_default();
            for c in &p.ctrs {
                let tag = rollback_tag(&target, of, &c.name);
                if let Ok(id) = self.image_id(&tag).await {
                    st.old.push((c.name.clone(), c.image.clone(), id));
                }
            }
            if st.old.is_empty() && p.kind() != Kind::Script {
                return Err(format!(
                    "nothing to roll back to: the images kept for run {of} are gone"
                ));
            }
            for (path, root) in &p.data {
                let dp = snapshot::resolve(path, root).await;
                let snap = dp.snapshot_at(&target, of);
                if snap.exists() {
                    log.push(format!("will restore {} from run {of}", path.display()));
                    st.snapshots.push((dp, snap));
                } else {
                    log.push(format!("no snapshot of {} from run {of}", path.display()));
                }
            }
            return Ok(log.join("\n"));
        }
        // Room for the snapshot, before anything happens.
        if !p.own_backup() {
            for (path, root) in &p.data {
                let dp = snapshot::resolve(path, root).await;
                let dest = dp.snapshot_at(&target, job.run_id);
                let method = snapshot::method(&dp, dest.parent().unwrap_or(&dp.root)).await;
                let size = snapshot::size(&dp.real).await?;
                let (avail, total) = snapshot::space(&dp.root).await?;
                snapshot::fits(snapshot::need(method, size), avail, total)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                log.push(format!(
                    "{}: {} MB, {} ({} MB free)",
                    dp.real.display(),
                    size >> 20,
                    method.as_str(),
                    avail >> 20
                ));
                st.paths.push((dp, method));
            }
        }
        if p.kind() == Kind::Unraid {
            st.unraid_script = Path::new(UPDATE_CONTAINER).exists()
                && exec("php", &["-r", "echo 1;"], Duration::from_secs(30))
                    .await
                    .is_ok();
            log.push(if st.unraid_script {
                "Unraid's update_container will recreate it".into()
            } else {
                "Unraid's update_container can't run here: recreating through the Docker API".into()
            });
        }
        // Keep every old image for the rollback.
        for c in &p.ctrs {
            let tag = rollback_tag(&target, job.run_id, &c.name);
            self.docker(&["tag", &c.image_id, &tag], Duration::from_secs(30))
                .await?;
            st.old
                .push((c.name.clone(), c.image.clone(), c.image_id.clone()));
        }
        // The approved version exactly, pulled before anything stops.
        for i in &job.images {
            let repo = repo_of(&i.image).ok_or("not an image reference")?;
            let pinned = format!("{repo}@{}", i.to);
            self.docker(&["pull", "--quiet", &pinned], PREFLIGHT_LIMIT)
                .await?;
            self.docker(&["tag", &pinned, &i.image], Duration::from_secs(30))
                .await?;
            let id = self.image_id(&pinned).await?;
            log.push(format!("pulled {pinned}"));
            st.new_ids.insert(i.container.clone(), id);
        }
        if let Some(stack) = &p.stack
            && stack.rollback.is_none()
        {
            let mut files: Vec<PathBuf> = p
                .ctrs
                .iter()
                .flat_map(|c| {
                    c.label(jobs::COMPOSE_FILES)
                        .split(',')
                        .map(|f| PathBuf::from(f.trim()))
                        .collect::<Vec<_>>()
                })
                .filter(|f| f.starts_with(&stack.working_dir))
                .collect();
            files.push(stack.working_dir.join(".env"));
            files.sort();
            files.dedup();
            for f in files {
                if let Ok(bytes) = std::fs::read(&f) {
                    st.saved_files.push((f, bytes));
                }
            }
        }
        Ok(log.join("\n"))
    }

    async fn stop(&self, p: &Plan, st: &mut State) -> StepResult {
        let mut log = Vec::new();
        for name in Self::watched(p) {
            self.docker(&["stop", "-t", "60", &name], STOP_LIMIT)
                .await?;
            log.push(format!("stopped {name}"));
            st.stopped.push(name);
        }
        if log.is_empty() {
            log.push("nothing was running".into());
        }
        Ok(log.join("\n"))
    }

    async fn snapshot(&self, p: &Plan, st: &mut State) -> StepResult {
        let job = p.job();
        let mut log = Vec::new();
        for (dp, method) in st.paths.clone() {
            let dest = dp.snapshot_at(&job.target.name, job.run_id);
            let t = Instant::now();
            let used = snapshot::take(&dp, &dest, method).await?;
            log.push(format!(
                "{} → {} ({}, {:.1} s)",
                dp.real.display(),
                dest.display(),
                used.as_str(),
                t.elapsed().as_secs_f32()
            ));
            st.snapshots.push((dp, dest));
        }
        if log.is_empty() {
            log.push("no data paths of its own under the agent's roots".into());
        }
        Ok(log.join("\n"))
    }

    async fn update(&self, p: &Plan, st: &mut State) -> StepResult {
        st.update_began = true;
        let job = p.job();
        let mut log = Vec::new();
        if job.kind == JobKind::Rollback {
            self.restore_data(p, st, &mut log).await?;
            if let (Some(stack), Some(script)) =
                (&p.stack, p.stack.as_ref().and_then(|s| s.rollback.clone()))
            {
                log.push(run_script(&script, &stack.working_dir, &stack.env, stack.timeout).await?);
            } else {
                self.back_to_old_images(p, st, &mut log).await?;
            }
            return Ok(log.join("\n"));
        }
        match p.kind() {
            Kind::Container => {
                let c = p.ctrs[0].clone();
                log.push(self.recreate(p, st, &c, &c.image).await?);
                log.push(self.verify(st, &c.name).await?);
            }
            Kind::Unraid => {
                let c = p.ctrs[0].clone();
                if st.unraid_script {
                    let temp = format!("{}-wren-old-{}", c.name, job.run_id);
                    self.docker(&["rename", &c.name, &temp], Duration::from_secs(30))
                        .await?;
                    st.renamed = Some(temp);
                    // Exit 0 even on errors (spike Q2): verified below.
                    let out = exec(UPDATE_CONTAINER, &[&c.name], UPDATE_LIMIT)
                        .await
                        .unwrap_or_else(|e| e);
                    log.push(tail(&out, 600));
                    let created = self.inspect(&c.name).await;
                    if created.is_none() {
                        return Err(format!("update_container didn't create {}", c.name));
                    }
                    let running = created
                        .and_then(|v| v.pointer("/State/Running").and_then(Value::as_bool))
                        .unwrap_or(false);
                    if c.running && !running {
                        self.docker(&["start", &c.name], Duration::from_secs(120))
                            .await?;
                    }
                } else {
                    log.push(self.recreate(p, st, &c, &c.image).await?);
                    for i in &job.images {
                        write_unraid_status(Path::new(UNRAID_STATUS), &i.image, &i.to, &i.to)?;
                    }
                }
                log.push(self.verify(st, &c.name).await?);
            }
            Kind::Compose => {
                log.push(
                    self.compose(p, &["up", "-d", "--pull", "never", "--no-build"])
                        .await?,
                );
                for i in &job.images {
                    log.push(self.verify(st, &i.container).await?);
                }
            }
            Kind::Script => {
                let stack = p.stack.as_ref().ok_or("no script stack")?;
                log.push(
                    run_script(&stack.update, &stack.working_dir, &stack.env, stack.timeout)
                        .await?,
                );
            }
        }
        Ok(log.join("\n"))
    }

    async fn health(&self, p: &Plan, _st: &mut State) -> StepResult {
        let names = Self::watched(p);
        if names.is_empty() {
            return Ok("nothing ran before the update, so nothing was started".into());
        }
        let need = Duration::from_secs(p.job().health_seconds);
        let mut watch = Watch::new(need);
        let start = Instant::now();
        loop {
            let mut samples = Vec::new();
            for n in &names {
                samples.push(Sample::from_inspect(n, self.inspect(n).await.as_ref()));
            }
            match watch.observe(&samples, start.elapsed()) {
                Verdict::Ok => {
                    return Ok(format!(
                        "{} running{} for {} s",
                        names.join(", "),
                        if samples.iter().any(|s| s.health.is_some()) {
                            " and healthy"
                        } else {
                            ""
                        },
                        need.as_secs()
                    ));
                }
                Verdict::Fail(why) => return Err(why),
                Verdict::Wait => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }

    async fn rollback(&self, p: &Plan, st: &mut State) -> StepResult {
        let mut log = Vec::new();
        // Stop what runs of the target now.
        for c in &p.ctrs {
            let running = self
                .inspect(&c.name)
                .await
                .and_then(|v| v.pointer("/State/Running").and_then(Value::as_bool))
                .unwrap_or(false);
            if running && st.update_began {
                self.docker(&["stop", "-t", "60", &c.name], STOP_LIMIT)
                    .await?;
                log.push(format!("stopped {}", c.name));
            }
        }
        if st.update_began {
            self.restore_data(p, st, &mut log).await?;
            match p.kind() {
                Kind::Container | Kind::Unraid => {
                    let c = &p.ctrs[0];
                    for (_, reference, id) in &st.old {
                        self.docker(&["tag", id, reference], Duration::from_secs(30))
                            .await?;
                    }
                    if let Some(temp) = st.renamed.take() {
                        if self.inspect(&c.name).await.is_some() {
                            self.docker(&["rm", "-f", &c.name], Duration::from_secs(60))
                                .await?;
                        }
                        self.docker(&["rename", &temp, &c.name], Duration::from_secs(30))
                            .await?;
                        log.push(format!("put the old {} back", c.name));
                    }
                    if p.kind() == Kind::Unraid {
                        for i in &p.job().images {
                            write_unraid_status(
                                Path::new(UNRAID_STATUS),
                                &i.image,
                                &i.from,
                                &i.to,
                            )?;
                        }
                    }
                    if c.running {
                        self.docker(&["start", &c.name], Duration::from_secs(120))
                            .await?;
                        log.push(format!("started {}", c.name));
                    }
                }
                Kind::Compose => self.back_to_old_images(p, st, &mut log).await?,
                Kind::Script => {
                    let stack = p.stack.as_ref().ok_or("no script stack")?;
                    match &stack.rollback {
                        Some(script) => log.push(
                            run_script(script, &stack.working_dir, &stack.env, stack.timeout)
                                .await?,
                        ),
                        None => {
                            for (f, bytes) in &st.saved_files {
                                std::fs::write(f, bytes)
                                    .map_err(|e| format!("can't restore {}: {e}", f.display()))?;
                                log.push(format!("restored {}", f.display()));
                            }
                            self.back_to_old_images(p, st, &mut log).await?;
                        }
                    }
                }
            }
        } else {
            self.start_stopped(st, &mut log).await?;
        }
        log.push(self.health(p, st).await?);
        Ok(log.join("\n"))
    }

    async fn close(&self, p: &Plan, st: &mut State, how: Close) {
        let job = p.job();
        let target = &job.target.name;
        if how == Close::Succeeded
            && let Some(temp) = st.renamed.take()
        {
            let _ = self.docker(&["rm", &temp], Duration::from_secs(60)).await;
        }
        if how == Close::Abandoned && job.kind == JobKind::Update {
            // The references point at the old images again, as before.
            for (_, reference, id) in &st.old {
                let _ = self
                    .docker(&["tag", id, reference], Duration::from_secs(30))
                    .await;
            }
            for c in &p.ctrs {
                let tag = rollback_tag(target, job.run_id, &c.name);
                let _ = self.docker(&["rmi", &tag], Duration::from_secs(60)).await;
            }
            return;
        }
        // Retention: the newest snapshots and kept images of this target only.
        let keep = job.keep_snapshots.clamp(1, jobs::MAX_KEEP);
        let mut roots: Vec<PathBuf> = st.paths.iter().map(|(d, _)| d.root.clone()).collect();
        roots.extend(st.snapshots.iter().map(|(d, _)| d.root.clone()));
        roots.sort();
        roots.dedup();
        for root in roots {
            snapshot::prune(&root, target, keep).await;
        }
        let repo = rollback_tag(target, 0, "x");
        let repo = repo.split(':').next().unwrap_or_default();
        if let Ok(tags) = self
            .docker(
                &["image", "ls", "--format", "{{.Tag}}", repo],
                Duration::from_secs(30),
            )
            .await
        {
            let mut runs: Vec<(i64, String)> = tags
                .lines()
                .filter_map(|t| Some((t.split('-').next()?.parse().ok()?, t.to_string())))
                .collect();
            let mut ids: Vec<i64> = runs.iter().map(|(r, _)| *r).collect();
            ids.sort_unstable_by(|a, b| b.cmp(a));
            ids.dedup();
            let kept: Vec<i64> = ids.into_iter().take(keep).collect();
            runs.retain(|(r, _)| !kept.contains(r));
            for (_, tag) in runs {
                let _ = self
                    .docker(&["rmi", &format!("{repo}:{tag}")], Duration::from_secs(60))
                    .await;
            }
        }
    }
}

/// Posts results through the job client.
pub struct ClientReport<'a> {
    pub client: &'a JobClient,
    pub run: i64,
}

impl Report for ClientReport<'_> {
    async fn step(&self, step: &str, status: &str, log: &str, ms: u64) -> Result<bool, String> {
        self.client
            .step(
                self.run,
                &StepReport {
                    step,
                    status,
                    log,
                    duration_ms: ms,
                },
            )
            .await
    }

    async fn finish(&self, outcome: &str, detail: &str) {
        if let Err(err) = self.client.finish(self.run, outcome, detail).await {
            tracing::warn!(run = self.run, "result not sent: {err}");
        }
    }

    async fn monitors(&self) -> Result<(String, String), String> {
        self.client.monitors(self.run).await
    }
}

/// This container's id, as Docker sets the hostname (empty outside one).
fn own_id() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .ok()
        .filter(|h| h.len() >= 12 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_default()
}

/// Checks and runs one job from Wren.
pub async fn handle(config: &Config, client: &JobClient, job: Job) {
    let run = job.run_id;
    let report = ClientReport { client, run };
    tracing::info!(run, target = %job.target.name, kind = ?job.kind, "job received");
    // The allowlist first: a target it leaves out isn't even looked at.
    if !config.updates.allows(&job.target.name) {
        let why = format!(
            "refused by agent config: {} is not allowed by [updates] in agent.toml",
            job.target.name
        );
        tracing::warn!(run, "{why}");
        report.finish("refused", &why).await;
        return;
    }
    let all = match read_all(&config.docker_socket).await {
        Ok(all) => all,
        Err(err) => {
            report
                .finish("failed", &format!("can't read Docker: {err}"))
                .await;
            return;
        }
    };
    let templates = crate::discover::unraid_templates(Path::new("/"));
    let plan = match jobs::check(&job, config, &all, &templates, &own_id()) {
        Ok(plan) => plan,
        Err(why) => {
            tracing::warn!(run, "{why}");
            report.finish("refused", &why).await;
            return;
        }
    };
    // The approved digest already runs (pulled by another digest of the
    // same image): nothing is stopped or recreated. Not for script stacks,
    // whose script may do more than the images show.
    if plan.kind() != Kind::Script && jobs::already_on(&job, &plan.ctrs) {
        let why = format!(
            "already up to date: {} already runs the approved version; nothing was stopped",
            job.target.name
        );
        tracing::info!(run, "{why}");
        report.finish("succeeded", &why).await;
        return;
    }
    let outcome = execute(&DockerHost { config }, &report, &plan).await;
    tracing::info!(run, outcome, "run finished");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what happened; fails the steps it is told to.
    #[derive(Default)]
    struct Fake {
        fail: Vec<&'static str>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn failing(fail: &[&'static str]) -> Fake {
            Fake {
                fail: fail.to_vec(),
                ..Fake::default()
            }
        }
        fn go(&self, name: &str) -> StepResult {
            self.calls.lock().unwrap().push(name.into());
            if self.fail.contains(&name) {
                Err(format!("{name} broke"))
            } else {
                Ok(format!("{name} ok"))
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Host for Fake {
        async fn preflight(&self, _: &Plan, _: &mut State) -> StepResult {
            self.go("preflight")
        }
        async fn stop(&self, _: &Plan, _: &mut State) -> StepResult {
            self.go("stop")
        }
        async fn snapshot(&self, _: &Plan, _: &mut State) -> StepResult {
            self.go("snapshot")
        }
        async fn update(&self, _: &Plan, s: &mut State) -> StepResult {
            s.update_began = true;
            self.go("update")
        }
        async fn health(&self, _: &Plan, _: &mut State) -> StepResult {
            self.go("health")
        }
        async fn rollback(&self, _: &Plan, _: &mut State) -> StepResult {
            self.go("rollback")
        }
        async fn close(&self, _: &Plan, _: &mut State, how: Close) {
            self.calls.lock().unwrap().push(format!("close:{how:?}"));
        }
    }

    #[derive(Default)]
    struct Log {
        cancel: bool,
        /// What Wren says about the monitors, in turn ('' = skipped).
        monitors: Mutex<Vec<&'static str>>,
        steps: Mutex<Vec<String>>,
        finished: Mutex<Option<(String, String)>>,
    }

    impl Report for Log {
        async fn step(&self, step: &str, status: &str, _: &str, _: u64) -> Result<bool, String> {
            self.steps.lock().unwrap().push(format!("{step}:{status}"));
            Ok(self.cancel && step == "preflight" && status != "running")
        }
        async fn finish(&self, outcome: &str, detail: &str) {
            *self.finished.lock().unwrap() = Some((outcome.into(), detail.into()));
        }
        async fn monitors(&self) -> Result<(String, String), String> {
            let mut m = self.monitors.lock().unwrap();
            match m.first().copied() {
                None => Ok(("skipped".into(), String::new())),
                Some("error") => {
                    m.remove(0);
                    Err("down".into())
                }
                Some(v) => {
                    m.remove(0);
                    Ok((v.into(), format!("monitors {v}")))
                }
            }
        }
        fn monitor_pause(&self) -> Duration {
            Duration::ZERO
        }
    }

    #[tokio::test]
    async fn monitor_failure_rolls_back() {
        let fake = Fake::default();
        let log = Log {
            monitors: Mutex::new(vec!["wait", "wait", "failed"]),
            ..Log::default()
        };
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Update, false)).await,
            "rolled_back"
        );
        assert!(fake.calls().contains(&"rollback".to_string()));
        assert_eq!(
            log.finished.lock().unwrap().as_ref().unwrap().1,
            "monitors failed"
        );
    }

    #[tokio::test]
    async fn monitors_ok_or_unreachable_keep_the_update() {
        for answers in [vec!["wait", "ok"], vec!["error", "error", "error"]] {
            let fake = Fake::default();
            let log = Log {
                monitors: Mutex::new(answers),
                ..Log::default()
            };
            assert_eq!(
                run(&fake, &log, &plan(JobKind::Update, false)).await,
                "succeeded"
            );
        }
    }

    fn plan(kind: JobKind, own_backup: bool) -> Plan {
        let job: Job = serde_json::from_value(json!({
            "run_id": 9, "kind": kind, "target": {"kind": "script", "name": "ww-test-stack"},
            "rollback_of": 3
        }))
        .unwrap();
        Plan {
            job: Some(job),
            kind: Some(Kind::Script),
            stack: Some(crate::config::ScriptStack {
                name: "ww-test-stack".into(),
                compose_project: "ww-test-stack".into(),
                working_dir: "/x".into(),
                handles_own_backup: own_backup,
                env: Default::default(),
                timeout: Duration::from_secs(60),
                update: "true".into(),
                rollback: None,
                version: None,
            }),
            ..Plan::default()
        }
    }

    async fn run(fake: &Fake, log: &Log, p: &Plan) -> String {
        execute(fake, log, p).await.to_string()
    }

    #[tokio::test]
    async fn preflight_failure_stops_nothing() {
        let (fake, log) = (Fake::failing(&["preflight"]), Log::default());
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Update, false)).await,
            "failed"
        );
        assert_eq!(fake.calls(), vec!["preflight", "close:Abandoned"]);
    }

    #[tokio::test]
    async fn cancel_after_preflight_stops_nothing() {
        let fake = Fake::default();
        let log = Log {
            cancel: true,
            ..Log::default()
        };
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Update, false)).await,
            "cancelled"
        );
        assert_eq!(fake.calls(), vec!["preflight", "close:Abandoned"]);
    }

    #[tokio::test]
    async fn a_good_run_goes_through_every_step() {
        let (fake, log) = (Fake::default(), Log::default());
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Update, false)).await,
            "succeeded"
        );
        assert_eq!(
            fake.calls(),
            vec![
                "preflight",
                "stop",
                "snapshot",
                "update",
                "health",
                "close:Succeeded"
            ]
        );
        assert!(log.steps.lock().unwrap().contains(&"health:ok".to_string()));
    }

    #[tokio::test]
    async fn health_failure_rolls_back() {
        let (fake, log) = (Fake::failing(&["health"]), Log::default());
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Update, false)).await,
            "rolled_back"
        );
        assert_eq!(
            fake.calls(),
            vec![
                "preflight",
                "stop",
                "snapshot",
                "update",
                "health",
                "rollback",
                "close:RolledBack"
            ]
        );
        let (outcome, detail) = log.finished.lock().unwrap().clone().unwrap();
        assert_eq!(
            (outcome.as_str(), detail.as_str()),
            ("rolled_back", "health broke")
        );
    }

    #[tokio::test]
    async fn snapshot_failure_restarts_without_update() {
        let (fake, log) = (Fake::failing(&["snapshot"]), Log::default());
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Update, false)).await,
            "failed"
        );
        assert_eq!(
            fake.calls(),
            vec![
                "preflight",
                "stop",
                "snapshot",
                "rollback",
                "close:RolledBack"
            ]
        );
    }

    #[tokio::test]
    async fn rollback_failure_needs_you() {
        let (fake, log) = (Fake::failing(&["update", "rollback"]), Log::default());
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Update, false)).await,
            "rollback_failed"
        );
        // Nothing is cleaned up: everything stays for a person.
        assert_eq!(
            fake.calls(),
            vec!["preflight", "stop", "snapshot", "update", "rollback"]
        );
        let detail = log.finished.lock().unwrap().clone().unwrap().1;
        assert!(
            detail.contains("update broke") && detail.contains("rollback broke"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn own_backup_skips_stop_and_snapshot() {
        let (fake, log) = (Fake::default(), Log::default());
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Update, true)).await,
            "succeeded"
        );
        assert_eq!(
            fake.calls(),
            vec!["preflight", "update", "health", "close:Succeeded"]
        );
        let steps = log.steps.lock().unwrap().clone();
        assert!(
            steps.contains(&"stop:skipped".to_string())
                && steps.contains(&"snapshot:skipped".to_string())
        );
    }

    #[tokio::test]
    async fn a_failed_manual_rollback_is_not_undone() {
        let (fake, log) = (Fake::failing(&["health"]), Log::default());
        assert_eq!(
            run(&fake, &log, &plan(JobKind::Rollback, false)).await,
            "rollback_failed"
        );
        assert_eq!(fake.calls(), vec!["preflight", "stop", "update", "health"]);
    }

    fn s(name: &str, status: &str, health: Option<&str>, started: &str, restarts: i64) -> Sample {
        Sample {
            name: name.into(),
            exists: true,
            status: status.into(),
            health: health.map(str::to_string),
            started_at: started.into(),
            restarts,
        }
    }

    #[test]
    fn health_rules() {
        let sec = Duration::from_secs;
        let mut w = Watch::new(sec(10));
        assert_eq!(
            w.observe(&[s("a", "running", None, "t1", 0)], sec(0)),
            Verdict::Wait
        );
        assert_eq!(
            w.observe(&[s("a", "running", None, "t1", 0)], sec(9)),
            Verdict::Wait
        );
        assert_eq!(
            w.observe(&[s("a", "running", None, "t1", 0)], sec(10)),
            Verdict::Ok
        );
        // A healthcheck must say healthy; starting doesn't count as stable.
        let mut w = Watch::new(sec(10));
        assert_eq!(
            w.observe(&[s("a", "running", Some("starting"), "t1", 0)], sec(0)),
            Verdict::Wait
        );
        assert_eq!(
            w.observe(&[s("a", "running", Some("healthy"), "t1", 0)], sec(5)),
            Verdict::Wait
        );
        assert_eq!(
            w.observe(&[s("a", "running", Some("healthy"), "t1", 0)], sec(15)),
            Verdict::Ok
        );
        let mut w = Watch::new(sec(10));
        assert!(matches!(
            w.observe(&[s("a", "running", Some("unhealthy"), "t1", 0)], sec(1)),
            Verdict::Fail(_)
        ));
        // Exits, restarts and vanishing fail at once.
        let mut w = Watch::new(sec(10));
        w.observe(&[s("a", "running", None, "t1", 0)], sec(0));
        assert!(
            matches!(w.observe(&[s("a", "running", None, "t2", 1)], sec(3)), Verdict::Fail(m) if m.contains("restarted"))
        );
        let mut w = Watch::new(sec(10));
        assert!(matches!(
            w.observe(&[s("a", "exited", None, "t1", 0)], sec(0)),
            Verdict::Fail(_)
        ));
        assert!(
            matches!(w.observe(&[Sample::from_inspect("b", None)], sec(0)), Verdict::Fail(m) if m.contains("gone"))
        );
        // Never healthy: fails after the grace.
        let mut w = Watch::new(sec(10));
        assert!(matches!(
            w.observe(
                &[s("a", "running", Some("starting"), "t1", 0)],
                sec(10) + START_GRACE
            ),
            Verdict::Fail(_)
        ));
    }

    #[test]
    fn recreate_body_drops_old_image_defaults() {
        let inspect = json!({
            "Id": "0123456789abcdef",
            "Config": {
                "Hostname": "0123456789ab", "Image": "a/b:1", "Cmd": ["/whoami"],
                "Env": ["PATH=/usr/bin", "NOTE=two words"], "Labels": {"org.opencontainers.image.version": "1.10", "wren.spike": "1"},
                "WorkingDir": "/", "User": "app"
            },
            "HostConfig": {"NetworkMode": "ww-net", "Binds": ["/mnt/x:/data"], "RestartPolicy": {"Name": "unless-stopped"}},
            "NetworkSettings": {"Networks": {"ww-net": {"IPAMConfig": {"IPv4Address": "172.31.250.10"}, "Aliases": ["0123456789ab", "web"], "MacAddress": "02:42", "IPAddress": "172.31.250.10"}}}
        });
        let image = json!({"Cmd": ["/whoami"], "Env": ["PATH=/usr/bin"], "Labels": {"org.opencontainers.image.version": "1.10"}, "WorkingDir": "/"});
        let body = create_body(&inspect, &image, "a/b:1");
        assert_eq!(body["Image"], "a/b:1");
        assert!(
            body.get("Cmd").is_none()
                && body.get("WorkingDir").is_none()
                && body.get("Hostname").is_none()
        );
        assert_eq!(body["User"], "app");
        assert_eq!(body["Env"], json!(["NOTE=two words"]));
        assert_eq!(body["Labels"], json!({"wren.spike": "1"}));
        assert_eq!(body["HostConfig"]["Binds"], json!(["/mnt/x:/data"]));
        let ep = &body["NetworkingConfig"]["EndpointsConfig"]["ww-net"];
        assert_eq!(ep["IPAMConfig"]["IPv4Address"], "172.31.250.10");
        assert_eq!(ep["Aliases"], json!(["web"]));
        assert!(ep.get("MacAddress").is_none() && ep.get("IPAddress").is_none());
        let host =
            json!({"Id": "x", "Config": {"Image": "a"}, "HostConfig": {"NetworkMode": "host"}});
        assert!(
            create_body(&host, &Value::Null, "a")
                .get("NetworkingConfig")
                .is_none()
        );
    }

    #[test]
    fn rollback_tags_and_unraid_status() {
        assert_eq!(
            rollback_tag("Newt Box", 12, "Newt"),
            "wren-rollback/newt-box:12-Newt"
        );
        let dir = std::env::temp_dir().join(format!("wren-status-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("unraid-update-status.json");
        assert!(!write_unraid_status(&file, "a/b:1", "sha256:1", "sha256:1").unwrap());
        std::fs::write(
            &file,
            r#"{"other:1": {"local": "x", "remote": "x", "status": "true"}}"#,
        )
        .unwrap();
        assert!(
            write_unraid_status(&file, "localhost:5056/a", "sha256:old", "sha256:new").unwrap()
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["localhost:5056/a:latest"]["status"], "false");
        assert_eq!(v["other:1"]["local"], "x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
