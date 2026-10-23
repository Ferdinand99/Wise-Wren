//! `/settings/notifications`: the instance's notification channels (where
//! Wren's notifications will go), with vole-kit's providers: the list, the add sheet (provider
//! picker, then the provider's form, rendered from its schema), the edit
//! page, test messages, pause and resume, delete. Editors and admins only.
//!
//! Secrets never leave the server: saved secrets render as empty inputs
//! with a "••••••" placeholder, an empty secret on save keeps the stored
//! value, and a changed destination needs the secrets typed again (the
//! kit's `merge_secrets_checked`). The provider picker and form are the
//! kit's (`vole_kit::notify::form` and its `kit/partials/notification_*`
//! templates); the channel list, its language and the toggles are
//! Wren's, copied from Keen Kestrel without its alert rules. Nothing sends
//! to them yet except "Send test": new-version, run and agent-offline
//! notifications come with the later phases.

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;
use serde_json::{Value, json};
use vole_kit::auth::CurrentUser;
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::{self, Tr as _};
use vole_kit::notify::form::{FieldView, OptionView, ProviderGroup, field_views, provider_groups};
use vole_kit::notify::schema::{self, FieldError, ProviderSchema};
use vole_kit::page::{PageCtx, SettingsTab, TabAccess, flash};
use vole_kit::state::Core;

use vole_kit::notify::alert::Alert;

use self::db::Channel;

/// The settings tab.
pub const TAB: SettingsTab = SettingsTab {
    id: "notifications",
    href: "/settings/notifications",
    label: "nav.notifications",
    icon: "bell",
    access: TabAccess::Editors,
};

pub fn router() -> Router<Core> {
    Router::new()
        .route("/settings/notifications", get(list_page).post(create))
        .route("/settings/notifications/providers", get(providers))
        .route("/settings/notifications/form", get(new_form))
        .route("/settings/notifications/test", post(test_unsaved))
        .route(
            "/settings/notifications/{id}",
            get(show).post(update).delete(remove),
        )
        .route("/settings/notifications/{id}/edit", get(edit_page))
        .route("/settings/notifications/{id}/test", post(test_saved))
        .route("/settings/notifications/{id}/active", post(toggle_active))
}

const NAME_MAX: usize = 100;

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

pub struct ChannelRow {
    pub id: i64,
    pub name: String,
    /// Provider kind, for the `provider-{kind}` tile.
    pub kind: String,
    pub kind_label: String,
    pub icon: &'static str,
    pub active: bool,
}

impl ChannelRow {
    fn new(channel: Channel) -> Self {
        let schema = vole_kit::notify::provider(&channel.kind);
        ChannelRow {
            id: channel.id,
            kind_label: schema.map_or_else(|| channel.kind.clone(), |s| s.tr_label().to_string()),
            icon: schema.map_or("bell", |s| s.icon),
            kind: channel.kind,
            name: channel.name,
            active: channel.active,
        }
    }
}

/// Everything the add and edit forms need.
pub struct ChannelForm {
    pub id: i64,
    pub kind: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub description: &'static str,
    pub docs_url: &'static str,
    pub name: String,
    pub name_error: String,
    pub active: bool,
    pub language: String,
    /// One message per check run instead of one per update.
    pub digest: bool,
    pub languages: Vec<OptionView>,
    pub fields: Vec<FieldView>,
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings_notifications.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<ChannelRow>,
    query: String,
    groups: Vec<ProviderGroup>,
}

#[derive(Template)]
#[template(path = "partials/notification_list.html")]
struct ListPartial {
    rows: Vec<ChannelRow>,
}

#[derive(Template)]
#[template(path = "kit/partials/notification_picker.html")]
struct PickerPartial {
    query: String,
    groups: Vec<ProviderGroup>,
}

#[derive(Template)]
#[template(path = "kit/partials/notification_provider_grid.html")]
struct GridPartial {
    query: String,
    groups: Vec<ProviderGroup>,
}

