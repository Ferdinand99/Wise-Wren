//! Settings → Vigilant Vole (admins): the VV address and API token (sealed,
//! never shown again), "Test connection"; and the monitors linked to a
//! target, by monitor or by tag, on the target's page (admins link them).

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
use vole_kit::notify::schema::SECRET_MASK;
use vole_kit::page::{PageCtx, SettingsTab, TabAccess, flash};
use vole_kit::state::Core;

use crate::Wren;
use crate::vv::{self, Client, LINK_MONITOR, LINK_TAG, Link};

pub const TAB: SettingsTab = SettingsTab {
    id: "vigilant-vole",
    href: "/settings/vigilant-vole",
    label: "vv.title",
    icon: "pulse",
    access: TabAccess::Admins,
};

pub fn router() -> Router<Core> {
    Router::new()
        .route("/settings/vigilant-vole", get(page).post(save))
        .route("/settings/vigilant-vole/test", post(test))
        .route("/settings/vigilant-vole/remove", post(remove))
        .route("/targets/{id}/monitors", post(link))
        .route("/targets/{id}/monitors/{link}/remove", post(unlink))
}

fn form_value(body: &[u8], key: &str) -> String {
    url::form_urlencoded::parse(body)
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "settings_vv.html")]
struct Page {
    ctx: PageCtx,
    url: String,
    has_token: bool,
    error: String,
}

impl Page {
    pub fn placeholder(&self) -> &'static str {
        if self.has_token {
            SECRET_MASK
        } else {
            "vv_…"
        }
    }
}

async fn render(
    core: &Core,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    url: Option<String>,
    error: String,
) -> ApiResult<(SignedCookieJar, HtmlTemplate<Page>)> {
    let (saved, has_token) = core.with_conn(|c| vv::saved(c)).await?;
    let (ctx, jar) = PageCtx::build(
        core,
        Some(user),
        jar,
        i18n::t("vv.title"),
        "settings",
        "vigilant-vole",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(Page {
            ctx,
            url: url.unwrap_or(saved),
            has_token,
            error,
        }),
    ))
}

async fn page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    user.require_admin()?;
    render(&core, &user, &jar, None, String::new()).await
}

