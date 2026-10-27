//! Jobs from Wren (design §6, §8): fetched with a long-poll
//! `GET /api/agent/v1/jobs`, results posted back step by step.
//!
//! A job is data, never a command: its kind (`update` or `rollback`), the
//! target's kind and name, version keys and, per image, the container, the
//! reference it runs and the two digests. Before anything happens the agent
//! re-reads Docker and [`check`]s the job against what runs here and its own
//! `agent.toml`; what they don't allow is refused and nothing changes.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, ScriptStack};
use crate::heartbeat::{MAX_JOB_BYTES, MAX_VERDICT_BYTES, read_json};

pub const JOBS_PATH: &str = "/api/agent/v1/jobs";
/// How long Wren holds a job request open.
pub const POLL_SECONDS: u64 = 25;
/// The most snapshots kept per target, whatever Wren asks.
pub const MAX_KEEP: usize = 10;
/// The longest health period Wren may ask for.
pub const MAX_HEALTH_SECONDS: u64 = 3600;

pub const COMPOSE_PROJECT: &str = "com.docker.compose.project";
pub const COMPOSE_DIR: &str = "com.docker.compose.project.working_dir";
pub const COMPOSE_FILES: &str = "com.docker.compose.project.config_files";
pub const COMPOSE_SERVICE: &str = "com.docker.compose.service";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    /// Update the target from one version key to another.
    Update,
    /// Roll back an earlier, successful run of the target (`rollback_of`).
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TargetRef {
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JobImage {
    pub container: String,
    pub image: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Job {
    pub run_id: i64,
    pub kind: JobKind,
    pub target: TargetRef,
    #[serde(default)]
    pub from_key: String,
    #[serde(default)]
    pub to_key: String,
    #[serde(default)]
    pub images: Vec<JobImage>,
    #[serde(default = "default_health")]
    pub health_seconds: u64,
    #[serde(default = "default_keep")]
    pub keep_snapshots: usize,
    #[serde(default)]
    pub rollback_of: Option<i64>,
    /// Stop before anything stops when no data path can be snapshotted
    /// (the target's "Require a snapshot").
    #[serde(default)]
    pub require_snapshot: bool,
}

/// A message Wren shows in the reader's language: a code and its values.
/// [`Note::text`] is the English, sent along for Wrens that don't know the
/// code. A note without a code is plain English only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Note {
    pub code: &'static str,
    pub params: BTreeMap<&'static str, String>,
    #[serde(skip)]
    plain: String,
}

impl Note {
    pub fn new(code: &'static str, params: &[(&'static str, &str)]) -> Note {
        Note {
            code,
            params: params.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
            plain: String::new(),
        }
    }

    /// English only, without a code.
    pub fn plain(text: impl Into<String>) -> Note {
        Note {
            plain: text.into(),
            ..Note::default()
        }
    }

    /// The English text.
    pub fn text(&self) -> String {
        let Some(mut out) = english(self.code).map(str::to_string) else {
            return self.plain.clone();
        };
        for (k, v) in &self.params {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

/// The English of every code the agent sends (Wren has the same codes in
/// every language).
fn english(code: &str) -> Option<&'static str> {
    Some(match code {
        "refused.not_allowed" => {
            "refused by agent config: {target} is not allowed by [updates] in agent.toml"
        }
        "refused.not_under_roots" => {
            "refused: the stack's folder {folder} is not under the agent's roots"
        }
        "refused.not_mounted" => "refused: {folder} is not mounted in the agent at the same path",
        "refused.file_outside_roots" => "refused: a Compose file is outside the agent's roots",
        "refused.no_script_stack" => "refused: agent.toml has no script stack named {target}",
        "refused.gone" => "refused: nothing of {target} runs here",
        "refused.changed" => {
            "refused: {container} changed since Wren's check (it no longer runs {digest})"
        }
        "done.already_on" => {
            "already up to date: {target} already runs the approved version; nothing was stopped"
        }
        "failed.no_snapshot" => {
            "stopped before anything was touched: no data could be snapshotted, and this target requires a snapshot"
        }
        "failed.unreachable" => "Wren couldn't be reached before anything stopped: {error}",
        "path.not_under_roots" => "{path} is not under the agent's roots",
        "path.root" => "{path} is a root itself",
        "path.shared" => "{path} is also used by {with}",
        "path.volume" => "volume {volume} of {container} is a named Docker volume",
        "snapshot.none" => {
            "nothing was snapshotted: this target has no data folder Wren can snapshot"
        }
        _ => return None,
    })
}

fn default_health() -> u64 {
    180
}

fn default_keep() -> usize {
    2
}

/// Target kinds, as Wren names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Container,
    Unraid,
    Compose,
    Script,
}

/// A container as the agent read it before acting.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ctr {
    pub name: String,
    pub id: String,
    /// `.Config.Image`.
    pub image: String,
    pub image_id: String,
    pub repo_digests: Vec<String>,
    pub running: bool,
    pub labels: std::collections::BTreeMap<String, String>,
    /// Bind mount sources.
    pub binds: Vec<PathBuf>,
    /// Named volumes (never snapshotted: outside the roots).
    pub volumes: Vec<String>,
    /// The whole `GET /containers/{id}/json`.
    pub inspect: Value,
}

