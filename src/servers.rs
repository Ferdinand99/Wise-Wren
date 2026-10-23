//! The Overview and Servers pages: the servers and their agents' status
//! (live over server-sent events), adding a server, its agent token (shown
//! once; new token, revoke) and the command that starts its agent. Everyone
//! signed in sees servers; only admins add them or touch tokens (§8).

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
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
use crate::agent_api::{Snippets, snippets};
use crate::db::{self, Server, Status};

/// Longest server name.
pub const NAME_MAX: usize = 60;

pub fn router() -> Router<Core> {
    Router::new()
        .route("/", get(overview))
        .route("/live", get(overview_live))
        .route("/settings", get(settings_home))
        .route("/servers", get(list).post(create))
        .route("/servers/live", get(list_live))
        .route("/servers/new", get(new_form))
        .route("/servers/{id}", get(detail).delete(remove))
        .route("/servers/{id}/token", post(new_token))
        .route("/servers/{id}/revoke", post(revoke))
}

/// The settings tab everyone can see.
async fn settings_home(user: CurrentUser) -> Redirect {
    Redirect::to(if user.is_admin() {
        "/settings/general"
    } else {
        "/settings/security"
    })
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

/// A server as the pages show it.
pub struct ServerView {
    pub id: i64,
    pub name: String,
    pub status: &'static str,
    pub status_class: &'static str,
    pub status_label: &'static str,
    pub last_seen_at: Option<String>,
    pub agent_version: String,
    pub hostname: String,
    pub os: String,
    pub docker_version: String,
    pub docker_error: String,
    pub unraid: bool,
    pub has_token: bool,
    pub token_prefix: String,
    pub created_at: String,
}

impl ServerView {
    fn new(s: Server) -> Self {
        let status: Status = s.status();
        ServerView {
            id: s.id,
            status: status.as_str(),
            status_class: status.class(),
            status_label: i18n::t(status.label_key()),
            name: s.name,
            last_seen_at: s.last_seen_at,
            agent_version: s.agent_version,
            hostname: s.hostname,
            os: s.os,
            docker_version: s.docker_version,
            docker_error: s.docker_error,
            unraid: s.unraid,
            has_token: s.has_token,
            token_prefix: s.token_prefix,
            created_at: s.created_at,
        }
    }

    /// The line under the name in lists: host and Docker, when known.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.hostname.is_empty() {
            parts.push(self.hostname.clone());
        }
        if !self.os.is_empty() {
            parts.push(self.os.clone());
        }
        if !self.docker_version.is_empty() {
            parts.push(format!("Docker {}", self.docker_version));
        }
        parts.join(" · ")
    }
}

/// The numbers on the Overview.
pub struct Counts {
    pub total: usize,
    pub online: usize,
    pub not_online: usize,
    /// Targets with an update.
    pub updates: i64,
}

async fn views(core: &Core) -> ApiResult<Vec<ServerView>> {
    let rows = core.with_conn(|c| db::servers(c)).await?;
    Ok(rows.into_iter().map(ServerView::new).collect())
}

fn counts(rows: &[ServerView], updates: i64) -> Counts {
    let online = rows.iter().filter(|r| r.status == "online").count();
    Counts {
        total: rows.len(),
        online,
        not_online: rows.len() - online,
        updates,
    }
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    ctx: PageCtx,
    rows: Vec<ServerView>,
    counts: Counts,
    can_add: bool,
    runs: Vec<crate::run_pages::RunView>,
    show_target: bool,
}

#[derive(Template)]
#[template(path = "partials/overview_body.html")]
struct OverviewBody {
    rows: Vec<ServerView>,
    counts: Counts,
    can_add: bool,
    runs: Vec<crate::run_pages::RunView>,
    show_target: bool,
}

#[derive(Template)]
#[template(path = "servers.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<ServerView>,
    can_add: bool,
}

#[derive(Template)]
#[template(path = "partials/server_list.html")]
struct ListPartial {
    rows: Vec<ServerView>,
    can_add: bool,
}

