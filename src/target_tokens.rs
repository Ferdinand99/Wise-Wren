//! Target tokens: an API token that may read and approve exactly one target.
//!
//! A service that offers an "Update now" button (Sylo's, for one) needs to
//! approve an update, but if it is compromised its token must not be able
//! to approve or change anything else. A target token has the product
//! scope [`SCOPE_NAME`] (vole-kit's `token_scopes`; it is read-only and
//! does not imply read, so the kit refuses it everywhere outside
//! [`SCOPE`]'s paths) and a row in `api_token_targets` that names its
//! target. [`guard`] then holds it to this list, for every request under
//! `/api/v1`, deny by default:
//!
//! | Route | Allowed when |
//! |---|---|
//! | `GET /api/v1/targets/{id}` | `id` is the token's target |
//! | `POST /api/v1/targets/{id}/approve`, `/schedule` | the same, and the user the token belongs to is an editor or admin |
//! | `GET /api/v1/updates` | always, but the answer holds only the token's target |
//! | `GET /api/v1/runs/{id}` | the run belongs to the token's target |
//!
//! Everything else answers 403: other targets and runs, servers, settings,
//! the lists, every other action, the push routes, the reference pages.
//! `/mcp` is out of reach too (a token that can't read can't use MCP). A
//! token with the scope but no target (the target was deleted, or the token
//! came from the plain API tokens page) is refused everywhere.
//!
//! The approval is logged and stored as `<user> via token "<name>"`, so the
//! actions list and the run's "requested by" name the token.
//!
//! Tokens are made here (Settings → Target tokens), where the target is
//! picked, and revoked here or on the API tokens page.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::cookie::SignedCookieJar;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use vole_kit::auth::{AuthMethod, CurrentUser, ExtraScope, Role, Scopes, TokenScope, tokens};
use vole_kit::db::api_tokens;
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::{self, Tr as _};
use vole_kit::page::{PageCtx, SettingsTab, TabAccess, flash};
use vole_kit::state::Core;

use crate::targets;

/// The scope's name as it is stored with the token.
pub const SCOPE_NAME: &str = "target:approve";

/// The product scope of target tokens. It doesn't imply read: outside
/// `paths` the kit refuses the token before any handler runs. Inside them
/// [`guard`] decides.
pub const SCOPE: ExtraScope = ExtraScope {
    name: SCOPE_NAME,
    label: "target_tokens.scope",
    help: "target_tokens.scope_help",
    implies_read: false,
    paths: &["/api/v1/targets/", "/api/v1/updates", "/api/v1/runs/"],
};

pub const TAB: SettingsTab = SettingsTab {
    id: "target-tokens",
    href: "/settings/target-tokens",
    label: "target_tokens.title",
    icon: "key",
    access: TabAccess::Editors,
};

/// Per-user cap, like the kit's for API tokens.
const MAX_PER_USER: i64 = 50;
/// Allowed expiry choices in days (`None` = never), as on the API tokens page.
const EXPIRY_CHOICES: &[(&str, Option<i64>)] = &[
    ("30", Some(30)),
    ("90", Some(90)),
    ("365", Some(365)),
    ("never", None),
];

pub fn router() -> Router<Core> {
    Router::new().route("/settings/target-tokens", get(page).post(create))
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// Which target the request's token is held to. Inserted into the request
/// extensions for a target token, so a list can leave the rest out.
#[derive(Debug, Clone, Copy)]
pub struct Scoped {
    pub target_id: i64,
}

/// What a request under `/api/v1` asks for, as far as target tokens care.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Target { id: i64, write: bool },
    Updates,
    Run(i64),
    Other,
}

/// A decimal id as the router would read it, nothing else: anything odd
/// (a sign, percent-encoding, a very long number) is `None`, which is
/// refused.
fn id_of(segment: &str) -> Option<i64> {
    if segment.is_empty() || segment.len() > 18 || !segment.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    segment.parse().ok()
}

