//! Update runs (design §5, §6): approvals, the job channel to the agents and
//! what they report back.
//!
//! **Nothing runs unless someone allowed it.** A run is created only by an
//! editor or admin approving one exact version key of a target ([`approve`])
//! or asking to roll back one earlier run ([`request_rollback`]). There is
//! no automatic mode yet (phase 5); `notify` targets can't be approved. When
//! the agent asks for work ([`jobs`]), the approval is checked again: a
//! newer version since then makes it lapse, and nothing happens.
//!
//! The job is data, never a command (§8): the target's kind and name, the
//! version keys and, per image, the container, the reference it already
//! runs and the two digests. The agent checks it against its own
//! `agent.toml` and Docker, runs the steps itself and reports each one
//! ([`step`], [`finish`]).

use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Deserialize;
use serde_json::{Value, json};
use vole_kit::http::ApiError;
use vole_kit::http::ratelimit::{Rate, too_many_requests};

use crate::Wren;
use crate::inventory::{COMPOSE, SCRIPT};
use crate::targets;

pub const JOBS_PATH: &str = "/api/agent/v1/jobs";
/// How long a job request is held open (seconds).
pub const POLL_SECONDS: u64 = 25;
/// Job polls and step reports per server: 120 a minute.
pub const RATE: Rate = Rate::per_minute(120);
/// The health period unless a target sets its own (seconds), and its range.
pub const HEALTH_SECONDS: i64 = 180;
pub const MIN_HEALTH: i64 = 5;
pub const MAX_HEALTH: i64 = 3600;
/// Snapshots (and kept images) per target.
pub const KEEP_SNAPSHOTS: i64 = 2;
/// The most of a step's log Wren keeps (characters).
pub const MAX_LOG: usize = 4000;
/// A running run whose agent went offline and hasn't reported for this long
/// is closed as failed ("needs you"); it is never retried on its own.
pub const SILENT_MINUTES: i64 = 10;
/// The longest a run may go without a report even with its agent online.
pub const MAX_SILENT_HOURS: i64 = 26;

/// Modes.
pub const APPROVE: &str = "approve";
pub const NOTIFY: &str = "notify";

/// Run statuses.
pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const SUCCEEDED: &str = "succeeded";
pub const ROLLED_BACK: &str = "rolled_back";
pub const FAILED: &str = "failed";
pub const ROLLBACK_FAILED: &str = "rollback_failed";
pub const REFUSED: &str = "refused";
pub const CANCELLED: &str = "cancelled";
pub const LAPSED: &str = "lapsed";
/// What an agent may report as the end of a run.
pub const OUTCOMES: [&str; 6] = [
    SUCCEEDED,
    ROLLED_BACK,
    FAILED,
    ROLLBACK_FAILED,
    REFUSED,
    CANCELLED,
];
pub const STEPS: [&str; 6] = [
    "preflight",
    "stop",
    "snapshot",
    "update",
    "health",
    "rollback",
];
pub const STEP_STATUSES: [&str; 4] = ["running", "ok", "failed", "skipped"];

/// The mode that applies: the target's, else the server's.
pub fn effective_mode<'a>(target: Option<&'a str>, server: &'a str) -> &'a str {
    match target {
        Some(m) if !m.is_empty() => m,
        _ => server,
    }
}

/// Why an approval (or another action) was refused; the translation key
/// says it in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotFound,
    NoUpdate,
    NotifyOnly,
    FilesMissing,
    /// The available version changed since the person looked.
    Changed,
    AlreadyQueued,
    TooLate,
    NotRollbackable,
}

impl Refusal {
    pub fn key(self) -> &'static str {
        match self {
            Refusal::NotFound => "runs.err_not_found",
            Refusal::NoUpdate => "runs.err_no_update",
            Refusal::NotifyOnly => "runs.err_notify_only",
            Refusal::FilesMissing => "runs.err_files_missing",
            Refusal::Changed => "runs.err_changed",
            Refusal::AlreadyQueued => "runs.err_queued",
            Refusal::TooLate => "runs.err_too_late",
            Refusal::NotRollbackable => "runs.err_not_rollbackable",
        }
    }

    pub fn message(self) -> String {
        vole_kit::i18n::t(self.key()).to_string()
    }
}