#[derive(Template)]
#[template(path = "partials/notification_form.html")]
struct FormPartial {
    form: ChannelForm,
}

#[derive(Template)]
#[template(path = "settings_notifications_edit.html")]
struct EditPage {
    ctx: PageCtx,
    form: ChannelForm,
    delete_url: String,
    delete_question: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

fn trigger(value: &Value) -> (HeaderName, HeaderValue) {
    let header = HeaderValue::from_str(&vole_kit::http::ascii_json(&value.to_string()))
        .unwrap_or_else(|_| HeaderValue::from_static("{}"));
    (HeaderName::from_static("hx-trigger"), header)
}

fn toast(message: &str, kind: &str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static("hx-trigger"),
        vole_kit::http::toast_trigger(message, kind, false),
    )
}

fn parse_pairs(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn pair<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn pair_flag(pairs: &[(String, String)], key: &str) -> bool {
    matches!(pair(pairs, key), Some("true" | "on" | "1"))
}

/// A language code from a form, "" for the instance's.
fn language_of(pairs: &[(String, String)]) -> String {
    let code = pair(pairs, "language").unwrap_or_default().trim();
    if i18n::Lang::from_exact_code(code).is_some() {
        code.to_string()
    } else {
        String::new()
    }
}

fn language_options(selected: &str) -> Vec<OptionView> {
    let mut out = vec![OptionView {
        value: String::new(),
        label: i18n::tf(
            "channels.language_default",
            &[("name", i18n::site_default().native_name())],
        ),
        selected: selected.is_empty(),
    }];
    out.extend(
        i18n::language_choices()
            .into_iter()
            .map(|(code, label)| OptionView {
                selected: code == selected,
                value: code.to_string(),
                label,
            }),
    );
    out
}

fn schema_for(kind: &str) -> ApiResult<&'static ProviderSchema> {
    vole_kit::notify::provider(kind)
        .ok_or_else(|| ApiError::bad_request(i18n::t("notif.err_service")))
}

fn blank_form(schema: &'static ProviderSchema, language: &str) -> ChannelForm {
    ChannelForm {
        id: 0,
        kind: schema.kind,
        label: schema.tr_label(),
        icon: schema.icon,
        description: schema.tr_description(),
        docs_url: schema.docs_url,
        name: String::new(),
        name_error: String::new(),
        active: true,
        language: language.to_string(),
        languages: language_options(language),
        digest: false,
        fields: field_views(schema, &Value::Null, &Value::Null, &[], true),
    }
}

fn validate_name(name: &str) -> Option<String> {
    if name.is_empty() {
        Some(i18n::t("notif.err_name").into())
    } else if name.chars().count() > NAME_MAX {
        Some(i18n::tf(
            "notif.err_name_long",
            &[("max", &NAME_MAX.to_string())],
        ))
    } else {
        None
    }
}

async fn list_rows(core: &Core) -> ApiResult<Vec<ChannelRow>> {
    let rows = core.with_conn(|c| db::channels(c)).await?;
    Ok(rows.into_iter().map(ChannelRow::new).collect())
}

async fn load_channel(core: &Core, id: i64) -> ApiResult<Channel> {
    core.with_conn(move |c| db::channel(c, id))
        .await?
        .ok_or(ApiError::NotFound)
}

/// The language the user reads (the request's), as a new channel's
/// default; "" when it is the instance's anyway.
fn user_language() -> String {
    let lang = i18n::current();
    if lang == i18n::site_default() {
        String::new()
    } else {
        lang.code().to_string()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    user.require_editor()?;
    let rows = list_rows(&core).await?;
    let (ctx, jar) = PageCtx::build(
        &core,
        Some(&user),
        &jar,
        i18n::t("nav.notifications"),
        "settings",
        "notifications",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(ListPage {
            ctx,
            rows,
            query: String::new(),
            groups: provider_groups(""),
        }),
    ))
}

#[derive(Deserialize)]
struct ProvidersQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    grid: bool,
}