impl Ctr {
    /// Built from a container inspect and its image's `RepoDigests`.
    pub fn from_inspect(inspect: Value, repo_digests: Vec<String>) -> Ctr {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let config = inspect.get("Config").cloned().unwrap_or(Value::Null);
        let labels = config
            .get("Labels")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let (mut binds, mut volumes) = (Vec::new(), Vec::new());
        for m in inspect
            .get("Mounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match s(m, "Type").as_str() {
                "bind" => binds.push(PathBuf::from(s(m, "Source"))),
                "volume" => volumes.push(s(m, "Name")),
                _ => {}
            }
        }
        Ctr {
            name: s(&inspect, "Name").trim_start_matches('/').to_string(),
            id: s(&inspect, "Id"),
            image: s(&config, "Image"),
            image_id: s(&inspect, "Image"),
            repo_digests,
            running: inspect
                .pointer("/State/Running")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            labels,
            binds,
            volumes,
            inspect,
        }
    }

    pub fn label(&self, key: &str) -> &str {
        self.labels.get(key).map_or("", String::as_str)
    }
}

/// The repository of an image reference without its tag (`None` when pinned
/// by digest or not a reference).
pub fn repo_of(image: &str) -> Option<String> {
    if image.is_empty() || image.contains('@') || image.starts_with("sha256:") {
        return None;
    }
    let slash = image.rfind('/').map_or(0, |i| i + 1);
    Some(match image[slash..].rfind(':') {
        Some(c) => image[..slash + c].to_string(),
        None => image.to_string(),
    })
}

/// A repository as Docker Hub short names compare (`docker.io/library/x` = `x`).
fn normal(repo: &str) -> &str {
    let r = repo
        .strip_prefix("docker.io/")
        .or_else(|| repo.strip_prefix("index.docker.io/"))
        .unwrap_or(repo);
    r.strip_prefix("library/").unwrap_or(r)
}

/// The container's repo digest for its own reference.
pub fn local_digest(image: &str, repo_digests: &[String]) -> Option<String> {
    local_digests(image, repo_digests).into_iter().next()
}

/// Every repo digest of the container's image for its own reference: an
/// image pulled again by another digest of the same content has several.
pub fn local_digests(image: &str, repo_digests: &[String]) -> Vec<String> {
    let Some(repo) = repo_of(image) else {
        return Vec::new();
    };
    repo_digests
        .iter()
        .filter_map(|d| {
            let (r, digest) = d.split_once('@')?;
            (normal(r) == normal(&repo)).then(|| digest.to_string())
        })
        .collect()
}

/// Whether every image of an update job already runs its `to` digest: the
/// approved version is there and there is nothing to do.
pub fn already_on(job: &Job, ctrs: &[Ctr]) -> bool {
    job.kind == JobKind::Update
        && !job.images.is_empty()
        && job.images.iter().all(|i| {
            ctrs.iter().any(|c| {
                c.name == i.container
                    && c.image == i.image
                    && local_digests(&c.image, &c.repo_digests).contains(&i.to)
            })
        })
}

pub fn valid_digest(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A Docker container or Compose project name.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 128
        && n.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// What the agent will act on, after [`check`].
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub job: Option<Job>,
    pub kind: Option<Kind>,
    /// The target's containers (Compose and script stacks: all of the project's).
    pub ctrs: Vec<Ctr>,
    pub stack: Option<ScriptStack>,
    /// A Compose project's working dir and files.
    pub working_dir: PathBuf,
    pub files: Vec<PathBuf>,
    /// The target's own, unshared bind paths under the roots, with the root.
    pub data: Vec<(PathBuf, PathBuf)>,
    /// What was left out of the snapshot, and why.
    pub notes: Vec<Note>,
}

impl Plan {
    pub fn job(&self) -> &Job {
        self.job.as_ref().expect("a checked plan has its job")
    }

    pub fn kind(&self) -> Kind {
        self.kind.unwrap_or(Kind::Container)
    }

    /// The script stack does its own stop, backup and verification.
    pub fn own_backup(&self) -> bool {
        self.stack.as_ref().is_some_and(|s| s.handles_own_backup)
    }
}