async fn save(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_admin()?;
    let raw = form_value(&body, "url");
    let token = form_value(&body, "token");
    let (_, has_token) = core.with_conn(|c| vv::saved(c)).await?;
    let error = match vv::normal_url(&raw) {
        None => Some(i18n::t("vv.err_url").to_string()),
        Some(_) if token.is_empty() && !has_token => Some(i18n::t("vv.err_no_token").to_string()),
        Some(_) => None,
    };
    if let Some(error) = error {
        let page = render(&core, &user, &jar, Some(raw), error).await?;
        return Ok((axum::http::StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    let url = vv::normal_url(&raw).unwrap_or_default();
    let token = (!token.is_empty() && token != SECRET_MASK).then_some(token);
    vv::save(&core, &url, token.as_deref()).await?;
    tracing::info!(user = %user.username, url, "Vigilant Vole connection saved");
    let jar = flash(jar, "success", i18n::t("vv.saved"));
    Ok((jar, Redirect::to("/settings/vigilant-vole")).into_response())
}

async fn test(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    user.require_admin()?;
    let jar = match vv::connection(&core).await {
        None => flash(jar, "error", i18n::t("vv.err_not_connected")),
        Some(conn) => {
            let t = vv::test(&core, conn).await;
            match (t.ok, t.too_old) {
                (false, _) => flash(jar, "error", t.message),
                (true, true) => flash(
                    jar,
                    "warning",
                    format!("{} {}", t.message, i18n::t("vv.too_old")),
                ),
                (true, false) => flash(jar, "success", t.message),
            }
        }
    };
    Ok((jar, Redirect::to("/settings/vigilant-vole")).into_response())
}

async fn remove(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    user.require_admin()?;
    vv::remove(&core).await?;
    tracing::info!(user = %user.username, "Vigilant Vole connection removed");
    let jar = flash(jar, "success", i18n::t("vv.removed"));
    Ok((jar, Redirect::to("/settings/vigilant-vole")).into_response())
}

// ---------------------------------------------------------------------------
// Monitors on a target's page
// ---------------------------------------------------------------------------

/// A choice in the "link a monitor" list: value `m:<id>` or `t:<tag>`.
pub struct Choice {
    pub value: String,
    pub label: String,
}

/// The Vigilant Vole card on a target's page.
pub struct VvCard {
    pub connected: bool,
    pub links: Vec<Link>,
    pub monitors: Vec<Choice>,
    pub tags: Vec<Choice>,
    /// Why the lists are empty ('' when they aren't).
    pub error: String,
}

/// The card; the monitor and tag lists only for admins (one call to VV).
pub async fn card(core: &Core, target_id: i64, is_admin: bool) -> ApiResult<VvCard> {
    let links = core.with_conn(move |c| vv::links(c, target_id)).await?;
    let conn = vv::connection(core).await;
    let mut card = VvCard {
        connected: conn.is_some(),
        links,
        monitors: Vec::new(),
        tags: Vec::new(),
        error: String::new(),
    };
    if let (true, Some(conn)) = (is_admin, conn) {
        match Client::new(&core.http, conn).monitors().await {
            Ok(all) => {
                let mut tags: Vec<String> = all.iter().flat_map(|m| m.tags.clone()).collect();
                tags.sort_by_key(|t| t.to_lowercase());
                tags.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
                let mut monitors = all;
                monitors.sort_by_key(|m| m.name.to_lowercase());
                card.monitors = monitors
                    .into_iter()
                    .map(|m| Choice {
                        value: format!("m:{}", m.id),
                        label: m.name,
                    })
                    .collect();
                card.tags = tags
                    .into_iter()
                    .map(|t| Choice {
                        value: format!("t:{t}"),
                        label: t,
                    })
                    .collect();
            }
            Err(e) => card.error = e.to_string(),
        }
    }
    Ok(card)
}

impl VvCard {
    pub fn kind_label(&self, link: &Link) -> &'static str {
        i18n::t(if link.kind == LINK_TAG {
            "vv.link_tag"
        } else {
            "vv.link_monitor"
        })
    }
}

async fn link(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_admin()?;
    let pick = form_value(&body, "pick");
    let to = Redirect::to(&format!("/targets/{id}#vv-title"));
    let (kind, r#ref) = match pick.split_once(':') {
        Some(("m", id)) if id.parse::<i64>().is_ok() => (LINK_MONITOR, id.to_string()),
        Some(("t", tag)) if !tag.trim().is_empty() => (LINK_TAG, tag.trim().to_string()),
        _ => {
            let jar = flash(jar, "error", i18n::t("vv.err_pick"));
            return Ok((jar, to).into_response());
        }
    };
    // The monitor's name, as VV calls it now (a tag is its own label).
    let mut label = r#ref.clone();
    if kind == LINK_MONITOR
        && let Some(conn) = vv::connection(&wren.core).await
        && let Ok(all) = Client::new(&wren.core.http, conn).monitors().await
        && let Some(m) = all.iter().find(|m| m.id.to_string() == r#ref)
    {
        label.clone_from(&m.name);
    }
    let who = user.username.clone();
    wren.core
        .with_conn(move |c| vv::add_link(c, id, kind, &r#ref, &label, &who))
        .await?;
    let _ = wren.changes.send(());
    let jar = flash(jar, "success", i18n::t("vv.linked"));
    Ok((jar, to).into_response())
}

async fn unlink(
    Extension(wren): Extension<Wren>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, link)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let who = user.username.clone();
    if !wren
        .core
        .with_conn(move |c| vv::remove_link(c, id, link, &who))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    let jar = flash(jar, "success", i18n::t("vv.unlinked"));
    Ok((jar, Redirect::to(&format!("/targets/{id}#vv-title"))).into_response())
}
