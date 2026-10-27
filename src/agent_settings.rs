//! Agent settings managed in Wren (docs/agent.md, "Managed settings").
//!
//! An admin edits a server's agent settings here: which targets may be
//! updated, the folders (roots), safety minimums, rollback points and script
//! stacks made from the presets built into the agent. Wren stores them with
//! a version that only counts up and sends them in the heartbeat's answer
//! while the agent doesn't report that version. The agent checks them
//! against hard limits of its own, keeps them in `managed.json` and reports
//! the version it runs, or why it refused (§8). Wren never sends a command
//! line: a script stack is a preset's name, a folder, a Compose project and
//! a name.
//!
//! The first time an unlocked agent without managed settings reports in, it
//! sends what its `agent.toml` says; Wren stores that as version 1 and
//! offers the same values back, so nothing changes on the agent.
//!
//! Every change is kept in a history (who, when, what). Changes that widen
//! what the agent may do (a new root, a broader allow list, a removed deny,
//! a new script stack, lower minimums, snapshots no longer required) are
//! marked and told to all notification channels; narrowing ones are only
//! logged.
//!
//! Admins edit; everyone signed in can read the page. An agent locked with
//! `WW_AGENT_LOCKED` or older than 0.8 shows the settings read-only (or not
//! at all) and keeps the [`crate::agent_toml`] helper.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::SignedCookieJar;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use vole_kit::auth::CurrentUser;
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::{self, Tr as _};
use vole_kit::notify::alert::{Alert, AlertKind, Level};
use vole_kit::page::{PageCtx, flash};
use vole_kit::state::Core;
use wren_agent::config::{MAX_RETENTION, valid_glob, valid_name};
use wren_agent::jobs::MAX_HEALTH_SECONDS;
use wren_agent::managed::{
    self, Keep, MAX_GLOBS, MAX_INTERVAL_SECONDS, MAX_ROOTS, MAX_STACKS, MIN_INTERVAL_SECONDS,
    Settings, Stack, Updates,
};

use crate::Wren;
use crate::policy::COMPOSE_MANAGER;
use crate::updates_notify::kind_label;

/// Longest text kept of what an agent reports.
const MAX_TEXT: usize = 600;
/// History rows shown.
const SHOWN: i64 = 25;

pub fn router() -> Router<Core> {
    Router::new().route("/servers/{id}/settings", get(page).post(save))
}

// ---------------------------------------------------------------------------
// What the agent reports
// ---------------------------------------------------------------------------

/// The settings part of a heartbeat (agents from 0.8). Unknown fields are
/// ignored; `import` is read leniently ([`Report::import`]).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Report {
    pub supported: bool,
    pub locked: bool,
    pub version: Option<i64>,
    pub error: String,
    pub error_version: Option<i64>,
    /// The folders mounted into the agent: where roots may be.
    pub mounts: Vec<String>,
    pub presets: Vec<String>,
    /// Script stacks with commands of their own in `agent.toml` (names).
    pub custom_stacks: Vec<String>,
    /// The settings in force, or while there are none what `agent.toml`
    /// says: Wren starts from it when it holds none for the server.
    #[serde(skip_serializing)]
    pub current: Option<serde_json::Value>,
}

fn clip(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

impl Report {
    /// The report with every text cut to size and every list to its limit.
    pub fn clean(mut self) -> Report {
        let list = |v: Vec<String>, n: usize, max: usize| -> Vec<String> {
            v.iter().take(n).map(|s| clip(s, max)).collect()
        };
        self.error = clip(&self.error, MAX_TEXT);
        self.mounts = list(self.mounts, 100, 300);
        self.presets = list(self.presets, 20, 40);
        self.custom_stacks = list(self.custom_stacks, 50, 64);
        self
    }

    /// The reported settings, when they are well-formed and within limits
    /// (version 1 when they are an import of `agent.toml`).
    pub fn current(&self) -> Option<Settings> {
        let mut s: Settings = serde_json::from_value(self.current.clone()?).ok()?;
        s.version = self.version.unwrap_or(1).max(1);
        let ok = s.updates.allow.len() <= MAX_GLOBS
            && s.updates.deny.len() <= MAX_GLOBS
            && s.roots.len() <= MAX_ROOTS
            && s.script_stacks.len() <= MAX_STACKS;
        ok.then(|| {
            let clipped = |v: &[String]| v.iter().map(|x| clip(x, 300)).collect();
            s.updates.allow = clipped(&s.updates.allow);
            s.updates.deny = clipped(&s.updates.deny);
            s.roots = clipped(&s.roots);
            for st in &mut s.script_stacks {
                st.preset = clip(&st.preset, 40);
                st.name = clip(&st.name, 64);
                st.compose_project = clip(&st.compose_project, 64);
                st.working_dir = clip(&st.working_dir, 300);
            }
            s
        })
    }
}

/// The agent's last report from `servers.settings_report`.
fn stored_report(c: &Connection, id: i64) -> rusqlite::Result<Option<Report>> {
    let raw: Option<String> = c
        .query_row(
            "SELECT settings_report FROM servers WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// The settings Wren holds for a server.
pub fn current(c: &Connection, id: i64) -> rusqlite::Result<Option<Settings>> {
    let raw: Option<(i64, String)> = c
        .query_row(
            "SELECT version, settings FROM agent_settings WHERE server_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(raw.and_then(|(version, text)| {
        let mut s: Settings = serde_json::from_str(&text).ok()?;
        s.version = version;
        Some(s)
    }))
}

/// One change between two settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub code: String,
    pub value: String,
    /// The change lets the agent do more than before.
    pub widening: bool,
}