#[derive(Template)]
#[template(path = "server_new.html")]
struct NewPage {
    ctx: PageCtx,
    name: String,
    name_error: String,
}

#[derive(Template)]
#[template(path = "server.html")]
struct DetailPage {
    ctx: PageCtx,
    server: ServerView,
    agent: AgentCard,
    delete_url: String,
    delete_question: String,
    targets: Vec<crate::pages::TargetView>,
    can_check: bool,
    /// The server's check interval (hours).
    interval: i64,
    /// The server's mode (approve, notify or auto).
    mode: String,
    inventory_at: Option<String>,
    inventory_error: String,
    is_admin: bool,
    /// The auto window (`HH:MM`, '' when none).
    auto_start: String,
    auto_end: String,
    /// The site's time zone.
    tz: String,
}

#[derive(Template)]
#[template(path = "partials/agent_card.html")]
struct AgentPartial {
    server: ServerView,
    agent: AgentCard,
}

/// The Agent card: the token (once, right after it was made) and how to
/// start the agent with it.
pub struct AgentCard {
    pub is_admin: bool,
    pub token: Option<String>,
    pub snippets: Option<Snippets>,
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

async fn overview(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    let rows = views(&core).await?;
    let (ctx, jar) = PageCtx::build(
        &core,
        Some(&user),
        &jar,
        i18n::t("overview.title"),
        "overview",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(OverviewPage {
            ctx,
            counts: counts(&rows, crate::pages::update_count(&core).await?),
            rows,
            can_add: user.is_admin(),
            runs: crate::run_pages::recent(&core, None, RECENT_RUNS).await?,
            show_target: true,
        }),
    ))
}

async fn list(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    let rows = views(&core).await?;
    let (ctx, jar) = PageCtx::build(
        &core,
        Some(&user),
        &jar,
        i18n::t("servers.title"),
        "servers",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(ListPage {
            ctx,
            rows,
            can_add: user.is_admin(),
        }),
    ))
}

async fn new_form(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    user.require_admin()?;
    Ok(render_new(&core, &user, &jar, String::new(), String::new()).await)
}

async fn render_new(
    core: &Core,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    name: String,
    name_error: String,
) -> (SignedCookieJar, HtmlTemplate<NewPage>) {
    let (ctx, jar) =
        PageCtx::build(core, Some(user), jar, i18n::t("servers.add"), "servers", "").await;
    (
        jar,
        HtmlTemplate(NewPage {
            ctx,
            name,
            name_error,
        }),
    )
}

/// What's wrong with a server name, if anything.
pub fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        Some(i18n::t("servers.err_name").into())
    } else if name.chars().count() > NAME_MAX {
        Some(i18n::tf(
            "servers.err_name_long",
            &[("max", &NAME_MAX.to_string())],
        ))
    } else {
        None
    }
}

fn form_value(body: &[u8], key: &str) -> String {
    url::form_urlencoded::parse(body)
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default()
}

