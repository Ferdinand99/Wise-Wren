//! The agent's inventory (`POST /api/agent/v1/inventory`): what runs on its
//! server, which Unraid templates exist, the Compose projects and the
//! script stacks from its `agent.toml` with what their version commands
//! printed. Wren turns it into targets ([`plan`], [`crate::targets::apply`]).
//!
//! Grouping (design §4, spike Q2/Q3): a container with
//! `com.docker.compose.project` belongs to its project's Compose stack, or to
//! the script stack built on that project; any other container is an Unraid
//! template container when a template's `<Name>` is its name, else a plain
//! container. Only Wren's own tables change; the agent is told nothing to do.

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use axum::Json;
use axum::body::Bytes;
use axum::extract::Extension;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use vole_kit::http::ApiError;
use vole_kit::http::ratelimit::{Rate, too_many_requests};

use crate::Wren;
use crate::registry::{self, ImageRef};

pub const PATH: &str = "/api/agent/v1/inventory";
/// The largest inventory Wren reads.
pub const MAX_BYTES: usize = 2 << 20;
/// Inventories per server: 6 at once, 6 a minute.
pub const RATE: Rate = Rate::per_minute(6);
/// Longest text Wren keeps of a reported value.
pub const MAX_TEXT: usize = 500;

pub const PROJECT_LABEL: &str = "com.docker.compose.project";
pub const SERVICE_LABEL: &str = "com.docker.compose.service";
pub const WORKING_DIR_LABEL: &str = "com.docker.compose.project.working_dir";
pub const SOURCE_LABEL: &str = "org.opencontainers.image.source";

