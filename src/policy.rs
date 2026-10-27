//! What an agent allows, as its inventory reports it (0.4.0+): its
//! `[updates]` allow and deny globs and its `roots`. Wren uses it to tell,
//! before anyone approves, what the agent would refuse ([`blocked`]), so an
//! approval it can't run isn't offered. The agent still decides last (§8):
//! this only saves people from approving what it will refuse.
//!
//! An agent before 0.4.0 reports no policy: then nothing is blocked by it.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vole_kit::i18n;

use crate::inventory::{COMPOSE, SCRIPT};

/// Where Unraid's Compose Manager plugin keeps its stacks.
pub const COMPOSE_MANAGER: &str = "/boot/config/plugins/compose.manager/projects";

/// The parts of an agent's `agent.toml` it reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentPolicy {
    /// `None`: no allow list, every name is allowed.
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
    pub roots: Vec<String>,
}

impl AgentPolicy {
    /// From the JSON in `servers.policy` (`None` for NULL or nonsense).
    pub fn parse(raw: Option<&str>) -> Option<AgentPolicy> {
        serde_json::from_str(raw?).ok()
    }

    /// Whether the agent would update a target with this name (deny wins),
    /// as the agent's own `[updates]` check does.
    pub fn allows(&self, name: &str) -> bool {
        if self.deny.iter().any(|g| glob(g, name)) {
            return false;
        }
        self.allow
            .as_ref()
            .is_none_or(|allow| allow.iter().any(|g| glob(g, name)))
    }

    /// Whether `folder` lies under one of the roots (whole components).
    pub fn under_roots(&self, folder: &str) -> bool {
        let p = Path::new(folder);
        p.is_absolute()
            && !folder.split('/').any(|c| c == "..")
            && self.roots.iter().any(|r| p.starts_with(r))
    }
}

/// `*` (any run of characters) and `?` (one character), the rest literal:
/// the agent's own glob.
pub fn glob(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Why the agent would refuse to update a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// Outside `[updates] allow`, or inside `deny`.
    NotAllowed,
    /// agent.toml has no `[updates] allow` list: the agent refuses every update.
    NoUpdates,
    /// A stack whose folder isn't under the agent's roots.
    NotUnderRoots(String),
    /// A stack whose folder isn't mounted in the agent.
    FilesNotReachable,
}

impl Blocked {
    /// In the reader's language.
    pub fn message(&self) -> String {
        match self {
            Blocked::NotAllowed => i18n::t("blocked.not_allowed").to_string(),
            Blocked::NoUpdates => i18n::t("blocked.no_updates").to_string(),
            Blocked::NotUnderRoots(f) => i18n::tf("blocked.roots", &[("folder", f)]),
            Blocked::FilesNotReachable => i18n::t("targets.files_missing").to_string(),
        }
    }

    /// For the API and MCP (English, like their other texts).
    pub fn english(&self) -> String {
        match self {
            Blocked::NotAllowed => "not allowed by [updates] in agent.toml".to_string(),
            Blocked::NoUpdates => "agent.toml has no [updates] allow list".to_string(),
            Blocked::NotUnderRoots(f) => format!("folder {f} not under the agent's roots"),
            Blocked::FilesNotReachable => "files not reachable".to_string(),
        }
    }
}

/// What the agent would refuse about a target (`policy`: `None` for an agent
/// that doesn't report one; then only unreachable stack files count, which
/// every agent reports).
pub fn blocked(
    policy: Option<&AgentPolicy>,
    kind: &str,
    name: &str,
    working_dir: &str,
    files_reachable: bool,
) -> Option<Blocked> {
    let stack = kind == COMPOSE || kind == SCRIPT;
    if let Some(p) = policy {
        if !p.allows(name) {
            // An agent from 0.7.0 on reports an empty list when agent.toml
            // has none: it refuses everything until one is added.
            return Some(if p.allow.as_ref().is_some_and(Vec::is_empty) {
                Blocked::NoUpdates
            } else {
                Blocked::NotAllowed
            });
        }
        if stack && !p.under_roots(working_dir) {
            return Some(Blocked::NotUnderRoots(working_dir.to_string()));
        }
    }
    (stack && !files_reachable).then_some(Blocked::FilesNotReachable)
}

