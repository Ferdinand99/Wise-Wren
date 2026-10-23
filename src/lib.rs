//! Wise Wren: keeps the Docker containers on your servers up to date,
//! without surprises (docs/design.md).
//!
//! Sign-in, users, API tokens, Security, General and the API reference come
//! from vole-kit. Phase 2 adds servers with their agents (registration,
//! agent tokens, heartbeats, online status on live pages), the notification
//! channels and the REST/MCP stubs. Agents only report; Wren sends them no
//! jobs yet (§8).

#![forbid(unsafe_code)]

pub mod agent_api;
pub mod api;
pub mod channels;
pub mod config;
pub mod db;
pub mod product;
pub mod server;
pub mod servers;

use std::path::Path;
use std::sync::Arc;

use axum::middleware::{from_fn, from_fn_with_state, map_response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use tokio::sync::broadcast;
use vole_kit::auth::{guard, pages as kit_pages};
use vole_kit::db::settings;
use vole_kit::http::headers;
use vole_kit::http::ratelimit::TokenBuckets;
use vole_kit::state::Core;

pub use product::{VERSION, install};

/// Why Wren could not start.
pub type StartError = Box<dyn std::error::Error + Send + Sync>;

/// A running Wren: the kit's state and what the agents and live pages share.
#[derive(Clone)]
pub struct Wren {
    pub core: Core,
    /// A heartbeat arrived, or a server came online or went offline (the
    /// live pages listen).
    pub changes: broadcast::Sender<()>,
    /// Heartbeats per server ([`agent_api::RATE`]).
    pub budgets: Arc<TokenBuckets<i64>>,
}

/// Installs the configuration, opens and migrates the database in
/// `data_dir` and returns the state. `public_url` (`WW_PUBLIC_URL`), when
/// given, is stored in the `base_url` setting, which the kit's same-origin
/// checks, the API examples and the agents' `docker run` line read; without
/// one the setting keeps what the General page saved.
pub async fn start(data_dir: &Path, public_url: Option<&str>) -> Result<Wren, StartError> {
    install();
    std::fs::create_dir_all(data_dir).map_err(|e| {
        format!(
            "can't create the data directory {}: {e}",
            data_dir.display()
        )
    })?;
    let pool = vole_kit::db::build_pool(&data_dir.join(product::DB_FILE));
    let report = vole_kit::db::migrate(&pool, &data_dir.join("backups")).await?;
    if !report.applied.is_empty() {
        tracing::info!(applied = ?report.applied, "database migrated");
    }
    let http = reqwest::Client::builder()
        .user_agent(product::USER_AGENT)
        .build()?;
    let core = Core::new(
        pool,
        vole_kit::auth::load_or_create_key(data_dir),
        http,
        &product::KIT,
    );
    if let Some(url) = public_url.map(str::to_string) {
        core.with_conn(move |c| settings::set(c, settings::BASE_URL, &url))
            .await?;
    }
    Ok(Wren {
        core,
        changes: broadcast::channel(64).0,
        budgets: Arc::new(TokenBuckets::new()),
    })
}

/// The whole app: Wren's pages and API, the agent endpoint, the kit's pages,
/// static files, MCP and the health probe, behind the kit's middleware.
pub fn router(wren: Wren) -> Router {
    let core = wren.core.clone();
    let protected = Router::new()
        .merge(servers::router())
        .merge(channels::router())
        .merge(api::router())
        .merge(kit_pages::router())
        .route_layer(from_fn_with_state(core.clone(), guard::require_auth));
    // Programs, not browsers: the machine headers on every answer.
    let machines = Router::new()
        .route("/api/agent/v1/heartbeat", post(agent_api::heartbeat))
        .route("/mcp", post(api::mcp).get(vole_kit::mcp::not_offered))
        .route_layer(map_response(headers::machine));
    let public = Router::new()
        .route("/healthz", get(vole_kit::http::healthz))
        .route("/static/{*path}", get(vole_kit::http::assets::static_asset))
        .merge(machines)
        .merge(kit_pages::public_router());
    protected
        .merge(public)
        .fallback(vole_kit::http::error::not_found)
        .layer(Extension(wren))
        .layer(from_fn_with_state(core.clone(), guard::csrf_guard))
        .layer(from_fn_with_state(core.clone(), vole_kit::i18n::layer))
        .layer(from_fn(vole_kit::http::error::negotiate))
        .layer(from_fn(vole_kit::http::headers::security_headers))
        .with_state(core)
}
