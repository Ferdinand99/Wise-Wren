//! Settings → Registries (admins): credentials for registries that need
//! them (private images, or Docker Hub's higher limits). Checks stay
//! anonymous for every other registry.
//!
//! The password (or token) is sealed with AES-256-GCM under a key derived
//! from the instance's key (`session.key`), never shown again and masked
//! like the kit's notification secrets: an edit that leaves it empty keeps
//! it. It is only opened for a check run.

use askama::Template;
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::digest;
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::SignedCookieJar;
use rusqlite::{OptionalExtension, params};
use vole_kit::auth::CurrentUser;
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::{self, Tr as _};
use vole_kit::notify::schema::SECRET_MASK;
use vole_kit::page::{PageCtx, SettingsTab, TabAccess, flash};
use vole_kit::state::Core;

use crate::registry::{Creds, DOCKER_HUB};

pub const TAB: SettingsTab = SettingsTab {
    id: "registries",
    href: "/settings/registries",
    label: "registries.title",
    icon: "key",
    access: TabAccess::Admins,
};

pub fn router() -> Router<Core> {
    Router::new()
        .route("/settings/registries", get(list_page).post(create))
        .route(
            "/settings/registries/{id}",
            get(edit_page).post(update).delete(remove),
        )
}

// ---------------------------------------------------------------------------
// Sealing
// ---------------------------------------------------------------------------

fn cipher(core: &Core) -> LessSafeKey {
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(b"wise-wren/registry-credentials/v1");
    ctx.update(core.session_key.master());
    let key = ctx.finish();
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key.as_ref()).expect("a 32-byte key"))
}

/// `nonce ‖ ciphertext ‖ tag`.
pub fn seal(core: &Core, secret: &str) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .expect("the system's random source");
    let mut buf = secret.as_bytes().to_vec();
    cipher(core)
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(b"registry"),
            &mut buf,
        )
        .expect("sealing a short secret");
    [nonce.as_slice(), &buf].concat()
}

