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
}

pub fn clip(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

/// `<name>-wren-old-<run>`: the container an update renamed for its rollback.
pub fn is_kept_aside(name: &str) -> bool {
    name.rsplit_once("-wren-old-")
        .is_some_and(|(_, run)| !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit()))
}

fn image_of(c: &Container) -> PlannedImage {
    let local = ImageRef::parse(&c.image)
        .ok()
        .and_then(|r| registry::local_digest(&r, &c.repo_digests))
        .unwrap_or_default();
    PlannedImage {
        container: clip(&c.name),
        service: clip(c.labels.get(SERVICE_LABEL).map_or("", String::as_str)),
        image: clip(&c.image),
        state: clip(&c.state),
        local_digest: clip(&local),
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
    let planned = plan(&inv);
    let error = clip(&inv.error);
    let count = planned.len();
    if let Err(err) = wren
        .core
        .with_conn(move |c| crate::targets::apply(c, id, &planned, &error))
        .await
    {
        return err.into_response();
    }
    tracing::info!(server_id = id, targets = count, "inventory received");
    let _ = wren.changes.send(());
    wren.checks.notify_one();
    Json(json!({ "targets": count })).into_response()
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