/// `path` lies under `root`, by whole components and without `..` (which
/// `Path::starts_with` would let climb back out).
pub fn under(path: &Path, root: &Path) -> bool {
    path.is_absolute()
        && path.starts_with(root)
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Host paths a container mounts that are never data: not worth a note.
fn system_path(p: &Path) -> bool {
    [
        "/etc/localtime",
        "/etc/timezone",
        "/var/run/docker.sock",
        "/run/docker.sock",
    ]
    .iter()
    .any(|s| p == Path::new(s))
        || ["/dev", "/sys", "/proc"].iter().any(|s| p.starts_with(s))
}

/// Whether another container's bind `other` uses the data at `path`: the
/// same folder or one inside it. A container that mounts a folder *above*
/// it (a backup tool, a file manager, the agent itself) doesn't count.
pub fn uses(other: &Path, path: &Path) -> bool {
    other.starts_with(path)
}

/// Checks a job against what runs here (`all`: every container) and
/// `agent.toml`. `self_id` is the agent's own container id (or its start),
/// left out of the "also used by" rule. The refusal says why; nothing has
/// changed then.
pub fn check(
    job: &Job,
    config: &Config,
    all: &[Ctr],
    templates: &[String],
    self_id: &str,
) -> Result<Plan, Note> {
    let name = job.target.name.as_str();
    if !config.updates.allows(name) {
        return Err(Note::new("refused.not_allowed", &[("target", name)]));
    }
    if job.health_seconds > MAX_HEALTH_SECONDS || job.keep_snapshots == 0 {
        return Err(Note::plain(
            "refused: the job's health period or snapshot count is out of range",
        ));
    }
    let mut plan = Plan::default();
    let kind = match job.target.kind.as_str() {
        "container" => Kind::Container,
        "unraid" => Kind::Unraid,
        "compose" => Kind::Compose,
        "script" => Kind::Script,
        other => return Err(Note::plain(format!("refused: unknown target kind {other}"))),
    };
    match kind {
        Kind::Container | Kind::Unraid => {
            if !valid_name(name) {
                return Err(Note::plain(format!(
                    "refused: {name} is not a container name"
                )));
            }
            let c = all
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| Note::new("refused.gone", &[("target", name)]))?;
            if !c.label(COMPOSE_PROJECT).is_empty() {
                return Err(Note::plain(format!(
                    "refused: {name} belongs to a Compose project, not a single container"
                )));
            }
            let templated = templates.iter().any(|t| t == name);
            if (kind == Kind::Unraid) != templated {
                return Err(Note::plain(format!(
                    "refused: {name} is {} an Unraid template container here",
                    if templated { "" } else { "not" }
                )));
            }
            plan.ctrs.push(c.clone());
        }
        Kind::Compose | Kind::Script => {
            let project = if kind == Kind::Script {
                let stack = config
                    .script_stacks
                    .iter()
                    .find(|s| s.name == name)
                    .ok_or_else(|| Note::new("refused.no_script_stack", &[("target", name)]))?;
                plan.stack = Some(stack.clone());
                plan.working_dir = stack.working_dir.clone();
                stack.compose_project.clone()
            } else {
                name.to_string()
            };
            if !valid_name(&project) {
                return Err(Note::plain(format!(
                    "refused: {project} is not a Compose project name"
                )));
            }
            // The policy names targets, and a project can have two: the
            // script stack's and its Compose project's. A deny on either
            // holds (an allow on the stack's name is enough).
            if config.updates.denies(&project) {
                return Err(Note::new("refused.not_allowed", &[("target", &project)]));
            }
            // A script stack owns its project: a plain `compose up` of it
            // would go round the stack's script (and its deny).
            if kind == Kind::Compose
                && config
                    .script_stacks
                    .iter()
                    .any(|s| s.compose_project == project)
            {
                return Err(Note::plain(format!(
                    "refused: {project} is the Compose project of a script stack in agent.toml; it is updated through the stack"
                )));
            }
            plan.ctrs = all
                .iter()
                .filter(|c| c.label(COMPOSE_PROJECT) == project)
                .cloned()
                .collect();
            if kind == Kind::Compose {
                let c = plan
                    .ctrs
                    .first()
                    .ok_or_else(|| Note::new("refused.gone", &[("target", name)]))?;
                plan.working_dir = PathBuf::from(c.label(COMPOSE_DIR));
                plan.files = c
                    .label(COMPOSE_FILES)
                    .split(',')
                    .map(str::trim)
                    .filter(|f| !f.is_empty())
                    .map(PathBuf::from)
                    .collect();
            }
            let wd = &plan.working_dir;
            if !config.roots.iter().any(|r| under(wd, r)) {
                return Err(Note::new(
                    "refused.not_under_roots",
                    &[("folder", &wd.to_string_lossy())],
                ));
            }
            if !wd.is_dir() {
                return Err(Note::new(
                    "refused.not_mounted",
                    &[("folder", &wd.to_string_lossy())],
                ));
            }
            if plan
                .files
                .iter()
                .any(|f| !config.roots.iter().any(|r| under(f, r)))
            {
                return Err(Note::new("refused.file_outside_roots", &[]));
            }
        }
    }
    // The agent never touches itself: stopping its own container mid-run
    // would leave the run unfinished (and the agent stopped).
    if self_id.len() >= 12 && plan.ctrs.iter().any(|c| c.id.starts_with(self_id)) {
        return Err(Note::plain(
            "refused: Wren never updates its own agent; this target includes the agent's container",
        ));
    }
    // Every image of the job is what that container of the target runs now,
    // at the digest Wren saw.
    for i in &job.images {
        let c = plan
            .ctrs
            .iter()
            .find(|c| c.name == i.container)
            .ok_or_else(|| {
                Note::plain(format!("refused: {} is not part of {name}", i.container))
            })?;
        if c.image != i.image {
            return Err(Note::plain(format!(
                "refused: {} runs {}, not {}",
                c.name, c.image, i.image
            )));
        }
        if !valid_digest(&i.to) || repo_of(&i.image).is_none() {
            return Err(Note::plain(format!(
                "refused: {} has no updatable reference",
                c.name
            )));
        }
        // A rollback's `from` ends up in Unraid's update status: a digest too.
        if job.kind == JobKind::Rollback && !valid_digest(&i.from) {
            return Err(Note::plain(format!(
                "refused: {} has no valid digest to roll back from",
                c.name
            )));
        }
        // Any repo digest counts; already on `to` is "already up to date".
        let digests = local_digests(&c.image, &c.repo_digests);
        if job.kind == JobKind::Update && !digests.contains(&i.from) && !digests.contains(&i.to) {
            return Err(Note::new(
                "refused.changed",
                &[("container", &c.name), ("digest", &i.from)],
            ));
        }
    }
    if job.kind == JobKind::Update && job.images.is_empty() && kind != Kind::Script {
        return Err(Note::plain("refused: the job names no image to update"));
    }
    if job.kind == JobKind::Rollback && job.rollback_of.is_none() {
        return Err(Note::plain("refused: a rollback job names no run"));
    }
    if plan.ctrs.is_empty() && kind != Kind::Script {
        return Err(Note::new("refused.gone", &[("target", name)]));
    }
    // Data paths: the target's own bind mounts under a root (not a root
    // itself), never one another container also uses (the same folder or
    // one inside it). Every path left out is noted with why.
    let own: Vec<&str> = plan.ctrs.iter().map(|c| c.name.as_str()).collect();
    for c in &plan.ctrs {
        for v in &c.volumes {
            plan.notes.push(Note::new(
                "path.volume",
                &[("volume", v), ("container", &c.name)],
            ));
        }
        for b in &c.binds {
            let path = b.to_string_lossy().to_string();
            let noted = |notes: &[Note]| {
                notes
                    .iter()
                    .any(|n| n.params.get("path").is_some_and(|p| *p == path))
            };
            let Some(root) = config.roots.iter().find(|r| under(b, r)) else {
                if !system_path(b) && !noted(&plan.notes) {
                    plan.notes
                        .push(Note::new("path.not_under_roots", &[("path", &path)]));
                }
                continue;
            };
            if b == root {
                if !noted(&plan.notes) {
                    plan.notes.push(Note::new("path.root", &[("path", &path)]));
                }
                continue;
            }
            if let Some(other) = all
                .iter()
                .filter(|o| !own.contains(&o.name.as_str()))
                // The agent itself.
                .filter(|o| self_id.len() < 12 || !o.id.starts_with(self_id))
                .find(|o| o.binds.iter().any(|ob| uses(ob, b)))
            {
                if !noted(&plan.notes) {
                    plan.notes.push(Note::new(
                        "path.shared",
                        &[("path", &path), ("with", &other.name)],
                    ));
                }
                continue;
            }
            if !plan.data.iter().any(|(p, _)| p == b) {
                plan.data.push((b.clone(), root.clone()));
            }
        }
    }
    plan.kind = Some(kind);
    plan.job = Some(job.clone());
    Ok(plan)
}