async fn providers(user: CurrentUser, Query(q): Query<ProvidersQuery>) -> ApiResult<Response> {
    user.require_editor()?;
    let groups = provider_groups(&q.q);
    Ok(if q.grid {
        HtmlTemplate(GridPartial { query: q.q, groups }).into_response()
    } else {
        HtmlTemplate(PickerPartial { query: q.q, groups }).into_response()
    })
}

#[derive(Deserialize)]
struct KindQuery {
    #[serde(default)]
    kind: String,
}

async fn new_form(user: CurrentUser, Query(q): Query<KindQuery>) -> ApiResult<impl IntoResponse> {
    user.require_editor()?;
    let schema = schema_for(&q.kind)?;
    Ok(HtmlTemplate(FormPartial {
        form: blank_form(schema, &user_language()),
    }))
}

async fn create(
    State(core): State<Core>,
    user: CurrentUser,
    headers: HeaderMap,
    jar: SignedCookieJar,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let pairs = parse_pairs(&body);
    let schema = schema_for(pair(&pairs, "kind").unwrap_or_default())?;
    let name = pair(&pairs, "name").unwrap_or_default().trim().to_string();
    let language = language_of(&pairs);
    let parsed = schema::parse_form(schema, &pairs);

    let name_error = validate_name(&name);
    let field_errors = schema::validate(schema, &parsed.config)
        .err()
        .unwrap_or_default();
    if name_error.is_some() || !field_errors.is_empty() {
        if !is_htmx(&headers) {
            let msg = name_error.unwrap_or_else(|| field_errors[0].1.clone());
            return Ok((
                flash(jar, "error", msg),
                Redirect::to("/settings/notifications"),
            )
                .into_response());
        }
        let mut form = blank_form(schema, &language);
        form.name = name;
        form.name_error = name_error.unwrap_or_default();
        // Echo what the user just typed (including secrets they typed now).
        form.fields = field_views(schema, &parsed.config, &Value::Null, &field_errors, false);
        return Ok(HtmlTemplate(FormPartial { form }).into_response());
    }

    let config = parsed.config.to_string();
    let kind = schema.kind;
    let saved_name = name.clone();
    let digest = pair_flag(&pairs, "digest");
    core.with_conn(move |c| db::insert_channel(c, &saved_name, kind, &config, &language, digest))
        .await?;
    tracing::info!(user = %user.username, kind, "notification channel created");

    let message = i18n::tf("notif.added", &[("name", &name)]);
    if !is_htmx(&headers) {
        return Ok((
            flash(jar, "success", message),
            Redirect::to("/settings/notifications"),
        )
            .into_response());
    }
    let rows = list_rows(&core).await?;
    let headers = [
        (
            HeaderName::from_static("hx-retarget"),
            HeaderValue::from_static("#channels"),
        ),
        (
            HeaderName::from_static("hx-reswap"),
            HeaderValue::from_static("outerHTML"),
        ),
        trigger(
            &json!({ "vv:toast": { "message": message, "kind": "success" }, "vv:close-sheet": true }),
        ),
    ];
    Ok((headers, HtmlTemplate(ListPartial { rows })).into_response())
}

async fn show(user: CurrentUser, Path(id): Path<i64>) -> ApiResult<Redirect> {
    user.require_editor()?;
    Ok(Redirect::to(&format!("/settings/notifications/{id}/edit")))
}

fn stored_config(channel: &Channel) -> Value {
    serde_json::from_str(&channel.config).unwrap_or_else(|_| json!({}))
}

fn edit_form(channel: &Channel, schema: &'static ProviderSchema) -> ChannelForm {
    let stored = stored_config(channel);
    let shown = schema::redact(Some(schema), &stored);
    ChannelForm {
        id: channel.id,
        name: channel.name.clone(),
        active: channel.active,
        digest: channel.digest,
        fields: field_views(schema, &shown, &stored, &[], false),
        ..blank_form(schema, &channel.language)
    }
}