fn change(code: &str, value: impl Into<String>, widening: bool) -> Change {
    Change {
        code: code.into(),
        value: value.into(),
        widening,
    }
}

/// What differs between `old` and `new`, with the widening ones marked.
pub fn diff(old: &Settings, new: &Settings) -> Vec<Change> {
    let mut out = Vec::new();
    let covered = |globs: &[String], g: &str| globs.iter().any(|o| crate::policy::glob(o, g));
    for g in &new.updates.allow {
        if !old.updates.allow.contains(g) {
            out.push(change("allow_added", g, !covered(&old.updates.allow, g)));
        }
    }
    for g in &old.updates.allow {
        if !new.updates.allow.contains(g) {
            out.push(change("allow_removed", g, false));
        }
    }
    for g in &new.updates.deny {
        if !old.updates.deny.contains(g) {
            out.push(change("deny_added", g, false));
        }
    }
    for g in &old.updates.deny {
        if !new.updates.deny.contains(g) {
            // Still refused by another entry that matches it: not wider.
            out.push(change("deny_removed", g, !covered(&new.updates.deny, g)));
        }
    }
    for r in &new.roots {
        if !old.roots.contains(r) {
            out.push(change("root_added", r, true));
        }
    }
    for r in &old.roots {
        if !new.roots.contains(r) {
            out.push(change("root_removed", r, false));
        }
    }
    for s in &new.script_stacks {
        match old.script_stacks.iter().find(|o| o.name == s.name) {
            None => out.push(change(
                "stack_added",
                format!("{} ({})", s.name, s.preset),
                true,
            )),
            Some(o) => {
                if o.working_dir != s.working_dir
                    || o.compose_project != s.compose_project
                    || o.preset != s.preset
                {
                    out.push(change(
                        "stack_changed",
                        format!("{}: {}", s.name, s.working_dir),
                        true,
                    ));
                }
                if o.min_interval_seconds != s.min_interval_seconds {
                    out.push(change(
                        "min_interval",
                        format!(
                            "{}: {} → {}",
                            s.name,
                            minutes(o.min_interval_seconds),
                            minutes(s.min_interval_seconds)
                        ),
                        s.min_interval_seconds < o.min_interval_seconds,
                    ));
                }
            }
        }
    }
    for o in &old.script_stacks {
        if !new.script_stacks.iter().any(|s| s.name == o.name) {
            out.push(change("stack_removed", &o.name, false));
        }
    }
    if old.updates.min_health_seconds != new.updates.min_health_seconds {
        out.push(change(
            "min_health",
            format!(
                "{} → {} s",
                old.updates.min_health_seconds, new.updates.min_health_seconds
            ),
            new.updates.min_health_seconds < old.updates.min_health_seconds,
        ));
    }
    if old.updates.require_snapshot != new.updates.require_snapshot {
        out.push(change(
            if new.updates.require_snapshot {
                "snapshot_on"
            } else {
                "snapshot_off"
            },
            "",
            !new.updates.require_snapshot,
        ));
    }
    if old.retention.rollback_images != new.retention.rollback_images {
        out.push(change(
            "rollback_images",
            format!(
                "{} → {}",
                old.retention.rollback_images, new.retention.rollback_images
            ),
            false,
        ));
    }
    if old.retention.snapshots != new.retention.snapshots {
        let shown = |n: Option<i64>| n.map_or("–".to_string(), |n| n.to_string());
        out.push(change(
            "snapshots",
            format!(
                "{} → {}",
                shown(old.retention.snapshots),
                shown(new.retention.snapshots)
            ),
            false,
        ));
    }
    out
}