/// The 64-hex container id right after one of `markers` in `text`.
fn id_after(text: &str, markers: &[&str]) -> Option<String> {
    for m in markers {
        let mut rest = text;
        while let Some(i) = rest.find(m) {
            rest = &rest[i + m.len()..];
            let id: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
            if id.len() == 64 {
                return Some(id);
            }
        }
    }
    None
}

/// This container's id: from `/proc/self/cgroup` (cgroup v1, systemd's
/// `docker-<id>.scope`), else `/proc/self/mountinfo` (Docker's
/// `/containers/<id>/` files, also with a private cgroup namespace), else
/// the hostname Docker sets by default. Empty when none says it.
pub fn own_container_id(cgroup: &str, mountinfo: &str, hostname: &str) -> String {
    if let Some(id) = id_after(cgroup, &["/docker/", "docker-", "/libpod-"]) {
        return id;
    }
    if let Some(id) = id_after(mountinfo, &["/docker/containers/", "/containers/"]) {
        return id;
    }
    let h = hostname.trim();
    if h.len() >= 12 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
        h.to_string()
    } else {
        String::new()
    }
}

/// [`own_container_id`] of this process.
pub fn own_id() -> String {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    own_container_id(
        &read("/proc/self/cgroup"),
        &read("/proc/self/mountinfo"),
        &read("/etc/hostname"),
    )
}

// ---------------------------------------------------------------------------
// The client side of the job channel
// ---------------------------------------------------------------------------

/// One step's result as posted to Wren.
#[derive(Debug, Clone, Serialize)]
pub struct StepReport<'a> {
    pub step: &'a str,
    pub status: &'a str,
    pub log: &'a str,
    pub duration_ms: u64,
    /// What Wren shows translated (why paths were left out of a snapshot).
    #[serde(skip_serializing_if = "<[Note]>::is_empty")]
    pub notes: &'a [Note],
}

#[derive(Debug, Deserialize)]
struct Poll {
    job: Value,
}

#[derive(Debug, Default, Deserialize)]
struct StepAnswer {
    #[serde(default)]
    cancel: bool,
}

/// Talks to Wren's job endpoints.
pub struct JobClient {
    http: reqwest::Client,
    server: String,
}

/// What a poll brought.
pub enum Polled {
    Nothing,
    /// A job, or the reason it can't be read (refused as such).
    Job(Result<Job, (i64, String)>),
}

