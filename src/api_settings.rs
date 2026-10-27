//! REST v1 (and MCP) for what the settings pages and the server and target
//! pages do besides updates (design §9): servers and their agent tokens,
//! "Check now" and check intervals, registry credentials, the Vigilant Vole
//! connection and monitor links, notification channels.
//!
//! The rules are the pages' (design §8): viewers read, editors check, set
//! intervals, admins manage notification channels and add and remove
//! servers, agent tokens, registries, the Vigilant Vole connection and
//! monitor links. Writes need an API token with write access, never a
//! session cookie (these paths skip the CSRF check). Secrets are
//! write-only: an agent token is shown once in the answer that made it,
//! registry passwords, the VV token and channel secrets never come back.

use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::http::HeaderMap;
use axum::{Json, extract::State};
use serde_json::{Value, json};
use vole_kit::auth::{AuthMethod, CurrentUser};
use vole_kit::http::{ApiError, ApiResult};
use vole_kit::state::Core;

use crate::Wren;
use crate::api::{Caller, writer};
use crate::db;

fn json_body(body: &Bytes) -> Result<Value, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("not JSON: {e}")))
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// An API writer: a write token of at least an editor.
fn write_caller(user: &CurrentUser, method: &AuthMethod) -> Result<Caller, ApiError> {
    writer(user, method)?;
    Ok(Caller::of(user))
}

// ---------------------------------------------------------------------------
// Servers
// ---------------------------------------------------------------------------

/// A new agent token and the commands that start the agent with it.
async fn token_answer(core: &Core, headers: &HeaderMap, id: i64, token: &str) -> Value {
    let url = vole_kit::docs::instance_url(core.base_url().await, headers);
    let s = crate::agent_api::snippets(&url, token);
    json!({
        "server_id": id,
        "token": token,
        "docker_run": { "unraid": s.unraid, "linux": s.linux },
        "note": "The token is shown only now; Wren keeps its hash.",
    })
}

/// `POST /api/v1/servers` `{"name"}` (admins): the server and its token.
pub async fn add_server(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    let b = json_body(&body)?;
    let name = text(&b, "name").unwrap_or_default().trim().to_string();
    if let Some(problem) = crate::servers::name_problem(&name) {
        return Err(ApiError::bad_request(problem));
    }
    let token = db::new_agent_token();
    let t = token.clone();
    let id = wren
        .core
        .with_conn(move |c| {
            if db::name_taken(c, &name)? {
                return Ok(None);
            }
            Ok(Some(db::insert_server(c, &name, &t)?))
        })
        .await?
        .ok_or_else(|| ApiError::bad_request(vole_kit::i18n::t("servers.err_name_taken")))?;
    tracing::info!(user = %caller.name, server_id = id, "server added");
    let _ = wren.changes.send(());
    Ok(Json(
        token_answer(&wren.core, &headers, id, &token.token).await,
    ))
}

/// `DELETE /api/v1/servers/{id}` (admins).
pub async fn delete_server(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    if !wren
        .core
        .with_conn(move |c| db::delete_server(c, id))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    tracing::info!(user = %caller.name, server_id = id, "server deleted");
    let _ = wren.changes.send(());
    Ok(Json(json!({ "deleted": true })))
}

/// A new agent token (`replace`) or none (revoke): the old one stops
/// working at once, also for an agent already waiting for a job.
pub async fn set_agent_token(
    wren: &Wren,
    caller: &Caller,
    id: i64,
    replace: bool,
) -> ApiResult<Option<String>> {
    caller.admin()?;
    let token = replace.then(db::new_agent_token);
    let t = token.clone();
    if !wren
        .core
        .with_conn(move |c| db::set_token(c, id, t.as_ref()))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    tracing::info!(
        user = %caller.name,
        server_id = id,
        "{}",
        if replace { "agent token replaced" } else { "agent token revoked" }
    );
    wren.jobs.notify_waiters();
    let _ = wren.changes.send(());
    Ok(token.map(|t| t.token))
}