impl From<Refusal> for ApiError {
    fn from(r: Refusal) -> ApiError {
        match r {
            Refusal::NotFound => ApiError::NotFound,
            other => ApiError::bad_request(other.message()),
        }
    }
}

/// Records who did what.
pub fn log_action(
    c: &Connection,
    user: &str,
    action: &str,
    target_id: Option<i64>,
    run_id: Option<i64>,
    detail: &str,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO actions (username, action, target_id, run_id, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![user, action, target_id, run_id, detail],
    )?;
    tracing::info!(user, action, ?target_id, ?run_id, detail, "action");
    Ok(())
}

/// What an approval needs to know about a target.
struct Approvable {
    server_id: i64,
    kind: String,
    name: String,
    status: String,
    available: String,
    available_key: String,
    mode: String,
    files_reachable: bool,
}

fn approvable(c: &Connection, target_id: i64) -> rusqlite::Result<Option<Approvable>> {
    c.query_row(
        "SELECT t.server_id, t.kind, t.name, t.status, t.available, t.available_key,
                COALESCE(NULLIF(t.mode, ''), s.mode), t.files_reachable
         FROM targets t JOIN servers s ON s.id = t.server_id WHERE t.id = ?1",
        [target_id],
        |r| {
            Ok(Approvable {
                server_id: r.get(0)?,
                kind: r.get(1)?,
                name: r.get(2)?,
                status: r.get(3)?,
                available: r.get(4)?,
                available_key: r.get(5)?,
                mode: r.get(6)?,
                files_reachable: r.get(7)?,
            })
        },
    )
    .optional()
}

/// Whether `t` may be updated now; the refusal otherwise.
fn may_update(t: &Approvable) -> Result<(), Refusal> {
    if t.status != targets::UPDATE || t.available_key.is_empty() {
        return Err(Refusal::NoUpdate);
    }
    if t.mode != APPROVE {
        return Err(Refusal::NotifyOnly);
    }
    if (t.kind == COMPOSE || t.kind == SCRIPT) && !t.files_reachable {
        return Err(Refusal::FilesMissing);
    }
    Ok(())
}

fn has_active_run(c: &Connection, target_id: i64) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE target_id = ?1 AND status IN ('queued', 'running'))",
        [target_id],
        |r| r.get(0),
    )
}

