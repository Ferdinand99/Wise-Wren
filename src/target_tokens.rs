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
//! | `POST /api/v1/targets/{id}/approve` | the same, and the user the token belongs to is an editor or admin |
//! | `GET /api/v1/updates` | always, but the answer holds only the token's target |
//! | `GET /api/v1/runs/{id}` | the run belongs to the token's target |
//!
//! Everything else answers 403: other targets and runs, servers, settings,
//! the lists, every other action, the push routes, the reference pages.
//! `/mcp` is out of reach too (a token that can't read can't use MCP). A
//! token with the scope but no target (the token came from the plain API tokens
//! page), or whose target is missing from the inventory for now, is refused
//! everywhere; the binding is by (server, kind, name) like `target_memory`, so
//! the token works again when the target returns.
//!
//! The approval is logged and stored as `<user> via token #<id> "<name>"`, so the
//! actions list and the run's "requested by" name the token (the id tells two
//! tokens of the same name apart).
//!
//! Making a token asks for the creator's password, like the API tokens page
//! does for tokens that outlast a stolen session.
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
use vole_kit::db::{api_tokens, users as db_users};
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

/// The kit's cap of API tokens per user (`MAX_TOKENS_PER_USER`); a target token
/// counts against it with the others.
const MAX_TOKENS_PER_USER: usize = 50;
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
        (&Method::POST, ["targets", id, "approve"]) => {
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
    // The token's target, found by its identity as it is now (none while
    // it is missing from the inventory), and the token's name.
    let found = core
        .with_conn(move |c| {
            c.query_row(
                "SELECT t.id, k.name FROM api_token_targets b
                 JOIN api_tokens k ON k.id = b.token_id
                 LEFT JOIN targets t ON t.server_id = b.server_id AND t.kind = b.kind AND t.name = b.name
                 WHERE b.token_id = ?1",
                [token_id],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
        })
        .await;
    let (target_id, name) = match found {
        Ok(Some((Some(target), name))) => (target, name),
        Ok(_) => return refused(token_id, &request),
        Err(e) => return e.into_response(),
    };
    let route = classify(request.method(), request.uri().path());
    let writes = matches!(route, Route::Target { write: true, .. });
    let allowed = match route {
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
    // The one write a target token may make is an approval, and for that it
    // is an editor (it was checked above that the user is one); it never is
    // more than the user it belongs to. The name is what the log and the run
    // record: the token's id tells tokens of one name apart, and no quote or
    // control character in the name can fake another entry.
    if writes {
        user.role = Role::Editor;
    }
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '"' { '\'' } else { c })
        .collect();
    user.username = format!("{} via token #{token_id} \"{name}\"", user.username);
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
    /// Its target is missing from the inventory for now.
    gone: bool,
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
            "SELECT t.id, t.name, t.prefix, u.username, s.name, b.name, t.last_used_at, t.expires_at,
                    (t.expires_at IS NOT NULL AND t.expires_at <= datetime('now')),
                    NOT EXISTS (SELECT 1 FROM targets g WHERE g.server_id = b.server_id
                                  AND g.kind = b.kind AND g.name = b.name)
             FROM api_token_targets b
             JOIN api_tokens t ON t.id = b.token_id
             JOIN users u ON u.id = t.user_id
             JOIN servers s ON s.id = b.server_id
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
                gone: r.get(9)?,
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
    /// The creator's current password: a target token can approve updates,
    /// so it outlasts a stolen session like a write token does.
    #[serde(default)]
    current_password: String,
}

/// Checks the signed-in user's password, as the kit does for tokens that
/// outlast a stolen session (its helper isn't public, but the pieces are):
/// the same per-account budget of wrong guesses (`reauth:<id>`), counted
/// before the password is checked. `Err` is the text for the form.
async fn confirm_password(
    core: &Core,
    user: &CurrentUser,
    password: String,
) -> ApiResult<Result<(), String>> {
    if password.is_empty() {
        return Ok(Err(i18n::t("sec.reauth_needed").into()));
    }
    let key = format!("reauth:{}", user.id);
    if let Err(wait) = core.limits.account.reserve(&key, std::time::Instant::now()) {
        let secs = wait.as_secs().max(1).to_string();
        return Ok(Err(i18n::tf("sec.too_many_secs", &[("n", &secs)])));
    }
    let id = user.id;
    let record = match core.with_conn(move |c| db_users::get_user(c, id)).await {
        Ok(Some(record)) => record,
        other => {
            // Not a guess: don't count it.
            core.limits.account.release(&key);
            return Err(other.err().unwrap_or(ApiError::Unauthorized));
        }
    };
    if password.len() <= vole_kit::auth::MAX_PASSWORD_LEN
        && vole_kit::auth::verify_password_async(record.password_hash, password).await?
    {
        core.limits.account.clear(&key);
        Ok(Ok(()))
    } else {
        // Already counted by `reserve`.
        Ok(Err(i18n::t("sec.wrong_password").into()))
    }
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
    // The target's identity, which is what the token is bound to.
    let target = match target_id {
        Some(id) => core.with_conn(move |c| targets::get(c, id)).await?,
        None => None,
    };
    let uid = user.id;
    let count = core
        .with_conn(move |c| api_tokens::list_for_user(c, uid))
        .await?
        .len();
    let error = if name.is_empty() || name.chars().count() > 80 {
        Some(i18n::t("tokens.err_name").to_string())
    } else if target.is_none() {
        Some(i18n::t("target_tokens.err_target").to_string())
    } else if expires.is_none() {
        Some(i18n::t("tokens.err_expiry").to_string())
    } else if count >= MAX_TOKENS_PER_USER {
        Some(i18n::t("tokens.err_limit").to_string())
    } else {
        // Last, so that a form that is wrong anyway doesn't cost a guess.
        confirm_password(&core, &user, form.current_password)
            .await?
            .err()
    };
    let (Some(target), None) = (target, &error) else {
        let (jar, response) = render(
            &core,
            &user,
            jar,
            None,
            error.unwrap_or_default(),
            (form.name, target_id.unwrap_or(0)),
        )
        .await?;
        let mut response = response;
        *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
        return Ok((jar, response).into_response());
    };
    let expires_in = expires.flatten();
    let token = tokens::generate();
    let (hash, prefix, stored_name) = (
        tokens::hash(&token),
        tokens::display_prefix(&token),
        name.clone(),
    );
    let (server_id, kind, target_name) = (target.server_id, target.kind, target.name);
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
            "INSERT INTO api_token_targets (token_id, server_id, kind, name) VALUES (?1, ?2, ?3, ?4)",
            params![id, server_id, kind, target_name],
        )?;
        tx.commit()
    })
    .await?;
    tracing::info!(user = %user.username, token = %name, target_id = target.id, "target token created");
    let jar = flash(jar, "success", i18n::t("tokens.created"));
    let created = Created {
        token,
        name,
        target_id: target.id,
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
        assert_eq!(classify(&M::GET, "/api/v1/updates"), Route::Updates);
        assert_eq!(classify(&M::GET, "/api/v1/runs/12"), Route::Run(12));
        for (method, path) in [
            (M::GET, "/api/v1/targets"),
            (M::GET, "/api/v1/targets/"),
            (M::GET, "/api/v1/targets/7/"),
            (M::GET, "/api/v1/targets/7/approve"),
            (M::POST, "/api/v1/targets/7"),
            (M::POST, "/api/v1/targets/7/schedule"),
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