fn classify(method: &Method, path: &str) -> Route {
    let Some(rest) = path.strip_prefix("/api/v1/") else {
        return Route::Other;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    let found = match (method, parts.as_slice()) {
        (&Method::GET, ["targets", id]) => id_of(id).map(|id| Route::Target { id, write: false }),
        (&Method::POST, ["targets", id, "approve" | "schedule"]) => {
            id_of(id).map(|id| Route::Target { id, write: true })
        }
        (&Method::GET, ["updates"]) => Some(Route::Updates),
        (&Method::GET, ["runs", id]) => id_of(id).map(Route::Run),
        _ => None,
    };
    found.unwrap_or(Route::Other)
}

/// Whether the request carries the target scope (and only then: sessions
/// and write tokens carry every scope, but they are not target tokens).
fn carries(user: &CurrentUser) -> bool {
    matches!(&user.scopes, Scopes::Only(given) if given.contains(&SCOPE_NAME))
}

/// Holds a target token to its one target (see the module). Runs after
/// `require_auth`, on the API routes; every other request passes.
pub async fn guard(State(core): State<Core>, mut request: Request, next: Next) -> Response {
    let Some(mut user) = request.extensions().get::<CurrentUser>().cloned() else {
        return next.run(request).await;
    };
    if !carries(&user) {
        return next.run(request).await;
    }
    let Some(AuthMethod::Token { token_id, .. }) =
        request.extensions().get::<AuthMethod>().copied()
    else {
        return ApiError::Forbidden.into_response();
    };
    let found = core
        .with_conn(move |c| {
            c.query_row(
                "SELECT b.target_id, t.name FROM api_token_targets b
                 JOIN api_tokens t ON t.id = b.token_id WHERE b.token_id = ?1",
                [token_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
        })
        .await;
    let (target_id, name) = match found {
        Ok(Some(bound)) => bound,
        Ok(None) => return refused(token_id, &request),
        Err(e) => return e.into_response(),
    };
    let allowed = match classify(request.method(), request.uri().path()) {
        Route::Target { id, write } if id == target_id => {
            // Approving is an editor's job, whatever the token says.
            !write || user.may_act_as(Role::Editor)
        }
        Route::Updates => true,
        Route::Run(id) => {
            let run = core
                .with_conn(move |c| {
                    c.query_row("SELECT target_id FROM runs WHERE id = ?1", [id], |r| {
                        r.get::<_, Option<i64>>(0)
                    })
                    .optional()
                })
                .await;
            match run {
                Ok(run) => run.flatten() == Some(target_id),
                Err(e) => return e.into_response(),
            }
        }
        _ => false,
    };
    if !allowed {
        return refused(token_id, &request);
    }
    // The routes below ask for an editor when they write: this token is
    // allowed that, for these two actions on its own target (never more than
    // the user it belongs to is). The name is what the log and the run record.
    user.role = Role::Editor;
    user.username = format!("{} via token \"{name}\"", user.username);
    request.extensions_mut().insert(user);
    request.extensions_mut().insert(Scoped { target_id });
    next.run(request).await
}

fn refused(token_id: i64, request: &Request) -> Response {
    tracing::warn!(
        token_id,
        method = %request.method(),
        path = request.uri().path(),
        "a target token asked for more than its target"
    );
    ApiError::Forbidden.into_response()
}

// ---------------------------------------------------------------------------
// Settings → Target tokens
// ---------------------------------------------------------------------------

struct Row {
    id: i64,
    name: String,
    prefix: String,
    user: String,
    target: String,
    last_used_at: Option<String>,
    expires_at: Option<String>,
    expired: bool,
}

struct Choice {
    id: i64,
    label: String,
}

/// A token created in this request, shown once.
struct Created {
    token: String,
    name: String,
    target_id: i64,
}

#[derive(Template)]
#[template(path = "settings_target_tokens.html")]
struct Page {
    ctx: PageCtx,
    rows: Vec<Row>,
    choices: Vec<Choice>,
    created: Option<Created>,
    base_url: String,
    error: String,
    form_name: String,
    form_target: i64,
}

async fn rows(core: &Core, user: &CurrentUser) -> ApiResult<Vec<Row>> {
    let (all, uid) = (user.is_admin(), user.id);
    core.with_conn(move |c| {
        c.prepare(
            "SELECT t.id, t.name, t.prefix, u.username, s.name, g.name, t.last_used_at, t.expires_at,
                    (t.expires_at IS NOT NULL AND t.expires_at <= datetime('now'))
             FROM api_token_targets b
             JOIN api_tokens t ON t.id = b.token_id
             JOIN users u ON u.id = t.user_id
             JOIN targets g ON g.id = b.target_id
             JOIN servers s ON s.id = g.server_id
             WHERE ?1 OR t.user_id = ?2
             ORDER BY t.created_at DESC, t.id DESC",
        )?
        .query_map(params![all, uid], |r| {
            let (server, target): (String, String) = (r.get(4)?, r.get(5)?);
            Ok(Row {
                id: r.get(0)?,
                name: r.get(1)?,
                prefix: r.get(2)?,
                user: r.get(3)?,
                target: format!("{server} / {target}"),
                last_used_at: r.get(6)?,
                expires_at: r.get(7)?,
                expired: r.get(8)?,
            })
        })?
        .collect()
    })
    .await
}

async fn choices(core: &Core) -> ApiResult<Vec<Choice>> {
    core.with_conn(|c| {
        Ok(targets::list(c, targets::Filter::default())?
            .into_iter()
            .map(|t| Choice {
                id: t.id,
                label: format!("{} / {} ({})", t.server_name, t.name, t.kind),
            })
            .collect())
    })
    .await
}

async fn render(
    core: &Core,
    user: &CurrentUser,
    jar: SignedCookieJar,
    created: Option<Created>,
    error: String,
    form: (String, i64),
) -> ApiResult<(SignedCookieJar, Response)> {
    let rows = rows(core, user).await?;
    let choices = choices(core).await?;
    let base_url = core.base_url().await.unwrap_or_default();
    let (ctx, jar) = PageCtx::build(
        core,
        Some(user),
        &jar,
        i18n::t("target_tokens.title"),
        "settings",
        "target-tokens",
    )
    .await;
    let page = Page {
        ctx,
        rows,
        choices,
        created,
        base_url,
        error,
        form_name: form.0,
        form_target: form.1,
    };
    // The page may hold a secret: keep it out of caches and history snapshots.
    let mut response = HtmlTemplate(page).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok((jar, response))
}

async fn page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    user.require_editor()?;
    render(&core, &user, jar, None, String::new(), (String::new(), 0)).await
}

#[derive(Deserialize)]
struct CreateForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    expires: String,
}

async fn create(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<CreateForm>,
) -> ApiResult<Response> {
    user.require_editor()?;
    let name = form.name.trim().to_string();
    let target_id = id_of(form.target.trim());
    let expires = EXPIRY_CHOICES
        .iter()
        .find(|(key, _)| *key == form.expires.trim())
        .map(|(_, days)| *days);
    let exists = match target_id {
        Some(id) => core
            .with_conn(move |c| targets::get(c, id))
            .await?
            .is_some(),
        None => false,
    };
    let uid = user.id;
    let count: i64 = core
        .with_conn(move |c| {
            c.query_row(
                "SELECT COUNT(*) FROM api_token_targets b JOIN api_tokens t ON t.id = b.token_id
                 WHERE t.user_id = ?1",
                [uid],
                |r| r.get(0),
            )
        })
        .await?;
    let error = if name.is_empty() || name.chars().count() > 80 {
        Some(i18n::t("tokens.err_name"))
    } else if !exists {
        Some(i18n::t("target_tokens.err_target"))
    } else if expires.is_none() {
        Some(i18n::t("tokens.err_expiry"))
    } else if count >= MAX_PER_USER {
        Some(i18n::t("tokens.err_limit"))
    } else {
        None
    };
    if let Some(error) = error {
        let (jar, response) = render(
            &core,
            &user,
            jar,
            None,
            error.to_string(),
            (form.name, target_id.unwrap_or(0)),
        )
        .await?;
        let mut response = response;
        *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
        return Ok((jar, response).into_response());
    }
    let (target_id, expires_in) = (target_id.unwrap_or(0), expires.flatten());
    let token = tokens::generate();
    let (hash, prefix, stored_name) = (
        tokens::hash(&token),
        tokens::display_prefix(&token),
        name.clone(),
    );
    core.with_conn(move |c| {
        let tx = c.transaction()?;
        let id = api_tokens::insert(
            &tx,
            uid,
            &stored_name,
            &prefix,
            &hash,
            TokenScope::Read,
            expires_in,
        )?;
        api_tokens::add_scope(&tx, id, SCOPE_NAME)?;
        tx.execute(
            "INSERT INTO api_token_targets (token_id, target_id) VALUES (?1, ?2)",
            params![id, target_id],
        )?;
        tx.commit()
    })
    .await?;
    tracing::info!(user = %user.username, token = %name, target_id, "target token created");
    let jar = flash(jar, "success", i18n::t("tokens.created"));
    let created = Created {
        token,
        name,
        target_id,
    };
    let (jar, response) = render(
        &core,
        &user,
        jar,
        Some(created),
        String::new(),
        (String::new(), 0),
    )
    .await?;
    Ok((jar, response).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_listed_routes_are_classified() {
        use Method as M;
        let target = |id, write| Route::Target { id, write };
        assert_eq!(classify(&M::GET, "/api/v1/targets/7"), target(7, false));
        assert_eq!(
            classify(&M::POST, "/api/v1/targets/7/approve"),
            target(7, true)
        );
        assert_eq!(
            classify(&M::POST, "/api/v1/targets/7/schedule"),
            target(7, true)
        );
        assert_eq!(classify(&M::GET, "/api/v1/updates"), Route::Updates);
        assert_eq!(classify(&M::GET, "/api/v1/runs/12"), Route::Run(12));
        for (method, path) in [
            (M::GET, "/api/v1/targets"),
            (M::GET, "/api/v1/targets/"),
            (M::GET, "/api/v1/targets/7/"),
            (M::GET, "/api/v1/targets/7/approve"),
            (M::POST, "/api/v1/targets/7"),
            (M::POST, "/api/v1/targets/7/skip"),
            (M::POST, "/api/v1/targets/7/mode"),
            (M::POST, "/api/v1/targets/7/monitors"),
            (M::DELETE, "/api/v1/targets/7/monitors/1"),
            (M::HEAD, "/api/v1/targets/7"),
            (M::GET, "/api/v1/targets/%37"),
            (M::GET, "/api/v1/targets/+7"),
            (M::GET, "/api/v1/targets/7%2Fapprove"),
            (M::GET, "/api/v1/targets/-1"),
            (M::GET, "/api/v1/targets/99999999999999999999"),
            (M::POST, "/api/v1/updates"),
            (M::GET, "/api/v1/updates/1"),
            (M::GET, "/api/v1/runs"),
            (M::POST, "/api/v1/runs/1/cancel"),
            (M::POST, "/api/v1/runs/1/rollback"),
            (M::GET, "/api/v1/servers/1"),
            (M::GET, "/mcp"),
            (M::GET, "/api/v1//targets/7"),
        ] {
            assert_eq!(classify(&method, path), Route::Other, "{method} {path}");
        }
    }
}
