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
    let version: String = c
        .query_row(
            "SELECT agent_version FROM servers WHERE id = ?1",
            [server_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let report = report_of(c, server_id)?;
    let active = active_run(c, server_id)?;
    Ok(standing(&version, report.as_ref(), release, active))
}

/// The agent release Wren offers right now.
pub fn release(wren: &Wren) -> Option<Release> {
    wren.agent_release.read().ok().and_then(|r| r.clone())
}

/// Looks up the digest of the agent image at Wren's own version, and whether
/// GitHub knows a newer Wren. A lookup that fails leaves what was known.
pub async fn refresh_release(wren: &Wren) {
    let version = crate::VERSION;
    let image = format!("{AGENT_REPO}:{version}");
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
