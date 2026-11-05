//! The agent's self-update, seen from the server: what the agent's last
//! heartbeat said about it ([`Report`]), the agent release this Wren offers
//! ([`Release`]: Wren's own version, as the agent image's tag of that
//! version) and what an admin may do about it right now ([`standing`]).
//!
//! The target version is always Wren's own `CARGO_PKG_VERSION`: agent and
//! server are released together, so an agent is never offered a version the
//! server doesn't know.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::Wren;
use crate::registry::{Checker, ImageRef};
use crate::runs::{self, Refusal, Run};
use crate::versions::semver_of;

pub use crate::agent_api::AGENT_REPO;

/// `runs.kind` of an agent update.
pub const KIND: &str = "agent_update";
/// `runs.target_kind` of an agent update.
pub const TARGET_KIND: &str = "agent";
/// The first agent version that can update itself.
pub const AGENT_SELF_UPDATE_FLOOR: &str = "0.10.0";
/// An agent update run that hears nothing from its agent for this long is
/// closed (the helper's longest wait plus a margin).
pub const SILENT_MINUTES: i64 = 45;
/// How often Wren looks up its own agent release again.
pub const REFRESH: Duration = Duration::from_secs(6 * 3600);
/// How soon the first look is repeated while it hasn't worked.
const RETRY: Duration = Duration::from_secs(10 * 60);
/// Where Wren's own releases are published.
const WREN_REPO: &str = "Ferdinand99/Wise-Wren";
/// Longest text kept of what the agent reports (characters).
const MAX_TEXT: usize = 200;

/// The self-update part of an agent's heartbeat (agents from 0.10). Unknown
/// fields are ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// The agent knows about self-updates.
    pub supported: bool,
    /// The agent's own switch (`[self_update] enabled`) is on.
    pub enabled: bool,
    /// Why the agent can't update itself, as a code; empty when it can.
    pub blocked: String,
    /// The agent's own container.
    pub container: String,
    /// The image reference that container was created from.
    pub reference: String,
    /// An update the agent is still working on or confirming.
    pub pending: Option<Pending>,
}

/// A self-update in the agent's state file that hasn't been reported yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pending {
    pub run_id: i64,
    pub phase: String,
    pub to_version: String,
}

fn clip(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

impl Report {
    /// The report with every text cut to size and free of control characters.
    pub fn clean(mut self) -> Report {
        self.blocked = clip(&self.blocked);
        self.container = clip(&self.container);
        self.reference = clip(&self.reference);
        if let Some(p) = &mut self.pending {
            p.phase = clip(&p.phase);
            p.to_version = clip(&p.to_version);
        }
        self
    }
}

/// The agent release Wren offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// Wren's own version: the one an agent is updated to.
    pub version: String,
    /// The digest of the agent image at that version's tag.
    pub digest: String,
    /// A newer Wren release than this one, when GitHub names one (the agent
    /// can't be offered it before Wren itself is updated).
    pub newest_wren: Option<String>,
}

/// Where an agent stands, for the card, the API and the approval check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// A newer agent is available and the agent can take it.
    Available,
    /// The agent runs the version Wren offers (or a newer one).
    Current,
    /// An agent update is queued or running.
    Running,
    /// The agent's own switch is off.
    Off,
    /// The agent can't update itself; `reason` says why.
    Blocked,
    /// The agent is older than the first one that can update itself.
    BelowFloor,
    /// The agent doesn't report self-update (an older one).
    Unsupported,
    /// Not known: no version, no release or no heartbeat yet.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Standing {
    pub state: State,
    /// The version the agent runs.
    pub current: String,
    /// The version it can be updated to (state `available`).
    pub available: Option<String>,
    /// For `blocked`, the agent's code; for `unknown`, why (`release_unknown`).
    pub reason: String,
    /// The queued or running agent update.
    pub run_id: Option<i64>,
}

