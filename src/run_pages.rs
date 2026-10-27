//! Runs in the web UI (design §9): approving (now or for a chosen time),
//! skipping and snoozing updates (one or several), a target's mode, health
//! period and "require VV", a server's mode and auto window, the run page
//! with each step live, cancelling a run before anything stops, rolling back
//! a successful one, and the Schedules page. Everyone signed in sees runs;
//! editors and admins act (only admins turn automatic updates on), and every
//! action is stored with who did it.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::SignedCookieJar;
use vole_kit::auth::CurrentUser;
use vole_kit::http::live::{self, Changes, SharedRenders, StreamLimit};
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::{self, Lang, Tr as _};
use vole_kit::page::{PageCtx, flash};
use vole_kit::state::Core;

use crate::Wren;
use crate::runs::{self, Refusal, Run, Step};
use crate::schedule;
use crate::updates_notify::kind_label;

pub fn router() -> Router<Core> {
    Router::new()
        .route("/updates/approve", post(approve_many))
        .route("/targets/{id}/approve", post(approve_one))
        .route("/targets/{id}/schedule", post(schedule_one))
        .route("/runs/{id}/reschedule", post(reschedule))
        .route("/schedules", get(schedules_page))
        .route("/targets/{id}/skip", post(skip))
        .route("/targets/{id}/snooze", post(snooze))
        .route("/targets/{id}/resume", post(resume))
        .route("/targets/{id}/policy", post(target_policy))
        .route("/servers/{id}/mode", post(server_mode))
        .route("/runs/{id}", get(run_page))
        .route("/runs/{id}/live", get(run_live))
        .route("/runs/{id}/cancel", post(cancel))
        .route("/runs/{id}/rollback", post(rollback))
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

/// A run's status: design-system class and label.
pub fn run_look(status: &str) -> (&'static str, &'static str) {
    let (class, key) = match status {
        runs::QUEUED => ("pending", "runs.status_queued"),
        runs::RUNNING => ("maintenance", "runs.status_running"),
        runs::SUCCEEDED => ("up", "runs.status_succeeded"),
        runs::ROLLED_BACK => ("warning", "runs.status_rolled_back"),
        runs::FAILED => ("down", "runs.status_failed"),
        runs::ROLLBACK_FAILED => ("down", "runs.status_rollback_failed"),
        runs::REFUSED => ("down", "runs.status_refused"),
        runs::CANCELLED => ("paused", "runs.status_cancelled"),
        _ => ("paused", "runs.status_lapsed"),
    };
    (class, i18n::t(key))
}

fn step_look(status: &str) -> (&'static str, &'static str) {
    let (class, key) = match status {
        "ok" => ("up", "runs.step_ok"),
        "failed" => ("down", "runs.step_failed"),
        "running" => ("maintenance", "runs.step_running"),
        _ => ("paused", "runs.step_skipped"),
    };
    (class, i18n::t(key))
}

fn step_label(step: &str) -> &'static str {
    i18n::t(match step {
        "preflight" => "runs.step_preflight",
        "stop" => "runs.step_stop",
        "snapshot" => "runs.step_snapshot",
        "update" => "runs.step_update",
        "health" => "runs.step_health",
        "maintenance" => "runs.step_maintenance",
        "monitors" => "runs.step_monitors",
        "close" => "runs.step_close",
        _ => "runs.step_rollback",
    })
}

/// A run as pages show it.
pub struct RunView {
    pub id: i64,
    pub target_id: Option<i64>,
    pub target_name: String,
    pub kind_label: &'static str,
    pub server_id: i64,
    pub server_name: String,
    pub is_rollback: bool,
    pub rollback_of: Option<i64>,
    pub shown: String,
    pub status: String,
    pub status_class: &'static str,
    pub status_label: &'static str,
    /// The result in the reader's language.
    pub detail: String,
    pub requested_by: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub cancel_requested: bool,
    pub trigger: String,
    /// Who or what started it ("Approved by …", "Scheduled by …", "Automatic").
    pub trigger_label: String,
    /// No data snapshot was taken: why, per path (`None`: one was taken).
    pub no_snapshot: Option<Vec<String>>,
    /// When a scheduled run starts, in the site's time zone.
    pub scheduled_for: Option<String>,
    /// The same, `YYYY-MM-DDTHH:MM` for a date-time field.
    pub scheduled_input: String,
    pub vv_state: String,
    pub vv_window_id: Option<i64>,
}