/// The running digests of the images that have an update, as a key.
fn from_key(c: &Connection, target_id: i64) -> rusqlite::Result<String> {
    let rows: Vec<String> = c
        .prepare(
            "SELECT container || '=' || local_digest FROM target_images
             WHERE target_id = ?1 AND status = 'update' ORDER BY container",
        )?
        .query_map([target_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let installed: String = c.query_row(
        "SELECT installed_version FROM targets WHERE id = ?1",
        [target_id],
        |r| r.get(0),
    )?;
    let mut parts = Vec::new();
    if !installed.is_empty() {
        parts.push(format!("version={installed}"));
    }
    parts.extend(rows);
    Ok(parts.join(","))
}

/// Approves the target's available version now. `key`, when given, must
/// be the version the person saw (else [`Refusal::Changed`]). The run's id.
pub fn approve(
    c: &Connection,
    target_id: i64,
    key: Option<&str>,
    user: &str,
) -> rusqlite::Result<Result<i64, Refusal>> {
    let Some(t) = approvable(c, target_id)? else {
        return Ok(Err(Refusal::NotFound));
    };
    if let Some(k) = key.filter(|k| !k.is_empty())
        && k != t.available_key
    {
        return Ok(Err(Refusal::Changed));
    }
    if let Err(r) = may_update(&t) {
        return Ok(Err(r));
    }
    if has_active_run(c, target_id)? {
        return Ok(Err(Refusal::AlreadyQueued));
    }
    c.execute(
        "INSERT INTO runs (server_id, target_id, target_kind, target_name, kind, from_key, to_key,
             shown, requested_by)
         VALUES (?1, ?2, ?3, ?4, 'update', ?5, ?6, ?7, ?8)",
        params![
            t.server_id,
            target_id,
            t.kind,
            t.name,
            from_key(c, target_id)?,
            t.available_key,
            t.available,
            user
        ],
    )?;
    let id = c.last_insert_rowid();
    c.execute(
        "UPDATE targets SET snoozed_until = NULL, skipped_key = '' WHERE id = ?1",
        [target_id],
    )?;
    log_action(
        c,
        user,
        "approve",
        Some(target_id),
        Some(id),
        &t.available_key,
    )?;
    Ok(Ok(id))
}

/// Skips the available version (a newer one shows again).
pub fn skip(c: &Connection, target_id: i64, user: &str) -> rusqlite::Result<Result<(), Refusal>> {
    let Some(t) = approvable(c, target_id)? else {
        return Ok(Err(Refusal::NotFound));
    };
    if t.status != targets::UPDATE {
        return Ok(Err(Refusal::NoUpdate));
    }
    c.execute(
        "UPDATE targets SET skipped_key = available_key WHERE id = ?1",
        [target_id],
    )?;
    log_action(c, user, "skip", Some(target_id), None, &t.available_key)?;
    Ok(Ok(()))
}

/// Hides the target's update until `hours` from now.
pub fn snooze(
    c: &Connection,
    target_id: i64,
    hours: i64,
    user: &str,
) -> rusqlite::Result<Result<String, Refusal>> {
    let Some(t) = approvable(c, target_id)? else {
        return Ok(Err(Refusal::NotFound));
    };
    if t.status != targets::UPDATE {
        return Ok(Err(Refusal::NoUpdate));
    }
    let until: String = c.query_row(
        "SELECT datetime('now', ?1 || ' hours')",
        [hours.clamp(1, 24 * 90)],
        |r| r.get(0),
    )?;
    c.execute(
        "UPDATE targets SET snoozed_until = ?2 WHERE id = ?1",
        params![target_id, until],
    )?;
    log_action(c, user, "snooze", Some(target_id), None, &until)?;
    Ok(Ok(until))
}

/// Undoes a skip or snooze.
pub fn resume(c: &Connection, target_id: i64, user: &str) -> rusqlite::Result<bool> {
    let n = c.execute(
        "UPDATE targets SET skipped_key = '', snoozed_until = NULL WHERE id = ?1",
        [target_id],
    )?;
    if n > 0 {
        log_action(c, user, "resume", Some(target_id), None, "")?;
    }
    Ok(n > 0)
}

/// Cancels a run that hasn't stopped anything yet: a queued one at once, a
/// running one when the agent reports its preflight.
pub fn cancel(c: &Connection, run_id: i64, user: &str) -> rusqlite::Result<Result<(), Refusal>> {
    let Some((status, target_id)): Option<(String, Option<i64>)> = c
        .query_row(
            "SELECT status, target_id FROM runs WHERE id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    else {
        return Ok(Err(Refusal::NotFound));
    };
    match status.as_str() {
        QUEUED => {
            c.execute(
                "UPDATE runs SET status = 'cancelled', finished_at = datetime('now') WHERE id = ?1",
                [run_id],
            )?;
        }
        RUNNING => {
            // Only before the preflight is through: after it things stop.
            let past: bool = c.query_row(
                "SELECT EXISTS (SELECT 1 FROM run_steps WHERE run_id = ?1
                     AND (step != 'preflight' OR status != 'running'))",
                [run_id],
                |r| r.get(0),
            )?;
            if past {
                return Ok(Err(Refusal::TooLate));
            }
            c.execute(
                "UPDATE runs SET cancel_requested = 1 WHERE id = ?1",
                [run_id],
            )?;
        }
        _ => return Ok(Err(Refusal::TooLate)),
    }
    log_action(c, user, "cancel", target_id, Some(run_id), "")?;
    Ok(Ok(()))
}

