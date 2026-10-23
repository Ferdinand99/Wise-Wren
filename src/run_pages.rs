//! Runs in the web UI (design §9): approving, skipping and snoozing updates
//! (one or several), a target's mode and health period, the run page with
//! each step live, cancelling a run before anything stops and rolling back
//! a successful one. Everyone signed in sees runs; editors and admins act,
//! and every action is stored with who did it.

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
use crate::updates_notify::kind_label;

pub fn router() -> Router<Core> {
    Router::new()
        .route("/updates/approve", post(approve_many))
        .route("/targets/{id}/approve", post(approve_one))
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
    pub detail: String,
    pub requested_by: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub cancel_requested: bool,
}

impl RunView {
    pub fn new(r: Run) -> RunView {
        let (status_class, status_label) = run_look(&r.status);
        RunView {
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
            detail: r.detail,
            requested_by: r.requested_by,
            created_at: r.created_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
            cancel_requested: r.cancel_requested,
        }
    }

    pub fn active(&self) -> bool {
        self.status == runs::QUEUED || self.status == runs::RUNNING
    }

    pub fn needs_you(&self) -> bool {
        self.status == runs::ROLLBACK_FAILED
    }
}

pub struct StepView {
    pub label: &'static str,
    pub status: String,
    pub status_class: &'static str,
    pub status_label: &'static str,
    pub duration_ms: i64,
    pub log: String,
}

impl StepView {
    fn new(s: Step) -> StepView {
        let (status_class, status_label) = step_look(&s.status);
        StepView {
            label: step_label(&s.step),
            status: s.status,
            status_class,
            status_label,
            duration_ms: s.duration_ms,
            log: s.log,
        }
    }
}

/// The newest runs (`target_id`: of one target).
pub async fn recent(core: &Core, target_id: Option<i64>, limit: i64) -> ApiResult<Vec<RunView>> {
    let rows = core
        .with_conn(move |c| {
            runs::list(
                c,
                runs::Filter {
                    target_id,
                    server_id: None,
                    limit,
                },
            )
        })
        .await?;
    Ok(rows.into_iter().map(RunView::new).collect())
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

/// Where to go back to: a path on this site, else `fallback`.
fn back(f: &BTreeMap<String, Vec<String>>, fallback: String) -> Redirect {
    let b = field(f, "back");
    if b.starts_with('/') && !b.starts_with("//") && !b.contains('\\') {
        Redirect::to(b)
    } else {
        Redirect::to(&fallback)
    }
}

fn changed(wren: &Wren) {
    wren.jobs.notify_waiters();
    let _ = wren.changes.send(());
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
    let refused: Vec<String> = results
        .iter()
        .filter_map(|r| r.as_ref().err().map(|e| e.message()))
        .collect();
    changed(&wren);
    let jar = if refused.is_empty() {
        flash(jar, "success", i18n::tn("runs.approved_many", ok as i64))
    } else {
        flash(
            jar,
            "error",
            format!(
                "{} {}",
                i18n::tn("runs.approved_many", ok as i64),
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

/// A mode as a form sends it: `""` (the server's), `approve` or `notify`.
fn parse_mode(raw: &str) -> Result<Option<&'static str>, String> {
    match raw {
        "" => Ok(None),
        runs::APPROVE => Ok(Some(runs::APPROVE)),
        runs::NOTIFY => Ok(Some(runs::NOTIFY)),
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
    let who = user.username.clone();
    if !wren
        .core
        .with_conn(move |c| runs::set_target_policy(c, id, mode, health, &who))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    changed(&wren);
    Ok((flash(jar, "success", i18n::t("runs.policy_saved")), to).into_response())
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
    let who = user.username.clone();
    if !wren
        .core
        .with_conn(move |c| runs::set_server_mode(c, id, mode, &who))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    changed(&wren);
    Ok((flash(jar, "success", i18n::t("runs.policy_saved")), to).into_response())
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
    let (run, steps, newest) = core
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
            Ok((run, steps, newest))
        })
        .await?;
    let run = RunView::new(run.ok_or(ApiError::NotFound)?);
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