impl RunView {
    /// Shows the scheduled time in `tz`.
    pub fn localize(mut self, tz: &jiff::tz::TimeZone) -> RunView {
        if let Some(at) = self.scheduled_for.take() {
            let local = schedule::local(&at, tz);
            self.scheduled_input = local.replacen(' ', "T", 1);
            self.scheduled_for = Some(local);
        }
        self
    }

    /// Waiting for its scheduled time.
    pub fn waiting(&self) -> bool {
        self.status == runs::QUEUED && self.scheduled_for.is_some()
    }

    /// The Vigilant Vole window in words ('' when there is none).
    pub fn vv_label(&self) -> &'static str {
        match self.vv_state.as_str() {
            "planned" | "replan" => i18n::t("vv.window_planned"),
            "open" => i18n::t("vv.window_open"),
            "closed" => i18n::t("vv.window_closed"),
            _ => "",
        }
    }

    pub fn new(r: Run) -> RunView {
        let (status_class, status_label) = run_look(&r.status);
        let detail = r.detail_text();
        let no_snapshot = r.no_snapshot();
        let trigger_label = crate::run_notify::trigger_text(&r);
        RunView {
            detail,
            no_snapshot,
            trigger_label,
            id: r.id,
            target_id: r.target_id,
            kind_label: kind_label(&r.target_kind),
            target_name: r.target_name,
            server_id: r.server_id,
            server_name: r.server_name,
            is_rollback: r.kind == "rollback",
            rollback_of: r.rollback_of,
            shown: r.shown,
            status_class,
            status_label,
            status: r.status,
            requested_by: r.requested_by,
            created_at: r.created_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
            cancel_requested: r.cancel_requested,
            trigger: r.trigger,
            scheduled_for: r.scheduled_for,
            scheduled_input: String::new(),
            vv_state: r.vv_state,
            vv_window_id: r.vv_window_id,
        }
    }

    pub fn active(&self) -> bool {
        self.status == runs::QUEUED || self.status == runs::RUNNING
    }

    pub fn needs_you(&self) -> bool {
        self.status == runs::ROLLBACK_FAILED
    }

    /// The reasons no snapshot was taken, as one line.
    pub fn no_snapshot_text(&self) -> String {
        self.no_snapshot
            .as_ref()
            .map(|why| why.join("; "))
            .unwrap_or_default()
    }
}

pub struct StepView {
    pub label: &'static str,
    pub status: String,
    pub status_class: &'static str,
    pub status_label: &'static str,
    pub duration_ms: i64,
    pub log: String,
    /// Done, but with a warning (the snapshot took nothing).
    pub warning: bool,
    /// The agent's notes in the reader's language.
    pub notes: Vec<String>,
}

impl StepView {
    fn new(s: Step) -> StepView {
        let (warning, notes) = crate::texts::notes(&s.notes);
        let warning = warning && s.status == "ok";
        let (status_class, status_label) = if warning {
            ("warning", i18n::t("snap.step_warning"))
        } else {
            step_look(&s.status)
        };
        StepView {
            label: step_label(&s.step),
            status: if warning { "warning".into() } else { s.status },
            status_class,
            status_label,
            duration_ms: s.duration_ms,
            log: s.log,
            warning,
            notes,
        }
    }
}

/// How many finished runs the Overview shows below the running and queued
/// ones.
pub const OVERVIEW_FINISHED: i64 = 8;

/// The Overview's runs: running, then queued, then the newest finished.
pub async fn overview_runs(core: &Core) -> ApiResult<Vec<RunView>> {
    let (rows, tz) = core
        .with_conn(move |c| Ok((runs::overview(c, OVERVIEW_FINISHED)?, schedule::site_tz(c)?)))
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| RunView::new(r).localize(&tz))
        .collect())
}

/// The newest runs (`target_id`: of one target).
pub async fn recent(core: &Core, target_id: Option<i64>, limit: i64) -> ApiResult<Vec<RunView>> {
    let (rows, tz) = core
        .with_conn(move |c| {
            Ok((
                runs::list(
                    c,
                    runs::Filter {
                        target_id,
                        server_id: None,
                        limit,
                    },
                )?,
                schedule::site_tz(c)?,
            ))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| RunView::new(r).localize(&tz))
        .collect())
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

fn form(body: &[u8]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(body) {
        out.entry(k.into_owned()).or_default().push(v.into_owned());
    }
    out
}

fn field<'a>(f: &'a BTreeMap<String, Vec<String>>, key: &str) -> &'a str {
    f.get(key).and_then(|v| v.first()).map_or("", |s| s.trim())
}

