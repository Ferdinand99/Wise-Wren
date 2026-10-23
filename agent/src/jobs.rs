//! Jobs from Wren (design §6, §8): fetched with a long-poll
//! `GET /api/agent/v1/jobs`, results posted back step by step.
//!
//! A job is data, never a command: its kind (`update` or `rollback`), the
//! target's kind and name, version keys and, per image, the container, the
//! reference it runs and the two digests. Before anything happens the agent
//! re-reads Docker and [`check`]s the job against what runs here and its own
//! `agent.toml`; what they don't allow is refused and nothing changes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, ScriptStack};

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
    let repo = repo_of(image)?;
    repo_digests.iter().find_map(|d| {
        let (r, digest) = d.split_once('@')?;
        (normal(r) == normal(&repo)).then(|| digest.to_string())
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
    pub notes: Vec<String>,
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

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Checks a job against what runs here (`all`: every container) and
/// `agent.toml`. `self_id` is the agent's own container id (or
/// its start), left out of the "also used by" rule. The refusal says why; nothing has changed then.
pub fn check(
    job: &Job,
    config: &Config,
    all: &[Ctr],
    templates: &[String],
    self_id: &str,
) -> Result<Plan, String> {
    let name = job.target.name.as_str();
    if !config.updates.allows(name) {
        return Err(format!(
            "refused by agent config: {name} is not allowed by [updates] in agent.toml"
        ));
    }
    if job.health_seconds > MAX_HEALTH_SECONDS || job.keep_snapshots == 0 {
        return Err("refused: the job's health period or snapshot count is out of range".into());
    }
    let mut plan = Plan::default();
    let kind = match job.target.kind.as_str() {
        "container" => Kind::Container,
        "unraid" => Kind::Unraid,
        "compose" => Kind::Compose,
        "script" => Kind::Script,
        other => return Err(format!("refused: unknown target kind {other}")),
    };
    match kind {
        Kind::Container | Kind::Unraid => {
            if !valid_name(name) {
                return Err(format!("refused: {name} is not a container name"));
            }
            let c = all
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| format!("refused: no container named {name} runs here"))?;
            if !c.label(COMPOSE_PROJECT).is_empty() {
                return Err(format!(
                    "refused: {name} belongs to a Compose project, not a single container"
                ));
            }
            let templated = templates.iter().any(|t| t == name);
            if (kind == Kind::Unraid) != templated {
                return Err(format!(
                    "refused: {name} is {} an Unraid template container here",
                    if templated { "" } else { "not" }
                ));
            }
            plan.ctrs.push(c.clone());
        }
        Kind::Compose | Kind::Script => {
            let project = if kind == Kind::Script {
                let stack = config
                    .script_stacks
                    .iter()
                    .find(|s| s.name == name)
                    .ok_or_else(|| {
                        format!("refused: agent.toml has no script stack named {name}")
                    })?;
                plan.stack = Some(stack.clone());
                plan.working_dir = stack.working_dir.clone();
                stack.compose_project.clone()
            } else {
                name.to_string()
            };
            if !valid_name(&project) {
                return Err(format!("refused: {project} is not a Compose project name"));
            }
            plan.ctrs = all
                .iter()
                .filter(|c| c.label(COMPOSE_PROJECT) == project)
                .cloned()
                .collect();
            if kind == Kind::Compose {
                let c = plan.ctrs.first().ok_or_else(|| {
                    format!("refused: no container of Compose project {name} runs here")
                })?;
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
            if !wd.is_absolute() || !config.roots.iter().any(|r| wd.starts_with(r)) {
                return Err(format!(
                    "refused: the stack's folder {} is not under the agent's roots",
                    wd.display()
                ));
            }
            if !wd.is_dir() {
                return Err(format!(
                    "refused: {} is not mounted in the agent at the same path",
                    wd.display()
                ));
            }
            if plan
                .files
                .iter()
                .any(|f| !config.roots.iter().any(|r| f.starts_with(r)))
            {
                return Err("refused: a Compose file is outside the agent's roots".into());
            }
        }
    }
    // Every image of the job is what that container of the target runs now,
    // at the digest Wren saw.
    for i in &job.images {
        let c = plan
            .ctrs
            .iter()
            .find(|c| c.name == i.container)
            .ok_or_else(|| format!("refused: {} is not part of {name}", i.container))?;
        if c.image != i.image {
            return Err(format!(
                "refused: {} runs {}, not {}",
                c.name, c.image, i.image
            ));
        }
        if !valid_digest(&i.to) || repo_of(&i.image).is_none() {
            return Err(format!("refused: {} has no updatable reference", c.name));
        }
        if job.kind == JobKind::Update
            && local_digest(&c.image, &c.repo_digests).as_deref() != Some(i.from.as_str())
        {
            return Err(format!(
                "refused: {} changed since Wren's check (it no longer runs {})",
                c.name, i.from
            ));
        }
    }
    if job.kind == JobKind::Update && job.images.is_empty() && kind != Kind::Script {
        return Err("refused: the job names no image to update".into());
    }
    if job.kind == JobKind::Rollback && job.rollback_of.is_none() {
        return Err("refused: a rollback job names no run".into());
    }
    if plan.ctrs.is_empty() && kind != Kind::Script {
        return Err(format!("refused: nothing of {name} runs here"));
    }
    // Data paths: the target's own bind mounts under a root (not a root
    // itself), never one another container also mounts.
    let own: Vec<&str> = plan.ctrs.iter().map(|c| c.name.as_str()).collect();
    for c in &plan.ctrs {
        for v in &c.volumes {
            plan.notes.push(format!(
                "volume {v} of {} is not snapshotted (outside the roots)",
                c.name
            ));
        }
        for b in &c.binds {
            let Some(root) = config.roots.iter().find(|r| b.starts_with(r)) else {
                continue;
            };
            if b == root {
                plan.notes.push(format!(
                    "{} is a root itself and is not snapshotted",
                    b.display()
                ));
                continue;
            }
            if let Some(other) = all
                .iter()
                .filter(|o| !own.contains(&o.name.as_str()))
                // The agent itself mounts every root.
                .filter(|o| self_id.len() < 12 || !o.id.starts_with(self_id))
                .find(|o| o.binds.iter().any(|ob| overlaps(ob, b)))
            {
                plan.notes.push(format!(
                    "{} is also used by {} and is not snapshotted",
                    b.display(),
                    other.name
                ));
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
            .get(format!("{}{JOBS_PATH}", self.server))
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
        let poll: Poll = res
            .json()
            .await
            .map_err(|e| SendError::Failed(format!("Wren's answer wasn't understood: {e}")))?;
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
                    return Ok(res.json().await.unwrap_or(Value::Null));
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
        let v: Value = res
            .json()
            .await
            .map_err(|e| format!("Wren's answer wasn't understood: {e}"))?;
        Ok((
            v["verdict"].as_str().unwrap_or("skipped").to_string(),
            v["log"].as_str().unwrap_or_default().to_string(),
        ))
    }

    pub async fn finish(&self, run: i64, outcome: &str, detail: &str) -> Result<(), String> {
        self.post(
            format!("/api/agent/v1/runs/{run}/finish"),
            &serde_json::json!({ "outcome": outcome, "detail": detail }),
        )
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
        assert!(err.starts_with("refused by agent config"), "{err}");
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
        .unwrap_err();
        assert!(
            err.contains("runs localhost:5056/ww-test/app:latest"),
            "{err}"
        );
        // A container of another target.
        let mut j = job("container", "ww-test-c", "other", "x:1");
        let err = check(&j, &c, &all, &[], "").unwrap_err();
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
                .contains("not an Unraid")
        );
        assert!(check(&j, &c, &all, &["ww-test-c".into()], "").is_ok());
        // A script stack must be in agent.toml.
        let mut j = job("script", "nope", "x", "x:1");
        j.images.clear();
        assert!(
            check(&j, &c, &all, &[], "")
                .unwrap_err()
                .contains("no script stack")
        );
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
        assert!(
            p.notes.iter().any(|n| n.contains("also used by other")),
            "{:?}",
            p.notes
        );
        assert!(
            p.notes.iter().any(|n| n.contains("a root itself")),
            "{:?}",
            p.notes
        );
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