/// "Check now" for a server or one target (editors): the agent sends a
/// fresh inventory and the check runs as soon as it arrives.
pub async fn check_now(
    wren: &Wren,
    caller: &Caller,
    server_id: Option<i64>,
    target_id: Option<i64>,
) -> ApiResult<Value> {
    caller.editor()?;
    let server_id = match (server_id, target_id) {
        (_, Some(t)) => {
            wren.core
                .with_conn(move |c| crate::targets::get(c, t))
                .await?
                .ok_or(ApiError::NotFound)?
                .server_id
        }
        (Some(s), None) => {
            wren.core
                .with_conn(move |c| db::server(c, s))
                .await?
                .ok_or(ApiError::NotFound)?
                .id
        }
        (None, None) => return Err(ApiError::bad_request("give server_id or target_id")),
    };
    crate::pages::request(wren, server_id, target_id).await?;
    tracing::info!(user = %caller.name, server_id, ?target_id, "check requested");
    Ok(json!({ "checking": true }))
}

/// A server's check interval (1–168 hours; `null`: the default 6) or a
/// target's (`null`: the server's).
pub async fn set_interval(
    wren: &Wren,
    caller: &Caller,
    server_id: Option<i64>,
    target_id: Option<i64>,
    hours: Option<i64>,
) -> ApiResult<Value> {
    caller.editor()?;
    if let Some(h) = hours
        && !(crate::pages::MIN_HOURS..=crate::pages::MAX_HOURS).contains(&h)
    {
        return Err(ApiError::bad_request(vole_kit::i18n::tf(
            "targets.err_hours",
            &[
                ("min", &crate::pages::MIN_HOURS.to_string()),
                ("max", &crate::pages::MAX_HOURS.to_string()),
            ],
        )));
    }
    let found = match (server_id, target_id) {
        (_, Some(t)) => {
            wren.core
                .with_conn(move |c| crate::targets::set_interval(c, t, hours))
                .await?
        }
        (Some(s), None) => {
            let h = hours.unwrap_or(db::DEFAULT_CHECK_HOURS);
            wren.core
                .with_conn(move |c| crate::targets::set_server_interval(c, s, h))
                .await?
        }
        (None, None) => return Err(ApiError::bad_request("give server_id or target_id")),
    };
    if !found {
        return Err(ApiError::NotFound);
    }
    wren.checks.notify_one();
    Ok(json!({ "check_interval_hours": hours }))
}