/// `raw` when it is a path on this site: one `/` and then not another `/` or
/// a `\` (browsers read `/\host` as `//host`), no control characters or
/// spaces. Tabs and line breaks are removed first, as browsers do before
/// they look at a URL (`/<tab>/host` would be `//host`).
fn local_path(raw: &str) -> Option<String> {
    let path: String = raw
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut chars = path.chars();
    let ok = chars.next() == Some('/')
        && !matches!(chars.next(), Some('/' | '\\'))
        && path.len() <= 2048
        && !path
            .chars()
            .any(|c| c == '\\' || c.is_control() || c.is_whitespace());
    ok.then_some(path)
}

/// Where to go back to: a path on this site, else `fallback`.
fn back(f: &BTreeMap<String, Vec<String>>, fallback: String) -> Redirect {
    match local_path(field(f, "back")) {
        Some(path) => Redirect::to(&path),
        None => Redirect::to(&fallback),
    }
}

fn changed(wren: &Wren) {
    wren.jobs.notify_waiters();
    wren.vv_sync.notify_one();
    let _ = wren.changes.send(());
}

/// A refusal as a flash message.
fn refused(jar: SignedCookieJar, r: Refusal) -> Result<SignedCookieJar, ApiError> {
    match r {
        Refusal::NotFound => Err(ApiError::NotFound),
        r => Ok(flash(jar, "error", r.message())),
    }
}

/// A date-time field (site time zone) as a timestamp.
async fn parse_local(core: &Core, raw: &str) -> ApiResult<Option<jiff::Timestamp>> {
    let tz = core.with_conn(|c| schedule::site_tz(c)).await?;
    Ok(schedule::parse_at(raw, &tz))
}

/// Approves the target's available version for a chosen time.
async fn schedule_one(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let key = field(&f, "key").to_string();
    let to = back(&f, format!("/targets/{id}"));
    let Some(at) = parse_local(&wren.core, field(&f, "at")).await? else {
        return Ok((flash(jar, "error", Refusal::BadTime.message()), to).into_response());
    };
    let who = user.username.clone();
    let res = wren
        .core
        .with_conn(move |c| runs::approve_at(c, id, Some(&key), &who, Some(at)))
        .await?;
    let jar = match res {
        Ok(run) => {
            changed(&wren);
            flash(
                jar,
                "success",
                i18n::tf("runs.scheduled", &[("id", &run.to_string())]),
            )
        }
        Err(r) => refused(jar, r)?,
    };
    Ok((jar, to).into_response())
}

async fn reschedule(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let to = back(&f, format!("/runs/{id}"));
    let Some(at) = parse_local(&wren.core, field(&f, "at")).await? else {
        return Ok((flash(jar, "error", Refusal::BadTime.message()), to).into_response());
    };
    let who = user.username.clone();
    let res = wren
        .core
        .with_conn(move |c| runs::reschedule(c, id, at, &who))
        .await?;
    let jar = match res {
        Ok(()) => {
            changed(&wren);
            flash(jar, "success", i18n::t("runs.rescheduled"))
        }
        Err(r) => refused(jar, r)?,
    };
    Ok((jar, to).into_response())
}

async fn approve_one(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let key = field(&f, "key").to_string();
    let who = user.username.clone();
    let res = wren
        .core
        .with_conn(move |c| runs::approve(c, id, Some(&key), &who))
        .await?;
    let to = back(&f, format!("/targets/{id}"));
    let jar = match res {
        Ok(run) => {
            changed(&wren);
            flash(
                jar,
                "success",
                i18n::tf("runs.approved", &[("id", &run.to_string())]),
            )
        }
        Err(Refusal::NotFound) => return Err(ApiError::NotFound),
        Err(r) => flash(jar, "error", r.message()),
    };
    Ok((jar, to).into_response())
}

