//! Wise Wren: keeps the Docker containers on your servers up to date,
//! without surprises (docs/design.md).
//!
//! Sign-in, users, API tokens, Security, General and the API reference come
//! from vole-kit. Wren adds servers with their agents (registration, agent
//! tokens, heartbeats, online status), targets and version checks, and
//! update runs: an editor approves one exact version, the agent fetches it
//! as a job, checks it against its own `agent.toml` and runs it with a
//! snapshot, a health check and an automatic rollback (§6, §8).

#![forbid(unsafe_code)]

pub mod agent_api;
pub mod agent_toml;
pub mod api;
pub mod api_settings;
pub mod channels;
pub mod checks;
pub mod config;
pub mod db;
pub mod fetch;
pub mod inventory;
pub mod pages;
pub mod policy;
pub mod product;
pub mod registries;
pub mod registry;
pub mod run_notify;
pub mod run_pages;
pub mod runs;
pub mod schedule;
pub mod server;
pub mod servers;
pub mod targets;
pub mod texts;
pub mod updates_notify;
pub mod versions;
pub mod vv;
pub mod vv_pages;

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
    /// Inventories per server ([`inventory::RATE`]).
    pub inventory_budgets: Arc<TokenBuckets<i64>>,
    /// Wakes the check scheduler (an inventory arrived, "Check now").
    pub checks: Arc<tokio::sync::Notify>,
    /// One check run at a time.
    pub check_lock: Arc<tokio::sync::Mutex<()>>,
    /// GitHub releases (release notes, version sources), cached.
    pub github: Arc<versions::GitHub>,
    /// Wakes agents waiting for a job (a run was approved or finished).
    pub jobs: Arc<tokio::sync::Notify>,
    /// Job polls and step reports per server ([`runs::RATE`]).
    pub job_budgets: Arc<TokenBuckets<i64>>,
    /// Wakes the Vigilant Vole reconciler ([`vv::sync`]): a run ended, a
    /// scheduled run was moved or cancelled.
    pub vv_sync: Arc<tokio::sync::Notify>,
    /// Runs whose maintenance window is being opened right now ([`vv::before_stop`]).
    pub vv_opening: Arc<std::sync::Mutex<std::collections::HashSet<i64>>>,
    /// Lets user-influenced fetches (version URLs) reach private addresses.
    /// Off unless `WW_ALLOW_PRIVATE_VERSION_URLS` is set ([`fetch`]); tests
    /// serve their fakes on loopback.
    pub allow_private_fetch: Arc<std::sync::atomic::AtomicBool>,
}

impl Wren {
    /// Which addresses a user-influenced fetch may reach.
    pub fn fetch_allow(&self) -> fetch::Allow {
        if self
            .allow_private_fetch
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            fetch::any
        } else {
            fetch::is_public
        }
    }
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
    // Channels saved before secrets were sealed (vole-kit 0.5) get sealed
    // now, once; sealed values are left as they are.
    let key = core.session_key.clone();
    if let Err(err) = core
        .with_conn(move |c| vole_kit::notify::sealed::seal_stored(c, &key))
        .await
    {
        tracing::warn!(?err, "couldn't seal the secrets of notification channels");
    }
    if let Some(url) = public_url.map(str::to_string) {
        core.with_conn(move |c| settings::set(c, settings::BASE_URL, &url))
            .await?;
    }
    Ok(Wren {
        core,
        changes: broadcast::channel(64).0,
        budgets: Arc::new(TokenBuckets::new()),
        inventory_budgets: Arc::new(TokenBuckets::new()),
        checks: Arc::new(tokio::sync::Notify::new()),
        check_lock: Arc::new(tokio::sync::Mutex::new(())),
        github: Arc::new(versions::GitHub::new("https://api.github.com")),
        jobs: Arc::new(tokio::sync::Notify::new()),
        job_budgets: Arc::new(TokenBuckets::new()),
        vv_sync: Arc::new(tokio::sync::Notify::new()),
        vv_opening: Arc::default(),
        allow_private_fetch: Arc::default(),
    })
}

/// The whole app: Wren's pages and API, the agent endpoint, the kit's pages,
/// static files, MCP and the health probe, behind the kit's middleware.
pub fn router(wren: Wren) -> Router {
    let core = wren.core.clone();
    let protected = Router::new()
        .merge(servers::router())
        .merge(agent_toml::router())
        .merge(channels::router())
        .merge(pages::router())
        .merge(run_pages::router())
        .merge(registries::router())
        .merge(vv_pages::router())
        .merge(api::router())
        .merge(kit_pages::router())
        .route_layer(from_fn_with_state(core.clone(), guard::require_auth));
    // Programs, not browsers: the machine headers on every answer.
    let machines = Router::new()
        .route("/api/agent/v1/heartbeat", post(agent_api::heartbeat))
        .route(inventory::PATH, post(inventory::receive))
        .route(runs::JOBS_PATH, get(runs::jobs))
        .route("/api/agent/v1/runs/{id}/steps", post(runs::step))
        .route("/api/agent/v1/runs/{id}/finish", post(runs::finish))
        .route("/api/agent/v1/runs/{id}/monitors", post(vv::monitors_route))
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