/// What an agent at `agent_version` may do. `active` is a queued or running
/// agent update's run id.
pub fn standing(
    agent_version: &str,
    report: Option<&Report>,
    release: Option<&Release>,
    active: Option<i64>,
) -> Standing {
    let make =
        |state: State, available: Option<String>, reason: &str, run_id: Option<i64>| Standing {
            state,
            current: agent_version.to_string(),
            available,
            reason: reason.to_string(),
            run_id,
        };
    if let Some(run) = active {
        return make(State::Running, None, "", Some(run));
    }
    let Some(current) = semver_of(agent_version).filter(|_| !agent_version.trim().is_empty())
    else {
        return make(State::Unknown, None, "", None);
    };
    let Some(report) = report.filter(|r| r.supported) else {
        return make(State::Unsupported, None, "", None);
    };
    if !report.enabled {
        return make(State::Off, None, "", None);
    }
    if !report.blocked.is_empty() {
        return make(State::Blocked, None, &report.blocked, None);
    }
    let Some(release) = release else {
        return make(State::Unknown, None, "release_unknown", None);
    };
    if semver_of(AGENT_SELF_UPDATE_FLOOR).is_some_and(|floor| current < floor) {
        return make(State::BelowFloor, None, "", None);
    }
    match semver_of(&release.version) {
        Some(target) if current < target => {
            make(State::Available, Some(release.version.clone()), "", None)
        }
        Some(_) => make(State::Current, None, "", None),
        None => make(State::Unknown, None, "release_unknown", None),
    }
}

/// Keeps the self-update part of an agent's heartbeat; `None` (an older
/// agent, or a report that isn't one) clears it.
pub fn store_report(
    c: &Connection,
    server_id: i64,
    report: Option<Report>,
) -> rusqlite::Result<()> {
    let json = report
        .filter(|r| r.supported)
        .map(|r| serde_json::to_string(&r.clean()).unwrap_or_default());
    c.execute(
        "UPDATE servers SET self_update_report = ?2 WHERE id = ?1",
        params![server_id, json],
    )?;
    Ok(())
}