/// Approves every ticked target, each for the version shown next to it.
async fn approve_many(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let picks: Vec<(i64, String)> = f
        .get("ids")
        .into_iter()
        .flatten()
        .filter_map(|id| id.parse::<i64>().ok())
        .map(|id| (id, field(&f, &format!("key_{id}")).to_string()))
        .collect();
    if picks.is_empty() {
        return Ok((
            flash(jar, "error", i18n::t("runs.err_none_picked")),
            Redirect::to("/updates"),
        )
            .into_response());
    }
    let who = user.username.clone();
    let results = wren
        .core
        .with_conn(move |c| {
            picks
                .iter()
                .map(|(id, key)| {
                    // Never "whatever is newest now": the version shown.
                    let key = if key.is_empty() { "-" } else { key.as_str() };
                    runs::approve(c, *id, Some(key), &who)
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    // What the agent would refuse is skipped, and counted.
    let blocked = results
        .iter()
        .filter(|r| matches!(r, Err(Refusal::Blocked(_))))
        .count();
    let refused: Vec<String> = results
        .iter()
        .filter_map(|r| r.as_ref().err())
        .filter(|e| !matches!(e, Refusal::Blocked(_)))
        .map(Refusal::message)
        .collect();
    changed(&wren);
    let mut text = i18n::tn("runs.approved_many", ok as i64);
    if blocked > 0 {
        text = format!("{text} {}", i18n::tn("blocked.skipped", blocked as i64));
    }
    let jar = if refused.is_empty() {
        flash(jar, "success", text)
    } else {
        flash(
            jar,
            "error",
            format!(
                "{text} {}",
                i18n::tf("runs.some_refused", &[("why", &refused.join(" "))])
            ),
        )
    };
    Ok((jar, Redirect::to("/updates")).into_response())
}

async fn skip(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let who = user.username.clone();
    let res = wren
        .core
        .with_conn(move |c| runs::skip(c, id, &who))
        .await?;
    let jar = match res {
        Ok(()) => {
            changed(&wren);
            flash(jar, "success", i18n::t("runs.skipped"))
        }
        Err(Refusal::NotFound) => return Err(ApiError::NotFound),
        Err(r) => flash(jar, "error", r.message()),
    };
    Ok((jar, back(&f, format!("/targets/{id}"))).into_response())
}

async fn snooze(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let hours = field(&f, "hours").parse::<i64>().unwrap_or(24);
    let who = user.username.clone();
    let res = wren
        .core
        .with_conn(move |c| runs::snooze(c, id, hours, &who))
        .await?;
    let jar = match res {
        Ok(until) => {
            changed(&wren);
            flash(
                jar,
                "success",
                i18n::tf("runs.snoozed", &[("until", &until)]),
            )
        }
        Err(Refusal::NotFound) => return Err(ApiError::NotFound),
        Err(r) => flash(jar, "error", r.message()),
    };
    Ok((jar, back(&f, format!("/targets/{id}"))).into_response())
}

async fn resume(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let who = user.username.clone();
    if !wren
        .core
        .with_conn(move |c| runs::resume(c, id, &who))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    changed(&wren);
    let jar = flash(jar, "success", i18n::t("runs.resumed"));
    Ok((jar, back(&f, format!("/targets/{id}"))).into_response())
}

/// A mode as a form sends it: `""` (the server's), `approve`, `notify` or
/// `auto`.
pub fn parse_mode(raw: &str) -> Result<Option<&'static str>, String> {
    match raw {
        "" => Ok(None),
        runs::APPROVE => Ok(Some(runs::APPROVE)),
        runs::NOTIFY => Ok(Some(runs::NOTIFY)),
        runs::AUTO => Ok(Some(runs::AUTO)),
        _ => Err(i18n::t("runs.err_mode").to_string()),
    }
}

async fn target_policy(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let to = Redirect::to(&format!("/targets/{id}"));
    let mode = match parse_mode(field(&f, "mode")) {
        Ok(m) => m,
        Err(msg) => return Ok((flash(jar, "error", msg), to).into_response()),
    };
    let health = match field(&f, "health_seconds") {
        "" => None,
        raw => match raw.parse::<i64>() {
            Ok(s) if (runs::MIN_HEALTH..=runs::MAX_HEALTH).contains(&s) => Some(s),
            _ => {
                let msg = i18n::tf(
                    "runs.err_health",
                    &[
                        ("min", &runs::MIN_HEALTH.to_string()),
                        ("max", &runs::MAX_HEALTH.to_string()),
                    ],
                );
                return Ok((flash(jar, "error", msg), to).into_response());
            }
        },
    };
    let settings = runs::TargetSettings {
        mode: mode.map(str::to_string),
        health_seconds: health,
        require_vv: matches!(field(&f, "require_vv"), "true" | "1" | "on"),
        require_snapshot: matches!(field(&f, "require_snapshot"), "true" | "1" | "on"),
    };
    let (who, admin) = (user.username.clone(), user.is_admin());
    let res = wren
        .core
        .with_conn(move |c| runs::set_target_policy(c, id, &settings, &who, admin))
        .await?;
    let jar = match res {
        Ok(()) => {
            changed(&wren);
            flash(jar, "success", i18n::t("runs.policy_saved"))
        }
        Err(r) => refused(jar, r)?,
    };
    Ok((jar, to).into_response())
}

async fn server_mode(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let f = form(&body);
    let to = Redirect::to(&format!("/servers/{id}"));
    let mode = match parse_mode(field(&f, "mode")) {
        Ok(Some(m)) => m,
        _ => return Ok((flash(jar, "error", i18n::t("runs.err_mode")), to).into_response()),
    };
    // The window fields come only with an admin's form.
    let window = if f.contains_key("auto_start") {
        match (field(&f, "auto_start"), field(&f, "auto_end")) {
            ("", "") => Some(None),
            (s, e) => match (schedule::normal_hhmm(s), schedule::normal_hhmm(e)) {
                (Some(s), Some(e)) if s != e => Some(Some((s, e))),
                _ => {
                    return Ok(
                        (flash(jar, "error", i18n::t("runs.err_window")), to).into_response()
                    );
                }
            },
        }
    } else {
        None
    };
    let (who, admin) = (user.username.clone(), user.is_admin());
    let res = wren
        .core
        .with_conn(move |c| runs::set_server_mode(c, id, mode, window, &who, admin))
        .await?;
    let jar = match res {
        Ok(()) => {
            changed(&wren);
            flash(jar, "success", i18n::t("runs.policy_saved"))
        }
        Err(r) => refused(jar, r)?,
    };
    Ok((jar, to).into_response())
}

async fn cancel(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_editor()?;
    let who = user.username.clone();
    let res = wren
        .core
        .with_conn(move |c| runs::cancel(c, id, &who))
        .await?;
    let jar = match res {
        Ok(()) => {
            changed(&wren);
            flash(jar, "success", i18n::t("runs.cancelled"))
        }
        Err(Refusal::NotFound) => return Err(ApiError::NotFound),
        Err(r) => flash(jar, "error", r.message()),
    };
    Ok((jar, Redirect::to(&format!("/runs/{id}"))).into_response())
}

async fn rollback(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_editor()?;
    let who = user.username.clone();
    let res = wren
        .core
        .with_conn(move |c| runs::request_rollback(c, id, &who))
        .await?;
    match res {
        Ok(run) => {
            changed(&wren);
            let jar = flash(jar, "success", i18n::t("runs.rollback_queued"));
            Ok((jar, Redirect::to(&format!("/runs/{run}"))).into_response())
        }
        Err(Refusal::NotFound) => Err(ApiError::NotFound),
        Err(r) => Ok((
            flash(jar, "error", r.message()),
            Redirect::to(&format!("/runs/{id}")),
        )
            .into_response()),
    }
}

// ---------------------------------------------------------------------------
// The run page
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "run.html")]
struct RunPage {
    ctx: PageCtx,
    body: RunBody,
    back_href: String,
}

#[derive(Template)]
#[template(path = "partials/run_body.html")]
struct RunBody {
    run: RunView,
    steps: Vec<StepView>,
    can_act: bool,
    can_roll_back: bool,
}

async fn body(core: &Core, id: i64, can_act: bool) -> ApiResult<RunBody> {
    let (run, steps, newest, tz) = core
        .with_conn(move |c| {
            let run = runs::get(c, id)?;
            let steps = runs::steps(c, id)?;
            let newest: Option<i64> = match run.as_ref().and_then(|r| r.target_id) {
                Some(t) => c.query_row(
                    "SELECT MAX(id) FROM runs WHERE target_id = ?1 AND kind = 'update' AND status = 'succeeded'",
                    [t],
                    |r| r.get(0),
                )?,
                None => None,
            };
            Ok((run, steps, newest, schedule::site_tz(c)?))
        })
        .await?;
    let run = RunView::new(run.ok_or(ApiError::NotFound)?).localize(&tz);
    let can_roll_back =
        can_act && !run.is_rollback && run.status == runs::SUCCEEDED && newest == Some(run.id);
    Ok(RunBody {
        run,
        steps: steps.into_iter().map(StepView::new).collect(),
        can_act,
        can_roll_back,
    })
}

async fn run_page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let body = body(&core, id, user.can_edit()).await?;
    let title = i18n::tf(
        "runs.title",
        &[("id", &id.to_string()), ("name", &body.run.target_name)],
    );
    let (ctx, jar) = PageCtx::build(&core, Some(&user), &jar, title, "updates", "").await;
    let back_href = match body.run.target_id {
        Some(t) => format!("/targets/{t}"),
        None => "/updates".to_string(),
    };
    Ok((
        jar,
        HtmlTemplate(RunPage {
            ctx,
            body,
            back_href,
        }),
    ))
}