/// What an agent reports. Unknown fields are ignored.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Inventory {
    pub containers: Vec<Container>,
    /// The `<Name>`s of the Unraid templates (`templates-user/my-*.xml`).
    pub unraid_templates: Vec<String>,
    pub compose_projects: Vec<ComposeProject>,
    pub script_stacks: Vec<ScriptStack>,
    /// Why Docker couldn't be read, if it couldn't.
    pub error: String,
    /// What the agent allows (0.4.0+; `None` from an older agent).
    pub policy: Option<crate::policy::AgentPolicy>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Container {
    pub name: String,
    /// `.Config.Image`: keeps `@sha256:` pins, unlike `docker ps`.
    pub image: String,
    pub image_id: String,
    /// The `RepoDigests` of the image the container runs.
    pub repo_digests: Vec<String>,
    pub state: String,
    /// `org.opencontainers.image.*`, `com.docker.compose.*`,
    /// `net.unraid.docker.*` labels.
    pub labels: BTreeMap<String, String>,
    /// Its bind mounts' host folders (0.4.0+).
    pub binds: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ComposeProject {
    pub name: String,
    pub working_dir: String,
    pub config_files: String,
    /// The working dir exists at the same path inside the agent.
    pub files_reachable: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ScriptStack {
    pub name: String,
    pub compose_project: String,
    pub working_dir: String,
    pub files_reachable: bool,
    pub version: Option<Version>,
}

/// A script stack's version source and what the agent's own commands
/// printed.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Version {
    pub source: crate::versions::Source,
    pub installed: String,
    pub latest: String,
    pub error: String,
}

/// Target kinds.
pub const CONTAINER: &str = "container";
pub const UNRAID: &str = "unraid";
pub const COMPOSE: &str = "compose";
pub const SCRIPT: &str = "script";

/// A target as the inventory describes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Planned {
    pub kind: &'static str,
    pub name: String,
    pub working_dir: String,
    pub files_reachable: bool,
    pub source_url: String,
    /// JSON of [`crate::versions::Source`] ('' without one).
    pub version_source: String,
    pub installed_version: String,
    /// What the agent's `latest_command` printed.
    pub reported_latest: String,
    pub version_error: String,
    pub images: Vec<PlannedImage>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlannedImage {
    pub container: String,
    pub service: String,
    pub image: String,
    pub state: String,
    pub local_digest: String,
    /// Every repo digest for the image's repository, space separated.
    pub local_digests: String,
}

pub fn clip(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

/// Hard limits of an inventory. Over any of them it is refused as a whole
/// (not cut down): a partial inventory would delete the targets it cut.
pub const MAX_CONTAINERS: usize = 2000;
pub const MAX_TARGETS: usize = 2000;
pub const MAX_IMAGES_PER_TARGET: usize = 50;
pub const MAX_LABELS: usize = 100;
/// Entries of the smaller lists (templates, projects, stacks, binds,
/// repo digests per container).
pub const MAX_LIST: usize = 2000;
pub const MAX_REPO_DIGESTS: usize = 100;
pub const MAX_BINDS: usize = 200;
/// Longest single string (names, paths, label keys, states).
pub const MAX_STRING: usize = 4096;
/// Longest label value.
pub const MAX_LABEL_VALUE: usize = 16 << 10;

/// Why an inventory is over the limits, if it is. Image references must fit
/// [`MAX_TEXT`] whole: a clipped one would name another image.
pub fn check_limits(inv: &Inventory) -> Result<(), String> {
    let list = |what: &str, n: usize, max: usize| {
        if n > max {
            Err(format!("too many {what} ({n}; at most {max})"))
        } else {
            Ok(())
        }
    };
    let text = |what: &str, s: &str, max: usize| {
        if s.len() > max {
            Err(format!("a {what} is too long (at most {max} bytes)"))
        } else {
            Ok(())
        }
    };
    list("containers", inv.containers.len(), MAX_CONTAINERS)?;
    list("Unraid templates", inv.unraid_templates.len(), MAX_LIST)?;
    list("Compose projects", inv.compose_projects.len(), MAX_LIST)?;
    list("script stacks", inv.script_stacks.len(), MAX_TARGETS)?;
    for t in &inv.unraid_templates {
        text("template name", t, MAX_STRING)?;
    }
    for p in &inv.compose_projects {
        text("project name", &p.name, MAX_STRING)?;
        text("project folder", &p.working_dir, MAX_STRING)?;
        text("project file list", &p.config_files, MAX_STRING)?;
    }
    for s in &inv.script_stacks {
        text("stack name", &s.name, MAX_STRING)?;
        text("stack folder", &s.working_dir, MAX_STRING)?;
        text("stack project", &s.compose_project, MAX_STRING)?;
    }
    for c in &inv.containers {
        text("container name", &c.name, MAX_STRING)?;
        text("image reference", &c.image, MAX_TEXT)?;
        text("image ID", &c.image_id, MAX_STRING)?;
        text("container state", &c.state, MAX_STRING)?;
        list("labels", c.labels.len(), MAX_LABELS)?;
        list("repo digests", c.repo_digests.len(), MAX_REPO_DIGESTS)?;
        list("bind mounts", c.binds.len(), MAX_BINDS)?;
        for (k, v) in &c.labels {
            text("label name", k, MAX_STRING)?;
            text("label value", v, MAX_LABEL_VALUE)?;
        }
        for d in &c.repo_digests {
            text("repo digest", d, 2 * MAX_TEXT)?;
        }
        for b in &c.binds {
            text("bind mount", b, MAX_STRING)?;
        }
    }
    Ok(())
}

/// Why the targets of an inventory are over the limits, if they are.
pub fn check_targets(planned: &[Planned]) -> Result<(), String> {
    if planned.len() > MAX_TARGETS {
        return Err(format!(
            "too many targets ({}; at most {MAX_TARGETS})",
            planned.len()
        ));
    }
    if let Some(p) = planned
        .iter()
        .find(|p| p.images.len() > MAX_IMAGES_PER_TARGET)
    {
        return Err(format!(
            "{} has {} images (at most {MAX_IMAGES_PER_TARGET} per target)",
            p.name,
            p.images.len()
        ));
    }
    Ok(())
}

/// `<name>-wren-old-<run>`: the container an update renamed for its rollback.
pub fn is_kept_aside(name: &str) -> bool {
    name.rsplit_once("-wren-old-")
        .is_some_and(|(_, run)| !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit()))
}

fn image_of(c: &Container) -> PlannedImage {
    let all = ImageRef::parse(&c.image)
        .map(|r| registry::local_digests(&r, &c.repo_digests))
        .unwrap_or_default();
    // A digest is 71 characters: a handful fit in MAX_TEXT whole.
    let mut kept = Vec::new();
    for d in all.iter().filter(|d| !d.contains(char::is_whitespace)) {
        if kept.iter().map(|k: &String| k.len() + 1).sum::<usize>() + d.len() > MAX_TEXT {
            break;
        }
        kept.push(d.clone());
    }
    PlannedImage {
        container: clip(&c.name),
        service: clip(c.labels.get(SERVICE_LABEL).map_or("", String::as_str)),
        image: clip(&c.image),
        state: clip(&c.state),
        local_digest: clip(all.first().map_or("", String::as_str)),
        local_digests: kept.join(" "),
    }
}

/// The source label of the first image that has one.
fn source_of<'a>(containers: impl Iterator<Item = &'a Container>) -> String {
    containers
        .filter_map(|c| c.labels.get(SOURCE_LABEL))
        .map(|s| clip(s))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

/// The targets an inventory describes, sorted by kind and name.
pub fn plan(inv: &Inventory) -> Vec<Planned> {
    let templates: HashSet<&str> = inv.unraid_templates.iter().map(String::as_str).collect();
    let projects: BTreeMap<&str, &ComposeProject> = inv
        .compose_projects
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    // Compose projects claimed by a script stack.
    let claimed: HashSet<&str> = inv
        .script_stacks
        .iter()
        .map(|s| s.compose_project.as_str())
        .collect();
    let mut by_project: BTreeMap<&str, Vec<&Container>> = BTreeMap::new();
    let mut out = Vec::new();
    for c in &inv.containers {
        // The old container an update keeps aside for its rollback.
        if c.name.trim().is_empty() || is_kept_aside(&c.name) {
            continue;
        }
        match c.labels.get(PROJECT_LABEL).filter(|p| !p.is_empty()) {
            Some(project) => by_project.entry(project.as_str()).or_default().push(c),
            None => out.push(Planned {
                kind: if templates.contains(c.name.as_str()) {
                    UNRAID
                } else {
                    CONTAINER
                },
                name: clip(&c.name),
                files_reachable: true,
                source_url: source_of(std::iter::once(c)),
                images: vec![image_of(c)],
                ..Planned::default()
            }),
        }
    }
    for (project, containers) in &by_project {
        if claimed.contains(project) {
            continue;
        }
        let (working_dir, reachable) = match projects.get(project) {
            Some(p) => (clip(&p.working_dir), p.files_reachable),
            None => (
                clip(
                    containers
                        .iter()
                        .find_map(|c| c.labels.get(WORKING_DIR_LABEL))
                        .map_or("", String::as_str),
                ),
                false,
            ),
        };
        out.push(Planned {
            kind: COMPOSE,
            name: clip(project),
            working_dir,
            files_reachable: reachable,
            source_url: source_of(containers.iter().copied()),
            images: containers.iter().map(|c| image_of(c)).collect(),
            ..Planned::default()
        });
    }
    for s in &inv.script_stacks {
        if s.name.trim().is_empty() {
            continue;
        }
        let containers = by_project
            .get(s.compose_project.as_str())
            .cloned()
            .unwrap_or_default();
        let version = s.version.clone().unwrap_or_default();
        out.push(Planned {
            kind: SCRIPT,
            name: clip(&s.name),
            working_dir: clip(&s.working_dir),
            files_reachable: s.files_reachable,
            source_url: source_of(containers.iter().copied()),
            version_source: if s.version.is_some() {
                serde_json::to_string(&version.source).unwrap_or_default()
            } else {
                String::new()
            },
            installed_version: clip(&version.installed),
            reported_latest: clip(&version.latest),
            version_error: clip(&version.error),
            images: containers.iter().map(|c| image_of(c)).collect(),
        });
    }
    out.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    out
}

/// `POST /api/agent/v1/inventory`.
pub async fn receive(
    Extension(wren): Extension<Wren>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (id, _name) = match crate::agent_api::authenticate(&wren, &headers).await {
        Ok(found) => found,
        Err(err) => return err.into_response(),
    };
    if let Err(wait) = wren.inventory_budgets.take(&id, RATE, Instant::now()) {
        tracing::warn!(server_id = id, "agent over its inventory budget");
        return too_many_requests(wait);
    }
    if body.len() > MAX_BYTES {
        return ApiError::bad_request("the inventory is too large").into_response();
    }
    let inv: Inventory = match serde_json::from_slice(&body) {
        Ok(inv) => inv,
        Err(err) => {
            return ApiError::bad_request(format!("not an inventory: {err}")).into_response();
        }
    };
    if let Err(why) = check_limits(&inv) {
        tracing::warn!(server_id = id, %why, "inventory refused");
        return ApiError::bad_request(format!("the inventory is over Wren's limits: {why}"))
            .into_response();
    }
    let planned = plan(&inv);
    if let Err(why) = check_targets(&planned) {
        tracing::warn!(server_id = id, %why, "inventory refused");
        return ApiError::bad_request(format!("the inventory is over Wren's limits: {why}"))
            .into_response();
    }
    let error = clip(&inv.error);
    let count = planned.len();
    let (policy, folders) = policy_and_folders(&inv);
    if let Err(err) = wren
        .core
        .with_conn(move |c| {
            crate::targets::apply(c, id, &planned, &error)?;
            c.execute(
                "UPDATE servers SET policy = ?2, folders = ?3 WHERE id = ?1",
                rusqlite::params![id, policy, folders],
            )?;
            Ok(())
        })
        .await
    {
        return err.into_response();
    }
    tracing::info!(server_id = id, targets = count, "inventory received");
    let _ = wren.changes.send(());
    wren.checks.notify_one();
    Json(json!({ "targets": count })).into_response()
}

/// The agent's policy as stored (`None` from an agent before 0.4.0) and
/// the roots to suggest from the folders the inventory showed, as JSON.
pub fn policy_and_folders(inv: &Inventory) -> (Option<String>, String) {
    let policy = inv.policy.as_ref().map(|p| {
        let clean =
            |list: &[String]| -> Vec<String> { list.iter().take(200).map(|s| clip(s)).collect() };
        let p = crate::policy::AgentPolicy {
            allow: p.allow.as_deref().map(clean),
            deny: clean(&p.deny),
            roots: clean(&p.roots),
        };
        serde_json::to_string(&p).unwrap_or_default()
    });
    let seen = inv
        .containers
        .iter()
        .flat_map(|c| c.binds.iter())
        .chain(inv.compose_projects.iter().map(|p| &p.working_dir))
        .chain(inv.script_stacks.iter().map(|s| &s.working_dir))
        .map(String::as_str);
    let folders: Vec<String> = crate::policy::suggest_roots(seen)
        .into_iter()
        .take(50)
        .map(|f| clip(&f))
        .collect();
    (
        policy,
        serde_json::to_string(&folders).unwrap_or_else(|_| "[]".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str, image: &str, labels: &[(&str, &str)]) -> Container {
        Container {
            name: name.into(),
            image: image.into(),
            image_id: "sha256:abc".into(),
            repo_digests: vec![format!(
                "{}@sha256:{name}",
                image.split(':').next().unwrap()
            )],
            state: "running".into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            binds: Vec::new(),
        }
    }

    #[test]
    fn containers_are_grouped_into_targets() {
        let inv = Inventory {
            containers: vec![
                container(
                    "Newt",
                    "fosrl/newt",
                    &[(SOURCE_LABEL, "https://github.com/fosrl/newt")],
                ),
                container("solo", "redis:7.2", &[]),
                container(
                    "postiz",
                    "ghcr.io/gitroomhq/postiz-app:latest",
                    &[(PROJECT_LABEL, "postiz"), (SERVICE_LABEL, "postiz")],
                ),
                container(
                    "postiz-postgres",
                    "postgres:17-alpine",
                    &[(PROJECT_LABEL, "postiz")],
                ),
                container(
                    "ww-test-stack-web-1",
                    "traefik/whoami:v1.10.3",
                    &[(PROJECT_LABEL, "ww-test-stack")],
                ),
            ],
            unraid_templates: vec!["Newt".into(), "Gone".into()],
            compose_projects: vec![ComposeProject {
                name: "postiz".into(),
                working_dir: "/boot/config/plugins/compose.manager/projects/postiz".into(),
                config_files: "compose.yaml".into(),
                files_reachable: true,
            }],
            script_stacks: vec![ScriptStack {
                name: "ww-test-stack".into(),
                compose_project: "ww-test-stack".into(),
                working_dir: "/mnt/user/appdata/ww-test-stack".into(),
                files_reachable: true,
                version: Some(Version {
                    installed: "v1.10.3".into(),
                    ..Version::default()
                }),
            }],
            error: String::new(),
            policy: None,
        };
        let p = plan(&inv);
        let kinds: Vec<(&str, &str)> = p.iter().map(|t| (t.kind, t.name.as_str())).collect();
        assert_eq!(
            kinds,
            [
                (COMPOSE, "postiz"),
                (CONTAINER, "solo"),
                (SCRIPT, "ww-test-stack"),
                (UNRAID, "Newt"),
            ]
        );
        assert_eq!(p[0].images.len(), 2);
        assert!(p[0].files_reachable);
        assert_eq!(p[0].images[0].service, "postiz");
        assert_eq!(p[0].images[1].local_digest, "sha256:postiz-postgres");
        assert_eq!(p[2].images.len(), 1);
        assert_eq!(p[2].installed_version, "v1.10.3");
        assert_eq!(p[3].source_url, "https://github.com/fosrl/newt");
    }

    #[test]
    fn inventories_over_the_limits_are_refused() {
        let ok = |inv: &Inventory| check_limits(inv).and_then(|()| check_targets(&plan(inv)));
        let with = |containers: Vec<Container>| Inventory {
            containers,
            ..Inventory::default()
        };
        let one = container("a", "redis:7", &[]);
        assert!(ok(&with(vec![one.clone()])).is_ok());

        let many = |n: usize| -> Vec<Container> {
            (0..n)
                .map(|i| container(&format!("c{i}"), "redis:7", &[]))
                .collect()
        };
        assert!(check_limits(&with(many(MAX_CONTAINERS))).is_ok());
        let err = check_limits(&with(many(MAX_CONTAINERS + 1))).unwrap_err();
        assert!(err.contains("containers"), "{err}");

        // Image references are refused, not clipped.
        let mut long = one.clone();
        long.image = format!("registry.example/{}", "a".repeat(MAX_TEXT));
        assert!(check_limits(&with(vec![long])).is_err());
        let mut edge = one.clone();
        edge.image = "a".repeat(MAX_TEXT);
        assert!(check_limits(&with(vec![edge])).is_ok());

        let mut labelled = one.clone();
        labelled.labels = (0..=MAX_LABELS)
            .map(|i| (format!("l{i}"), String::new()))
            .collect();
        let err = check_limits(&with(vec![labelled])).unwrap_err();
        assert!(err.contains("labels"), "{err}");
        let mut fat = one.clone();
        fat.labels
            .insert("k".into(), "v".repeat(MAX_LABEL_VALUE + 1));
        assert!(check_limits(&with(vec![fat])).is_err());
        let mut digests = one.clone();
        digests.repo_digests = vec!["redis@sha256:1".into(); MAX_REPO_DIGESTS + 1];
        assert!(check_limits(&with(vec![digests])).is_err());

        // 51 containers in one Compose project are 51 images in one target.
        let project = (0..=MAX_IMAGES_PER_TARGET)
            .map(|i| container(&format!("p{i}"), "redis:7", &[(PROJECT_LABEL, "big")]))
            .collect();
        let err = ok(&with(project)).unwrap_err();
        assert!(err.contains("big"), "{err}");

        let stacks = (0..=MAX_TARGETS)
            .map(|i| ScriptStack {
                name: format!("s{i}"),
                ..ScriptStack::default()
            })
            .collect();
        let inv = Inventory {
            script_stacks: stacks,
            ..Inventory::default()
        };
        assert!(check_limits(&inv).is_err());
    }

    #[test]
    fn projects_without_a_report_are_not_reachable() {
        let inv = Inventory {
            containers: vec![container(
                "a",
                "redis:7",
                &[(PROJECT_LABEL, "x"), (WORKING_DIR_LABEL, "/opt/x")],
            )],
            ..Inventory::default()
        };
        let p = plan(&inv);
        assert_eq!(p[0].working_dir, "/opt/x");
        assert!(!p[0].files_reachable);
    }
}
