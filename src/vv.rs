//! Vigilant Vole (design §6 steps 2, 6 and 8; §8): Wren talks to VV's REST
//! API v1 with one API token, sealed like registry credentials.
//!
//! - **Step 2** ([`before_stop`]): when the agent's preflight is through,
//!   Wren opens a maintenance window for the target's linked monitors (or
//!   makes the run's planned window start now). If VV can't be reached the
//!   run goes on without one (logged on the run and notified), unless the
//!   target requires VV: then it is cancelled before anything stops.
//! - **Step 6** ([`monitors_route`]): after its container checks the agent
//!   asks Wren, and Wren answers from the monitors' *real* results during
//!   maintenance (`raw_status`, VV 0.9.0 and later; an older VV leaves the
//!   container checks alone in charge).
//! - **Step 8** and planned windows ([`sync`]): a reconciler closes the
//!   window of every run that ended (however it ended), creates and moves
//!   planned windows for scheduled runs and deletes those no run needs.
//!
//! The agent never talks to VV and never sees the token.

use std::time::Duration;

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use vole_kit::db::settings;
use vole_kit::http::ApiError;
use vole_kit::state::Core;

use crate::Wren;
use crate::schedule::{db_time, parse_db_time};

pub const URL_KEY: &str = "vv_url";
pub const TOKEN_KEY: &str = "vv_token";
const AAD: &[u8] = b"vv-token";
/// Each request to VV.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// What an older VV can't do (shown in the run log; the UI translates it).
pub const TOO_OLD: &str = "Vigilant Vole is too old to report results during maintenance; update to 0.9.0 or later. The health check uses the container checks only.";
/// How often the reconciler looks.
pub const SYNC_SECONDS: u64 = 30;

// ---------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------

/// Where VV is and the token Wren uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conn {
    pub url: String,
    pub token: String,
}

/// The saved URL (`''` when not connected) and whether a token is saved.
pub fn saved(c: &Connection) -> rusqlite::Result<(String, bool)> {
    Ok((
        settings::get(c, URL_KEY)?.unwrap_or_default(),
        settings::get(c, TOKEN_KEY)?.is_some_and(|t| !t.is_empty()),
    ))
}

/// The connection, opened, or `None` when there is none (or the token can't
/// be opened with this instance's key).
pub async fn connection(core: &Core) -> Option<Conn> {
    let (url, sealed) = core
        .with_conn(|c| Ok((settings::get(c, URL_KEY)?, settings::get(c, TOKEN_KEY)?)))
        .await
        .ok()?;
    let (url, sealed) = (url.filter(|u| !u.is_empty())?, sealed?);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .ok()?;
    let token = crate::registries::open_for(core, AAD, &bytes);
    if token.is_none() {
        tracing::warn!("the Vigilant Vole token can't be opened (another instance key?)");
    }
    Some(Conn { url, token: token? })
}

/// Saves the URL and, when given, a new token (`None` keeps the saved one).
pub async fn save(core: &Core, url: &str, token: Option<&str>) -> Result<(), ApiError> {
    let url = url.trim().trim_end_matches('/').to_string();
    let sealed = token.map(|t| {
        base64::engine::general_purpose::STANDARD.encode(crate::registries::seal_for(
            core,
            AAD,
            t.trim(),
        ))
    });
    core.with_conn(move |c| {
        settings::set(c, URL_KEY, &url)?;
        if let Some(s) = sealed {
            settings::set(c, TOKEN_KEY, &s)?;
        }
        Ok(())
    })
    .await
}

/// Forgets the connection.
pub async fn remove(core: &Core) -> Result<(), ApiError> {
    core.with_conn(|c| {
        settings::set(c, URL_KEY, "")?;
        settings::set(c, TOKEN_KEY, "")
    })
    .await
}

/// An `http(s)://` URL, or `None`.
pub fn normal_url(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return None;
    }
    Some(u.as_str().trim_end_matches('/').to_string())
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VvError {
    /// No answer (down, wrong address, timeout).
    Unreachable(String),
    /// An answer that isn't a success.
    Answer(u16, String),
}

impl std::fmt::Display for VvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VvError::Unreachable(e) => write!(f, "Vigilant Vole can't be reached: {e}"),
            VvError::Answer(s, m) => write!(f, "Vigilant Vole answered {s}: {m}"),
        }
    }
}

/// A monitor as Wren needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub tags: Vec<String>,
    pub active: bool,
    /// The monitor JSON has `raw_status` (VV 0.9.0 and later).
    pub has_raw: bool,
}