/// `POST /api/v1/servers/{id}/{token,revoke,check,interval,mode}`.
pub async fn server_action(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    headers: HeaderMap,
    Path((id, action)): Path<(i64, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    let v = match action.as_str() {
        "token" => {
            let token = set_agent_token(&wren, &caller, id, true)
                .await?
                .unwrap_or_default();
            token_answer(&wren.core, &headers, id, &token).await
        }
        "revoke" => {
            set_agent_token(&wren, &caller, id, false).await?;
            json!({ "revoked": true })
        }
        "check" => check_now(&wren, &caller, Some(id), None).await?,
        "interval" => {
            let b = json_body(&body)?;
            let hours = b.get("hours").and_then(Value::as_i64);
            set_interval(&wren, &caller, Some(id), None, hours).await?
        }
        "mode" => {
            let b = crate::api::body_of(&body)?;
            crate::api::server_mode(&wren, &caller.name, caller.is_admin, id, b).await?
        }
        _ => return Err(ApiError::NotFound),
    };
    Ok(Json(v))
}

// ---------------------------------------------------------------------------
// Registries (admins)
// ---------------------------------------------------------------------------

pub async fn registries_list(
    State(core): State<Core>,
    user: CurrentUser,
) -> ApiResult<Json<Value>> {
    user.require_admin()?;
    Ok(Json(crate::registries::api_list(&core).await?))
}

pub async fn registries_add(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    let b = json_body(&body)?;
    Ok(Json(
        crate::registries::api_add(
            &core,
            text(&b, "host").unwrap_or_default(),
            text(&b, "username").unwrap_or_default(),
            text(&b, "password").unwrap_or_default(),
            &caller.name,
        )
        .await?,
    ))
}

pub async fn registry_change(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    let b = json_body(&body)?;
    Ok(Json(
        crate::registries::api_change(
            &core,
            id,
            text(&b, "username"),
            text(&b, "password"),
            &caller.name,
        )
        .await?,
    ))
}

pub async fn registry_delete(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    Ok(Json(
        crate::registries::api_delete(&core, id, &caller.name).await?,
    ))
}

// ---------------------------------------------------------------------------
// Vigilant Vole (admins)
// ---------------------------------------------------------------------------

/// The connection without its token.
pub async fn vv_json(core: &Core) -> ApiResult<Value> {
    let (url, token_saved) = core.with_conn(|c| crate::vv::saved(c)).await?;
    Ok(json!({
        "url": url,
        "token_saved": token_saved,
        "connected": !url.is_empty() && token_saved,
    }))
}

pub async fn vv_get(State(core): State<Core>, user: CurrentUser) -> ApiResult<Json<Value>> {
    user.require_admin()?;
    Ok(Json(vv_json(&core).await?))
}

/// `POST /api/v1/vigilant-vole` `{"url", "token"}`: a missing, empty or
/// masked token keeps the saved one, but only for the same URL.
pub async fn vv_save(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    let b = json_body(&body)?;
    let url = crate::vv::normal_url(text(&b, "url").unwrap_or_default())
        .ok_or_else(|| ApiError::bad_request(vole_kit::i18n::t("vv.err_url")))?;
    let token = text(&b, "token")
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != vole_kit::notify::schema::SECRET_MASK);
    // `save` refuses a missing token for a new URL (the token is bound to
    // the URL it was given for).
    crate::vv::save(&core, &url, token).await?;
    tracing::info!(user = %caller.name, "Vigilant Vole connection saved");
    Ok(Json(vv_json(&core).await?))
}

pub async fn vv_delete(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    crate::vv::remove(&core).await?;
    tracing::info!(user = %caller.name, "Vigilant Vole connection removed");
    Ok(Json(vv_json(&core).await?))
}

/// Test connection: the token, VV's version and monitors.
pub async fn vv_test_json(core: &Core) -> Value {
    match crate::vv::connection(core).await {
        None => {
            json!({ "ok": false, "message": vole_kit::i18n::t("vv.err_not_connected"), "too_old": false })
        }
        Some(conn) => {
            let t = crate::vv::test(core, conn).await;
            json!({ "ok": t.ok, "message": t.message, "too_old": t.too_old })
        }
    }
}

/// `POST /api/v1/vigilant-vole/test` (admins; like every POST, a write
/// token).
pub async fn vv_test(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
) -> ApiResult<Json<Value>> {
    write_caller(&user, &method)?.admin()?;
    Ok(Json(vv_test_json(&core).await))
}

/// The monitors and tags in VV, for linking.
pub async fn vv_monitors(State(core): State<Core>, user: CurrentUser) -> ApiResult<Json<Value>> {
    user.require_admin()?;
    let conn = crate::vv::connection(&core)
        .await
        .ok_or_else(|| ApiError::bad_request(vole_kit::i18n::t("vv.err_not_connected")))?;
    let all = crate::vv::Client::new(&core.http, conn)
        .monitors()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let mut tags: Vec<String> = all.iter().flat_map(|m| m.tags.clone()).collect();
    tags.sort_by_key(|t| t.to_lowercase());
    tags.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(Json(json!({
        "monitors": all.iter().map(|m| json!({"id": m.id, "name": m.name, "tags": m.tags, "active": m.active})).collect::<Vec<_>>(),
        "tags": tags,
    })))
}