/// Adds a server and shows its page with the new token, once.
async fn create(
    State(core): State<Core>,
    user: CurrentUser,
    headers: HeaderMap,
    jar: SignedCookieJar,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_admin()?;
    let name = form_value(&body, "name");
    let check = name.clone();
    let problem = match name_problem(&name) {
        Some(p) => Some(p),
        None => core
            .with_conn(move |c| db::name_taken(c, &check))
            .await?
            .then(|| i18n::t("servers.err_name_taken").to_string()),
    };
    if let Some(problem) = problem {
        let page = render_new(&core, &user, &jar, name, problem).await;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    let token = db::new_agent_token();
    let (saved, t) = (name.clone(), token.clone());
    let id = core
        .with_conn(move |c| db::insert_server(c, &saved, &t))
        .await?;
    tracing::info!(user = %user.username, server_id = id, "server added");
    let jar = flash(
        jar,
        "success",
        i18n::tf("servers.added", &[("name", &name)]),
    );
    render_detail(&core, &user, &jar, &headers, id, Some(token.token)).await
}

async fn load(core: &Core, id: i64) -> ApiResult<ServerView> {
    core.with_conn(move |c| db::server(c, id))
        .await?
        .map(ServerView::new)
        .ok_or(ApiError::NotFound)
}

async fn agent_card(
    core: &Core,
    user: &CurrentUser,
    headers: &HeaderMap,
    token: Option<String>,
) -> AgentCard {
    let snippets = match &token {
        Some(t) => {
            let url = vole_kit::docs::instance_url(core.base_url().await, headers);
            Some(snippets(&url, t))
        }
        None => None,
    };
    AgentCard {
        is_admin: user.is_admin(),
        token,
        snippets,
    }
}

async fn render_detail(
    core: &Core,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    headers: &HeaderMap,
    id: i64,
    token: Option<String>,
) -> ApiResult<Response> {
    let server = load(core, id).await?;
    let agent = agent_card(core, user, headers, token).await;
    let (ctx, jar) =
        PageCtx::build(core, Some(user), jar, server.name.clone(), "servers", "").await;
    let delete_url = format!("/servers/{id}");
    let delete_question = i18n::tf("servers.delete_q", &[("name", &server.name)]);
    let targets = crate::pages::server_targets(core, id).await?;
    let (interval, inventory_at, inventory_error, mode, auto_start, auto_end, tz) = core
        .with_conn(move |c| {
            let tz = crate::schedule::site_tz_name(c)?;
            c.query_row(
                "SELECT check_interval_hours, inventory_at, inventory_error, mode,
                        COALESCE(auto_start, ''), COALESCE(auto_end, '') FROM servers WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, tz)),
            )
        })
        .await?;
    let page = DetailPage {
        ctx,
        server,
        agent,
        delete_url,
        delete_question,
        targets,
        can_check: user.can_edit(),
        interval,
        mode,
        inventory_at,
        inventory_error,
        is_admin: user.is_admin(),
        auto_start,
        auto_end,
        tz,
    };
    Ok((jar, HtmlTemplate(page)).into_response())
}

async fn detail(
    State(core): State<Core>,
    user: CurrentUser,
    headers: HeaderMap,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    render_detail(&core, &user, &jar, &headers, id, None).await
}

/// A new token for the server: the old one stops working at once. Answers
/// with the Agent card showing the token (htmx swaps it in).
async fn new_token(
    State(core): State<Core>,
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    load(&core, id).await?;
    let token = db::new_agent_token();
    let t = token.clone();
    core.with_conn(move |c| db::set_token(c, id, Some(&t)))
        .await?;
    tracing::info!(user = %user.username, server_id = id, "agent token replaced");
    let _ = wren.changes.send(());
    let server = load(&core, id).await?;
    let agent = agent_card(&core, &user, &headers, Some(token.token)).await;
    Ok(HtmlTemplate(AgentPartial { server, agent }).into_response())
}

fn hx_redirect(to: &str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static("hx-redirect"),
        HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/servers")),
    )
}

async fn revoke(
    State(core): State<Core>,
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let server = load(&core, id).await?;
    core.with_conn(move |c| db::set_token(c, id, None)).await?;
    tracing::info!(user = %user.username, server_id = id, "agent token revoked");
    let _ = wren.changes.send(());
    let jar = flash(
        jar,
        "success",
        i18n::tf("servers.revoked", &[("name", &server.name)]),
    );
    Ok((
        jar,
        [hx_redirect(&format!("/servers/{id}"))],
        StatusCode::OK,
    )
        .into_response())
}

async fn remove(
    State(core): State<Core>,
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let server = load(&core, id).await?;
    core.with_conn(move |c| db::delete_server(c, id)).await?;
    tracing::info!(user = %user.username, server_id = id, "server deleted");
    let _ = wren.changes.send(());
    let jar = flash(
        jar,
        "success",
        i18n::tf("servers.deleted", &[("name", &server.name)]),
    );
    Ok((jar, [hx_redirect("/servers")], StatusCode::OK).into_response())
}