/// Folders a container or stack keeps its files in that are never data.
fn system(path: &str) -> bool {
    [
        "/etc",
        "/dev",
        "/sys",
        "/proc",
        "/run",
        "/var/run",
        "/usr",
        "/lib",
        "/bin",
        "/sbin",
        "/tmp",
        "/var/lib/docker",
        "/var/local",
        "/boot",
    ]
    .iter()
    .any(|s| Path::new(path).starts_with(s))
}

/// `roots` to suggest from the host folders an inventory showed (bind
/// mounts, Compose folders): Compose Manager's projects folder, an Unraid
/// share or pool folder (`/mnt/<pool>/<share>`), else the first two parts
/// of the path (`/srv/apps`). System folders are left out.
pub fn suggest_roots<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out = BTreeSet::new();
    for p in paths {
        let p = p.trim_end_matches('/');
        if !p.starts_with('/') || p.split('/').any(|c| c == "..") {
            continue;
        }
        if Path::new(p).starts_with(COMPOSE_MANAGER) {
            out.insert(COMPOSE_MANAGER.to_string());
            continue;
        }
        if system(p) {
            continue;
        }
        let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
        let keep = match parts.first() {
            Some(&"mnt") => 3,
            _ => 2,
        };
        if parts.len() >= keep {
            out.insert(format!("/{}", parts[..keep].join("/")));
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tower() -> AgentPolicy {
        AgentPolicy {
            allow: Some(vec!["fluxer".into()]),
            deny: Vec::new(),
            roots: vec!["/mnt/cache/appdata".into(), "/mnt/user/appdata".into()],
        }
    }

    #[test]
    fn what_the_agent_would_refuse() {
        let p = tower();
        assert_eq!(
            blocked(Some(&p), "container", "sonarr", "", true),
            Some(Blocked::NotAllowed)
        );
        assert_eq!(
            blocked(
                Some(&p),
                "script",
                "fluxer",
                "/mnt/cache/appdata/fluxer",
                true
            ),
            None
        );
        let all = AgentPolicy {
            allow: None,
            ..tower()
        };
        assert_eq!(
            blocked(
                Some(&all),
                "compose",
                "sylo-docs",
                "/boot/config/plugins/compose.manager/projects/sylo-docs",
                true
            ),
            Some(Blocked::NotUnderRoots(
                "/boot/config/plugins/compose.manager/projects/sylo-docs".into()
            ))
        );
        assert_eq!(
            blocked(Some(&all), "compose", "x", "/mnt/user/appdata/x", false),
            Some(Blocked::FilesNotReachable)
        );
        // An older agent: only what every agent reports counts.
        assert_eq!(blocked(None, "container", "sonarr", "", true), None);
        assert_eq!(
            blocked(None, "compose", "x", "/opt/x", false),
            Some(Blocked::FilesNotReachable)
        );
        // Deny wins; globs as the agent reads them.
        let p = AgentPolicy {
            allow: Some(vec!["*".into()]),
            deny: vec!["immich*".into()],
            roots: Vec::new(),
        };
        assert!(p.allows("sonarr") && !p.allows("immich-server"));
        assert!(!p.under_roots("/mnt/user/appdata/../x"));
        assert_eq!(
            Blocked::NotUnderRoots("/x".into()).english(),
            "folder /x not under the agent's roots"
        );
    }

    #[test]
    fn roots_suggested_from_the_folders_seen() {
        let seen = [
            "/mnt/user/appdata/sonarr",
            "/mnt/user/plex_data",
            "/mnt/cache/appdata/immich/db",
            "/boot/config/plugins/compose.manager/projects/sylo-docs",
            "/etc/localtime",
            "/var/run/docker.sock",
            "/srv/apps/x/data",
            "relative",
        ];
        assert_eq!(
            suggest_roots(seen.into_iter()),
            [
                COMPOSE_MANAGER,
                "/mnt/cache/appdata",
                "/mnt/user/appdata",
                "/mnt/user/plex_data",
                "/srv/apps"
            ]
        );
    }
}