static STREAMS: StreamLimit = StreamLimit::new(64);
static RENDERS: LazyLock<SharedRenders<(i64, bool, Lang)>> =
    LazyLock::new(|| SharedRenders::new(64));

/// The run page's body, live: re-rendered when a step is reported.
async fn run_live(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Response {
    let Some(slot) = STREAMS.acquire() else {
        return live::busy();
    };
    let lang = i18n::current();
    let can_act = user.can_edit();
    let changes = Changes::all(wren.changes.subscribe())
        .quiet(Duration::from_millis(300))
        .lifetime(Duration::from_secs(60 * 60));
    live::sse(futures_util::stream::unfold(
        (changes, wren, slot),
        move |(mut changes, wren, slot)| async move {
            loop {
                let since = changes.next().await?;
                let core = wren.core.clone();
                let render = RENDERS.get((id, can_act, lang), since, || async move {
                    body(&core, id, can_act)
                        .await?
                        .render()
                        .map_err(ApiError::internal)
                });
                if let Ok(html) = i18n::scope(lang, render).await {
                    return Some((
                        live::html_event("run-updated", &html),
                        (changes, wren, slot),
                    ));
                }
            }
        },
    ))
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "schedules.html")]
struct SchedulesPage {
    ctx: PageCtx,
    tz: String,
    runs: Vec<RunView>,
    servers: Vec<schedule::ServerAuto>,
    can_act: bool,
}