/// Asks to roll back a successful update: its target's newest one, while
/// nothing else runs for it. The new run's id.
pub fn request_rollback(
    c: &Connection,
    run_id: i64,
    user: &str,
) -> rusqlite::Result<Result<i64, Refusal>> {
    let Some(r) = get(c, run_id)? else {
        return Ok(Err(Refusal::NotFound));
    };
    let Some(target_id) = r.target_id else {
        return Ok(Err(Refusal::NotRollbackable));
    };
    let newest: Option<i64> = c.query_row(
        "SELECT MAX(id) FROM runs WHERE target_id = ?1 AND kind = 'update' AND status = 'succeeded'",
        [target_id],
        |row| row.get(0),
    )?;
    let later_rollback: bool = c.query_row(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE rollback_of = ?1 AND status = 'succeeded')",
        [run_id],
        |row| row.get(0),
    )?;
    if r.kind != "update" || r.status != SUCCEEDED || newest != Some(run_id) || later_rollback {
        return Ok(Err(Refusal::NotRollbackable));
    }
    let mode: String = c.query_row(
        "SELECT COALESCE(NULLIF(t.mode, ''), s.mode) FROM targets t JOIN servers s ON s.id = t.server_id WHERE t.id = ?1",
        [target_id],
        |row| row.get(0),
    ).optional()?.unwrap_or_default();
    if mode != APPROVE {
        return Ok(Err(Refusal::NotifyOnly));
    }
    if has_active_run(c, target_id)? {
        return Ok(Err(Refusal::AlreadyQueued));
    }
    c.execute(
        "INSERT INTO runs (server_id, target_id, target_kind, target_name, kind, rollback_of,
             from_key, to_key, shown, requested_by)
         VALUES (?1, ?2, ?3, ?4, 'rollback', ?5, ?6, ?7, ?8, ?9)",
        params![
            r.server_id,
            target_id,
            r.target_kind,
            r.target_name,
            run_id,
            r.to_key,
            r.from_key,
            format!("#{run_id}"),
            user
        ],
    )?;
    let id = c.last_insert_rowid();
    log_action(
        c,
        user,
        "rollback",
        Some(target_id),
        Some(id),
        &format!("run {run_id}"),
    )?;
    Ok(Ok(id))
}