async fn render_edit(
    core: &Core,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    form: ChannelForm,
) -> (SignedCookieJar, HtmlTemplate<EditPage>) {
    let (ctx, jar) = PageCtx::build(
        core,
        Some(user),
        jar,
        form.name.clone(),
        "settings",
        "notifications",
    )
    .await;
    let delete_url = format!("/settings/notifications/{}", form.id);
    let delete_question = i18n::tf("channels.delete_q", &[("name", &form.name)]);
    (
        jar,
        HtmlTemplate(EditPage {
            ctx,
            form,
            delete_url,
            delete_question,
        }),
    )
}

async fn edit_page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_editor()?;
    let channel = load_channel(&core, id).await?;
    let Some(schema) = vole_kit::notify::provider(&channel.kind) else {
        let jar = flash(
            jar,
            "error",
            i18n::tf("notif.cant_edit", &[("name", &channel.name)]),
        );
        return Ok((jar, Redirect::to("/settings/notifications")).into_response());
    };
    let form = edit_form(&channel, schema);
    Ok(render_edit(&core, &user, &jar, form).await.into_response())
}

/// Secret re-entry errors first, then validation errors for other fields.
fn combine_errors(
    secret: Option<Vec<FieldError>>,
    validation: Option<Vec<FieldError>>,
) -> Vec<FieldError> {
    let mut errors = secret.unwrap_or_default();
    for (key, msg) in validation.unwrap_or_default() {
        if !errors.iter().any(|(k, _)| *k == key) {
            errors.push((key, msg));
        }
    }
    errors
}

async fn update(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let channel = load_channel(&core, id).await?;
    let schema = schema_for(&channel.kind)?;
    let pairs = parse_pairs(&body);
    let name = pair(&pairs, "name").unwrap_or_default().trim().to_string();
    let language = language_of(&pairs);
    let parsed = schema::parse_form(schema, &pairs);
    let stored = stored_config(&channel);
    let mut config = parsed.config.clone();
    let secret_errors =
        schema::merge_secrets_checked(schema, &mut config, &stored, &parsed.clear).err();
    let active = pair_flag(&pairs, "active");

    let name_error = validate_name(&name);
    let field_errors = combine_errors(secret_errors, schema::validate(schema, &config).err());
    if name_error.is_some() || !field_errors.is_empty() {
        let mut form = edit_form(&channel, schema);
        form.name = name;
        form.name_error = name_error.unwrap_or_default();
        form.active = active;
        form.languages = language_options(&language);
        form.language = language;
        form.fields = field_views(schema, &parsed.config, &stored, &field_errors, false);
        let (jar, page) = render_edit(&core, &user, &jar, form).await;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, page).into_response());
    }

    let config = config.to_string();
    let saved_name = name.clone();
    let digest = pair_flag(&pairs, "digest");
    core.with_conn(move |c| {
        db::update_channel(c, id, &saved_name, &config, active, &language, digest)
    })
    .await?;
    tracing::info!(user = %user.username, channel_id = id, "notification channel updated");
    let jar = flash(jar, "success", i18n::tf("notif.saved", &[("name", &name)]));
    Ok((jar, Redirect::to("/settings/notifications")).into_response())
}

async fn remove(
    State(core): State<Core>,
    user: CurrentUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_editor()?;
    let channel = load_channel(&core, id).await?;
    core.with_conn(move |c| db::delete_channel(c, id)).await?;
    tracing::info!(user = %user.username, channel_id = id, "notification channel deleted");
    let message = i18n::tf("notif.deleted", &[("name", &channel.name)]);
    let from_list = headers.get("hx-target").and_then(|v| v.to_str().ok()) == Some("channels");
    if from_list {
        let rows = list_rows(&core).await?;
        return Ok((
            [toast(&message, "success")],
            HtmlTemplate(ListPartial { rows }),
        )
            .into_response());
    }
    let redirect = (
        HeaderName::from_static("hx-redirect"),
        HeaderValue::from_static("/settings/notifications"),
    );
    Ok(([redirect, toast(&message, "success")], StatusCode::OK).into_response())
}