impl SchedulesPage {
    pub fn mode_label(&self, mode: &str) -> &'static str {
        i18n::t(match mode {
            runs::AUTO => "runs.mode_auto",
            runs::NOTIFY => "runs.mode_notify",
            _ => "runs.mode_approve",
        })
    }
}

/// Upcoming scheduled updates, and per server its auto window and the
/// targets that update on their own (design §9).
async fn schedules_page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    let (runs, servers, tz, tz_name) = core
        .with_conn(|c| {
            Ok((
                runs::scheduled(c)?,
                schedule::servers_auto(c, jiff::Timestamp::now())?,
                schedule::site_tz(c)?,
                schedule::site_tz_name(c)?,
            ))
        })
        .await?;
    let (ctx, jar) = PageCtx::build(
        &core,
        Some(&user),
        &jar,
        i18n::t("schedules.title"),
        "schedules",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(SchedulesPage {
            ctx,
            tz: tz_name,
            runs: runs
                .into_iter()
                .map(|r| RunView::new(r).localize(&tz))
                .collect(),
            servers,
            can_act: user.can_edit(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_only_goes_to_paths_on_this_site() {
        for ok in ["/", "/targets/3", "/servers?x=1#y", "/a/b"] {
            assert_eq!(local_path(ok).as_deref(), Some(ok), "{ok}");
        }
        // A tab or line break inside is removed, not kept.
        assert_eq!(local_path("/ta\tb").as_deref(), Some("/tab"));
        for bad in [
            "",
            "evil.example",
            "https://evil.example/",
            "//evil.example",
            "/\\evil.example",
            "/\t/evil.example",
            "/\n/evil.example",
            "/\r\n\\evil.example",
            "/ /evil.example",
            "/a\\b",
            "/a\u{0}b",
            "/a b",
            "javascript:alert(1)",
        ] {
            assert_eq!(local_path(bad), None, "{bad:?}");
        }
        assert_eq!(local_path(&format!("/{}", "a".repeat(2048))), None);
    }
}