/// The job for a run (JSON, as the agent reads it).
fn job_json(c: &Connection, run: &Run) -> rusqlite::Result<Value> {
    let target_id = run.target_id.unwrap_or_default();
    let images: Vec<Value> = if run.kind == "update" {
        c.prepare(
            "SELECT container, image, local_digest, remote_digest FROM target_images
             WHERE target_id = ?1 AND status = 'update' ORDER BY container",
        )?
        .query_map([target_id], |r| {
            Ok(json!({
                "container": r.get::<_, String>(0)?,
                "image": r.get::<_, String>(1)?,
                "from": r.get::<_, String>(2)?,
                "to": r.get::<_, String>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?
    } else {
        Vec::new()
    };
    let health: Option<i64> = c
        .query_row(
            "SELECT health_seconds FROM targets WHERE id = ?1",
            [target_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(json!({
        "run_id": run.id,
        "kind": run.kind,
        "target": { "kind": run.target_kind, "name": run.target_name },
        "from_key": run.from_key,
        "to_key": run.to_key,
        "images": images,
        "health_seconds": health.unwrap_or(HEALTH_SECONDS).clamp(MIN_HEALTH, MAX_HEALTH),
        "keep_snapshots": KEEP_SNAPSHOTS,
        "rollback_of": run.rollback_of,
    }))
}

fn close(c: &Connection, run_id: i64, status: &str, detail: &str) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE runs SET status = ?2, detail = ?3, finished_at = datetime('now') WHERE id = ?1",
        params![run_id, status, detail],
    )?;
    Ok(())
}

/// The next job for a server's agent: its oldest queued run, checked once
/// more (one run at a time per server). Runs that can't go any more lapse.
pub fn dispatch(c: &Connection, server_id: i64) -> rusqlite::Result<Option<Value>> {
    let busy: bool = c.query_row(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE server_id = ?1 AND status = 'running')",
        [server_id],
        |r| r.get(0),
    )?;
    if busy {
        return Ok(None);
    }
    let queued: Vec<i64> = c
        .prepare("SELECT id FROM runs WHERE server_id = ?1 AND status = 'queued' ORDER BY id")?
        .query_map([server_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in queued {
        let Some(run) = get(c, id)? else { continue };
        let Some(target_id) = run.target_id else {
            close(c, id, LAPSED, "the target is gone")?;
            continue;
        };
        if run.kind == "update" {
            let Some(t) = approvable(c, target_id)? else {
                close(c, id, LAPSED, "the target is gone")?;
                continue;
            };
            // The approval is for one version: a newer one needs a new one.
            if t.available_key != run.to_key {
                let why = if t.status == targets::UPDATE {
                    "a newer version appeared before the run started; approve it again"
                } else {
                    "the update is no longer available"
                };
                close(c, id, LAPSED, why)?;
                continue;
            }
            if let Err(r) = may_update(&t) {
                close(c, id, LAPSED, &r.message())?;
                continue;
            }
        } else if approvable(c, target_id)?.is_none_or(|t| t.mode != APPROVE) {
            // A rollback changes the target too: only while it may be changed.
            close(c, id, LAPSED, &Refusal::NotifyOnly.message())?;
            continue;
        }
        let job = job_json(c, &run)?;
        c.execute(
            "UPDATE runs SET status = 'running', job = ?2, started_at = datetime('now'),
                 last_report_at = datetime('now') WHERE id = ?1",
            params![id, job.to_string()],
        )?;
        return Ok(Some(job));
    }
    Ok(None)
}

/// Phase 5's hook: called when the agent's preflight is through and
/// nothing has stopped yet; this is where the Vigilant Vole maintenance
/// window will open. An error cancels the run before anything stops.
pub fn before_stop(_c: &Connection, _run_id: i64) -> Result<(), String> {
    Ok(())
}

/// What the agent reports about one step.
#[derive(Debug, Deserialize)]
pub struct StepBody {
    pub step: String,
    pub status: String,
    #[serde(default)]
    pub log: String,
    #[serde(default)]
    pub duration_ms: i64,
}

/// Records a step of a running run of this server; whether to cancel
/// (`None`: not this server's running run).
pub fn record_step(
    c: &Connection,
    server_id: i64,
    run_id: i64,
    b: &StepBody,
) -> rusqlite::Result<Option<bool>> {
    let cancel: Option<bool> = c
        .query_row(
            "SELECT cancel_requested FROM runs WHERE id = ?1 AND server_id = ?2 AND status = 'running'",
            params![run_id, server_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(mut cancel) = cancel else {
        return Ok(None);
    };
    let log: String = clip_log(&b.log);
    c.execute(
        "INSERT INTO run_steps (run_id, step, status, duration_ms, log) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (run_id, step) DO UPDATE SET status = excluded.status,
             duration_ms = excluded.duration_ms, log = excluded.log",
        params![run_id, b.step, b.status, b.duration_ms.max(0), log],
    )?;
    c.execute(
        "UPDATE runs SET last_report_at = datetime('now') WHERE id = ?1",
        [run_id],
    )?;
    if b.step == "preflight"
        && b.status == "ok"
        && !cancel
        && let Err(why) = before_stop(c, run_id)
    {
        tracing::warn!(run_id, "{why}");
        cancel = true;
    }
    Ok(Some(cancel && b.step == "preflight"))
}

fn clip_log(s: &str) -> String {
    let clean: String = s
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect();
    let n = clean.chars().count();
    clean.chars().skip(n.saturating_sub(MAX_LOG)).collect()
}

/// Closes a running run of this server with the agent's outcome; the run
/// (`None`: not this server's running run).
pub fn record_finish(
    c: &Connection,
    server_id: i64,
    run_id: i64,
    outcome: &str,
    detail: &str,
) -> rusqlite::Result<Option<Run>> {
    let ok: bool = c.query_row(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE id = ?1 AND server_id = ?2 AND status = 'running')",
        params![run_id, server_id],
        |r| r.get(0),
    )?;
    if !ok {
        return Ok(None);
    }
    close(c, run_id, outcome, &clip_log(detail))?;
    // A fresh inventory shows what runs now.
    let target_id: Option<i64> =
        c.query_row("SELECT target_id FROM runs WHERE id = ?1", [run_id], |r| {
            r.get(0)
        })?;
    targets::request_check(c, server_id, target_id)?;
    get(c, run_id)
}

/// Closes running runs whose agent went silent (offline and no report for
/// [`SILENT_MINUTES`], or no report for [`MAX_SILENT_HOURS`]); they are
/// failed with "needs you" and never retried.
pub fn close_silent(c: &Connection) -> rusqlite::Result<Vec<Run>> {
    let ids: Vec<i64> = c
        .prepare(&format!(
            "SELECT r.id FROM runs r JOIN servers s ON s.id = r.server_id
             WHERE r.status = 'running' AND (
                 (r.last_report_at < datetime('now', '-{SILENT_MINUTES} minutes')
                  AND COALESCE(s.last_seen_at < datetime('now', '-{} seconds'), 1))
                 OR r.last_report_at < datetime('now', '-{MAX_SILENT_HOURS} hours'))",
            crate::db::ONLINE_WINDOW_SECONDS
        ))?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for id in ids {
        close(
            c,
            id,
            ROLLBACK_FAILED,
            "the agent stopped reporting during the run; check the target by hand",
        )?;
        if let Some(r) = get(c, id)? {
            out.push(r);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub id: i64,
    pub server_id: i64,
    pub server_name: String,
    pub target_id: Option<i64>,
    pub target_kind: String,
    pub target_name: String,
    pub kind: String,
    pub rollback_of: Option<i64>,
    pub from_key: String,
    pub to_key: String,
    pub shown: String,
    pub status: String,
    pub detail: String,
    pub requested_by: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub cancel_requested: bool,
}

const COLUMNS: &str =
    "r.id, r.server_id, s.name, r.target_id, r.target_kind, r.target_name, r.kind,
    r.rollback_of, r.from_key, r.to_key, r.shown, r.status, r.detail, r.requested_by, r.created_at,
    r.started_at, r.finished_at, r.cancel_requested";

fn row(r: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        server_id: r.get(1)?,
        server_name: r.get(2)?,
        target_id: r.get(3)?,
        target_kind: r.get(4)?,
        target_name: r.get(5)?,
        kind: r.get(6)?,
        rollback_of: r.get(7)?,
        from_key: r.get(8)?,
        to_key: r.get(9)?,
        shown: r.get(10)?,
        status: r.get(11)?,
        detail: r.get(12)?,
        requested_by: r.get(13)?,
        created_at: r.get(14)?,
        started_at: r.get(15)?,
        finished_at: r.get(16)?,
        cancel_requested: r.get(17)?,
    })
}

pub fn get(c: &Connection, id: i64) -> rusqlite::Result<Option<Run>> {
    c.query_row(
        &format!(
            "SELECT {COLUMNS} FROM runs r JOIN servers s ON s.id = r.server_id WHERE r.id = ?1"
        ),
        [id],
        row,
    )
    .optional()
}

/// Which runs to list, newest first.
#[derive(Debug, Clone, Copy, Default)]
pub struct Filter {
    pub target_id: Option<i64>,
    pub server_id: Option<i64>,
    pub limit: i64,
}

pub fn list(c: &Connection, f: Filter) -> rusqlite::Result<Vec<Run>> {
    c.prepare(&format!(
        "SELECT {COLUMNS} FROM runs r JOIN servers s ON s.id = r.server_id
         WHERE (?1 IS NULL OR r.target_id = ?1) AND (?2 IS NULL OR r.server_id = ?2)
         ORDER BY r.id DESC LIMIT ?3"
    ))?
    .query_map(
        params![
            f.target_id,
            f.server_id,
            if f.limit > 0 { f.limit } else { 50 }
        ],
        row,
    )?
    .collect()
}

/// The target's queued or running run, if any.
pub fn active_for(c: &Connection, target_id: i64) -> rusqlite::Result<Option<Run>> {
    c.query_row(
        &format!(
            "SELECT {COLUMNS} FROM runs r JOIN servers s ON s.id = r.server_id
             WHERE r.target_id = ?1 AND r.status IN ('queued', 'running') ORDER BY r.id DESC LIMIT 1"
        ),
        [target_id],
        row,
    )
    .optional()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub step: String,
    pub status: String,
    pub started_at: String,
    pub duration_ms: i64,
    pub log: String,
}

pub fn steps(c: &Connection, run_id: i64) -> rusqlite::Result<Vec<Step>> {
    c.prepare(
        "SELECT step, status, started_at, duration_ms, log FROM run_steps WHERE run_id = ?1 ORDER BY id",
    )?
    .query_map([run_id], |r| {
        Ok(Step {
            step: r.get(0)?,
            status: r.get(1)?,
            started_at: r.get(2)?,
            duration_ms: r.get(3)?,
            log: r.get(4)?,
        })
    })?
    .collect()
}

/// Mode, health period, skip and snooze of a target.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Policy {
    pub mode: Option<String>,
    pub server_mode: String,
    pub health_seconds: Option<i64>,
    pub skipped: bool,
    pub snoozed_until: Option<String>,
}

pub fn policy(c: &Connection, target_id: i64) -> rusqlite::Result<Option<Policy>> {
    c.query_row(
        "SELECT t.mode, s.mode, t.health_seconds,
                t.skipped_key != '' AND t.skipped_key = t.available_key,
                CASE WHEN t.snoozed_until > datetime('now') THEN t.snoozed_until END
         FROM targets t JOIN servers s ON s.id = t.server_id WHERE t.id = ?1",
        [target_id],
        |r| {
            Ok(Policy {
                mode: r.get(0)?,
                server_mode: r.get(1)?,
                health_seconds: r.get(2)?,
                skipped: r.get(3)?,
                snoozed_until: r.get(4)?,
            })
        },
    )
    .optional()
}

/// Sets a target's mode (`None`: the server's) and health period.
pub fn set_target_policy(
    c: &Connection,
    target_id: i64,
    mode: Option<&str>,
    health_seconds: Option<i64>,
    user: &str,
) -> rusqlite::Result<bool> {
    let n = c.execute(
        "UPDATE targets SET mode = ?2, health_seconds = ?3 WHERE id = ?1",
        params![target_id, mode, health_seconds],
    )?;
    if n > 0 {
        log_action(
            c,
            user,
            "settings",
            Some(target_id),
            None,
            &format!(
                "mode={} health={}",
                mode.unwrap_or("server"),
                health_seconds.map_or("default".into(), |h| h.to_string())
            ),
        )?;
    }
    Ok(n > 0)
}

pub fn set_server_mode(
    c: &Connection,
    server_id: i64,
    mode: &str,
    user: &str,
) -> rusqlite::Result<bool> {
    let n = c.execute(
        "UPDATE servers SET mode = ?2 WHERE id = ?1",
        params![server_id, mode],
    )?;
    if n > 0 {
        log_action(
            c,
            user,
            "server_mode",
            None,
            None,
            &format!("server {server_id}: {mode}"),
        )?;
    }
    Ok(n > 0)
}

// ---------------------------------------------------------------------------
// The agent's side: GET /api/agent/v1/jobs, POST /api/agent/v1/runs/{id}/…
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PollQuery {
    /// Seconds to wait for a job (at most [`POLL_SECONDS`]).
    pub wait: Option<u64>,
}

async fn agent(wren: &Wren, headers: &HeaderMap) -> Result<i64, Box<Response>> {
    let (id, _) = crate::agent_api::authenticate(wren, headers)
        .await
        .map_err(|e| Box::new(e.into_response()))?;
    if let Err(wait) = wren.job_budgets.take(&id, RATE, Instant::now()) {
        tracing::warn!(server_id = id, "agent over its job budget");
        return Err(Box::new(too_many_requests(wait)));
    }
    Ok(id)
}

/// `GET /api/agent/v1/jobs`: the next job (`200 {"job": …}`) or, after
/// waiting, `204`.
pub async fn jobs(
    Extension(wren): Extension<Wren>,
    headers: HeaderMap,
    Query(q): Query<PollQuery>,
) -> Response {
    let server_id = match agent(&wren, &headers).await {
        Ok(id) => id,
        Err(res) => return *res,
    };
    let deadline =
        Instant::now() + Duration::from_secs(q.wait.unwrap_or(POLL_SECONDS).min(POLL_SECONDS));
    loop {
        let notified = wren.jobs.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        match wren.core.with_conn(move |c| dispatch(c, server_id)).await {
            Ok(Some(job)) => {
                tracing::info!(server_id, run_id = ?job.get("run_id"), "job sent to the agent");
                let _ = wren.changes.send(());
                return Json(json!({ "job": job })).into_response();
            }
            Ok(None) => {}
            Err(err) => return err.into_response(),
        }
        let now = Instant::now();
        if now >= deadline {
            return StatusCode::NO_CONTENT.into_response();
        }
        tokio::select! {
            () = &mut notified => {}
            () = tokio::time::sleep(deadline - now) => return StatusCode::NO_CONTENT.into_response(),
        }
    }
}

/// `POST /api/agent/v1/runs/{id}/steps` → `{"cancel": bool}`.
pub async fn step(
    Extension(wren): Extension<Wren>,
    headers: HeaderMap,
    Path(run_id): Path<i64>,
    body: Bytes,
) -> Response {
    let server_id = match agent(&wren, &headers).await {
        Ok(id) => id,
        Err(res) => return *res,
    };
    let b: StepBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return ApiError::bad_request(format!("not a step: {e}")).into_response(),
    };
    if !STEPS.contains(&b.step.as_str()) || !STEP_STATUSES.contains(&b.status.as_str()) {
        return ApiError::bad_request("unknown step or status").into_response();
    }
    match wren
        .core
        .with_conn(move |c| record_step(c, server_id, run_id, &b))
        .await
    {
        Ok(Some(cancel)) => {
            let _ = wren.changes.send(());
            Json(json!({ "cancel": cancel })).into_response()
        }
        Ok(None) => ApiError::NotFound.into_response(),
        Err(err) => err.into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct FinishBody {
    outcome: String,
    #[serde(default)]
    detail: String,
}

/// `POST /api/agent/v1/runs/{id}/finish`.
pub async fn finish(
    Extension(wren): Extension<Wren>,
    headers: HeaderMap,
    Path(run_id): Path<i64>,
    body: Bytes,
) -> Response {
    let server_id = match agent(&wren, &headers).await {
        Ok(id) => id,
        Err(res) => return *res,
    };
    let b: FinishBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return ApiError::bad_request(format!("not a result: {e}")).into_response(),
    };
    if !OUTCOMES.contains(&b.outcome.as_str()) {
        return ApiError::bad_request("unknown outcome").into_response();
    }
    let run = match wren
        .core
        .with_conn(move |c| record_finish(c, server_id, run_id, &b.outcome, &b.detail))
        .await
    {
        Ok(Some(run)) => run,
        Ok(None) => return ApiError::NotFound.into_response(),
        Err(err) => return err.into_response(),
    };
    tracing::info!(server_id, run_id, outcome = %run.status, "run finished");
    let _ = wren.changes.send(());
    wren.checks.notify_one();
    // The next queued run of this server may go now.
    wren.jobs.notify_waiters();
    let w = wren.clone();
    tokio::spawn(async move { crate::run_notify::send_result(&w, &run).await });
    Json(json!({})).into_response()
}

/// Looks for silent runs every minute.
pub fn spawn_watchdog(wren: Wren) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            match wren.core.with_conn(|c| close_silent(c)).await {
                Ok(runs) => {
                    for run in runs {
                        tracing::error!(run_id = run.id, "run closed: the agent went silent");
                        let _ = wren.changes.send(());
                        crate::run_notify::send_result(&wren, &run).await;
                    }
                }
                Err(err) => tracing::warn!(%err, "can't look for silent runs"),
            }
        }
    });
}