/// The secret, or `None` when it can't be opened (another instance key).
pub fn open(core: &Core, sealed: &[u8]) -> Option<String> {
    if sealed.len() < NONCE_LEN {
        return None;
    }
    let (nonce, rest) = sealed.split_at(NONCE_LEN);
    let mut buf = rest.to_vec();
    let plain = cipher(core)
        .open_in_place(
            Nonce::try_assume_unique_for_key(nonce).ok()?,
            Aad::from(b"registry"),
            &mut buf,
        )
        .ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

/// Every saved credential, opened, for one check run.
pub async fn credentials(core: &Core) -> Creds {
    let rows: Vec<(String, String, Vec<u8>)> = core
        .with_conn(|c| {
            c.prepare("SELECT host, username, secret FROM registries")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
        .await
        .unwrap_or_default();
    rows.into_iter()
        .filter_map(|(host, user, sealed)| {
            let secret = open(core, &sealed);
            if secret.is_none() {
                tracing::warn!(
                    host,
                    "a registry password can't be opened (another instance key?)"
                );
            }
            Some((host, (user, secret?)))
        })
        .collect()
}

/// A registry host as image references spell it (`docker.io` for Docker
/// Hub), or `None` when it isn't one.
pub fn normal_host(raw: &str) -> Option<String> {
    let host = raw
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.:_".contains(&b));
    if !ok {
        return None;
    }
    Some(match host.as_str() {
        "index.docker.io"
        | "registry-1.docker.io"
        | "hub.docker.com"
        | "registry.hub.docker.com" => DOCKER_HUB.to_string(),
        _ => host,
    })
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

pub struct RegistryRow {
    pub id: i64,
    pub host: String,
    pub username: String,
}

/// The add or edit form (`id` 0: add).
#[derive(Default)]
pub struct RegistryForm {
    pub id: i64,
    pub host: String,
    pub username: String,
    pub error: String,
}

impl RegistryForm {
    pub fn action(&self) -> String {
        if self.id == 0 {
            "/settings/registries".into()
        } else {
            format!("/settings/registries/{}", self.id)
        }
    }

    /// The password field's placeholder: the mask when one is saved.
    pub fn placeholder(&self) -> &'static str {
        if self.id == 0 { "" } else { SECRET_MASK }
    }
}

#[derive(Template)]
#[template(path = "settings_registries.html")]
struct Page {
    ctx: PageCtx,
    rows: Vec<RegistryRow>,
    form: RegistryForm,
    editing: bool,
    action: String,
    delete_question: String,
}

async fn rows(core: &Core) -> ApiResult<Vec<RegistryRow>> {
    core.with_conn(|c| {
        c.prepare("SELECT id, host, username FROM registries ORDER BY host")?
            .query_map([], |r| {
                Ok(RegistryRow {
                    id: r.get(0)?,
                    host: r.get(1)?,
                    username: r.get(2)?,
                })
            })?
            .collect()
    })
    .await
}

async fn render(
    core: &Core,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    form: RegistryForm,
    editing: bool,
) -> ApiResult<(SignedCookieJar, HtmlTemplate<Page>)> {
    let rows = if editing {
        Vec::new()
    } else {
        rows(core).await?
    };
    let title = if editing {
        form.host.clone()
    } else {
        i18n::t("registries.title").to_string()
    };
    let (ctx, jar) = PageCtx::build(core, Some(user), jar, title, "settings", "registries").await;
    let action = form.action();
    let delete_question = i18n::tf("registries.delete_q", &[("host", &form.host)]);
    Ok((
        jar,
        HtmlTemplate(Page {
            ctx,
            rows,
            form,
            editing,
            action,
            delete_question,
        }),
    ))
}

async fn list_page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    user.require_admin()?;
    render(&core, &user, &jar, RegistryForm::default(), false).await
}

fn form_value(body: &[u8], key: &str) -> String {
    url::form_urlencoded::parse(body)
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default()
}

/// What the form says: host, user, password (`""`: keep the saved one).
fn read_form(body: &[u8]) -> (Result<String, String>, String, String) {
    let host = normal_host(&form_value(body, "host"))
        .ok_or_else(|| i18n::t("registries.err_host").to_string());
    (
        host,
        form_value(body, "username"),
        form_value(body, "password"),
    )
}

async fn create(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_admin()?;
    let (host, username, password) = read_form(&body);
    let error = match &host {
        Err(e) => Some(e.clone()),
        Ok(_) if username.is_empty() || password.is_empty() => {
            Some(i18n::t("registries.err_login").to_string())
        }
        Ok(h) => {
            let h = h.clone();
            core.with_conn(move |c| {
                c.query_row(
                    "SELECT EXISTS (SELECT 1 FROM registries WHERE host = ?1)",
                    [h],
                    |r| r.get::<_, bool>(0),
                )
            })
            .await?
            .then(|| i18n::t("registries.err_taken").to_string())
        }
    };
    if let Some(error) = error {
        let form = RegistryForm {
            host: form_value(&body, "host"),
            username,
            error,
            ..RegistryForm::default()
        };
        let page = render(&core, &user, &jar, form, false).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    let host = host.unwrap_or_default();
    let sealed = seal(&core, &password);
    let (h, u) = (host.clone(), username);
    core.with_conn(move |c| {
        c.execute(
            "INSERT INTO registries (host, username, secret) VALUES (?1, ?2, ?3)",
            params![h, u, sealed],
        )
    })
    .await?;
    tracing::info!(user = %user.username, host, "registry credentials added");
    let jar = flash(
        jar,
        "success",
        i18n::tf("registries.added", &[("host", &host)]),
    );
    Ok((jar, Redirect::to("/settings/registries")).into_response())
}

async fn load(core: &Core, id: i64) -> ApiResult<RegistryRow> {
    core.with_conn(move |c| {
        c.query_row(
            "SELECT id, host, username FROM registries WHERE id = ?1",
            [id],
            |r| {
                Ok(RegistryRow {
                    id: r.get(0)?,
                    host: r.get(1)?,
                    username: r.get(2)?,
                })
            },
        )
        .optional()
    })
    .await?
    .ok_or(ApiError::NotFound)
}

async fn edit_page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    user.require_admin()?;
    let row = load(&core, id).await?;
    let form = RegistryForm {
        id,
        host: row.host,
        username: row.username,
        error: String::new(),
    };
    render(&core, &user, &jar, form, true).await
}

async fn update(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_admin()?;
    let row = load(&core, id).await?;
    let (_, username, password) = read_form(&body);
    if username.is_empty() {
        let form = RegistryForm {
            id,
            host: row.host,
            username,
            error: i18n::t("registries.err_login").to_string(),
        };
        let page = render(&core, &user, &jar, form, true).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    // The mask or nothing keeps the saved password.
    let sealed = (!password.is_empty() && password != SECRET_MASK).then(|| seal(&core, &password));
    core.with_conn(move |c| {
        c.execute(
            "UPDATE registries SET username = ?2, secret = COALESCE(?3, secret) WHERE id = ?1",
            params![id, username, sealed],
        )
    })
    .await?;
    tracing::info!(user = %user.username, host = %row.host, "registry credentials changed");
    let jar = flash(
        jar,
        "success",
        i18n::tf("registries.saved", &[("host", &row.host)]),
    );
    Ok((jar, Redirect::to("/settings/registries")).into_response())
}

async fn remove(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let row = load(&core, id).await?;
    core.with_conn(move |c| c.execute("DELETE FROM registries WHERE id = ?1", [id]))
        .await?;
    tracing::info!(user = %user.username, host = %row.host, "registry credentials deleted");
    let jar = flash(
        jar,
        "success",
        i18n::tf("registries.deleted", &[("host", &row.host)]),
    );
    Ok((
        jar,
        [(
            HeaderName::from_static("hx-redirect"),
            HeaderValue::from_static("/settings/registries"),
        )],
        StatusCode::OK,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_normalised() {
        assert_eq!(normal_host("https://GHCR.io/").as_deref(), Some("ghcr.io"));
        assert_eq!(normal_host("index.docker.io").as_deref(), Some("docker.io"));
        assert_eq!(
            normal_host("registry.local:5000").as_deref(),
            Some("registry.local:5000")
        );
        assert_eq!(normal_host("a b"), None);
        assert_eq!(normal_host("ghcr.io/x"), None);
    }
}