impl JobClient {
    pub fn new(server: &str, token: &str) -> Result<JobClient, String> {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "the token has characters a header can't carry")?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        let http = reqwest::Client::builder()
            .user_agent(concat!("WiseWrenAgent/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(POLL_SECONDS + 20))
            // Step results and finishes are posted once; a redirect would
            // re-post them somewhere else.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(JobClient {
            http,
            server: server.trim_end_matches('/').to_string(),
        })
    }

    /// Waits for a job (Wren answers within [`POLL_SECONDS`]).
    pub async fn poll(&self) -> Result<Polled, crate::heartbeat::SendError> {
        use crate::heartbeat::SendError;
        let res = self
            .http
            // Which process asks (hex and dashes): a run handed to an
            // earlier one is over.
            .get(format!(
                "{}{JOBS_PATH}?boot={}",
                self.server,
                crate::heartbeat::boot().id
            ))
            .send()
            .await
            .map_err(|e| SendError::Failed(format!("can't reach Wren: {e}")))?;
        let status = res.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Polled::Nothing);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SendError::Refused(format!(
                "Wren refused the token ({status})"
            )));
        }
        if !status.is_success() {
            return Err(SendError::Failed(format!("Wren answered {status}")));
        }
        let poll: Poll = read_json(res, MAX_JOB_BYTES)
            .await
            .map_err(SendError::Failed)?;
        let run_id = poll.job.get("run_id").and_then(Value::as_i64).unwrap_or(0);
        Ok(Polled::Job(
            serde_json::from_value::<Job>(poll.job)
                .map_err(|e| (run_id, format!("refused: the job isn't understood ({e})"))),
        ))
    }

    async fn post(&self, path: String, body: &impl Serialize) -> Result<Value, String> {
        let mut last = String::new();
        // A few tries: a step result shouldn't get lost to a blip.
        for attempt in 0..4u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
            }
            match self
                .http
                .post(format!("{}{path}", self.server))
                .json(body)
                .send()
                .await
            {
                Ok(res) if res.status().is_success() => {
                    // Too big or not JSON: no answer, as before.
                    return Ok(read_json(res, MAX_VERDICT_BYTES)
                        .await
                        .unwrap_or(Value::Null));
                }
                // The run is not (or no longer) this agent's: no retry.
                Ok(res) if res.status().is_client_error() => {
                    return Err(format!("Wren answered {}", res.status()));
                }
                Ok(res) => last = format!("Wren answered {}", res.status()),
                Err(e) => last = format!("can't reach Wren: {e}"),
            }
        }
        Err(last)
    }

    /// Posts a step's result; whether Wren wants the run cancelled.
    pub async fn step(&self, run: i64, report: &StepReport<'_>) -> Result<bool, String> {
        let v = self
            .post(format!("/api/agent/v1/runs/{run}/steps"), report)
            .await?;
        Ok(serde_json::from_value::<StepAnswer>(v)
            .unwrap_or_default()
            .cancel)
    }

    /// Asks about the target's Vigilant Vole monitors: `(verdict, log)`. A
    /// Wren without the endpoint (404) counts as `skipped`.
    pub async fn monitors(&self, run: i64) -> Result<(String, String), String> {
        let res = self
            .http
            .post(format!("{}/api/agent/v1/runs/{run}/monitors", self.server))
            .send()
            .await
            .map_err(|e| format!("can't reach Wren: {e}"))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(("skipped".into(), String::new()));
        }
        if !res.status().is_success() {
            return Err(format!("Wren answered {}", res.status()));
        }
        let v: Value = read_json(res, MAX_VERDICT_BYTES).await?;
        Ok((
            v["verdict"].as_str().unwrap_or("skipped").to_string(),
            v["log"].as_str().unwrap_or_default().to_string(),
        ))
    }

    /// Ends a run. `detail` is the note's English; Wren shows its code in the
    /// reader's language (older Wrens the English).
    pub async fn finish(&self, run: i64, outcome: &str, note: &Note) -> Result<(), String> {
        let mut body = serde_json::json!({ "outcome": outcome, "detail": note.text() });
        if !note.code.is_empty() {
            body["code"] = serde_json::json!(note.code);
            body["params"] = serde_json::json!(note.params);
        }
        self.post(format!("/api/agent/v1/runs/{run}/finish"), &body)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TOKEN: &str = "wwa_0123456789abcdefghijABCDEFGHIJ0123456789xyz";
    const A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn config(extra: &str) -> Config {
        Config::from_parts(
            Some(&format!(
                "server = \"http://x\"\ntoken = \"{TOKEN}\"\nroots = [\"/mnt/user/appdata/ww-test\"]\n{extra}"
            )),
            |_| None,
        )
        .unwrap()
    }

    fn ctr(name: &str, image: &str, binds: &[&str]) -> Ctr {
        let repo = repo_of(image).unwrap();
        Ctr::from_inspect(
            json!({
                "Name": format!("/{name}"), "Id": (if name == "wren-agent" { "c0ffee000000abc" } else { "abc" }), "Image": "sha256:old",
                "Config": {"Image": image, "Labels": {}},
                "State": {"Running": true},
                "Mounts": binds.iter().map(|b| json!({"Type": "bind", "Source": b})).collect::<Vec<_>>(),
            }),
            vec![format!("{repo}@{A}")],
        )
    }

    fn job(kind: &str, name: &str, container: &str, image: &str) -> Job {
        Job {
            run_id: 5,
            kind: JobKind::Update,
            target: TargetRef {
                kind: kind.into(),
                name: name.into(),
            },
            from_key: format!("{container}={A}"),
            to_key: format!("{container}={B}"),
            images: vec![JobImage {
                container: container.into(),
                image: image.into(),
                from: A.into(),
                to: B.into(),
            }],
            health_seconds: 5,
            keep_snapshots: 2,
            rollback_of: None,
            require_snapshot: false,
        }
    }

    #[test]
    fn job_outside_allowlist_is_refused() {
        let c = config("[updates]\nallow = [\"ww-test-*\"]\n");
        let all = vec![ctr("postiz", "ghcr.io/gitroomhq/postiz-app:latest", &[])];
        let err = check(
            &job(
                "container",
                "postiz",
                "postiz",
                "ghcr.io/gitroomhq/postiz-app:latest",
            ),
            &c,
            &all,
            &[],
            "",
        )
        .unwrap_err();
        assert_eq!(err.code, "refused.not_allowed");
        assert!(err.text().starts_with("refused by agent config"), "{err}");
        // Allowed by default (no section).
        let c = config("");
        assert!(
            check(
                &job(
                    "container",
                    "postiz",
                    "postiz",
                    "ghcr.io/gitroomhq/postiz-app:latest"
                ),
                &c,
                &all,
                &[],
                ""
            )
            .is_ok()
        );
    }

    #[test]
    fn job_for_a_foreign_image_is_refused() {
        let c = config("");
        let all = vec![ctr("ww-test-c", "localhost:5056/ww-test/app:latest", &[])];
        // Another image than the container runs.
        let err = check(
            &job("container", "ww-test-c", "ww-test-c", "evil/miner:latest"),
            &c,
            &all,
            &[],
            "",
        )
        .unwrap_err()
        .text();
        assert!(
            err.contains("runs localhost:5056/ww-test/app:latest"),
            "{err}"
        );
        // A container of another target.
        let mut j = job("container", "ww-test-c", "other", "x:1");
        let err = check(&j, &c, &all, &[], "").unwrap_err().text();
        assert!(err.contains("not part of"), "{err}");
        // The running digest moved since the check.
        j = job(
            "container",
            "ww-test-c",
            "ww-test-c",
            "localhost:5056/ww-test/app:latest",
        );
        j.images[0].from = B.into();
        assert!(
            check(&j, &c, &all, &[], "")
                .unwrap_err()
                .text()
                .contains("changed since")
        );
        // Not a digest.
        j.images[0].from = A.into();
        j.images[0].to = "latest; rm -rf /".into();
        assert!(check(&j, &c, &all, &[], "").is_err());
        // Kind must match what runs here.
        let j = job(
            "unraid",
            "ww-test-c",
            "ww-test-c",
            "localhost:5056/ww-test/app:latest",
        );
        assert!(
            check(&j, &c, &all, &[], "")
                .unwrap_err()
                .text()
                .contains("not an Unraid")
        );
        assert!(check(&j, &c, &all, &["ww-test-c".into()], "").is_ok());
        // A script stack must be in agent.toml.
        let mut j = job("script", "nope", "x", "x:1");
        j.images.clear();
        assert!(
            check(&j, &c, &all, &[], "")
                .unwrap_err()
                .text()
                .contains("no script stack")
        );
    }

    /// A folder under the temp dir that exists, as a root.
    fn tmp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wren-jobs-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_in(root: &Path, extra: &str) -> Config {
        Config::from_parts(
            Some(&format!(
                "server = \"http://x\"\ntoken = \"{TOKEN}\"\nroots = [\"{}\"]\n{extra}",
                root.display()
            )),
            |_| None,
        )
        .unwrap()
    }

    /// A container of a Compose project.
    fn member(name: &str, id: &str, project: &str, dir: &Path) -> Ctr {
        let mut c = ctr(name, "a/b:1", &[]);
        c.id = id.into();
        c.labels.insert(COMPOSE_PROJECT.into(), project.into());
        c.labels
            .insert(COMPOSE_DIR.into(), dir.to_string_lossy().into());
        c
    }

    #[test]
    fn a_job_that_includes_the_agents_own_container_is_refused() {
        let c = config("");
        let own = "c0ffee000000";
        let all = vec![
            ctr("wren-agent", "wise-wren-agent:dev", &[]),
            ctr("other", "x/y:1", &[]),
        ];
        let update = job(
            "container",
            "wren-agent",
            "wren-agent",
            "wise-wren-agent:dev",
        );
        let mut rollback = update.clone();
        rollback.kind = JobKind::Rollback;
        rollback.rollback_of = Some(3);
        for j in [update, rollback] {
            let err = check(&j, &c, &all, &[], own).unwrap_err().text();
            assert!(err.contains("Wren never updates its own agent"), "{err}");
        }
        // Other containers are fine, with or without knowing our own id.
        let other = job("container", "other", "other", "x/y:1");
        assert!(check(&other, &c, &all, &[], own).is_ok());
        assert!(check(&other, &c, &all, &[], "").is_ok());

        // A Compose project the agent belongs to is refused as a whole.
        let root = tmp_root("own");
        let c = config_in(&root, "");
        let all = vec![
            member("web", "aaaaaaaaaaaaaaaa", "stack", &root),
            member("wren-agent", "c0ffee000000abc", "stack", &root),
        ];
        let j = job("compose", "stack", "web", "a/b:1");
        let err = check(&j, &c, &all, &[], own).unwrap_err().text();
        assert!(err.contains("its own agent"), "{err}");
        assert!(check(&j, &c, &all, &[], "").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_compose_job_cannot_go_round_a_script_stack() {
        let root = tmp_root("stack");
        std::fs::create_dir_all(root.join("fluxer")).unwrap();
        let stack = format!(
            "[[script_stack]]\nname = \"fluxer-stack\"\ncompose_project = \"fluxer\"\nworking_dir = \"{}/fluxer\"\ntimeout = \"1m\"\nupdate = {{ command = \"true\" }}\n",
            root.display()
        );
        let all = vec![member("fluxer-web", "aaaaaaaaaaaaaaaa", "fluxer", &root)];
        let compose = |name: &str| job("compose", name, "fluxer-web", "a/b:1");
        let c = config_in(&root, &stack);
        let err = check(&compose("fluxer"), &c, &all, &[], "")
            .unwrap_err()
            .text();
        assert!(err.contains("Compose project of a script stack"), "{err}");
        // Without the stack, the same job is a plain Compose job.
        let c = config_in(&root, "");
        assert!(check(&compose("fluxer"), &c, &all, &[], "").is_ok());

        // The script job itself runs, and so does an allow on the stack's
        // name alone ...
        let script = {
            let mut j = job("script", "fluxer-stack", "fluxer-web", "a/b:1");
            j.images.clear();
            j
        };
        let c = config_in(&root, &stack);
        assert!(check(&script, &c, &all, &[], "").is_ok());
        let c = config_in(
            &root,
            &format!("[updates]\nallow = [\"fluxer-stack\"]\n{stack}"),
        );
        assert!(check(&script, &c, &all, &[], "").is_ok());
        // ... but a deny on the Compose project holds for the script job,
        // and a deny on the stack's name still does.
        for deny in ["fluxer", "fluxer-stack", "flux*"] {
            let c = config_in(
                &root,
                &format!("[updates]\nallow = [\"*\"]\ndeny = [\"{deny}\"]\n{stack}"),
            );
            let err = check(&script, &c, &all, &[], "").unwrap_err();
            assert_eq!(err.code, "refused.not_allowed", "{deny}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rollback_job_needs_digests_in_its_images() {
        let c = config("");
        let img = "localhost:5056/ww-test/app:latest";
        let all = vec![ctr("ww-test-c", img, &[])];
        let mut j = job("container", "ww-test-c", "ww-test-c", img);
        j.kind = JobKind::Rollback;
        j.rollback_of = Some(4);
        assert!(check(&j, &c, &all, &[], "").is_ok());
        for from in ["", "latest", "sha256:short", "x; rm -rf /"] {
            j.images[0].from = from.into();
            let err = check(&j, &c, &all, &[], "").unwrap_err().text();
            assert!(err.contains("no valid digest to roll back from"), "{err}");
        }
    }

    #[test]
    fn data_paths_are_own_unshared_and_under_roots() {
        let c = config("");
        let all = vec![
            ctr(
                "ww-test-c",
                "a/b:1",
                &[
                    "/mnt/user/appdata/ww-test/c/data",
                    "/mnt/user/appdata/ww-test/shared",
                    "/mnt/user/appdata/ww-test",
                    "/etc/localtime",
                ],
            ),
            ctr("other", "x/y:2", &["/mnt/user/appdata/ww-test/shared/sub"]),
            ctr(
                "wren-agent",
                "wise-wren-agent:dev",
                &["/mnt/user/appdata/ww-test"],
            ),
        ];
        let j = job("container", "ww-test-c", "ww-test-c", "a/b:1");
        // The agent itself (its id is the hostname) mounts the root: not a sharer.
        let p = check(&j, &c, &all, &[], "c0ffee000000").unwrap();
        assert_eq!(
            p.data,
            vec![(
                PathBuf::from("/mnt/user/appdata/ww-test/c/data"),
                PathBuf::from("/mnt/user/appdata/ww-test")
            )]
        );
        let notes: Vec<String> = p.notes.iter().map(Note::text).collect();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("shared is also used by other")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("a root itself")),
            "{notes:?}"
        );
        assert!(
            !notes.iter().any(|n| n.contains("localtime")),
            "system paths need no note: {notes:?}"
        );
    }

    /// Tower's first night: a backup container and the agent mount all of
    /// appdata. A container above the data doesn't block its snapshot; one
    /// at the same folder or inside it does.
    #[test]
    fn only_the_same_folder_or_one_inside_it_counts_as_shared() {
        let c = Config::from_parts(
            Some(&format!(
                "server = \"http://x\"\ntoken = \"{TOKEN}\"\nroots = [\"/mnt/user/appdata\", \"/mnt/cache/appdata\"]\n"
            )),
            |_| None,
        )
        .unwrap();
        let sonarr = ctr(
            "sonarr",
            "lscr.io/linuxserver/sonarr:latest",
            &["/mnt/user/appdata/sonarr", "/mnt/user/plex_data"],
        );
        let j = job(
            "container",
            "sonarr",
            "sonarr",
            "lscr.io/linuxserver/sonarr:latest",
        );
        // rclone and the agent (not recognised as itself) mount the parent.
        let all = vec![
            sonarr.clone(),
            ctr("rclone", "rclone/rclone:latest", &["/mnt/user/appdata"]),
            ctr(
                "Wise-Wren-Agent",
                "wise-wren-agent:latest",
                &["/mnt/cache/appdata", "/mnt/user/appdata"],
            ),
        ];
        let p = check(&j, &c, &all, &[], "").unwrap();
        assert_eq!(
            p.data,
            vec![(
                PathBuf::from("/mnt/user/appdata/sonarr"),
                PathBuf::from("/mnt/user/appdata")
            )]
        );
        let notes: Vec<&str> = p.notes.iter().map(|n| n.code).collect();
        assert_eq!(notes, ["path.not_under_roots"], "{:?}", p.notes);
        assert_eq!(p.notes[0].params["path"], "/mnt/user/plex_data");
        // The same folder, or one inside it: shared, not snapshotted.
        for other in [
            "/mnt/user/appdata/sonarr",
            "/mnt/user/appdata/sonarr/backups",
        ] {
            let mut all = all.clone();
            all.push(ctr("tool", "x/y:1", &[other]));
            let p = check(&j, &c, &all, &[], "").unwrap();
            assert!(p.data.is_empty(), "{other}");
            assert_eq!(p.notes[0].code, "path.shared");
            assert_eq!(p.notes[0].params["with"], "tool");
            assert_eq!(
                p.notes[0].text(),
                "/mnt/user/appdata/sonarr is also used by tool"
            );
        }
    }

    #[test]
    fn own_container_id_from_cgroup_mountinfo_or_hostname() {
        let id = "65cd0cbed6c29b774ff72da96547e9dd5294b9f0f6302121c112be1911873d1d";
        let layer = "a".repeat(64);
        assert_eq!(
            own_container_id(&format!("12:pids:/docker/{id}\n"), "", ""),
            id
        );
        assert_eq!(
            own_container_id(&format!("0::/system.slice/docker-{id}.scope\n"), "", ""),
            id
        );
        // cgroup v2 with its own namespace says "0::/": mountinfo knows.
        let mountinfo = format!(
            "1 0 0:1 / / rw - overlay overlay rw,lowerdir=/var/lib/docker/overlay2/{layer}/diff\n\
             2 1 0:2 /var/lib/docker/containers/{id}/hostname /etc/hostname rw - xfs /dev/md1p1 rw\n"
        );
        assert_eq!(own_container_id("0::/\n", &mountinfo, "Tower"), id);
        assert_eq!(
            own_container_id("0::/\n", "", "65cd0cbed6c2\n"),
            "65cd0cbed6c2"
        );
        assert_eq!(own_container_id("0::/\n", "", "Tower"), "");
    }

    #[test]
    fn paths_with_dotdot_are_not_under_a_root() {
        let root = Path::new("/mnt/user/appdata/ww-test");
        assert!(under(Path::new("/mnt/user/appdata/ww-test/compose"), root));
        assert!(!under(
            Path::new("/mnt/user/appdata/ww-test/../../../etc"),
            root
        ));
        assert!(!under(Path::new("/mnt/user/appdata/ww-test-other"), root));
        assert!(!under(Path::new("ww-test/compose"), root));
    }

    #[test]
    fn several_repo_digests_and_already_up_to_date() {
        let c = config("");
        let img = "localhost:5056/ww-test/app:latest";
        // Pulled again by a new index digest: the image has both.
        let mut both = ctr("ww-test-c", img, &[]);
        both.repo_digests = vec![
            format!("localhost:5056/ww-test/app@{A}"),
            format!("localhost:5056/ww-test/app@{B}"),
            format!("other/app@sha256:{}", "f".repeat(64)),
        ];
        assert_eq!(local_digests(img, &both.repo_digests), [A, B]);
        let j = job("container", "ww-test-c", "ww-test-c", img);
        // From A to B while B is already the image: checked, nothing to do.
        let plan = check(&j, &c, std::slice::from_ref(&both), &[], "").unwrap();
        assert!(already_on(&j, &plan.ctrs));
        // Wren saw another digest, but the approved one runs: still fine.
        let mut moved = j.clone();
        moved.images[0].from = format!("sha256:{}", "c".repeat(64));
        assert!(check(&moved, &c, std::slice::from_ref(&both), &[], "").is_ok());
        // Only on A: a real update.
        let one = ctr("ww-test-c", img, &[]);
        let plan = check(&j, &c, std::slice::from_ref(&one), &[], "").unwrap();
        assert!(!already_on(&j, &plan.ctrs));
        // A rollback is never "already up to date".
        let mut back = j.clone();
        back.kind = JobKind::Rollback;
        assert!(!already_on(&back, &[both]));
    }

    #[test]
    fn refs_and_digests() {
        assert_eq!(
            repo_of("localhost:5056/a/b:latest").unwrap(),
            "localhost:5056/a/b"
        );
        assert_eq!(repo_of("localhost:5056/a/b").unwrap(), "localhost:5056/a/b");
        assert_eq!(repo_of("redis").unwrap(), "redis");
        assert!(repo_of("redis@sha256:abc").is_none());
        assert_eq!(
            local_digest("redis:7", &[format!("docker.io/library/redis@{A}")]).as_deref(),
            Some(A)
        );
        assert!(local_digest("redis:7", &[format!("other/redis@{A}")]).is_none());
        assert!(valid_name("ww-test-c") && !valid_name("-x") && !valid_name("a b"));
        let j: Job = serde_json::from_value(json!({
            "run_id": 1, "kind": "update", "target": {"kind": "container", "name": "x"}
        }))
        .unwrap();
        assert_eq!((j.health_seconds, j.keep_snapshots), (180, 2));
        assert!(
            serde_json::from_value::<Job>(json!({
                "run_id": 1, "kind": "shell", "target": {"kind": "container", "name": "x"}
            }))
            .is_err()
        );
    }
}