async fn toggle_active(
    State(core): State<Core>,
    user: CurrentUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let active = pair_flag(&parse_pairs(&body), "active");
    let channel = load_channel(&core, id).await?;
    core.with_conn(move |c| db::set_channel_active(c, id, active))
        .await?;
    let message = if active {
        i18n::tf("notif.resumed", &[("name", &channel.name)])
    } else {
        i18n::tf("notif.paused", &[("name", &channel.name)])
    };
    Ok(([toast(&message, "success")], StatusCode::OK).into_response())
}

/// Sends a test and reports the result as a toast (always 200 so htmx shows
/// it).
async fn run_test(
    core: &Core,
    kind: &str,
    config: &Value,
    language: &str,
    label: &str,
) -> Response {
    match send_test(core, kind, config, language).await {
        Ok(()) => (
            [toast(
                &i18n::tf("notif.test_sent", &[("label", label)]),
                "success",
            )],
            StatusCode::OK,
        )
            .into_response(),
        Err(err) => {
            tracing::info!(kind, %err, "test notification failed");
            let message = i18n::tf("notif.test_failed", &[("err", &err)]);
            ([toast(&message, "error")], message).into_response()
        }
    }
}

fn first_error_toast(schema: &ProviderSchema, errors: &[FieldError]) -> Response {
    let (key, msg) = &errors[0];
    let label = schema
        .field(key)
        .map_or(i18n::t("notif.settings_label"), |f| f.tr_label(schema.kind));
    ([toast(&format!("{label}: {msg}"), "error")], StatusCode::OK).into_response()
}

async fn test_unsaved(
    State(core): State<Core>,
    user: CurrentUser,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let pairs = parse_pairs(&body);
    let schema = schema_for(pair(&pairs, "kind").unwrap_or_default())?;
    let parsed = schema::parse_form(schema, &pairs);
    if let Err(errors) = schema::validate(schema, &parsed.config) {
        return Ok(first_error_toast(schema, &errors));
    }
    let language = language_of(&pairs);
    Ok(run_test(
        &core,
        schema.kind,
        &parsed.config,
        &language,
        schema.tr_label(),
    )
    .await)
}

/// Tests a saved channel. When the edit form is included (it carries
/// `kind`), its unsaved values are tested, with blank secrets taken from
/// storage (only while the destination stays the same).
async fn test_saved(
    State(core): State<Core>,
    user: CurrentUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let channel = load_channel(&core, id).await?;
    let label = format!("“{}”", channel.name);
    let pairs = parse_pairs(&body);
    let Some(schema) = vole_kit::notify::provider(&channel.kind) else {
        let msg = i18n::tf("notif.cant_send", &[("label", &label)]);
        return Ok(([toast(&msg, "error")], StatusCode::OK).into_response());
    };
    let stored = stored_config(&channel);
    let (config, language) = if pair(&pairs, "kind").is_some() {
        let parsed = schema::parse_form(schema, &pairs);
        let mut config = parsed.config;
        let secret_errors =
            schema::merge_secrets_checked(schema, &mut config, &stored, &parsed.clear).err();
        let errors = combine_errors(secret_errors, schema::validate(schema, &config).err());
        if !errors.is_empty() {
            return Ok(first_error_toast(schema, &errors));
        }
        (config, language_of(&pairs))
    } else {
        (stored, channel.language.clone())
    };
    Ok(run_test(&core, schema.kind, &config, &language, &label).await)
}