// ---------------------------------------------------------------------------
// Live updates
// ---------------------------------------------------------------------------

/// Runs on the Overview.
pub const RECENT_RUNS: i64 = 8;

/// At most this many live views at once.
static STREAMS: StreamLimit = StreamLimit::new(64);

/// Which live view: the Overview's body or the Servers list.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum View {
    Overview,
    Servers,
}

/// One render per (view, admin or not, language) and change.
static RENDERS: LazyLock<SharedRenders<(View, bool, Lang)>> =
    LazyLock::new(|| SharedRenders::new(64));

/// How long a burst of heartbeats is merged into one update.
pub const LIVE_QUIET: Duration = Duration::from_millis(500);

async fn render_view(core: Core, view: View, can_add: bool) -> Result<String, ApiError> {
    let rows = views(&core).await?;
    let html = match view {
        View::Overview => OverviewBody {
            counts: counts(&rows, crate::pages::update_count(&core).await?),
            rows,
            can_add,
            runs: crate::run_pages::recent(&core, None, RECENT_RUNS).await?,
            show_target: true,
        }
        .render(),
        View::Servers => ListPartial { rows, can_add }.render(),
    };
    html.map_err(ApiError::internal)
}

fn stream(wren: Wren, view: View, can_add: bool) -> Response {
    let Some(slot) = STREAMS.acquire() else {
        return live::busy();
    };
    let lang = i18n::current();
    let changes = Changes::all(wren.changes.subscribe())
        .quiet(LIVE_QUIET)
        .lifetime(Duration::from_secs(60 * 60));
    let event = match view {
        View::Overview => "overview-updated",
        View::Servers => "servers-updated",
    };
    live::sse(futures_util::stream::unfold(
        (changes, wren, slot),
        move |(mut changes, wren, slot)| async move {
            loop {
                let since = changes.next().await?;
                let core = wren.core.clone();
                let render = RENDERS.get((view, can_add, lang), since, || {
                    render_view(core, view, can_add)
                });
                if let Ok(html) = i18n::scope(lang, render).await {
                    return Some((live::html_event(event, &html), (changes, wren, slot)));
                }
            }
        },
    ))
}

async fn overview_live(Extension(wren): Extension<Wren>, user: CurrentUser) -> Response {
    stream(wren, View::Overview, user.is_admin())
}

async fn list_live(Extension(wren): Extension<Wren>, user: CurrentUser) -> Response {
    stream(wren, View::Servers, user.is_admin())
}

/// How often the watcher looks for agents that went quiet.
pub const WATCH_EVERY: Duration = Duration::from_secs(5);

/// Tells the live pages when the set of online servers changes. A heartbeat
/// announces itself; an agent that stops sending doesn't, so this notices
/// when it falls out of the online window.
pub fn spawn_watcher(wren: Wren) {
    tokio::spawn(async move {
        let mut online: Option<HashSet<i64>> = None;
        let mut tick = tokio::time::interval(WATCH_EVERY);
        loop {
            tick.tick().await;
            crate::run_notify::check_agents(&wren).await;
            if check_online(&wren, &mut online).await == Some(true) {
                let _ = wren.changes.send(());
            }
        }
    });
}

/// Compares the online servers with the last look; `Some(true)` when they
/// changed, `None` when the database couldn't be read.
pub async fn check_online(wren: &Wren, last: &mut Option<HashSet<i64>>) -> Option<bool> {
    let now: HashSet<i64> = match wren.core.with_conn(|c| db::online_ids(c)).await {
        Ok(ids) => ids.into_iter().collect(),
        Err(err) => {
            tracing::warn!(%err, "can't read the online servers");
            return None;
        }
    };
    let changed = last.as_ref().is_some_and(|l| *l != now);
    *last = Some(now);
    Some(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        crate::product::install();
        assert!(name_problem("").is_some());
        assert!(name_problem(&"x".repeat(NAME_MAX + 1)).is_some());
        assert!(name_problem("LANBox").is_none());
    }
}