fn minutes(seconds: i64) -> String {
    if seconds % 60 == 0 {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

/// What a save did.
#[derive(Debug, PartialEq)]
pub enum Saved {
    /// The new settings equal the stored ones.
    Nothing,
    Changed {
        version: i64,
        changes: Vec<Change>,
        widening: bool,
    },
}

fn record(
    c: &Connection,
    id: i64,
    s: &Settings,
    by: &str,
    source: &str,
    changes: &[Change],
) -> rusqlite::Result<bool> {
    let widening = changes.iter().any(|ch| ch.widening);
    let text = serde_json::to_string(s).unwrap_or_default();
    c.execute(
        "INSERT INTO agent_settings (server_id, version, settings, updated_at)
         VALUES (?1, ?2, ?3, datetime('now'))
         ON CONFLICT(server_id) DO UPDATE SET version = excluded.version,
             settings = excluded.settings, updated_at = excluded.updated_at",
        params![id, s.version, text],
    )?;
    c.execute(
        "INSERT INTO agent_settings_history (server_id, version, changed_by, source, changes, widening)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            s.version,
            by,
            source,
            serde_json::to_string(changes).unwrap_or_else(|_| "[]".into()),
            widening
        ],
    )?;
    Ok(widening)
}

/// Stores `new` as the next version (counting past any version the agent
/// reports, so a restored Wren database still moves forward), with its
/// history entry.
pub fn save_settings(
    c: &mut Connection,
    id: i64,
    mut new: Settings,
    by: &str,
    agent_version: Option<i64>,
) -> rusqlite::Result<Saved> {
    // Two admins saving at once get two versions, not one over the other.
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let old = current(&tx, id)?;
    let changes = diff(&old.clone().unwrap_or_default(), &new);
    if old.is_some() && changes.is_empty() {
        return Ok(Saved::Nothing);
    }
    new.version = old
        .as_ref()
        .map_or(0, |o| o.version)
        .max(agent_version.unwrap_or(0))
        + 1;
    let widening = record(&tx, id, &new, by, "user", &changes)?;
    tx.commit()?;
    Ok(Saved::Changed {
        version: new.version,
        changes,
        widening,
    })
}

// ---------------------------------------------------------------------------
// The heartbeat
// ---------------------------------------------------------------------------

/// Takes in an agent's report and gives back the settings to send: the
/// stored ones while the agent runs another version, or, for an unlocked
/// agent with no managed settings and nothing stored yet, its own
/// `agent.toml` as version 1 (an import; the agent applies the same values).
/// An agent that sends no report (before 0.8) gets nothing.
pub fn on_heartbeat(
    c: &Connection,
    id: i64,
    report: Option<Report>,
) -> rusqlite::Result<Option<Settings>> {
    let Some(report) = report.filter(|r| r.supported).map(Report::clean) else {
        c.execute(
            "UPDATE servers SET settings_report = NULL WHERE id = ?1",
            [id],
        )?;
        return Ok(None);
    };
    c.execute(
        "UPDATE servers SET settings_report = ?2 WHERE id = ?1",
        params![id, serde_json::to_string(&report).unwrap_or_default()],
    )?;
    if report.locked {
        return Ok(None);
    }
    match current(c, id)? {
        Some(stored) => Ok((report.version != Some(stored.version)).then_some(stored)),
        None => {
            let Some(import) = report.current() else {
                return Ok(None);
            };
            record(
                c,
                id,
                &import,
                "",
                "import",
                &[change("imported", "", false)],
            )?;
            // An agent that already runs these (Wren lost its copy) is told nothing.
            Ok((report.version != Some(import.version)).then_some(import))
        }
    }
}

// ---------------------------------------------------------------------------
// The state of a server's settings
// ---------------------------------------------------------------------------

/// How the settings stand, for the status line.
#[derive(Debug, Clone, PartialEq)]
pub enum Standing {
    /// The agent hasn't reported in.
    Unknown,
    /// An agent before 0.8.
    Unsupported,
    Locked,
    /// Nothing stored and nothing to import yet.
    Empty,
    Active(i64),
    Pending(i64),
    Refused(i64, String),
    /// The agent runs a newer version than Wren knows (a restored database).
    Ahead(i64),
}

pub fn state(report: Option<&Report>, stored: Option<i64>, seen: bool) -> Standing {
    let Some(r) = report.filter(|r| r.supported) else {
        return if seen {
            Standing::Unsupported
        } else {
            Standing::Unknown
        };
    };
    if r.locked {
        return Standing::Locked;
    }
    let Some(v) = stored else {
        return Standing::Empty;
    };
    match r.version {
        Some(a) if a == v => Standing::Active(v),
        Some(a) if a > v => Standing::Ahead(a),
        _ if !r.error.is_empty() && r.error_version == Some(v) => {
            Standing::Refused(v, r.error.clone())
        }
        _ => Standing::Pending(v),
    }
}

impl Standing {
    fn code(&self) -> &'static str {
        match self {
            Standing::Unknown => "unknown",
            Standing::Unsupported => "unsupported",
            Standing::Locked => "locked",
            Standing::Empty => "empty",
            Standing::Active(_) => "active",
            Standing::Pending(_) => "pending",
            Standing::Refused(..) => "refused",
            Standing::Ahead(_) => "ahead",
        }
    }

    fn hint(&self) -> String {
        match self {
            Standing::Unsupported => i18n::t("settings.unsupported_help").to_string(),
            Standing::Locked => i18n::t("settings.locked_help").to_string(),
            _ => String::new(),
        }
    }

    fn text(&self) -> String {
        match self {
            Standing::Unknown => i18n::t("settings.status_unknown").to_string(),
            Standing::Unsupported => i18n::t("settings.status_unsupported").to_string(),
            Standing::Locked => i18n::t("settings.status_locked").to_string(),
            Standing::Empty => i18n::t("settings.status_empty").to_string(),
            Standing::Active(v) => {
                i18n::tf("settings.status_active", &[("version", &v.to_string())])
            }
            Standing::Pending(v) => {
                i18n::tf("settings.status_pending", &[("version", &v.to_string())])
            }
            Standing::Refused(v, e) => i18n::tf(
                "settings.status_refused",
                &[("version", &v.to_string()), ("error", e)],
            ),
            Standing::Ahead(v) => i18n::tf("settings.status_ahead", &[("version", &v.to_string())]),
        }
    }

    fn class(&self) -> &'static str {
        match self {
            Standing::Active(_) => "success",
            Standing::Pending(_) | Standing::Empty | Standing::Unknown => "info",
            Standing::Locked => "warning",
            Standing::Unsupported | Standing::Refused(..) | Standing::Ahead(_) => "warning",
        }
    }
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

/// Whether `p` is an absolute folder without `.`/`..` parts, not `/`.
fn folder_ok(p: &str) -> bool {
    p.starts_with('/')
        && p != "/"
        && !p.split('/').any(|c| c == "." || c == "..")
        && !p.chars().any(char::is_control)
}

fn under(p: &str, parent: &str) -> bool {
    Path::new(p).starts_with(parent)
}

/// A script stack as the form holds it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StackDraft {
    pub preset: String,
    pub name: String,
    pub project: String,
    pub dir: String,
    /// Minutes, as typed.
    pub minutes: String,
}

/// The form's values, kept as typed so a refused save shows them again.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub all: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub min_health: String,
    pub require_snapshot: bool,
    pub rollback_images: String,
    pub snapshots: String,
    pub roots: Vec<String>,
    pub stacks: Vec<StackDraft>,
}