/// Sends the test message to a channel in its language (`""`: the
/// instance's), linking to the Overview when the public URL is known.
async fn send_test(core: &Core, kind: &str, config: &Value, language: &str) -> Result<(), String> {
    let lang = match i18n::Lang::from_exact_code(language) {
        Some(lang) => lang,
        None => vole_kit::i18n::site_language(core)
            .await
            .unwrap_or_default(),
    };
    let url = core.base_url().await.map(|base| format!("{base}/"));
    vole_kit::notify::send_in(&core.http, kind, config, lang, || {
        Alert::test(
            i18n::tf(
                "notify_msg.test_title",
                &[("product", crate::product::NAME)],
            ),
            i18n::t("notify_msg.test_body"),
            i18n::t("notify_msg.event_test"),
            i18n::t("notify_msg.view_overview"),
            url,
        )
    })
    .await
}

// ---------------------------------------------------------------------------
// REST API (api_settings.rs): the same rules as the pages. Saved secrets are
// never returned (redacted with the kit's mask), an absent or masked secret
// keeps the stored one, and a changed destination needs the secrets again.
// ---------------------------------------------------------------------------

/// A channel as the API shows it: its settings with secrets masked.
fn channel_json(ch: &Channel) -> Value {
    let schema = vole_kit::notify::provider(&ch.kind);
    json!({
        "id": ch.id,
        "name": ch.name,
        "kind": ch.kind,
        "active": ch.active,
        "language": ch.language,
        "digest": ch.digest,
        "config": schema::redact(schema, &stored_config(ch)),
    })
}

/// `GET /api/v1/notification-channels`.
pub async fn api_list(core: &Core) -> ApiResult<Value> {
    let rows = core.with_conn(|c| db::channels(c)).await?;
    Ok(json!({ "channels": rows.iter().map(channel_json).collect::<Vec<_>>() }))
}

/// `GET /api/v1/notification-channels/{id}`.
pub async fn api_get(core: &Core, id: i64) -> ApiResult<Value> {
    Ok(channel_json(&load_channel(core, id).await?))
}

/// The providers and their settings' keys, for building a channel.
pub fn api_providers() -> Value {
    json!({ "providers": vole_kit::notify::PROVIDERS.iter().map(|p| json!({
        "kind": p.kind,
        "label": p.label,
        "docs_url": p.docs_url,
        "fields": p.fields.iter().map(|f| json!({
            "key": f.key,
            "type": format!("{:?}", f.kind).to_lowercase(),
            "required": f.required,
            "secret": f.secret,
        })).collect::<Vec<_>>(),
    })).collect::<Vec<_>>() })
}

/// JSON `config` → the form pairs the kit's parser reads.
fn config_pairs(schema: &ProviderSchema, config: Option<&Value>) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let Some(obj) = config.and_then(Value::as_object) else {
        return pairs;
    };
    for f in schema.fields {
        let Some(v) = obj.get(f.key) else { continue };
        let text = match v {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            other => other.to_string(),
        };
        pairs.push((f.form_name(), text));
    }
    pairs
}

fn invalid(schema: &ProviderSchema, errors: &[FieldError]) -> ApiError {
    let (key, msg) = errors.first().cloned().unwrap_or_default();
    let label = schema
        .field(&key)
        .map_or(i18n::t("notif.settings_label"), |f| f.tr_label(schema.kind));
    ApiError::bad_request(format!("{label} ({key}): {msg}"))
}