/// Links a VV monitor (`monitor_id`) or every monitor with a tag (`tag`)
/// to a target (admins).
pub async fn link_monitor(
    wren: &Wren,
    caller: &Caller,
    target_id: i64,
    monitor_id: Option<i64>,
    tag: Option<&str>,
) -> ApiResult<Value> {
    caller.admin()?;
    wren.core
        .with_conn(move |c| crate::targets::get(c, target_id))
        .await?
        .ok_or(ApiError::NotFound)?;
    let (kind, r#ref) = match (monitor_id, tag.map(str::trim).filter(|t| !t.is_empty())) {
        (Some(id), None) => (crate::vv::LINK_MONITOR, id.to_string()),
        (None, Some(tag)) => (crate::vv::LINK_TAG, tag.to_string()),
        _ => return Err(ApiError::bad_request(vole_kit::i18n::t("vv.err_pick"))),
    };
    let mut label = r#ref.clone();
    if kind == crate::vv::LINK_MONITOR
        && let Some(conn) = crate::vv::connection(&wren.core).await
        && let Ok(all) = crate::vv::Client::new(&wren.core.http, conn)
            .monitors()
            .await
        && let Some(m) = all.iter().find(|m| m.id.to_string() == r#ref)
    {
        label.clone_from(&m.name);
    }
    let who = caller.name.clone();
    wren.core
        .with_conn(move |c| crate::vv::add_link(c, target_id, kind, &r#ref, &label, &who))
        .await?;
    let _ = wren.changes.send(());
    monitors_of(&wren.core, target_id).await
}

/// Removes a monitor link (admins).
pub async fn unlink_monitor(
    wren: &Wren,
    caller: &Caller,
    target_id: i64,
    link_id: i64,
) -> ApiResult<Value> {
    caller.admin()?;
    let who = caller.name.clone();
    if !wren
        .core
        .with_conn(move |c| crate::vv::remove_link(c, target_id, link_id, &who))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    let _ = wren.changes.send(());
    monitors_of(&wren.core, target_id).await
}

/// A target's monitor links.
pub async fn monitors_of(core: &Core, target_id: i64) -> ApiResult<Value> {
    let links = core
        .with_conn(move |c| crate::vv::links(c, target_id))
        .await?;
    Ok(
        json!({ "monitors": links.iter().map(|l| json!({"id": l.id, "kind": l.kind, "ref": l.r#ref, "label": l.label})).collect::<Vec<_>>() }),
    )
}

/// `DELETE /api/v1/targets/{id}/monitors/{link}`.
pub async fn unlink_route(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path((id, link)): Path<(i64, i64)>,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    Ok(Json(unlink_monitor(&wren, &caller, id, link).await?))
}

// ---------------------------------------------------------------------------
// Notification channels (admins)
// ---------------------------------------------------------------------------

pub async fn channels_list(State(core): State<Core>, user: CurrentUser) -> ApiResult<Json<Value>> {
    user.require_admin()?;
    Ok(Json(crate::channels::api_list(&core).await?))
}

pub async fn channel_get(
    State(core): State<Core>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    user.require_admin()?;
    Ok(Json(crate::channels::api_get(&core, id).await?))
}

pub async fn providers(user: CurrentUser) -> ApiResult<Json<Value>> {
    user.require_admin()?;
    Ok(Json(crate::channels::api_providers()))
}

pub async fn channel_add(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    let b = json_body(&body)?;
    Ok(Json(
        crate::channels::api_save(&core, None, &b, &caller.name).await?,
    ))
}

/// `POST /api/v1/notification-channels/{id}/{update,active,test}`.
pub async fn channel_action(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path((id, action)): Path<(i64, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    let b = json_body(&body)?;
    Ok(Json(match action.as_str() {
        "update" => crate::channels::api_save(&core, Some(id), &b, &caller.name).await?,
        "active" => {
            let active = b
                .get("active")
                .and_then(Value::as_bool)
                .ok_or_else(|| ApiError::bad_request("send {\"active\": true|false}"))?;
            crate::channels::api_active(&core, id, active).await?
        }
        "test" => crate::channels::api_test(&core, id).await?,
        _ => return Err(ApiError::NotFound),
    }))
}

pub async fn channel_delete(
    State(core): State<Core>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let caller = write_caller(&user, &method)?;
    caller.admin()?;
    Ok(Json(
        crate::channels::api_delete(&core, id, &caller.name).await?,
    ))
}