/// What the agent's last heartbeat said, if it said anything.
pub fn report_of(c: &Connection, server_id: i64) -> rusqlite::Result<Option<Report>> {
    let raw: Option<String> = c
        .query_row(
            "SELECT self_update_report FROM servers WHERE id = ?1",
            [server_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
}

fn run_with(c: &Connection, server_id: i64, statuses: &str) -> rusqlite::Result<Option<i64>> {
    c.query_row(
        &format!(
            "SELECT id FROM runs WHERE server_id = ?1 AND kind = ?2 AND status IN ({statuses})
             ORDER BY id DESC LIMIT 1"
        ),
        params![server_id, KIND],
        |r| r.get(0),
    )
    .optional()
}

/// The server's agent update that is queued or running.
pub fn active_run(c: &Connection, server_id: i64) -> rusqlite::Result<Option<i64>> {
    run_with(c, server_id, "'queued', 'running'")
}

/// The server's agent update that is running (the agent took it).
pub fn running_run(c: &Connection, server_id: i64) -> rusqlite::Result<Option<i64>> {
    run_with(c, server_id, "'running'")
}

/// [`standing`] from what the database holds about `server_id`.
pub fn standing_of(
    c: &Connection,
    server_id: i64,
    release: Option<&Release>,
) -> rusqlite::Result<Standing> {
    let active = active_run(c, server_id)?;
    standing_with(c, server_id, release, active)
}

/// [`standing_of`] for a given queued or running update (`None`: as if there
/// were none).
fn standing_with(
    c: &Connection,
    server_id: i64,
    release: Option<&Release>,
    active: Option<i64>,
) -> rusqlite::Result<Standing> {
    let version: String = c
        .query_row(
            "SELECT agent_version FROM servers WHERE id = ?1",
            [server_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let report = report_of(c, server_id)?;
    Ok(standing(&version, report.as_ref(), release, active))
}

/// The `to_key` of an update to `release`: the version and the digest the
/// agent is told to pull.
fn key_of(release: &Release) -> String {
    format!("{}@{}", release.version, release.digest)
}

/// The target of this server named `container` that is the agent's own
/// container, when the inventory has it (the run then links to it).
pub fn own_target(
    c: &Connection,
    server_id: i64,
    container: &str,
) -> rusqlite::Result<Option<i64>> {
    c.query_row(
        "SELECT id FROM targets WHERE server_id = ?1 AND name = ?2
             AND kind IN ('container', 'unraid') ORDER BY id LIMIT 1",
        params![server_id, container],
        |r| r.get(0),
    )
    .optional()
}

/// Approves the update of `server_id`'s agent to `version` now, or at `at`.
/// Only an admin may; `version` must be the one the person saw. The run's id.
pub fn approve(
    c: &Connection,
    server_id: i64,
    version: &str,
    user: &str,
    is_admin: bool,
    at: Option<jiff::Timestamp>,
    release: Option<&Release>,
) -> rusqlite::Result<Result<i64, Refusal>> {
    if !is_admin {
        return Ok(Err(Refusal::AdminOnly));
    }
    let known: bool = c.query_row(
        "SELECT EXISTS (SELECT 1 FROM servers WHERE id = ?1)",
        [server_id],
        |r| r.get(0),
    )?;
    if !known {
        return Ok(Err(Refusal::NotFound));
    }
    if at.is_some_and(|t| !runs::time_ok(t, jiff::Timestamp::now())) {
        return Ok(Err(Refusal::BadTime));
    }
    let standing = standing_of(c, server_id, release)?;
    let release = match (standing.state, release) {
        (State::Running, _) => return Ok(Err(Refusal::AlreadyQueued)),
        (State::Available, Some(release)) => release,
        _ => return Ok(Err(Refusal::NoAgentUpdate)),
    };
    if version != release.version {
        return Ok(Err(Refusal::Changed));
    }
    let container = report_of(c, server_id)?
        .map(|r| r.container)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "wren-agent".to_string());
    let target_id = own_target(c, server_id, &container)?;
    let at = at.map(crate::schedule::db_time);
    let trigger = if at.is_some() {
        runs::BY_SCHEDULE
    } else {
        runs::BY_USER
    };
    let to_key = key_of(release);
    c.execute(
        "INSERT INTO runs (server_id, target_id, target_kind, target_name, kind, from_key,
             to_key, shown, requested_by, trigger, scheduled_for)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            server_id,
            target_id,
            TARGET_KIND,
            container,
            KIND,
            standing.current,
            to_key,
            format!("{} \u{2192} {}", standing.current, release.version),
            user,
            trigger,
            at
        ],
    )?;
    let id = c.last_insert_rowid();
    let detail = match &at {
        Some(at) => format!("{to_key} at {at} UTC"),
        None => to_key,
    };
    runs::log_action(c, user, KIND, target_id, Some(id), &detail)?;
    Ok(Ok(id))
}

/// Whether the queued update `run` may still start: its agent can still take
/// the update, and the release Wren offers now is the one that was approved.
/// The reason it can't.
pub fn still_ok(
    c: &Connection,
    run: &Run,
    release: Option<&Release>,
) -> rusqlite::Result<Result<(), String>> {
    let standing = standing_with(c, run.server_id, release, None)?;
    let (State::Available, Some(release)) = (standing.state, release) else {
        let state = serde_json::to_value(standing.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let why = if standing.reason.is_empty() {
            state
        } else {
            format!("{state}, {}", standing.reason)
        };
        return Ok(Err(format!("the agent can no longer be updated: {why}")));
    };
    if run.to_key != key_of(release) {
        return Ok(Err(
            "Wren's agent release changed before the run started; approve it again".to_string(),
        ));
    }
    Ok(Ok(()))
}

/// The job the agent gets for `run`: the version and digest to move to, and
/// nothing else (the agent decides the rest from its own state).
pub fn job_json(run: &Run) -> serde_json::Value {
    let (version, digest) = run.to_key.split_once('@').unwrap_or((&run.to_key, ""));
    serde_json::json!({
        "run_id": run.id,
        "kind": KIND,
        "target": { "kind": TARGET_KIND, "name": run.target_name },
        "from_key": run.from_key,
        "to_key": run.to_key,
        "to_version": version,
        "to_digest": digest,
    })
}

/// The agent release Wren offers right now.
pub fn release(wren: &Wren) -> Option<Release> {
    wren.agent_release.read().ok().and_then(|r| r.clone())
}

/// Looks up the digest of the agent image at Wren's own version, and whether
/// GitHub knows a newer Wren. A lookup that fails leaves what was known.
pub async fn refresh_release(wren: &Wren) {
    refresh_release_of(wren, AGENT_REPO, crate::VERSION).await;
}

/// [`refresh_release`] for the agent image `repo` at tag `version` (the
/// Docker tests look up their own registry).
///
/// The digest is the one the registry names for that tag: the index of a
/// multi-arch image, which is what `docker pull repo:version` puts in the
/// image's `RepoDigests` and what the agent checks for.
pub async fn refresh_release_of(wren: &Wren, repo: &str, version: &str) {
    let image = format!("{repo}:{version}");
    let Ok(parsed) = ImageRef::parse(&image) else {
        return;
    };
    let creds = crate::registries::credentials(&wren.core).await;
    let mut checker = Checker::new(creds, wren.fetch_allow());
    let digest = match checker.digest(&parsed).await {
        Ok(remote) => remote.digest,
        Err(err) => {
            tracing::warn!(%err, image, "can't look up the agent release");
            return;
        }
    };
    let newest_wren = match wren.github.latest_release(&wren.core.http, WREN_REPO).await {
        Ok(latest) => match (semver_of(&latest.version), semver_of(version)) {
            (Some(found), Some(own)) if found > own => Some(latest.version),
            _ => None,
        },
        Err(_) => None,
    };
    if let Ok(mut slot) = wren.agent_release.write() {
        *slot = Some(Release {
            version: version.to_string(),
            digest,
            newest_wren,
        });
    }
    let _ = wren.changes.send(());
}

/// Tells everyone once that the agent of `server_id` can be updated to the
/// release Wren offers. Runs after every heartbeat; the version is marked
/// (compare-and-set) before anything is sent, so two at once send one
/// message and a version is never announced twice.
pub async fn announce(wren: Wren, server_id: i64) {
    let release = release(&wren);
    let found = wren
        .core
        .with_conn(move |c| {
            let standing = standing_of(c, server_id, release.as_ref())?;
            let (State::Available, Some(release)) = (standing.state, release) else {
                return Ok(None);
            };
            let changed = c.execute(
                "UPDATE servers SET agent_update_announced = ?2
                 WHERE id = ?1 AND agent_update_announced != ?2",
                params![server_id, release.version],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            let name: String =
                c.query_row("SELECT name FROM servers WHERE id = ?1", [server_id], |r| {
                    r.get(0)
                })?;
            Ok(Some((name, standing.current, release.version)))
        })
        .await;
    let (name, from, to) = match found {
        Ok(Some(found)) => found,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(%err, server_id, "can't work out the agent update notice");
            return;
        }
    };
    crate::run_notify::broadcast(&wren.core, |base| {
        crate::run_notify::agent_update_available_alert(server_id, &name, &from, &to, base)
    })
    .await;
}

/// Looks the release up at start, then every [`REFRESH`] (sooner while the
/// first look hasn't worked).
pub fn spawn_refresher(wren: Wren) {
    tokio::spawn(async move {
        loop {
            refresh_release(&wren).await;
            let wait = if release(&wren).is_some() {
                REFRESH
            } else {
                RETRY
            };
            tokio::time::sleep(wait).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standing_follows_the_rules() {
        let rel = Release {
            version: "0.10.1".into(),
            digest: format!("sha256:{}", "b".repeat(64)),
            newest_wren: None,
        };
        let on = Report {
            supported: true,
            enabled: true,
            container: "wren-agent".into(),
            ..Report::default()
        };
        let s = |v: &str, r: Option<&Report>, rl: Option<&Release>, a: Option<i64>| {
            standing(v, r, rl, a).state
        };
        assert_eq!(s("0.10.0", Some(&on), Some(&rel), None), State::Available);
        assert_eq!(
            standing("0.10.0", Some(&on), Some(&rel), None)
                .available
                .as_deref(),
            Some("0.10.1")
        );
        assert_eq!(s("0.10.1", Some(&on), Some(&rel), None), State::Current);
        assert_eq!(s("0.10.0", Some(&on), Some(&rel), Some(9)), State::Running);
        assert_eq!(s("0.9.2", None, Some(&rel), None), State::Unsupported);
        assert_eq!(
            s(
                "0.10.0",
                Some(&Report {
                    enabled: false,
                    ..on.clone()
                }),
                Some(&rel),
                None
            ),
            State::Off
        );
        let pinned = Report {
            blocked: "pinned".into(),
            ..on.clone()
        };
        assert_eq!(
            standing("0.10.0", Some(&pinned), Some(&rel), None).reason,
            "pinned"
        );
        assert_eq!(s("0.10.0", Some(&on), None, None), State::Unknown);
        assert_eq!(s("0.9.9", Some(&on), Some(&rel), None), State::BelowFloor);
        assert_eq!(s("", None, Some(&rel), None), State::Unknown);
    }

    #[test]
    fn standing_edges() {
        let rel = Release {
            version: "0.10.1".into(),
            digest: String::new(),
            newest_wren: None,
        };
        let on = Report {
            supported: true,
            enabled: true,
            ..Report::default()
        };
        // A newer agent than Wren's release counts as current.
        assert_eq!(
            standing("0.11.0", Some(&on), Some(&rel), None).state,
            State::Current
        );
        // Not a version at all.
        assert_eq!(
            standing("dev", Some(&on), Some(&rel), None).state,
            State::Unknown
        );
        // No release says why.
        assert_eq!(
            standing("0.10.0", Some(&on), None, None).reason,
            "release_unknown"
        );
        // A running update wins over everything, even a missing report.
        let running = standing("", None, None, Some(4));
        assert_eq!((running.state, running.run_id), (State::Running, Some(4)));
        // `current` is always what the agent runs.
        assert_eq!(standing("0.10.0", None, None, None).current, "0.10.0");
    }

    #[test]
    fn a_report_is_cleaned() {
        let long = format!("a\u{7}{}", "x".repeat(300));
        let r = Report {
            supported: true,
            blocked: long.clone(),
            container: long.clone(),
            reference: long.clone(),
            pending: Some(Pending {
                run_id: 3,
                phase: long.clone(),
                to_version: long,
            }),
            ..Report::default()
        }
        .clean();
        for text in [&r.blocked, &r.container, &r.reference] {
            assert_eq!(text.chars().count(), MAX_TEXT);
            assert!(text.starts_with("axx") && !text.chars().any(char::is_control));
        }
        let p = r.pending.unwrap();
        assert_eq!((p.run_id, p.phase.chars().count()), (3, MAX_TEXT));
        assert_eq!(p.to_version.chars().count(), MAX_TEXT);
    }

    #[test]
    fn the_agent_repo_is_the_images() {
        assert_eq!(
            crate::agent_api::AGENT_IMAGE,
            format!("{AGENT_REPO}:latest")
        );
    }
}