/// Creates (`id` None) or changes a channel from JSON: `kind` (create
/// only), `name`, `language`, `digest`, `active` (change only), `config`.
pub async fn api_save(core: &Core, id: Option<i64>, body: &Value, who: &str) -> ApiResult<Value> {
    let text = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default();
    let old = match id {
        Some(id) => Some(load_channel(core, id).await?),
        None => None,
    };
    let kind = old
        .as_ref()
        .map_or(text("kind").to_string(), |c| c.kind.clone());
    let schema = vole_kit::notify::provider(&kind)
        .ok_or_else(|| ApiError::bad_request(format!("unknown kind “{kind}”")))?;
    let name = match (body.get("name").and_then(Value::as_str), &old) {
        (Some(n), _) => n.trim().to_string(),
        (None, Some(c)) => c.name.clone(),
        (None, None) => String::new(),
    };
    if let Some(problem) = validate_name(&name) {
        return Err(ApiError::bad_request(problem));
    }
    let language = match body.get("language").and_then(Value::as_str) {
        Some(code) => language_of(&[("language".into(), code.into())]),
        None => old.as_ref().map(|c| c.language.clone()).unwrap_or_default(),
    };
    let flag = |k: &str, default: bool| body.get(k).and_then(Value::as_bool).unwrap_or(default);
    let digest = flag("digest", old.as_ref().is_some_and(|c| c.digest));
    let active = flag("active", old.as_ref().is_none_or(|c| c.active));
    if let Some(obj) = body.get("config").and_then(Value::as_object)
        && let Some(unknown) = obj.keys().find(|k| schema.field(k).is_none())
    {
        return Err(ApiError::bad_request(format!(
            "{kind} has no setting “{unknown}” (see GET /api/v1/notification-providers)"
        )));
    }
    let mut config = schema::parse_form(schema, &config_pairs(schema, body.get("config"))).config;
    if let Some(ch) = &old {
        // Unchanged settings and secrets stay as stored.
        let stored = stored_config(ch);
        if body.get("config").is_none() {
            config = stored.clone();
        } else if let Err(errors) = schema::merge_secrets_checked(schema, &mut config, &stored, &[])
        {
            return Err(invalid(schema, &errors));
        }
    }
    if let Err(errors) = schema::validate(schema, &config) {
        return Err(invalid(schema, &errors));
    }
    let config = config.to_string();
    let kind = schema.kind;
    let id = match id {
        Some(id) => {
            core.with_conn(move |c| {
                db::update_channel(c, id, &name, &config, active, &language, digest)
            })
            .await?;
            tracing::info!(user = who, channel_id = id, "notification channel updated");
            id
        }
        None => {
            let id = core
                .with_conn(move |c| db::insert_channel(c, &name, kind, &config, &language, digest))
                .await?;
            tracing::info!(user = who, kind, "notification channel created");
            id
        }
    };
    api_get(core, id).await
}

/// Pauses or resumes a channel.
pub async fn api_active(core: &Core, id: i64, active: bool) -> ApiResult<Value> {
    load_channel(core, id).await?;
    core.with_conn(move |c| db::set_channel_active(c, id, active))
        .await?;
    api_get(core, id).await
}

/// Sends a test message with the saved settings: `{"sent": true}` or
/// `{"sent": false, "error": …}` (the kit scrubs secrets from errors).
pub async fn api_test(core: &Core, id: i64) -> ApiResult<Value> {
    let ch = load_channel(core, id).await?;
    Ok(
        match send_test(core, &ch.kind, &stored_config(&ch), &ch.language).await {
            Ok(()) => json!({ "sent": true }),
            Err(err) => json!({ "sent": false, "error": err }),
        },
    )
}

/// Deletes a channel.
pub async fn api_delete(core: &Core, id: i64, who: &str) -> ApiResult<Value> {
    load_channel(core, id).await?;
    core.with_conn(move |c| db::delete_channel(c, id)).await?;
    tracing::info!(user = who, channel_id = id, "notification channel deleted");
    Ok(json!({ "deleted": true }))
}

/// The `notification_channels` table.
mod db {
    use rusqlite::{Connection, OptionalExtension, Row, params};

