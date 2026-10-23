//! Targets in the web UI (design §9): the Updates page (everything with a
//! new version), a target's page, the Targets card on a server's page,
//! "Check now" and the check intervals. Everyone signed in sees them;
//! editors and admins check and change intervals.

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::SignedCookieJar;
use vole_kit::auth::CurrentUser;
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::{self, Tr as _};
use vole_kit::page::{PageCtx, flash};
use vole_kit::state::Core;

use crate::Wren;
use crate::inventory::{COMPOSE, CONTAINER, SCRIPT, UNRAID};
use crate::registry;
use crate::targets::{self, Filter, Image, NOT_CHECKED, REGISTRY, Target};
use crate::updates_notify::kind_label;

/// Shortest and longest check interval (hours).
pub const MIN_HOURS: i64 = 1;
pub const MAX_HOURS: i64 = 168;

pub fn router() -> Router<Core> {
    Router::new()
        .route("/updates", get(updates))
        .route("/targets/{id}", get(target_page))
        .route("/targets/{id}/check", post(check_target))
        .route("/targets/{id}/interval", post(target_interval))
        .route("/servers/{id}/check", post(check_server))
        .route("/servers/{id}/interval", post(server_interval))
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

/// A target as the pages show it.
pub struct TargetView {
    pub id: i64,
    pub server_id: i64,
    pub server_name: String,
    pub kind: String,
    pub kind_label: &'static str,
    pub name: String,
    pub status: String,
    pub status_class: &'static str,
    pub status_label: &'static str,
    /// Why the status is unknown, in words ('' otherwise).
    pub why: String,
    pub current: String,
    pub available: String,
    pub last_checked_at: Option<String>,
    pub check_requested: bool,
    pub files_reachable: bool,
    pub working_dir: String,
    pub notes_url: String,
    pub notes_title: String,
    pub installed_version: String,
    pub latest_version: String,
    pub version_error: String,
    pub version_source: String,
    pub source_url: String,
    pub interval: String,
    pub server_interval: i64,
}

/// The status's design-system class and label.
pub fn status_look(status: &str) -> (&'static str, &'static str) {
    match status {
        targets::UPDATE => ("update", i18n::t("targets.status_update")),
        targets::CURRENT => ("up", i18n::t("targets.status_current")),
        _ => ("unknown", i18n::t("targets.status_unknown")),
    }
}

/// Why an image (or target) is unknown, in words.
pub fn why_unknown(detail: &str) -> String {
    match detail {
        "" => String::new(),
        registry::PINNED => i18n::t("targets.why_pinned").to_string(),
        registry::IMAGE_ID => i18n::t("targets.why_image_id").to_string(),
        registry::LOCAL => i18n::t("targets.why_local").to_string(),
        NOT_CHECKED => i18n::t("targets.why_not_checked").to_string(),
        other => match other.strip_prefix(&format!("{REGISTRY}: ")) {
            Some(error) => i18n::tf("targets.why_registry", &[("error", error)]),
            None => other.to_string(),
        },
    }
}

impl TargetView {
    pub fn new(t: Target) -> TargetView {
        let (status_class, status_label) = status_look(&t.status);
        let current = match t.kind.as_str() {
            CONTAINER | UNRAID => t.image.clone(),
            _ if !t.installed_version.is_empty() => t.installed_version.clone(),
            _ => i18n::tf(
                "targets.image_count",
                &[("count", &t.image_count.to_string())],
            ),
        };
        let source = match serde_json::from_str::<crate::versions::Source>(&t.version_source) {
            Ok(s) if !s.github.is_empty() => format!("GitHub · {}", s.github),
            Ok(s) if !s.url.is_empty() => s.url,
            Ok(s) if s.command => i18n::t("targets.source_command").to_string(),
            _ => String::new(),
        };
        TargetView {
            id: t.id,
            server_id: t.server_id,
            server_name: t.server_name,
            kind_label: kind_label(&t.kind),
            kind: t.kind,
            name: t.name,
            why: why_unknown(&t.status_detail),
            status: t.status,
            status_class,
            status_label,
            current,
            available: t.available,
            last_checked_at: t.last_checked_at,
            check_requested: t.check_requested,
            files_reachable: t.files_reachable,
            working_dir: t.working_dir,
            notes_url: t.notes_url,
            notes_title: t.notes_title,
            installed_version: t.installed_version,
            latest_version: t.latest_version,
            version_error: t.version_error,
            version_source: source,
            source_url: t.source_url,
            interval: t
                .check_interval_hours
                .map(|h| h.to_string())
                .unwrap_or_default(),
            server_interval: t.server_interval_hours,
        }
    }

    /// A Compose or script stack whose files the agent can't see.
    pub fn files_missing(&self) -> bool {
        (self.kind == COMPOSE || self.kind == SCRIPT) && !self.files_reachable
    }

    pub fn is_update(&self) -> bool {
        self.status == targets::UPDATE
    }
}

/// An image row on a target's page.
pub struct ImageView {
    pub container: String,
    pub service: String,
    pub image: String,
    pub state: String,
    pub local: String,
    pub remote: String,
    pub status: String,
    pub status_class: &'static str,
    pub status_label: &'static str,
    pub why: String,
}

impl ImageView {
    fn new(i: Image) -> ImageView {
        let (status_class, status_label) = status_look(&i.status);
        ImageView {
            container: i.container,
            service: i.service,
            image: i.image,
            state: i.state,
            local: targets::short(&i.local_digest),
            remote: targets::short(&i.remote_digest),
            why: why_unknown(&i.detail),
            status: i.status,
            status_class,
            status_label,
        }
    }
}

/// A server's targets for its page.
pub async fn server_targets(core: &Core, server_id: i64) -> ApiResult<Vec<TargetView>> {
    let rows = core
        .with_conn(move |c| {
            targets::list(
                c,
                Filter {
                    server_id: Some(server_id),
                    updates_only: false,
                },
            )
        })
        .await?;
    Ok(rows.into_iter().map(TargetView::new).collect())
}

/// How many targets have an update (the Overview).
pub async fn update_count(core: &Core) -> ApiResult<i64> {
    core.with_conn(|c| targets::update_count(c)).await
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "updates.html")]
struct UpdatesPage {
    ctx: PageCtx,
    rows: Vec<TargetView>,
}