pub struct Client<'a> {
    http: &'a reqwest::Client,
    conn: Conn,
}

impl<'a> Client<'a> {
    pub fn new(http: &'a reqwest::Client, conn: Conn) -> Client<'a> {
        Client { http, conn }
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, VvError> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.conn.url))
            .bearer_auth(&self.conn.token)
            .timeout(TIMEOUT);
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req
            .send()
            .await
            .map_err(|e| VvError::Unreachable(e.without_url().to_string()))?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(VvError::Answer(status.as_u16(), msg));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub async fn get(&self, path: &str) -> Result<Value, VvError> {
        self.call(reqwest::Method::GET, path, None).await
    }

    /// Every page of a list (`{"data": [...], "next_cursor": …}`).
    async fn list(&self, path: &str) -> Result<Vec<Value>, VvError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let sep = if path.contains('?') { '&' } else { '?' };
        for _ in 0..50 {
            let p = match &cursor {
                Some(c) => format!("{path}{sep}limit=500&cursor={c}"),
                None => format!("{path}{sep}limit=500"),
            };
            let v = self.get(&p).await?;
            out.extend(v["data"].as_array().cloned().unwrap_or_default());
            cursor = match &v["next_cursor"] {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            };
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn monitors(&self) -> Result<Vec<Monitor>, VvError> {
        Ok(self
            .list("/api/v1/monitors")
            .await?
            .iter()
            .filter_map(|m| {
                Some(Monitor {
                    id: m["id"].as_i64()?,
                    name: m["name"].as_str().unwrap_or_default().to_string(),
                    tags: m["tags"]
                        .as_array()
                        .map(|t| {
                            t.iter()
                                .filter_map(|x| x["name"].as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    active: m["active"].as_bool().unwrap_or(true),
                    has_raw: m.as_object().is_some_and(|o| o.contains_key("raw_status")),
                })
            })
            .collect())
    }

    /// `(checked_at, raw_status)` of a monitor's results since `since`.
    pub async fn results(&self, id: i64, since: &str) -> Result<Vec<(String, Value)>, VvError> {
        let since = since.replace(' ', "%20");
        let v = self
            .get(&format!(
                "/api/v1/monitors/{id}/heartbeats?since={since}&limit=500"
            ))
            .await?;
        Ok(v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|h| {
                        (
                            h["checked_at"].as_str().unwrap_or_default().to_string(),
                            h.get("raw_status").cloned().unwrap_or(Value::Null),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The status pages that show any of `ids` (the list leaves the
    /// sections out; each page is read on its own).
    pub async fn pages_showing(&self, ids: &[i64]) -> Result<Vec<i64>, VvError> {
        let mut pages = Vec::new();
        for p in self.list("/api/v1/status-pages").await? {
            if p.get("sections").is_some() {
                pages.push(p);
            } else if let Some(id) = p["id"].as_i64() {
                pages.push(self.get(&format!("/api/v1/status-pages/{id}")).await?);
            }
        }
        Ok(pages
            .iter()
            .filter(|p| {
                p["sections"].as_array().is_some_and(|s| {
                    s.iter().any(|sec| {
                        sec["monitors"].as_array().is_some_and(|m| {
                            m.iter()
                                .any(|x| x["id"].as_i64().is_some_and(|i| ids.contains(&i)))
                        })
                    })
                })
            })
            .filter_map(|p| p["id"].as_i64())
            .collect())
    }

    pub async fn create_window(&self, body: &Value) -> Result<i64, VvError> {
        let v = self
            .call(reqwest::Method::POST, "/api/v1/maintenance", Some(body))
            .await?;
        v["id"]
            .as_i64()
            .ok_or_else(|| VvError::Answer(200, "no window id in the answer".into()))
    }

    pub async fn patch_window(&self, id: i64, body: &Value) -> Result<(), VvError> {
        self.call(
            reqwest::Method::PATCH,
            &format!("/api/v1/maintenance/{id}"),
            Some(body),
        )
        .await
        .map(|_| ())
    }

    pub async fn delete_window(&self, id: i64) -> Result<(), VvError> {
        match self
            .call(
                reqwest::Method::DELETE,
                &format!("/api/v1/maintenance/{id}"),
                None,
            )
            .await
        {
            Ok(_) | Err(VvError::Answer(404, _)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// What "Test connection" found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tested {
    pub ok: bool,
    pub message: String,
    pub too_old: bool,
}

/// Checks the connection: the token works and may write maintenance, the
/// version, the monitors, and whether VV reports results during
/// maintenance.
pub async fn test(core: &Core, conn: Conn) -> Tested {
    use vole_kit::i18n;
    let client = Client::new(&core.http, conn);
    let fail = |message: String| Tested {
        ok: false,
        message,
        too_old: false,
    };
    let me = match client.get("/api/v1/me").await {
        Ok(me) => me,
        Err(VvError::Answer(401 | 403, _)) => return fail(i18n::t("vv.err_token").to_string()),
        Err(e) => return fail(i18n::tf("vv.err_unreachable", &[("error", &e.to_string())])),
    };
    if me["can_edit"].as_bool() != Some(true) {
        return fail(i18n::t("vv.err_write").to_string());
    }
    let version = client
        .get("/api/v1/health")
        .await
        .ok()
        .and_then(|h| h["version"].as_str().map(str::to_string))
        .unwrap_or_default();
    let monitors = match client.monitors().await {
        Ok(m) => m,
        Err(e) => return fail(i18n::tf("vv.err_unreachable", &[("error", &e.to_string())])),
    };
    let too_old = match monitors.first() {
        Some(_) => monitors.iter().any(|m| !m.has_raw),
        None => semver::Version::parse(&version).is_ok_and(|v| v < semver::Version::new(0, 9, 0)),
    };
    let message = i18n::tf(
        "vv.test_ok",
        &[
            ("version", &version),
            ("user", me["username"].as_str().unwrap_or_default()),
            ("n", &monitors.len().to_string()),
        ],
    );
    Tested {
        ok: true,
        message,
        too_old,
    }
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

pub const LINK_MONITOR: &str = "monitor";
pub const LINK_TAG: &str = "tag";

/// A monitor or tag linked to a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub id: i64,
    pub kind: String,
    pub r#ref: String,
    pub label: String,
}

pub fn links(c: &Connection, target_id: i64) -> rusqlite::Result<Vec<Link>> {
    c.prepare(
        "SELECT id, kind, ref, label FROM target_monitors WHERE target_id = ?1 ORDER BY kind, label, ref",
    )?
    .query_map([target_id], |r| {
        Ok(Link {
            id: r.get(0)?,
            kind: r.get(1)?,
            r#ref: r.get(2)?,
            label: r.get(3)?,
        })
    })?
    .collect()
}

/// Links a monitor (`ref` = its id) or a tag (`ref` = its name).
pub fn add_link(
    c: &Connection,
    target_id: i64,
    kind: &str,
    r#ref: &str,
    label: &str,
    user: &str,
) -> rusqlite::Result<bool> {
    let n = c.execute(
        "INSERT OR IGNORE INTO target_monitors (target_id, kind, ref, label)
         SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM targets WHERE id = ?1)",
        params![target_id, kind, r#ref, label],
    )?;
    if n > 0 {
        crate::runs::log_action(
            c,
            user,
            "link_monitor",
            Some(target_id),
            None,
            &format!("{kind} {ref}"),
        )?;
    }
    Ok(n > 0)
}

pub fn remove_link(
    c: &Connection,
    target_id: i64,
    link_id: i64,
    user: &str,
) -> rusqlite::Result<bool> {
    let n = c.execute(
        "DELETE FROM target_monitors WHERE id = ?1 AND target_id = ?2",
        params![link_id, target_id],
    )?;
    if n > 0 {
        crate::runs::log_action(
            c,
            user,
            "unlink_monitor",
            Some(target_id),
            None,
            &link_id.to_string(),
        )?;
    }
    Ok(n > 0)
}

/// The monitors the links name (a tag: every monitor with it), by id.
pub fn resolve<'m>(links: &[Link], all: &'m [Monitor]) -> Vec<&'m Monitor> {
    let mut out: Vec<&Monitor> = all
        .iter()
        .filter(|m| {
            links.iter().any(|l| match l.kind.as_str() {
                LINK_MONITOR => l.r#ref == m.id.to_string(),
                _ => m.tags.iter().any(|t| t.eq_ignore_ascii_case(&l.r#ref)),
            })
        })
        .collect();
    out.sort_by_key(|m| m.id);
    out
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// How long a window is expected to take: twice the health period and 15
/// minutes for the rest (minutes).
pub fn expected_minutes(health_seconds: i64) -> i64 {
    (2 * health_seconds + 59) / 60 + 15
}

/// An open window lasts at least this long; it closes when the run ends.
pub const OPEN_MINUTES: i64 = 120;

fn window_body(
    title: &str,
    description: &str,
    start: Timestamp,
    minutes: i64,
    ids: &[i64],
    pages: &[i64],
) -> Value {
    let end = start + jiff::SignedDuration::from_mins(minutes);
    json!({
        "title": title,
        "description": description,
        "strategy": "once",
        "active": true,
        "timezone": "UTC",
        "start_at": db_time(start),
        "end_at": db_time(end),
        "monitor_ids": ids,
        "status_page_ids": pages,
    })
}

/// What a run's windows need from the database.
#[derive(Debug, Clone)]
struct RunInfo {
    run: crate::runs::Run,
    links: Vec<Link>,
    require_vv: bool,
    health: i64,
}

fn run_info(c: &Connection, run_id: i64) -> rusqlite::Result<Option<RunInfo>> {
    let Some(run) = crate::runs::get(c, run_id)? else {
        return Ok(None);
    };
    let tid = run.target_id.unwrap_or_default();
    let (require_vv, health): (bool, Option<i64>) = c
        .query_row(
            "SELECT require_vv, health_seconds FROM targets WHERE id = ?1",
            [tid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((false, None));
    Ok(Some(RunInfo {
        links: links(c, tid)?,
        require_vv,
        health: health
            .unwrap_or(crate::runs::HEALTH_SECONDS)
            .clamp(crate::runs::MIN_HEALTH, crate::runs::MAX_HEALTH),
        run,
    }))
}

/// Records one of Wren's own steps of a run (`maintenance`, `monitors`,
/// `close`) next to the agent's.
pub fn server_step(
    c: &Connection,
    run_id: i64,
    step: &str,
    status: &str,
    log: &str,
) -> rusqlite::Result<()> {
    let log: String = log.chars().take(crate::runs::MAX_LOG).collect();
    c.execute(
        "INSERT INTO run_steps (run_id, step, status, log) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (run_id, step) DO UPDATE SET status = excluded.status, log = excluded.log,
             duration_ms = CAST((julianday('now') - julianday(started_at)) * 86400000 AS INTEGER)",
        params![run_id, step, status, log],
    )?;
    Ok(())
}

/// The window's title and description in the site's language.
async fn texts(core: &Core, run: &crate::runs::Run) -> (String, String) {
    use vole_kit::i18n;
    let lang = i18n::site_language(core).await.unwrap_or_default();
    i18n::sync_scope(lang, || {
        let args = [
            ("name", run.target_name.as_str()),
            ("server", run.server_name.as_str()),
        ];
        (
            i18n::tf("vv.window_title", &args),
            i18n::tf("vv.window_text", &args),
        )
    })
}

/// Step 2: opens the run's maintenance window (or makes its planned window
/// start now). `Err`: the run must stop before anything stops (the target
/// requires VV and it couldn't be done).
pub async fn before_stop(wren: &Wren, run_id: i64) -> Result<(), String> {
    let core = &wren.core;
    let Ok(Some(info)) = core.with_conn(move |c| run_info(c, run_id)).await else {
        return Ok(());
    };
    if info.links.is_empty() {
        let _ = core
            .with_conn(move |c| {
                server_step(
                    c,
                    run_id,
                    "maintenance",
                    "skipped",
                    "No Vigilant Vole monitors are linked to this target: no maintenance window.",
                )?;
                c.execute(
                    "UPDATE runs SET vv_state = CASE WHEN vv_state IN ('planned', 'replan') THEN vv_state ELSE 'none' END WHERE id = ?1",
                    [run_id],
                )
            })
            .await;
        return Ok(());
    }
    let Some(conn) = connection(core).await else {
        return not_opened(
            wren,
            &info,
            "Vigilant Vole isn't connected (Settings → Vigilant Vole)".into(),
        )
        .await;
    };
    let client = Client::new(&core.http, conn);
    let (title, text) = texts(core, &info.run).await;
    let opened: Result<(i64, Vec<String>), VvError> = async {
        let all = client.monitors().await?;
        let found = resolve(&info.links, &all);
        if found.is_empty() {
            return Err(VvError::Answer(
                404,
                "none of the linked monitors exist in Vigilant Vole".into(),
            ));
        }
        let ids: Vec<i64> = found.iter().map(|m| m.id).collect();
        let names: Vec<String> = found.iter().map(|m| m.name.clone()).collect();
        let pages = client.pages_showing(&ids).await.unwrap_or_default();
        let minutes = expected_minutes(info.health).max(OPEN_MINUTES);
        let body = window_body(&title, &text, Timestamp::now(), minutes, &ids, &pages);
        let planned = matches!(info.run.vv_state.as_str(), "planned" | "replan");
        let id = match info.run.vv_window_id {
            Some(id) if planned => match client.patch_window(id, &body).await {
                Ok(()) => id,
                Err(VvError::Answer(404, _)) => client.create_window(&body).await?,
                Err(e) => return Err(e),
            },
            _ => client.create_window(&body).await?,
        };
        Ok((id, names))
    }
    .await;
    match opened {
        Ok((window, names)) => {
            let log = format!(
                "Maintenance window #{window} is open in Vigilant Vole for: {}.",
                names.join(", ")
            );
            let _ = core
                .with_conn(move |c| {
                    server_step(c, run_id, "maintenance", "ok", &log)?;
                    c.execute(
                        "UPDATE runs SET vv_window_id = ?2, vv_state = 'open' WHERE id = ?1",
                        params![run_id, window],
                    )
                })
                .await;
            tracing::info!(run_id, window, "maintenance window open");
            Ok(())
        }
        Err(e) => not_opened(wren, &info, e.to_string()).await,
    }
}

async fn not_opened(wren: &Wren, info: &RunInfo, why: String) -> Result<(), String> {
    let run_id = info.run.id;
    if info.require_vv {
        let msg = format!(
            "{why}. This target requires Vigilant Vole, so the run stopped before anything was touched."
        );
        let m = msg.clone();
        let _ = wren
            .core
            .with_conn(move |c| {
                server_step(c, run_id, "maintenance", "failed", &m)?;
                c.execute(
                    "UPDATE runs SET detail = ?2 WHERE id = ?1",
                    params![run_id, m],
                )
            })
            .await;
        return Err(msg);
    }
    let msg = format!("{why}. The run goes on without a maintenance window.");
    let m = msg.clone();
    let _ = wren
        .core
        .with_conn(move |c| {
            server_step(c, run_id, "maintenance", "failed", &m)?;
            c.execute(
                "UPDATE runs SET vv_state = CASE WHEN vv_state IN ('planned', 'replan') THEN vv_state ELSE 'none' END WHERE id = ?1",
                [run_id],
            )
        })
        .await;
    tracing::warn!(run_id, "{msg}");
    let (w, run) = (wren.clone(), info.run.clone());
    tokio::spawn(async move { crate::run_notify::send_no_window(&w, &run, &msg).await });
    Ok(())
}

/// One pass of the reconciler: closes the window of every run that ended,
/// deletes planned windows no run needs, and creates or moves the planned
/// windows of scheduled runs. Failures are retried on the next pass.
/// Whether there was anything to do.
pub async fn sync(wren: &Wren) -> bool {
    let core = &wren.core;
    let Ok(ids) = core
        .with_conn(|c| {
            c.prepare(
                "SELECT id FROM runs WHERE
                    (vv_state IN ('open', 'planned', 'replan') AND status NOT IN ('queued', 'running'))
                 OR (status = 'queued' AND scheduled_for IS NOT NULL AND vv_state IN ('', 'replan'))
                 ORDER BY id",
            )?
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
    else {
        return false;
    };
    if ids.is_empty() {
        return false;
    }
    let conn = connection(core).await;
    for id in ids {
        if let Err(e) = sync_one(wren, conn.as_ref(), id).await {
            tracing::warn!(run_id = id, "maintenance window not updated: {e}");
        }
    }
    true
}

async fn sync_one(wren: &Wren, conn: Option<&Conn>, run_id: i64) -> Result<(), VvError> {
    let core = &wren.core;
    let Ok(Some(info)) = core.with_conn(move |c| run_info(c, run_id)).await else {
        return Ok(());
    };
    let run = &info.run;
    let ended = run.status != crate::runs::QUEUED && run.status != crate::runs::RUNNING;
    let set = |state: &'static str, window: Option<i64>, step: Option<(&'static str, String)>| {
        core.with_conn(move |c| {
            c.execute(
                "UPDATE runs SET vv_state = ?2, vv_window_id = COALESCE(?3, vv_window_id) WHERE id = ?1",
                params![run_id, state, window],
            )?;
            if let Some((status, log)) = step {
                server_step(c, run_id, "close", status, &log)?;
            }
            Ok(())
        })
    };
    // Planned for a run that needs no window now.
    if !ended && info.links.is_empty() {
        let _ = set("none", None, None).await;
        if let (Some(w), Some(conn)) = (run.vv_window_id, conn) {
            Client::new(&core.http, conn.clone())
                .delete_window(w)
                .await?;
        }
        return Ok(());
    }
    let Some(conn) = conn else {
        return Err(VvError::Unreachable("not connected".into()));
    };
    let client = Client::new(&core.http, conn.clone());
    match (ended, run.vv_state.as_str(), run.vv_window_id) {
        (true, "open", Some(w)) => {
            // Ends a minute from now: the monitors have a moment after the
            // last restart before they count publicly again.
            let end = Timestamp::now() + jiff::SignedDuration::from_mins(1);
            match client
                .patch_window(w, &json!({ "end_at": db_time(end) }))
                .await
            {
                Ok(()) | Err(VvError::Answer(404, _)) => {}
                Err(e) => return Err(e),
            }
            let log = format!(
                "Maintenance window #{w} in Vigilant Vole ends at {} UTC.",
                db_time(end)
            );
            let _ = set("closed", None, Some(("ok", log))).await;
            tracing::info!(run_id, window = w, "maintenance window closed");
        }
        (true, _, Some(w)) => {
            client.delete_window(w).await?;
            let log = format!("The planned maintenance window #{w} in Vigilant Vole is deleted.");
            let _ = set("closed", None, Some(("ok", log))).await;
        }
        (true, _, None) => {
            let _ = set("closed", None, None).await;
        }
        (false, state, window) => {
            let Some(at) = run.scheduled_for.as_deref().and_then(parse_db_time) else {
                return Ok(());
            };
            let all = client.monitors().await?;
            let found = resolve(&info.links, &all);
            if found.is_empty() {
                let _ = set("none", None, None).await;
                return Ok(());
            }
            let ids: Vec<i64> = found.iter().map(|m| m.id).collect();
            let pages = client.pages_showing(&ids).await.unwrap_or_default();
            let (title, text) = texts(core, run).await;
            let body = window_body(
                &title,
                &text,
                at,
                expected_minutes(info.health),
                &ids,
                &pages,
            );
            let id = match (state, window) {
                ("replan", Some(w)) => match client.patch_window(w, &body).await {
                    Ok(()) => w,
                    Err(VvError::Answer(404, _)) => client.create_window(&body).await?,
                    Err(e) => return Err(e),
                },
                _ => client.create_window(&body).await?,
            };
            // Only while the run still waits; else the window goes again.
            let old = state.to_string();
            let kept = core
                .with_conn(move |c| {
                    c.execute(
                        "UPDATE runs SET vv_state = 'planned', vv_window_id = ?2
                         WHERE id = ?1 AND vv_state = ?3 AND status = 'queued'",
                        params![run_id, id, old],
                    )
                })
                .await
                .unwrap_or(0);
            if kept == 0 && window != Some(id) {
                client.delete_window(id).await?;
            } else {
                tracing::info!(run_id, window = id, at = %db_time(at), "planned maintenance window");
            }
        }
    }
    Ok(())
}

/// Runs the reconciler every [`SYNC_SECONDS`] and when nudged
/// ([`Wren::vv_sync`]).
pub fn spawn_sync(wren: Wren) {
    tokio::spawn(async move {
        loop {
            if sync(&wren).await {
                let _ = wren.changes.send(());
            }
            tokio::select! {
                () = wren.vv_sync.notified() => {}
                () = tokio::time::sleep(Duration::from_secs(SYNC_SECONDS)) => {}
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Monitor health (step 6)
// ---------------------------------------------------------------------------

/// What Wren tells the agent about the monitors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Ok(String),
    Wait(String),
    Failed(String),
    /// Not counted: the container checks decide.
    Skipped(String),
}

impl Verdict {
    pub fn word(&self) -> &'static str {
        match self {
            Verdict::Ok(_) => "ok",
            Verdict::Wait(_) => "wait",
            Verdict::Failed(_) => "failed",
            Verdict::Skipped(_) => "skipped",
        }
    }

    pub fn log(&self) -> &str {
        match self {
            Verdict::Ok(l) | Verdict::Wait(l) | Verdict::Failed(l) | Verdict::Skipped(l) => l,
        }
    }
}

/// One real result of a monitor: when (Unix seconds) and whether it was up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub at: i64,
    pub up: bool,
    pub down: bool,
}

/// The rules: from `t0` (the start of the health step) every monitor needs
/// results, the latest `up`, and `up` without a break for `need` seconds
/// (counted from `t0`, or from the first `up` after the last other result).
/// A `down` after `t0 + need` fails at once; past `t0 + 2·need + 300 s` it
/// fails too. Anything else waits.
pub fn verdict(now: i64, t0: i64, need: i64, monitors: &[(String, Vec<Sample>)]) -> Verdict {
    let deadline = t0 + 2 * need + 300;
    let mut waiting = Vec::new();
    let mut good = Vec::new();
    for (name, samples) in monitors {
        let mut s: Vec<Sample> = samples.iter().copied().filter(|x| x.at >= t0).collect();
        s.sort_by_key(|x| x.at);
        let Some(last) = s.last() else {
            waiting.push(format!("{name}: no result yet"));
            continue;
        };
        if last.down && last.at >= t0 + need {
            return Verdict::Failed(format!(
                "{name} is down in Vigilant Vole {} s after the update",
                last.at - t0
            ));
        }
        if !last.up {
            waiting.push(format!(
                "{name}: {}",
                if last.down { "down" } else { "pending" }
            ));
            continue;
        }
        let since = s
            .iter()
            .rposition(|x| !x.up)
            .map_or(t0, |i| s.get(i + 1).map_or(now, |x| x.at));
        if now - since >= need {
            good.push(name.clone());
        } else {
            waiting.push(format!("{name}: up for {} of {need} s", now - since));
        }
    }
    if waiting.is_empty() {
        return Verdict::Ok(format!(
            "Up in Vigilant Vole for {need} s: {}.",
            good.join(", ")
        ));
    }
    if now >= deadline {
        return Verdict::Failed(format!(
            "The monitors weren't up for {need} s within {} s: {}",
            deadline - t0,
            waiting.join("; ")
        ));
    }
    Verdict::Wait(waiting.join("; "))
}

fn unix(raw: &str) -> Option<i64> {
    parse_db_time(raw).map(|t| t.as_second())
}

/// The verdict for a running run of this server (`None`: not its run).
pub async fn check_monitors(wren: &Wren, server_id: i64, run_id: i64) -> Option<Verdict> {
    let core = &wren.core;
    let (info, t0, job_health) = core
        .with_conn(move |c| {
            let running: bool = c.query_row(
                "SELECT EXISTS (SELECT 1 FROM runs WHERE id = ?1 AND server_id = ?2 AND status = 'running')",
                params![run_id, server_id],
                |r| r.get(0),
            )?;
            if !running {
                return Ok(None);
            }
            let t0: Option<String> = c
                .query_row(
                    "SELECT started_at FROM run_steps WHERE run_id = ?1 AND step = 'health'",
                    [run_id],
                    |r| r.get(0),
                )
                .optional()?;
            let job: String =
                c.query_row("SELECT job FROM runs WHERE id = ?1", [run_id], |r| r.get(0))?;
            let health = serde_json::from_str::<Value>(&job)
                .ok()
                .and_then(|j| j["health_seconds"].as_i64());
            Ok(run_info(c, run_id)?.map(|i| (i, t0, health)))
        })
        .await
        .ok()
        .flatten()?;
    let now = Timestamp::now().as_second();
    let t0s = t0.as_deref().and_then(unix).unwrap_or(now);
    let need = job_health.unwrap_or(info.health);
    let v = monitor_verdict(
        wren,
        &info,
        t0.as_deref().unwrap_or_default(),
        t0s,
        need,
        now,
    )
    .await;
    let (status, log) = (
        match &v {
            Verdict::Wait(_) => "running",
            Verdict::Ok(_) => "ok",
            Verdict::Failed(_) => "failed",
            Verdict::Skipped(_) => "skipped",
        },
        v.log().to_string(),
    );
    let _ = core
        .with_conn(move |c| server_step(c, run_id, "monitors", status, &log))
        .await;
    let _ = wren.changes.send(());
    Some(v)
}

async fn monitor_verdict(
    wren: &Wren,
    info: &RunInfo,
    since: &str,
    t0: i64,
    need: i64,
    now: i64,
) -> Verdict {
    if info.links.is_empty() {
        return Verdict::Skipped(
            "No Vigilant Vole monitors are linked to this target: the container checks decide."
                .into(),
        );
    }
    let deadline = t0 + 2 * need + 300;
    let unreachable = |e: String| {
        if now < deadline {
            Verdict::Wait(format!("{e}; trying again"))
        } else if info.require_vv {
            Verdict::Failed(format!(
                "{e}; this target requires Vigilant Vole, so its health can't be confirmed"
            ))
        } else {
            Verdict::Skipped(format!("{e}; the container checks decide"))
        }
    };
    let Some(conn) = connection(&wren.core).await else {
        return unreachable("Vigilant Vole isn't connected".into());
    };
    let client = Client::new(&wren.core.http, conn);
    let all = match client.monitors().await {
        Ok(m) => m,
        Err(e) => return unreachable(e.to_string()),
    };
    let found = resolve(&info.links, &all);
    if found.iter().any(|m| !m.has_raw) {
        return Verdict::Skipped(TOO_OLD.into());
    }
    let active: Vec<&&Monitor> = found.iter().filter(|m| m.active).collect();
    if active.is_empty() {
        return Verdict::Skipped(
            "None of the linked monitors is active in Vigilant Vole: the container checks decide."
                .into(),
        );
    }
    let since = if since.is_empty() {
        db_time(Timestamp::from_second(t0).unwrap_or_else(|_| Timestamp::now()))
    } else {
        since.to_string()
    };
    let mut monitors = Vec::new();
    for m in active {
        let results = match client.results(m.id, &since).await {
            Ok(r) => r,
            Err(e) => return unreachable(e.to_string()),
        };
        let samples = results
            .iter()
            .filter_map(|(at, raw)| {
                let raw = raw.as_str().unwrap_or_default();
                Some(Sample {
                    at: unix(at)?,
                    up: raw == "up",
                    down: raw == "down",
                })
            })
            .collect();
        monitors.push((m.name.clone(), samples));
    }
    verdict(now, t0, need, &monitors)
}

/// `POST /api/agent/v1/runs/{id}/monitors` → `{"verdict", "log"}`.
pub async fn monitors_route(
    Extension(wren): Extension<Wren>,
    headers: HeaderMap,
    Path(run_id): Path<i64>,
) -> Response {
    let server_id = match crate::runs::agent(&wren, &headers).await {
        Ok(id) => id,
        Err(res) => return *res,
    };
    match check_monitors(&wren, server_id, run_id).await {
        Some(v) => Json(json!({ "verdict": v.word(), "log": v.log() })).into_response(),
        None => ApiError::NotFound.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(at: i64) -> Sample {
        Sample {
            at,
            up: true,
            down: false,
        }
    }
    fn down(at: i64) -> Sample {
        Sample {
            at,
            up: false,
            down: true,
        }
    }

    #[test]
    fn monitor_verdict_rules() {
        let m = |s: Vec<Sample>| vec![("web".to_string(), s)];
        // Up all along for the period.
        assert!(matches!(
            verdict(1180, 1000, 180, &m(vec![up(1020), up(1080)])),
            Verdict::Ok(_)
        ));
        // Results from before the update don't count.
        assert!(matches!(
            verdict(1180, 1000, 180, &m(vec![up(900)])),
            Verdict::Wait(_)
        ));
        // Down while starting, then up: the period counts from the first up.
        assert!(matches!(
            verdict(1180, 1000, 180, &m(vec![down(1005), up(1065)])),
            Verdict::Wait(_)
        ));
        assert!(matches!(
            verdict(1250, 1000, 180, &m(vec![down(1005), up(1065), up(1245)])),
            Verdict::Ok(_)
        ));
        // Down after the period: fails at once.
        assert!(matches!(
            verdict(1200, 1000, 180, &m(vec![up(1020), down(1190)])),
            Verdict::Failed(_)
        ));
        // Never up: fails at the deadline (t0 + 2·need + 300).
        assert!(matches!(
            verdict(1659, 1000, 180, &m(vec![])),
            Verdict::Wait(_)
        ));
        assert!(matches!(
            verdict(1660, 1000, 180, &m(vec![])),
            Verdict::Failed(_)
        ));
        // Every monitor counts.
        let two = vec![("a".to_string(), vec![up(1010)]), ("b".to_string(), vec![])];
        assert!(matches!(verdict(1180, 1000, 180, &two), Verdict::Wait(_)));
    }

    #[test]
    fn links_resolve_by_id_and_tag() {
        let m = |id, tag: &str| Monitor {
            id,
            name: format!("m{id}"),
            tags: vec![tag.into()],
            active: true,
            has_raw: true,
        };
        let all = vec![m(1, "web"), m(2, "Wren"), m(3, "wren")];
        let l = |kind: &str, r: &str| Link {
            id: 0,
            kind: kind.into(),
            r#ref: r.into(),
            label: String::new(),
        };
        let ids: Vec<i64> = resolve(&[l("monitor", "1"), l("tag", "WREN")], &all)
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(expected_minutes(180), 21);
        assert_eq!(
            normal_url(" http://10.0.3.26:3001/ ").as_deref(),
            Some("http://10.0.3.26:3001")
        );
        assert_eq!(normal_url("ftp://x"), None);
    }
}