    /// A notification channel with the language its messages are written in.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Channel {
        pub id: i64,
        pub name: String,
        pub kind: String,
        /// JSON with the provider's settings, secrets included: never shown
        /// unredacted.
        pub config: String,
        pub active: bool,
        /// A language code, or "" for the instance's language.
        pub language: String,
        /// One message per check run.
        pub digest: bool,
    }

    const COLUMNS: &str = "id, name, kind, config, active, language, digest";

    fn row(r: &Row<'_>) -> rusqlite::Result<Channel> {
        Ok(Channel {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            config: r.get(3)?,
            active: r.get(4)?,
            language: r.get(5)?,
            digest: r.get(6)?,
        })
    }

    pub fn channels(c: &Connection) -> rusqlite::Result<Vec<Channel>> {
        c.prepare(&format!(
            "SELECT {COLUMNS} FROM notification_channels ORDER BY name COLLATE NOCASE, id"
        ))?
        .query_map([], row)?
        .collect()
    }

    pub fn channel(c: &Connection, id: i64) -> rusqlite::Result<Option<Channel>> {
        c.query_row(
            &format!("SELECT {COLUMNS} FROM notification_channels WHERE id = ?1"),
            [id],
            row,
        )
        .optional()
    }

    pub fn insert_channel(
        c: &Connection,
        name: &str,
        kind: &str,
        config: &str,
        language: &str,
        digest: bool,
    ) -> rusqlite::Result<i64> {
        c.execute(
            "INSERT INTO notification_channels (name, kind, config, language, digest) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, kind, config, language, digest],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn update_channel(
        c: &Connection,
        id: i64,
        name: &str,
        config: &str,
        active: bool,
        language: &str,
        digest: bool,
    ) -> rusqlite::Result<bool> {
        Ok(c.execute(
            "UPDATE notification_channels SET name = ?2, config = ?3, active = ?4, language = ?5,
                 digest = ?6 WHERE id = ?1",
            params![id, name, config, active, language, digest],
        )? > 0)
    }

    pub fn set_channel_active(c: &Connection, id: i64, active: bool) -> rusqlite::Result<bool> {
        Ok(c.execute(
            "UPDATE notification_channels SET active = ?2 WHERE id = ?1",
            params![id, active],
        )? > 0)
    }

    pub fn delete_channel(c: &Connection, id: i64) -> rusqlite::Result<bool> {
        Ok(c.execute("DELETE FROM notification_channels WHERE id = ?1", [id])? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vole_kit::notify::schema::SECRET_MASK;
    use vole_kit::notify::{PROVIDERS, discord, telegram, webhook};

    #[test]
    fn provider_search_filters_and_groups() {
        crate::product::install();
        let all = provider_groups("");
        assert_eq!(
            all.iter().map(|g| g.providers.len()).sum::<usize>(),
            PROVIDERS.len()
        );
        let chat = provider_groups("disc");
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].providers[0].kind, "discord");
        assert!(provider_groups("zzz-nothing").is_empty());
    }

    #[test]
    fn saved_secrets_render_empty_with_the_mask() {
        crate::product::install();
        let stored = json!({"bot_token": "1:SECRET", "chat_id": "42", "silent": true});
        let shown = schema::redact(Some(&telegram::SCHEMA), &stored);
        let views = field_views(&telegram::SCHEMA, &shown, &stored, &[], false);
        let token = views.iter().find(|v| v.name == "c_bot_token").unwrap();
        assert!(token.value.is_empty());
        assert!(token.saved);
        assert_eq!(token.placeholder, SECRET_MASK);
        assert_eq!(token.input, "password");
        for v in &views {
            assert!(!v.value.contains("SECRET"));
        }
        let stored = json!({"url": "https://discord.com/api/webhooks/1/SECRET"});
        let shown = schema::redact(Some(&discord::SCHEMA), &stored);
        let views = field_views(&discord::SCHEMA, &shown, &stored, &[], false);
        assert!(views.iter().all(|v| !v.value.contains("SECRET")));
        let stored = json!({"url": "https://x", "headers": {"Authorization": "Bearer SECRET"}});
        let shown = schema::redact(Some(&webhook::SCHEMA), &stored);
        let views = field_views(&webhook::SCHEMA, &shown, &stored, &[], false);
        let headers = views.iter().find(|v| v.input == "kv").unwrap();
        assert_eq!(
            headers.rows,
            [("Authorization".to_string(), SECRET_MASK.to_string())]
        );
    }

    #[test]
    fn names_are_checked() {
        crate::product::install();
        assert!(validate_name("").is_some());
        assert!(validate_name(&"x".repeat(101)).is_some());
        assert!(validate_name("Ops").is_none());
    }
}