impl Draft {
    pub fn from_settings(s: &Settings) -> Draft {
        let all = s.updates.allow.iter().any(|g| g == "*");
        Draft {
            all,
            allow: if all {
                Vec::new()
            } else {
                s.updates.allow.clone()
            },
            deny: s.updates.deny.clone(),
            min_health: s.updates.min_health_seconds.to_string(),
            require_snapshot: s.updates.require_snapshot,
            rollback_images: s.retention.rollback_images.to_string(),
            snapshots: s
                .retention
                .snapshots
                .map(|n| n.to_string())
                .unwrap_or_default(),
            roots: s.roots.clone(),
            stacks: s
                .script_stacks
                .iter()
                .map(|st| StackDraft {
                    preset: st.preset.clone(),
                    name: st.name.clone(),
                    project: st.compose_project.clone(),
                    dir: st.working_dir.clone(),
                    minutes: (st.min_interval_seconds / 60).to_string(),
                })
                .collect(),
        }
    }

    /// From a submitted form: `stacks` is how many stack rows were shown.
    pub fn from_form(f: &BTreeMap<String, Vec<String>>) -> Draft {
        let one = |k: &str| {
            f.get(k)
                .and_then(|v| v.first())
                .map_or(String::new(), |s| s.trim().to_string())
        };
        let many = |k: &str| -> Vec<String> {
            let mut v: Vec<String> = f
                .get(k)
                .map(|v| v.iter().map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();
            v.retain(|s| !s.is_empty());
            v.sort();
            v.dedup();
            v
        };
        let rows: usize = one("stacks").parse().unwrap_or(0).min(MAX_STACKS + 1);
        let stacks = (0..rows)
            .filter(|i| one(&format!("stack_{i}_remove")) != "true")
            .map(|i| StackDraft {
                preset: one(&format!("stack_{i}_preset")),
                name: one(&format!("stack_{i}_name")),
                project: one(&format!("stack_{i}_project")),
                dir: one(&format!("stack_{i}_dir")),
                minutes: one(&format!("stack_{i}_minutes")),
            })
            // The empty row at the end is "add one": nothing typed, nothing added.
            .filter(|s| !(s.name.is_empty() && s.dir.is_empty() && s.project.is_empty()))
            // Typing only the folder is enough: the preset names the rest.
            .map(|mut s| {
                if s.name.is_empty() {
                    s.name = s.preset.clone();
                }
                if s.project.is_empty() {
                    s.project = s.name.clone();
                }
                s
            })
            .collect();
        Draft {
            all: one("updates") == "all",
            allow: many("allow"),
            deny: many("deny"),
            min_health: one("min_health"),
            require_snapshot: one("require_snapshot") == "true",
            rollback_images: one("rollback_images"),
            snapshots: one("snapshots"),
            roots: many("root"),
            stacks,
        }
    }
}

/// What a form may choose for roots: folders under the agent's mounts that
/// are not system folders, from what the inventory and the mounts showed,
/// plus the roots the agent has now.
pub fn root_choices(report: Option<&Report>, seen: &[String], current: &[String]) -> Vec<String> {
    let mut out: BTreeSet<String> = current.iter().cloned().collect();
    let Some(r) = report else {
        return out.into_iter().collect();
    };
    let mut offered: Vec<&str> = r.mounts.iter().chain(seen).map(String::as_str).collect();
    offered.push(COMPOSE_MANAGER);
    for p in offered {
        if folder_ok(p)
            && managed::forbidden(Path::new(p)).is_none()
            && r.mounts.iter().any(|m| under(p, m))
        {
            out.insert(p.to_string());
        }
    }
    out.into_iter().collect()
}

impl Draft {
    /// The settings this form describes, or what is wrong with it
    /// (translated). `choices` are the roots that may be chosen.
    pub fn to_settings(
        &self,
        choices: &[String],
        presets: &[String],
    ) -> Result<Settings, Vec<String>> {
        let mut errors = Vec::new();
        let number =
            |raw: &str, field: &str, min: i64, max: i64, errors: &mut Vec<String>| match raw
                .parse::<i64>()
            {
                Ok(n) if (min..=max).contains(&n) => n,
                _ => {
                    errors.push(i18n::tf(
                        "settings.err_number",
                        &[
                            ("field", field),
                            ("min", &min.to_string()),
                            ("max", &max.to_string()),
                        ],
                    ));
                    min
                }
            };
        let allow = if self.all {
            vec!["*".to_string()]
        } else {
            self.allow.clone()
        };
        for g in allow.iter().chain(&self.deny) {
            if !valid_glob(g) {
                errors.push(i18n::tf("settings.err_glob", &[("value", &clip(g, 40))]));
            }
        }
        let min_health = number(
            &self.min_health,
            i18n::t("settings.min_health"),
            0,
            i64::try_from(MAX_HEALTH_SECONDS).unwrap_or(3600),
            &mut errors,
        );
        let rollback_images = number(
            &self.rollback_images,
            i18n::t("settings.rollback_images"),
            1,
            i64::try_from(MAX_RETENTION).unwrap_or(10),
            &mut errors,
        );
        let snapshots = if self.snapshots.is_empty() {
            None
        } else {
            Some(number(
                &self.snapshots,
                i18n::t("settings.snapshots"),
                1,
                i64::try_from(MAX_RETENTION).unwrap_or(10),
                &mut errors,
            ))
        };
        for r in &self.roots {
            if !choices.contains(r) {
                errors.push(i18n::tf("settings.err_root", &[("path", &clip(r, 100))]));
            }
        }
        let mut stacks = Vec::new();
        for st in &self.stacks {
            let at = if st.name.is_empty() { "?" } else { &st.name };
            let minutes = st.minutes.parse::<i64>().ok().map(|m| m.saturating_mul(60));
            let ok = presets.contains(&st.preset)
                && valid_name(&st.name)
                && valid_name(&st.project)
                && folder_ok(&st.dir)
                && self.roots.iter().any(|r| under(&st.dir, r))
                && minutes
                    .is_some_and(|s| (MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&s))
                && !stacks
                    .iter()
                    .any(|o: &Stack| o.name == st.name || o.compose_project == st.project);
            if ok {
                stacks.push(Stack {
                    preset: st.preset.clone(),
                    name: st.name.clone(),
                    compose_project: st.project.clone(),
                    working_dir: st.dir.clone(),
                    min_interval_seconds: minutes.unwrap_or(3600),
                });
            } else {
                errors.push(i18n::tf("settings.err_stack", &[("name", &clip(at, 40))]));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Settings {
            version: 0,
            updates: Updates {
                allow,
                deny: self.deny.clone(),
                min_health_seconds: min_health,
                require_snapshot: self.require_snapshot,
            },
            roots: self.roots.clone(),
            retention: Keep {
                rollback_images,
                snapshots,
            },
            script_stacks: stacks,
        })
    }
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// A target (or pattern) to tick.
pub struct Pick {
    pub name: String,
    pub kind_label: &'static str,
    pub allow: bool,
    pub deny: bool,
}

pub struct RootRow {
    pub path: String,
    pub checked: bool,
}

/// A stack row with the field names it posts.
pub struct StackRow {
    pub index: usize,
    pub s: StackDraft,
    pub f_preset: String,
    pub f_name: String,
    pub f_project: String,
    pub f_dir: String,
    pub f_minutes: String,
    pub f_remove: String,
    pub is_new: bool,
    pub options: Vec<PresetOption>,
}

/// A preset in a stack's list.
pub struct PresetOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// A preset's name for people.
pub fn preset_label(preset: &str) -> String {
    match preset {
        wren_agent::presets::FLUXER => i18n::t("settings.preset_fluxer").to_string(),
        other => other.to_string(),
    }
}

pub struct HistoryRow {
    pub version: i64,
    pub at: String,
    pub by: String,
    pub changes: Vec<String>,
    pub widening: bool,
}

#[derive(Template)]
#[template(path = "agent_settings.html")]
struct SettingsPage {
    ctx: PageCtx,
    server_id: i64,
    server_name: String,
    back_href: String,
    status: String,
    status_class: &'static str,
    /// `active`, `pending`, `refused` ...: for scripts and tests.
    status_code: &'static str,
    /// A line under the status, where it helps.
    hint: String,
    is_admin: bool,
    /// The form is shown (the agent can take settings).
    form: bool,
    can_edit: bool,
    draft: Draft,
    picks: Vec<Pick>,
    roots: Vec<RootRow>,
    no_mounts: bool,
    stacks: Vec<StackRow>,
    stack_count: usize,
    dirs: Vec<String>,
    custom: String,
    history: Vec<HistoryRow>,
    errors: Vec<String>,
}

fn form(raw: &[u8]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(raw) {
        out.entry(k.into_owned()).or_default().push(v.into_owned());
    }
    out
}

/// What the page needs from the database.
struct Loaded {
    name: String,
    seen: bool,
    report: Option<Report>,
    stored: Option<Settings>,
    seen_folders: Vec<String>,
    targets: Vec<(String, String, String)>,
    compose_dirs: Vec<String>,
    history: Vec<(i64, String, String, String, String, bool)>,
}

fn load(c: &Connection, id: i64) -> rusqlite::Result<Option<Loaded>> {
    let row: Option<(String, bool, String)> = c
        .query_row(
            "SELECT name, last_seen_at IS NOT NULL, folders FROM servers WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((name, seen, folders)) = row else {
        return Ok(None);
    };
    let targets: Vec<(String, String, String)> = c
        .prepare(
            "SELECT name, kind, working_dir FROM targets WHERE server_id = ?1
             ORDER BY name COLLATE NOCASE",
        )?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let compose_dirs = targets
        .iter()
        .filter(|(_, _, d)| folder_ok(d))
        .map(|(_, _, d)| d.clone())
        .collect();
    let history = c
        .prepare(
            "SELECT version, at, changed_by, source, changes, widening FROM agent_settings_history
             WHERE server_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?
        .query_map(params![id, SHOWN], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(Loaded {
        name,
        seen,
        report: stored_report(c, id)?,
        stored: current(c, id)?,
        seen_folders: serde_json::from_str(&folders).unwrap_or_default(),
        targets,
        compose_dirs,
        history,
    }))
}

/// One change in the reader's words.
pub fn change_text(ch: &Change) -> String {
    let key = match ch.code.as_str() {
        "imported" => "settings.change_imported",
        "allow_added" => "settings.change_allow_added",
        "allow_removed" => "settings.change_allow_removed",
        "deny_added" => "settings.change_deny_added",
        "deny_removed" => "settings.change_deny_removed",
        "root_added" => "settings.change_root_added",
        "root_removed" => "settings.change_root_removed",
        "stack_added" => "settings.change_stack_added",
        "stack_removed" => "settings.change_stack_removed",
        "stack_changed" => "settings.change_stack_changed",
        "min_health" => "settings.change_min_health",
        "snapshot_on" => "settings.change_snapshot_on",
        "snapshot_off" => "settings.change_snapshot_off",
        "min_interval" => "settings.change_min_interval",
        "rollback_images" => "settings.change_rollback_images",
        _ => "settings.change_snapshots",
    };
    i18n::tf(key, &[("value", &ch.value)])
}

fn picks(targets: &[(String, String, String)], draft: &Draft) -> Vec<Pick> {
    let mut rows: BTreeMap<String, &'static str> = BTreeMap::new();
    for (name, kind, _) in targets {
        if valid_glob(name) {
            rows.insert(name.clone(), kind_label(kind));
        }
    }
    // Patterns already in the lists (`media-*`) stay editable too.
    for g in draft.allow.iter().chain(&draft.deny) {
        if g != "*" {
            rows.entry(g.clone()).or_insert("");
        }
    }
    rows.into_iter()
        .map(|(name, kind_label)| Pick {
            allow: draft.allow.contains(&name),
            deny: draft.deny.contains(&name),
            name,
            kind_label,
        })
        .collect()
}

async fn render(
    core: &Core,
    user: &CurrentUser,
    jar: SignedCookieJar,
    id: i64,
    status: StatusCode,
    draft: Option<Draft>,
    errors: Vec<String>,
) -> ApiResult<Response> {
    let Some(l) = core.with_conn(move |c| load(c, id)).await? else {
        return Err(ApiError::NotFound);
    };
    let st = state(
        l.report.as_ref(),
        l.stored.as_ref().map(|s| s.version),
        l.seen,
    );
    let supported = l.report.as_ref().is_some_and(|r| r.supported);
    let form = supported && !matches!(st, Standing::Locked) && l.stored.is_some();
    let can_edit = user.is_admin() && form;
    let draft = draft
        .or_else(|| l.stored.as_ref().map(Draft::from_settings))
        .unwrap_or_default();
    let current_roots = l
        .stored
        .as_ref()
        .map(|s| s.roots.clone())
        .unwrap_or_default();
    let mut choices = root_choices(l.report.as_ref(), &l.seen_folders, &current_roots);
    // A refused form keeps what was typed, even a root that isn't offered.
    for r in &draft.roots {
        if !choices.contains(r) {
            choices.push(r.clone());
        }
    }
    let roots: Vec<RootRow> = choices
        .iter()
        .map(|p| RootRow {
            checked: draft.roots.contains(p),
            path: p.clone(),
        })
        .collect();
    let presets = l
        .report
        .as_ref()
        .map(|r| r.presets.clone())
        .unwrap_or_default();
    let mut stacks: Vec<StackRow> = draft
        .stacks
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, s)| stack_row(index, s, false, &presets))
        .collect();
    if can_edit && !presets.is_empty() && stacks.len() < MAX_STACKS {
        let index = stacks.len();
        let preset = presets.first().cloned().unwrap_or_default();
        stacks.push(stack_row(
            index,
            StackDraft {
                preset,
                name: String::new(),
                project: String::new(),
                dir: String::new(),
                minutes: "60".into(),
            },
            true,
            &presets,
        ));
    }
    // Folder suggestions for a stack: Compose folders seen, the stacks'
    // own, and each root's `<root>/<preset>`.
    let mut dirs: BTreeSet<String> = l.compose_dirs.iter().cloned().collect();
    for r in &draft.roots {
        for p in &presets {
            dirs.insert(format!("{}/{p}", r.trim_end_matches('/')));
        }
    }
    let dirs: Vec<String> = dirs
        .into_iter()
        .filter(|d| draft.roots.iter().any(|r| under(d, r)))
        .collect();
    let history = l
        .history
        .iter()
        .map(|(version, at, by, source, changes, widening)| {
            let changes: Vec<Change> = serde_json::from_str(changes).unwrap_or_default();
            HistoryRow {
                version: *version,
                at: at.clone(),
                by: if source == "import" {
                    i18n::t("settings.by_import").to_string()
                } else {
                    by.clone()
                },
                changes: changes.iter().map(change_text).collect(),
                widening: *widening,
            }
        })
        .collect();
    let (ctx, jar) = PageCtx::build(
        core,
        Some(user),
        &jar,
        i18n::t("settings.title"),
        "servers",
        "",
    )
    .await;
    let custom = l
        .report
        .as_ref()
        .map(|r| r.custom_stacks.join(", "))
        .unwrap_or_default();
    let page = SettingsPage {
        ctx,
        server_id: id,
        back_href: format!("/servers/{id}"),
        server_name: l.name,
        status: st.text(),
        status_class: st.class(),
        status_code: st.code(),
        hint: st.hint(),
        is_admin: user.is_admin(),
        form,
        can_edit,
        picks: picks(&l.targets, &draft),
        no_mounts: l.report.as_ref().is_none_or(|r| r.mounts.is_empty()),
        roots,
        stack_count: stacks.len(),
        stacks,
        dirs,
        custom,
        history,
        errors,
        draft,
    };
    Ok((status, jar, HtmlTemplate(page)).into_response())
}

fn stack_row(index: usize, s: StackDraft, is_new: bool, presets: &[String]) -> StackRow {
    StackRow {
        options: presets
            .iter()
            .map(|p| PresetOption {
                value: p.clone(),
                label: preset_label(p),
                selected: *p == s.preset,
            })
            .collect(),
        index,
        f_preset: format!("stack_{index}_preset"),
        f_name: format!("stack_{index}_name"),
        f_project: format!("stack_{index}_project"),
        f_dir: format!("stack_{index}_dir"),
        f_minutes: format!("stack_{index}_minutes"),
        f_remove: format!("stack_{index}_remove"),
        s,
        is_new,
    }
}

async fn page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    UrlPath(id): UrlPath<i64>,
) -> ApiResult<Response> {
    render(&core, &user, jar, id, StatusCode::OK, None, Vec::new()).await
}

async fn save(
    State(core): State<Core>,
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    UrlPath(id): UrlPath<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_admin()?;
    let Some(l) = core.with_conn(move |c| load(c, id)).await? else {
        return Err(ApiError::NotFound);
    };
    let st = state(
        l.report.as_ref(),
        l.stored.as_ref().map(|s| s.version),
        l.seen,
    );
    // Only an agent that takes settings, and isn't locked, is edited.
    let editable = l.report.as_ref().is_some_and(|r| r.supported)
        && !matches!(st, Standing::Locked)
        && l.stored.is_some();
    if !editable {
        return Err(ApiError::bad_request(
            "this agent doesn't take settings from Wren (older, locked or not connected yet)",
        ));
    }
    let draft = Draft::from_form(&form(&body));
    let current_roots = l
        .stored
        .as_ref()
        .map(|s| s.roots.clone())
        .unwrap_or_default();
    let choices = root_choices(l.report.as_ref(), &l.seen_folders, &current_roots);
    let presets = l
        .report
        .as_ref()
        .map(|r| r.presets.clone())
        .unwrap_or_default();
    let settings = match draft.to_settings(&choices, &presets) {
        Ok(s) => s,
        Err(errors) => {
            return render(
                &core,
                &user,
                jar,
                id,
                StatusCode::BAD_REQUEST,
                Some(draft),
                errors,
            )
            .await;
        }
    };
    let by = user.username.clone();
    let agent_version = l.report.as_ref().and_then(|r| r.version);
    let saved = core
        .with_conn(move |c| save_settings(c, id, settings, &by, agent_version))
        .await?;
    let message = match saved {
        Saved::Nothing => i18n::t("settings.no_change").to_string(),
        Saved::Changed {
            version,
            changes,
            widening,
        } => {
            tracing::info!(user = %user.username, server_id = id, version, widening, "agent settings saved");
            let _ = wren.changes.send(());
            if widening {
                notify_widened(&wren, id, &l.name, &user.username, &changes).await;
            }
            i18n::tf(
                if widening {
                    "settings.saved_widened"
                } else {
                    "settings.saved"
                },
                &[("version", &version.to_string())],
            )
        }
    };
    let jar = flash(jar, "success", message);
    Ok((jar, Redirect::to(&format!("/servers/{id}/settings"))).into_response())
}

// ---------------------------------------------------------------------------
// Telling the admins
// ---------------------------------------------------------------------------

/// The message about a change that widens what the agent may do.
pub fn widened_alert(
    server_id: i64,
    server: &str,
    by: &str,
    changes: &[Change],
    base: Option<&str>,
) -> Alert {
    let mut facts = vec![
        (
            i18n::t("notify_msg.fact_server").to_string(),
            server.to_string(),
        ),
        (
            i18n::t("notify_msg.fact_changed_by").to_string(),
            by.to_string(),
        ),
    ];
    facts.extend(changes.iter().filter(|c| c.widening).take(10).map(|c| {
        (
            i18n::t("notify_msg.fact_change").to_string(),
            change_text(c),
        )
    }));
    // The changes go into the text too: not every channel shows facts.
    let list = changes
        .iter()
        .filter(|c| c.widening)
        .take(10)
        .map(change_text)
        .collect::<Vec<_>>()
        .join("; ");
    let detail = Some(i18n::tf(
        "notify_msg.settings_widened_body",
        &[("user", by), ("changes", &list)],
    ));
    Alert {
        kind: AlertKind::Unstable,
        event: "agent_settings_widened".into(),
        event_label: i18n::t("notify_msg.event_settings").to_string(),
        status: "agent_settings_widened".into(),
        level: Level::Warning,
        colour: Level::Warning.colour(),
        emoji: "\u{26a0}\u{fe0f}".into(),
        title: i18n::tf("notify_msg.settings_widened_title", &[("server", server)]),
        message: detail.clone(),
        detail,
        facts,
        link_label: i18n::t("notify_msg.view_settings").to_string(),
        subject_id: Some(server_id),
        subject: server.to_string(),
        subject_type: "server".into(),
        target: String::new(),
        duration: None,
        expires_in_days: None,
        url: base.map(|b| format!("{}/servers/{server_id}/settings", b.trim_end_matches('/'))),
        occurred_at: OffsetDateTime::now_utc(),
    }
}

/// Tells every active notification channel (admins manage them) that the
/// agent may now do more.
pub async fn notify_widened(wren: &Wren, id: i64, server: &str, by: &str, changes: &[Change]) {
    crate::run_notify::broadcast(&wren.core, |base| {
        widened_alert(id, server, by, changes, base)
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Settings {
        Settings {
            version: 1,
            updates: Updates {
                allow: vec!["nextcloud".into(), "media-*".into()],
                deny: vec!["media-db*".into()],
                min_health_seconds: 30,
                require_snapshot: true,
            },
            roots: vec!["/mnt/user/appdata".into()],
            retention: Keep {
                rollback_images: 1,
                snapshots: None,
            },
            script_stacks: vec![Stack {
                preset: "fluxer".into(),
                name: "fluxer".into(),
                compose_project: "fluxer".into(),
                working_dir: "/mnt/user/appdata/fluxer".into(),
                min_interval_seconds: 3600,
            }],
        }
    }

    /// The codes of the changes between `base()` and the edited copy, with `!` before narrowing ones.
    fn codes(edit: impl Fn(&mut Settings)) -> Vec<String> {
        let mut new = base();
        edit(&mut new);
        diff(&base(), &new)
            .into_iter()
            .map(|c| format!("{}{}", if c.widening { "" } else { "!" }, c.code))
            .collect()
    }

    #[test]
    fn widening_changes_are_told_from_narrowing_ones() {
        assert!(codes(|_| {}).is_empty());
        // Wider.
        assert_eq!(codes(|s| s.roots.push("/srv".into())), ["root_added"]);
        assert_eq!(
            codes(|s| s.updates.allow.push("immich".into())),
            ["allow_added"]
        );
        assert_eq!(
            codes(|s| s.updates.allow = vec!["*".into()]),
            ["allow_added", "!allow_removed", "!allow_removed"]
        );
        assert_eq!(codes(|s| s.updates.deny.clear()), ["deny_removed"]);
        assert_eq!(codes(|s| s.updates.min_health_seconds = 10), ["min_health"]);
        assert_eq!(
            codes(|s| s.updates.require_snapshot = false),
            ["snapshot_off"]
        );
        assert_eq!(
            codes(|s| s.script_stacks.push(Stack {
                name: "chat".into(),
                ..s.script_stacks[0].clone()
            })),
            ["stack_added"]
        );
        assert_eq!(
            codes(|s| s.script_stacks[0].working_dir = "/mnt/user/appdata/other".into()),
            ["stack_changed"]
        );
        assert_eq!(
            codes(|s| s.script_stacks[0].min_interval_seconds = 60),
            ["min_interval"]
        );
        // Narrower, or neutral: logged only.
        assert_eq!(codes(|s| s.roots.clear()), ["!root_removed"]);
        assert_eq!(
            codes(|s| s.updates.allow = vec!["nextcloud".into()]),
            ["!allow_removed"]
        );
        assert_eq!(
            codes(|s| s.updates.deny.push("nextcloud".into())),
            ["!deny_added"]
        );
        assert_eq!(
            codes(|s| s.updates.min_health_seconds = 90),
            ["!min_health"]
        );
        assert_eq!(
            codes(|s| s.updates.require_snapshot = true),
            Vec::<String>::new()
        );
        assert_eq!(codes(|s| s.script_stacks.clear()), ["!stack_removed"]);
        assert_eq!(
            codes(|s| s.script_stacks[0].min_interval_seconds = 7200),
            ["!min_interval"]
        );
        assert_eq!(
            codes(|s| s.retention.rollback_images = 3),
            ["!rollback_images"]
        );
        assert_eq!(codes(|s| s.retention.snapshots = Some(4)), ["!snapshots"]);
        // An allow entry another one already covers, or a deny entry another
        // one still covers, widens nothing.
        assert_eq!(
            codes(|s| s.updates.allow.push("media-x".into())),
            ["!allow_added"]
        );
        assert_eq!(
            codes(|s| s.updates.deny = vec!["media-*".into()]),
            ["!deny_added", "!deny_removed"]
        );
    }

    #[test]
    fn a_form_draft_round_trips_through_settings() {
        crate::install();
        let s = base();
        let choices = vec!["/mnt/user/appdata".to_string()];
        let back = Draft::from_settings(&s)
            .to_settings(&choices, &["fluxer".to_string()])
            .unwrap();
        assert_eq!(
            Settings {
                version: 0,
                ..s.clone()
            },
            back
        );
        // "All targets" is the one entry `*`.
        let mut all = s;
        all.updates.allow = vec!["*".into()];
        let d = Draft::from_settings(&all);
        assert!(d.all && d.allow.is_empty());
        assert_eq!(
            d.to_settings(&choices, &["fluxer".to_string()])
                .unwrap()
                .updates
                .allow,
            ["*"]
        );
    }

    #[test]
    fn mounts_offer_roots_but_never_system_folders() {
        let report = Report {
            supported: true,
            mounts: vec![
                "/mnt/user/appdata".into(),
                "/boot/config".into(),
                "/var/lib/docker".into(),
                "/usr/local/emhttp".into(),
            ],
            ..Report::default()
        };
        let seen = vec!["/mnt/user/appdata/sonarr".to_string(), "/etc/x".to_string()];
        assert_eq!(
            root_choices(Some(&report), &seen, &["/opt/mine".to_string()]),
            [
                "/mnt/user/appdata",
                "/mnt/user/appdata/sonarr",
                "/boot/config/plugins/compose.manager/projects",
                "/opt/mine",
            ]
            .map(String::from)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        // No report: only what the agent has.
        assert_eq!(
            root_choices(None, &seen, &["/opt/mine".to_string()]),
            ["/opt/mine"]
        );
    }

    #[test]
    fn the_standing_follows_what_the_agent_reports() {
        let mut r = Report {
            supported: true,
            ..Report::default()
        };
        assert_eq!(state(None, None, false), Standing::Unknown);
        assert_eq!(state(None, None, true), Standing::Unsupported);
        assert_eq!(
            state(Some(&Report::default()), Some(1), true),
            Standing::Unsupported
        );
        assert_eq!(state(Some(&r), None, true), Standing::Empty);
        assert_eq!(state(Some(&r), Some(2), true), Standing::Pending(2));
        r.version = Some(2);
        assert_eq!(state(Some(&r), Some(2), true), Standing::Active(2));
        r.version = Some(5);
        assert_eq!(state(Some(&r), Some(2), true), Standing::Ahead(5));
        r.version = Some(1);
        r.error = "no".into();
        r.error_version = Some(2);
        assert_eq!(
            state(Some(&r), Some(2), true),
            Standing::Refused(2, "no".into())
        );
        r.locked = true;
        assert_eq!(state(Some(&r), Some(2), true), Standing::Locked);
    }
}