async fn updates(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    let rows = core
        .with_conn(|c| {
            targets::list(
                c,
                Filter {
                    server_id: None,
                    updates_only: true,
                },
            )
        })
        .await?;
    let (ctx, jar) = PageCtx::build(
        &core,
        Some(&user),
        &jar,
        i18n::t("updates.title"),
        "updates",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(UpdatesPage {
            ctx,
            rows: rows.into_iter().map(TargetView::new).collect(),
        }),
    ))
}

#[derive(Template)]
#[template(path = "target.html")]
struct TargetPage {
    ctx: PageCtx,
    target: TargetView,
    images: Vec<ImageView>,
    can_check: bool,
    back_href: String,
}

async fn load(core: &Core, id: i64) -> ApiResult<Target> {
    core.with_conn(move |c| targets::get(c, id))
        .await?
        .ok_or(ApiError::NotFound)
}

async fn target_page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let target = TargetView::new(load(&core, id).await?);
    let back_href = format!("/servers/{}", target.server_id);
    let images = core
        .with_conn(move |c| targets::images(c, id))
        .await?
        .into_iter()
        .map(ImageView::new)
        .collect();
    let (ctx, jar) =
        PageCtx::build(&core, Some(&user), &jar, target.name.clone(), "servers", "").await;
    Ok((
        jar,
        HtmlTemplate(TargetPage {
            ctx,
            target,
            images,
            can_check: user.can_edit(),
            back_href,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async fn request(wren: &Wren, server_id: i64, target_id: Option<i64>) -> ApiResult<()> {
    wren.core
        .with_conn(move |c| targets::request_check(c, server_id, target_id))
        .await?;
    // Wakes the scheduler; it waits for the agent's fresh scan when the
    // agent is online.
    wren.checks.notify_one();
    let _ = wren.changes.send(());
    Ok(())
}

async fn check_server(
    State(core): State<Core>,
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_editor()?;
    let server = core
        .with_conn(move |c| crate::db::server(c, id))
        .await?
        .ok_or(ApiError::NotFound)?;
    request(&wren, id, None).await?;
    tracing::info!(user = %user.username, server_id = id, "check requested");
    let jar = flash(
        jar,
        "success",
        i18n::tf("targets.checking", &[("name", &server.name)]),
    );
    Ok((jar, Redirect::to(&format!("/servers/{id}"))).into_response())
}

async fn check_target(
    State(core): State<Core>,
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_editor()?;
    let target = load(&core, id).await?;
    request(&wren, target.server_id, Some(id)).await?;
    tracing::info!(user = %user.username, target_id = id, "check requested");
    let jar = flash(
        jar,
        "success",
        i18n::tf("targets.checking", &[("name", &target.name)]),
    );
    Ok((jar, Redirect::to(&format!("/targets/{id}"))).into_response())
}

/// The `hours` field: `Ok(None)` when empty, else 1–168.
pub fn parse_hours(body: &[u8]) -> Result<Option<i64>, String> {
    let raw = url::form_urlencoded::parse(body)
        .find(|(k, _)| k == "hours")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<i64>() {
        Ok(h) if (MIN_HOURS..=MAX_HOURS).contains(&h) => Ok(Some(h)),
        _ => Err(i18n::tf(
            "targets.err_hours",
            &[
                ("min", &MIN_HOURS.to_string()),
                ("max", &MAX_HOURS.to_string()),
            ],
        )),
    }
}

async fn server_interval(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let back = Redirect::to(&format!("/servers/{id}"));
    let hours = match parse_hours(&body) {
        Ok(Some(h)) => h,
        Ok(None) => crate::db::DEFAULT_CHECK_HOURS,
        Err(msg) => return Ok((flash(jar, "error", msg), back).into_response()),
    };
    if !core
        .with_conn(move |c| targets::set_server_interval(c, id, hours))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    let jar = flash(jar, "success", i18n::t("targets.interval_saved"));
    Ok((jar, back).into_response())
}

async fn target_interval(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let back = Redirect::to(&format!("/targets/{id}"));
    let hours = match parse_hours(&body) {
        Ok(h) => h,
        Err(msg) => return Ok((flash(jar, "error", msg), back).into_response()),
    };
    if !core
        .with_conn(move |c| targets::set_interval(c, id, hours))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    let jar = flash(jar, "success", i18n::t("targets.interval_saved"));
    Ok((jar, back).into_response())
}
