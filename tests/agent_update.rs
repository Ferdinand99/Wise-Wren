//! The agent's self-update on the server: the heartbeat report, its storage
//! and what an admin may do about it (design: Agent self-update).

mod common;

use axum::http::StatusCode;
use std::sync::{Arc, Mutex};

use common::{
    App, Web, add_hook, add_user, api_token, hooks, poll_as, post_json, set_remote, step,
};
use serde_json::{Value, json};
use vole_kit::auth::TokenScope;
use wise_wren::agent_update::{self, Release};
use wise_wren::runs;

pub const D: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A heartbeat of agent `version` (process `boot`), with `su` as its
/// self-update report when given.
async fn beat(app: &App, token: &str, version: &str, boot: &str, su: Option<Value>) -> common::Res {
    let mut body = json!({"agent_version": version, "boot_id": boot, "uptime_seconds": 100});
    if let Some(su) = su {
        body["self_update"] = su;
    }
    let res = post_json(app, "/api/agent/v1/heartbeat", token, body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res
}

fn su(enabled: bool, blocked: &str) -> Value {
    json!({"supported": true, "enabled": enabled, "blocked": blocked, "container": "wren-agent",
           "reference": "ghcr.io/ferdinand99/wise-wren-agent:latest"})
}

async fn setup() -> (App, i64, String) {
    setup_with(None).await
}

/// [`setup`] with a webhook channel that exists before the first heartbeat.
async fn setup_with_hook(url: String) -> (App, i64, String) {
    setup_with(Some(url)).await
}

async fn setup_with(hook: Option<String>) -> (App, i64, String) {
    let app = App::new().await;
    let (id, token) = app.add_server("Tower").await;
    *app.wren.agent_release.write().unwrap() = Some(Release {
        version: "0.10.1".into(),
        digest: D.into(),
        newest_wren: None,
    });
    if let Some(url) = hook {
        add_hook(&app, url).await;
    }
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, ""))).await;
    (app, id, token)
}

#[tokio::test]
async fn the_migration_adds_the_columns() {
    let (app, _, _) = setup().await;
    assert_eq!(
        app.count(
            "SELECT COUNT(*) FROM pragma_table_info('servers') WHERE name IN \
             ('self_update_report', 'agent_auto_update', 'agent_update_announced')"
        )
        .await,
        3
    );
}

#[tokio::test]
async fn the_heartbeat_report_is_stored_and_cleared() {
    let (app, id, token) = setup().await;
    let rel = agent_update::release(&app.wren);
    let st = app
        .wren
        .core
        .with_conn(move |c| agent_update::standing_of(c, id, rel.as_ref()))
        .await
        .unwrap();
    assert_eq!(st.state, agent_update::State::Available);
    assert_eq!(st.available.as_deref(), Some("0.10.1"));
    beat(&app, &token, "0.10.0", "boot-a", None).await;
    assert_eq!(
        app.count("SELECT COUNT(*) FROM servers WHERE self_update_report IS NULL")
            .await,
        1
    );
}

#[tokio::test]
async fn a_blocked_or_odd_report_is_read_leniently() {
    let (app, id, token) = setup().await;
    let state = |app: &App| {
        let rel = agent_update::release(&app.wren);
        let core = app.wren.core.clone();
        async move {
            core.with_conn(move |c| agent_update::standing_of(c, id, rel.as_ref()))
                .await
                .unwrap()
        }
    };
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, "pinned"))).await;
    let st = state(&app).await;
    assert_eq!(
        (st.state, st.reason.as_str()),
        (agent_update::State::Blocked, "pinned")
    );
    // A report of the wrong shape counts as none, and the heartbeat still works.
    beat(&app, &token, "0.10.0", "boot-a", Some(json!("nonsense"))).await;
    assert_eq!(state(&app).await.state, agent_update::State::Unsupported);
    // Unknown fields and a pending update are fine.
    let mut report = su(true, "");
    report["pending"] =
        json!({"run_id": 7, "phase": "probation", "to_version": "0.10.1", "extra": 1});
    report["future"] = json!(true);
    beat(&app, &token, "0.10.0", "boot-a", Some(report)).await;
    let stored = app
        .wren
        .core
        .with_conn(move |c| agent_update::report_of(c, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.pending.unwrap().run_id, 7);
}

#[tokio::test]
async fn an_active_run_makes_the_agent_running() {
    let (app, id, _) = setup().await;
    app.exec(format!(
        "INSERT INTO runs (server_id, target_kind, target_name, kind, status, requested_by)
         VALUES ({id}, 'agent', 'wren-agent', 'agent_update', 'queued', 'admin')"
    ))
    .await;
    let rel = agent_update::release(&app.wren);
    let (active, running, st) = app
        .wren
        .core
        .with_conn(move |c| {
            Ok((
                agent_update::active_run(c, id)?,
                agent_update::running_run(c, id)?,
                agent_update::standing_of(c, id, rel.as_ref())?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((active, running), (Some(1), None));
    assert_eq!(
        (st.state, st.run_id),
        (agent_update::State::Running, Some(1))
    );
    app.exec("UPDATE runs SET status = 'running'".into()).await;
    let running = app
        .wren
        .core
        .with_conn(move |c| agent_update::running_run(c, id))
        .await
        .unwrap();
    assert_eq!(running, Some(1));
}

// --- Approval, dispatch and the job ----------------------------------------

async fn poll(app: &App, token: &str, boot: &str) -> common::Res {
    poll_as(app, token, boot).await
}

async fn runs_get(app: &App, id: i64) -> runs::Run {
    app.wren
        .core
        .with_conn(move |c| runs::get(c, id))
        .await
        .unwrap()
        .unwrap()
}

async fn admin_token(app: &App) -> String {
    let id = add_user(app, "ad", "admin").await;
    api_token(app, id, TokenScope::Write).await
}

/// Approves the agent update to `version` as an admin; the run's id.
async fn approve_as_admin(app: &App, id: i64, version: &str) -> i64 {
    let ad = admin_token(app).await;
    let res = post_json(
        app,
        &format!("/api/v1/servers/{id}/agent-update"),
        &ad,
        json!({"version": version, "at": null}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.json()["run_id"].as_i64().unwrap()
}

/// An inventory with one container, running an older digest of `image`.
async fn send_inventory_with(app: &App, token: &str, name: &str, image: &str) {
    let repo = image.rsplit_once(':').map_or(image, |(repo, _)| repo);
    let inv = json!({"containers": [{
        "name": name, "image": image, "image_id": "sha256:1",
        "repo_digests": [format!("{repo}@sha256:{}", "a".repeat(64))],
        "state": "running", "labels": {},
    }]});
    let res = post_json(app, "/api/agent/v1/inventory", token, inv).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn only_admins_approve_an_agent_update() {
    let (app, id, _) = setup().await;
    let ed = api_token(
        &app,
        add_user(&app, "ed", "editor").await,
        TokenScope::Write,
    )
    .await;
    let ad = api_token(&app, add_user(&app, "ad", "admin").await, TokenScope::Write).await;
    let body = json!({"version": "0.10.1", "at": null});
    let url = format!("/api/v1/servers/{id}/agent-update");
    assert_eq!(
        post_json(&app, &url, &ed, body.clone()).await.status,
        StatusCode::FORBIDDEN
    );
    let res = post_json(&app, &url, &ad, body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let run = runs_get(&app, res.json()["run_id"].as_i64().unwrap()).await;
    assert_eq!(
        (
            run.kind.as_str(),
            run.target_kind.as_str(),
            run.status.as_str()
        ),
        ("agent_update", "agent", "queued")
    );
    assert_eq!(run.to_key, format!("0.10.1@{D}"));
    assert_eq!(run.from_key, "0.10.0");
    assert_eq!(run.shown, "0.10.0 → 0.10.1");
    assert_eq!(run.target_name, "wren-agent");
    assert_eq!(
        app.count("SELECT COUNT(*) FROM actions WHERE action = 'agent_update'")
            .await,
        1
    );
}

#[tokio::test]
async fn the_job_names_only_version_and_digest() {
    let (app, id, token) = setup().await;
    let run = approve_as_admin(&app, id, "0.10.1").await;
    let job = poll(&app, &token, "boot-a").await.json()["job"].clone();
    let mut keys: Vec<&str> = job
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "from_key",
            "kind",
            "run_id",
            "target",
            "to_digest",
            "to_key",
            "to_version"
        ]
    );
    assert_eq!(
        (
            job["kind"].as_str(),
            job["to_version"].as_str(),
            job["to_digest"].as_str()
        ),
        (Some("agent_update"), Some("0.10.1"), Some(D))
    );
    assert_eq!(job["run_id"], run);
    assert_eq!(runs_get(&app, run).await.status, "running");
}

#[tokio::test]
async fn a_queued_agent_update_lapses_when_wren_moved_on() {
    let (app, id, token) = setup().await;
    let run = approve_as_admin(&app, id, "0.10.1").await;
    *app.wren.agent_release.write().unwrap() = Some(Release {
        version: "0.10.2".into(),
        digest: D.into(),
        newest_wren: None,
    });
    assert_eq!(
        poll(&app, &token, "boot-a").await.status,
        StatusCode::NO_CONTENT
    );
    let lapsed = runs_get(&app, run).await;
    assert_eq!(lapsed.status, "lapsed");
    assert!(
        lapsed.detail.contains("release changed"),
        "{}",
        lapsed.detail
    );
}

#[tokio::test]
async fn an_agent_update_lapses_when_the_agent_can_no_longer_update() {
    let (app, id, token) = setup().await;
    let run = approve_as_admin(&app, id, "0.10.1").await;
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, "pinned"))).await;
    assert_eq!(
        poll(&app, &token, "boot-a").await.status,
        StatusCode::NO_CONTENT
    );
    let lapsed = runs_get(&app, run).await;
    assert_eq!(lapsed.status, "lapsed");
    assert!(
        lapsed.detail.contains("can no longer be updated"),
        "{}",
        lapsed.detail
    );
}

#[tokio::test]
async fn agent_update_refusals() {
    let (app, id, token) = setup().await;
    let ad = admin_token(&app).await;
    let url = format!("/api/v1/servers/{id}/agent-update");
    // Wren moved on since the person looked.
    assert_eq!(
        post_json(&app, &url, &ad, json!({"version": "0.10.2"}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    // A time in the past.
    let past = json!({"version": "0.10.1", "at": "2020-01-01T00:00:00Z"});
    assert_eq!(
        post_json(&app, &url, &ad, past).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post_json(&app, &url, &ad, json!({"version": "0.10.1"}))
            .await
            .status,
        StatusCode::OK
    );
    // Already queued.
    assert_eq!(
        post_json(&app, &url, &ad, json!({"version": "0.10.1"}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, "pinned"))).await;
    app.exec("UPDATE runs SET status = 'cancelled'".into())
        .await;
    let res = post_json(&app, &url, &ad, json!({"version": "0.10.1"})).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert!(
        res.body.contains("No agent update is available"),
        "{}",
        res.body
    );
}

#[tokio::test]
async fn a_scheduled_agent_update_waits_for_its_time() {
    let (app, id, token) = setup().await;
    let ad = admin_token(&app).await;
    let at = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_hours(2))
        .unwrap();
    let res = post_json(
        &app,
        &format!("/api/v1/servers/{id}/agent-update"),
        &ad,
        json!({"version": "0.10.1", "at": at.to_string()}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.json()["scheduled_for"].is_string());
    let run = runs_get(&app, res.json()["run_id"].as_i64().unwrap()).await;
    assert_eq!(run.trigger, "schedule");
    assert_eq!(
        poll(&app, &token, "boot-a").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(runs_get(&app, run.id).await.status, "queued");
}

#[tokio::test]
async fn the_own_container_target_links_the_run() {
    let (app, id, token) = setup().await;
    send_inventory_with(
        &app,
        &token,
        "wren-agent",
        "ghcr.io/ferdinand99/wise-wren-agent:latest",
    )
    .await;
    let run = approve_as_admin(&app, id, "0.10.1").await;
    let tid = app
        .count("SELECT id FROM targets WHERE name = 'wren-agent'")
        .await;
    assert_eq!(runs_get(&app, run).await.target_id, Some(tid));
}

#[tokio::test]
async fn a_read_only_token_cannot_approve_an_agent_update() {
    let (app, id, _) = setup().await;
    let read = api_token(&app, add_user(&app, "rd", "admin").await, TokenScope::Read).await;
    let url = format!("/api/v1/servers/{id}/agent-update");
    let res = post_json(&app, &url, &read, json!({"version": "0.10.1"})).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

#[tokio::test]
async fn a_new_agent_process_does_not_close_an_agent_update() {
    let (app, id, token) = setup().await;
    let run = approve_as_admin(&app, id, "0.10.1").await;
    // Handed to the process boot-a, which then replaces itself.
    poll(&app, &token, "boot-a").await;
    common::step(&app, &token, run, "preflight", "ok").await;
    beat(&app, &token, "0.10.1", "boot-b", Some(su(true, ""))).await;
    assert_eq!(
        poll(&app, &token, "boot-b").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(runs_get(&app, run).await.status, "running");
    assert_eq!(
        common::finish(&app, &token, run, "succeeded").await.status,
        StatusCode::OK
    );
    assert_eq!(runs_get(&app, run).await.status, "succeeded");
}

#[tokio::test]
async fn an_agent_update_may_be_silent_for_45_minutes() {
    let (app, id, token) = setup().await;
    let run = approve_as_admin(&app, id, "0.10.1").await;
    poll(&app, &token, "boot-a").await;
    app.exec(format!(
        "UPDATE runs SET last_report_at = datetime('now', '-30 minutes'); \
         UPDATE servers SET last_seen_at = datetime('now', '-1 hour') WHERE id = {id}"
    ))
    .await;
    assert!(close_silent(&app).await.is_empty());
    app.exec("UPDATE runs SET last_report_at = datetime('now', '-50 minutes')".into())
        .await;
    assert_eq!(close_silent(&app).await.len(), 1);
    let r = runs_get(&app, run).await;
    assert_eq!(r.status, "rollback_failed");
    assert!(
        r.detail.contains("stopped reporting for 45 min"),
        "{}",
        r.detail
    );
    assert!(
        r.detail.contains("docker rename wren-agent-wren-old-"),
        "{}",
        r.detail
    );
}

#[tokio::test]
async fn an_agent_update_is_closed_after_45_minutes_even_online() {
    let (app, id, token) = setup().await;
    let run = approve_as_admin(&app, id, "0.10.1").await;
    poll(&app, &token, "boot-a").await;
    app.exec("UPDATE runs SET last_report_at = datetime('now', '-50 minutes')".into())
        .await;
    // The agent is online: its heartbeats do not count as the run's report.
    assert_eq!(close_silent(&app).await.len(), 1);
    assert_eq!(runs_get(&app, run).await.status, "rollback_failed");
}

async fn close_silent(app: &App) -> Vec<runs::Run> {
    app.wren
        .core
        .with_conn(|c| runs::close_silent(c))
        .await
        .unwrap()
}

/// A heartbeat of process `boot` with a settings report that says it runs
/// settings version `version`.
async fn beat_with_settings_report(app: &App, token: &str, boot: &str, version: i64) -> Value {
    let settings = json!({
        "supported": true, "locked": false, "version": version, "error": "",
        "error_version": null, "mounts": ["/srv"], "presets": [], "custom_stacks": [],
        "current": null,
    });
    let body = json!({"agent_version": "0.10.1", "boot_id": boot, "uptime_seconds": 100,
                      "settings": settings});
    let res = post_json(app, "/api/agent/v1/heartbeat", token, body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.json()
}

#[tokio::test]
async fn settings_wait_while_the_agent_updates() {
    let (app, id, token) = setup().await;
    // Version 1 stored (as if imported), then an admin saves version 2.
    app.wren
        .core
        .with_conn(move |c| {
            let mut s = wren_agent::managed::Settings {
                version: 1,
                ..Default::default()
            };
            c.execute(
                "INSERT INTO agent_settings (server_id, version, settings, updated_at)
                 VALUES (?1, 1, ?2, datetime('now'))",
                rusqlite::params![id, serde_json::to_string(&s).unwrap()],
            )?;
            s.updates.min_health_seconds = 90;
            wise_wren::agent_settings::save_settings(c, id, s, "admin", Some(1)).map(|_| ())
        })
        .await
        .unwrap();
    let run = approve_as_admin(&app, id, "0.10.1").await;
    poll(&app, &token, "boot-a").await;
    // The new agent's probation heartbeat: version 2 waits, the report is kept.
    let reply = beat_with_settings_report(&app, &token, "boot-b", 1).await;
    assert!(reply.get("settings").is_none(), "{reply}");
    assert_eq!(
        app.count("SELECT COUNT(*) FROM servers WHERE settings_report IS NOT NULL")
            .await,
        1
    );
    // Without a report the stored one stays too.
    beat(&app, &token, "0.10.1", "boot-b", None).await;
    assert_eq!(
        app.count("SELECT COUNT(*) FROM servers WHERE settings_report IS NOT NULL")
            .await,
        1
    );
    common::finish(&app, &token, run, "succeeded").await;
    let reply = beat_with_settings_report(&app, &token, "boot-b", 1).await;
    assert_eq!(reply["settings"]["version"], 2, "{reply}");
}

#[tokio::test]
async fn no_offline_notice_during_an_agent_update() {
    let (app, id, token) = setup().await;
    approve_as_admin(&app, id, "0.10.1").await;
    poll(&app, &token, "boot-a").await;
    app.exec(format!(
        "UPDATE servers SET last_seen_at = datetime('now', '-5 minutes') WHERE id = {id}"
    ))
    .await;
    let take = || {
        app.wren
            .core
            .with_conn(|c| wise_wren::run_notify::take_agent_changes(c))
    };
    assert!(take().await.unwrap().is_empty());
    // Past the 45 minutes the run is overdue and the notice may go.
    app.exec("UPDATE runs SET started_at = datetime('now', '-50 minutes')".into())
        .await;
    assert_eq!(take().await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_offline_agent_is_told_after_an_agent_update_ended() {
    let (app, id, token) = setup().await;
    approve_as_admin(&app, id, "0.10.1").await;
    poll(&app, &token, "boot-a").await;
    app.exec(format!(
        "UPDATE servers SET last_seen_at = datetime('now', '-5 minutes') WHERE id = {id}; \
         UPDATE runs SET status = 'succeeded'"
    ))
    .await;
    let changes = app
        .wren
        .core
        .with_conn(|c| wise_wren::run_notify::take_agent_changes(c))
        .await
        .unwrap();
    assert_eq!(changes.len(), 1);
}

#[tokio::test]
async fn no_maintenance_window_for_an_agent_update() {
    let (app, id, token) = setup().await;
    let run = approve_as_admin(&app, id, "0.10.1").await;
    poll(&app, &token, "boot-a").await;
    assert_eq!(
        common::step(&app, &token, run, "preflight", "ok")
            .await
            .json()["cancel"],
        false
    );
    wise_wren::vv::before_stop(&app.wren, run).await.unwrap();
    assert_eq!(
        app.count_sql(format!(
            "SELECT COUNT(*) FROM run_steps WHERE run_id = {run} AND step = 'maintenance'"
        ))
        .await,
        0
    );
}

#[tokio::test]
async fn a_scheduled_agent_update_is_left_alone_by_the_maintenance_sync() {
    let (app, id, _token) = setup().await;
    let ad = admin_token(&app).await;
    let at = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_hours(2))
        .unwrap();
    let res = post_json(
        &app,
        &format!("/api/v1/servers/{id}/agent-update"),
        &ad,
        json!({"version": "0.10.1", "at": at.to_string()}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let run = res.json()["run_id"].as_i64().unwrap();
    // Nothing for the maintenance reconciler to do: it would otherwise pick
    // the scheduled run every pass and announce a change each time.
    assert!(!wise_wren::vv::sync(&app.wren).await);
    assert_eq!(
        app.count_sql(format!(
            "SELECT COUNT(*) FROM run_steps WHERE run_id = {run}"
        ))
        .await,
        0
    );
}

#[tokio::test]
async fn a_probation_report_during_an_agent_update_imports_nothing() {
    let (app, id, token) = setup().await;
    assert_eq!(app.count("SELECT COUNT(*) FROM agent_settings").await, 0);
    approve_as_admin(&app, id, "0.10.1").await;
    poll(&app, &token, "boot-a").await;
    // A report with settings in force, as an unlocked agent without stored
    // settings would see imported: not while the update runs.
    let settings = json!({
        "supported": true, "locked": false, "version": null, "error": "",
        "error_version": null, "mounts": ["/srv"], "presets": [], "custom_stacks": [],
        "current": wren_agent::managed::Settings::default(),
    });
    let body = json!({"agent_version": "0.10.1", "boot_id": "boot-b", "uptime_seconds": 100,
                      "settings": settings});
    let res = post_json(&app, "/api/agent/v1/heartbeat", &token, body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.json().get("settings").is_none(), "{}", res.body);
    assert_eq!(app.count("SELECT COUNT(*) FROM agent_settings").await, 0);
}

// --- Notifications -----------------------------------------------------------

type Got = Arc<Mutex<Vec<Value>>>;

fn count_of(got: &Got) -> usize {
    got.lock().unwrap().len()
}

/// The title of each webhook message so far ("<emoji> <title> — <detail>").
fn titles(got: &Got) -> Vec<String> {
    got.lock()
        .unwrap()
        .iter()
        .map(|m| {
            let summary = m["summary"].as_str().unwrap_or_default();
            let text = summary.split_once(' ').map_or(summary, |(_, rest)| rest);
            text.split(" \u{2014} ")
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// Waits (up to 5 s) until `n` messages came in; their titles.
async fn wait_titles(got: &Got, n: usize) -> Vec<String> {
    for _ in 0..100 {
        if count_of(got) >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    titles(got)
}

/// Waits (up to 5 s) for a message that `found` accepts.
async fn wait_for(got: &Got, found: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..100 {
        if let Some(m) = got.lock().unwrap().iter().find(|m| found(m)) {
            return m.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no such message in {:?}", got.lock().unwrap());
}

/// Gives anything still on its way time to arrive.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn an_available_agent_update_is_announced_once() {
    let (url, got) = hooks().await;
    let (app, _, token) = setup_with_hook(url).await;
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, ""))).await;
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, ""))).await;
    let titles = wait_titles(&got, 1).await;
    assert_eq!(
        titles,
        ["Agent update available for Tower: 0.10.0 \u{2192} 0.10.1"]
    );
    assert_eq!(got.lock().unwrap()[0]["event"], "agent_update_available");
    // A newer release is news again.
    *app.wren.agent_release.write().unwrap() = Some(Release {
        version: "0.10.2".into(),
        digest: D.into(),
        newest_wren: None,
    });
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, ""))).await;
    let titles = wait_titles(&got, 2).await;
    assert_eq!(
        titles[1],
        "Agent update available for Tower: 0.10.0 \u{2192} 0.10.2"
    );
    // And that is all there was.
    settle().await;
    assert_eq!(count_of(&got), 2);
}

#[tokio::test]
async fn announcements_at_the_same_time_send_one_message() {
    let (url, got) = hooks().await;
    let (app, id, _) = setup_with_hook(url).await;
    // The setup's heartbeat announced it already; start over.
    wait_titles(&got, 1).await;
    app.exec("UPDATE servers SET agent_update_announced = ''".into())
        .await;
    got.lock().unwrap().clear();
    let tasks: Vec<_> = (0..8)
        .map(|_| tokio::spawn(agent_update::announce(app.wren.clone(), id)))
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    settle().await;
    assert_eq!(count_of(&got), 1, "{:?}", titles(&got));
    assert_eq!(
        app.count_sql(format!(
            "SELECT COUNT(*) FROM servers WHERE id = {id} AND agent_update_announced = '0.10.1'"
        ))
        .await,
        1
    );
}

#[tokio::test]
async fn nothing_is_announced_for_an_agent_that_cannot_take_the_update() {
    for (version, report) in [
        ("0.10.1", su(true, "")),
        ("0.10.0", su(false, "")),
        ("0.10.0", su(true, "pinned")),
        ("0.9.2", json!(null)),
    ] {
        let (url, got) = hooks().await;
        let app = App::new().await;
        let (id, token) = app.add_server("Tower").await;
        *app.wren.agent_release.write().unwrap() = Some(Release {
            version: "0.10.1".into(),
            digest: D.into(),
            newest_wren: None,
        });
        add_hook(&app, url).await;
        beat(&app, &token, version, "boot-a", Some(report)).await;
        settle().await;
        assert_eq!(count_of(&got), 0, "{version}: {:?}", titles(&got));
        assert_eq!(
            app.count_sql(format!(
                "SELECT COUNT(*) FROM servers WHERE id = {id} AND agent_update_announced = ''"
            ))
            .await,
            1
        );
    }
}

#[tokio::test]
async fn agent_update_results_are_announced() {
    for (outcome, title, line) in [
        ("succeeded", "The agent on Tower is updated to 0.10.1", None),
        (
            "rolled_back",
            "The agent update on Tower was rolled back",
            None,
        ),
        (
            "rollback_failed",
            "The agent update on Tower needs you",
            Some("docker rm -f wren-agent"),
        ),
    ] {
        let (url, got) = hooks().await;
        let (app, id, token) = setup_with_hook(url).await;
        let run = approve_as_admin(&app, id, "0.10.1").await;
        poll(&app, &token, "boot-a").await;
        common::finish(&app, &token, run, outcome).await;
        let msg = wait_for(&got, |m| m.to_string().contains(title)).await;
        if let Some(l) = line {
            assert!(msg.to_string().contains(l), "{msg}");
        }
    }
}

// --- Pages -------------------------------------------------------------------

const AGENT_IMAGE: &str = "ghcr.io/ferdinand99/wise-wren-agent:latest";

/// The server page's text, as `web` sees it now.
async fn server_page(web: &mut Web<'_>, id: i64) -> String {
    web.get(&format!("/servers/{id}")).await.body
}

#[tokio::test]
async fn the_server_page_offers_the_agent_update_to_admins_only() {
    let (app, id, _) = setup().await;
    let mut admin = Web::new(&app, "ad", "admin").await;
    let page = admin.get(&format!("/servers/{id}")).await.body;
    assert!(page.contains("Update the agent to 0.10.1"), "{page}");
    assert!(page.contains(&format!("action=\"/servers/{id}/agent-update\"")));
    assert!(page.contains("0.10.0 → 0.10.1"));
    // The notification links to /servers/{id}#agent-update: the card has it.
    assert!(page.contains("id=\"agent-update\""));
    let mut ed = Web::new(&app, "ed", "editor").await;
    let page = ed.get(&format!("/servers/{id}")).await.body;
    assert!(page.contains("0.10.0 → 0.10.1") && !page.contains("Update the agent to 0.10.1"));
    assert!(!page.contains("/agent-update\""), "an editor gets no form");
    assert_eq!(
        ed.post(
            &format!("/servers/{id}/agent-update"),
            &[("version", "0.10.1"), ("when", "now")]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.count("SELECT COUNT(*) FROM runs WHERE kind = 'agent_update'")
            .await,
        0
    );
}

#[tokio::test]
async fn the_card_says_why_not() {
    let (app, id, token) = setup().await;
    let mut web = Web::new(&app, "ad", "admin").await;
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, "pinned"))).await;
    assert!(
        server_page(&mut web, id)
            .await
            .contains("change it to :latest or :0")
    );
    // A reason only a newer agent knows gets the generic text, with its code.
    let odd = su(true, "from_the_future");
    beat(&app, &token, "0.10.0", "boot-a", Some(odd)).await;
    let page = server_page(&mut web, id).await;
    assert!(page.contains("reports a reason it can"), "{page}");
    assert!(page.contains("from_the_future"), "{page}");
    assert!(!page.contains("au_blocked"), "{page}");
    beat(&app, &token, "0.10.0", "boot-a", Some(su(false, ""))).await;
    assert!(
        server_page(&mut web, id)
            .await
            .contains("WW_AGENT_SELF_UPDATE=1")
    );
    beat(&app, &token, "0.9.2", "boot-a", None).await;
    assert!(server_page(&mut web, id).await.contains("Older agent"));
    beat(&app, &token, "0.9.2", "boot-a", Some(su(true, ""))).await;
    assert!(
        server_page(&mut web, id)
            .await
            .contains("older than 0.10.0")
    );
    beat(&app, &token, "0.10.1", "boot-a", Some(su(true, ""))).await;
    let page = server_page(&mut web, id).await;
    assert!(
        page.contains("Running 0.10.1") && page.contains("Up to date"),
        "{page}"
    );
}

#[tokio::test]
async fn the_card_names_a_newer_wren() {
    let (app, id, _) = setup().await;
    *app.wren.agent_release.write().unwrap() = Some(Release {
        version: "0.10.1".into(),
        digest: D.into(),
        newest_wren: Some("0.11.0".into()),
    });
    let mut web = Web::new(&app, "ad", "admin").await;
    let page = server_page(&mut web, id).await;
    assert!(page.contains("Wren 0.11.0 is out"), "{page}");
}

#[tokio::test]
async fn posting_the_form_queues_the_run_and_the_run_page_names_it() {
    let (app, id, token) = setup().await;
    let mut web = Web::new(&app, "ad", "admin").await;
    let res = web
        .post(
            &format!("/servers/{id}/agent-update"),
            &[("version", "0.10.1"), ("when", "now")],
        )
        .await;
    let to = res.header("location").to_string();
    assert!(to.starts_with("/runs/"), "{to}");
    let run: i64 = to.trim_start_matches("/runs/").parse().unwrap();
    poll(&app, &token, "boot-a").await;
    step(&app, &token, run, "update", "running").await;
    let page = web.get(&to).await.body;
    assert!(
        page.contains("Agent update") && page.contains("Helper started; the agent restarts"),
        "{page}"
    );
    assert!(
        web.get("/servers")
            .await
            .body
            .contains("Agent update running")
    );
    let card = server_page(&mut web, id).await;
    assert!(
        card.contains(&format!("href=\"/runs/{run}\"")) && card.contains("Last agent update"),
        "{card}"
    );
}

#[tokio::test]
async fn the_form_schedules_and_refuses_in_words() {
    let (app, id, _) = setup().await;
    let mut web = Web::new(&app, "ad", "admin").await;
    let url = format!("/servers/{id}/agent-update");
    let back = format!("/servers/{id}");
    let queued = || app.count("SELECT COUNT(*) FROM runs WHERE kind = 'agent_update'");
    // No time, an unknown choice, an old version, the past, too far ahead.
    let (bad_time, changed) = (
        "Pick a time in the future",
        "A newer version appeared since you looked",
    );
    for (form, said) in [
        (
            vec![("version", "0.10.1"), ("when", "at"), ("at", "")],
            bad_time,
        ),
        (vec![("version", "0.10.1"), ("when", "later")], bad_time),
        (vec![("version", "0.10.0"), ("when", "now")], changed),
        (
            vec![
                ("version", "0.10.1"),
                ("when", "at"),
                ("at", "2020-01-01T03:00"),
            ],
            bad_time,
        ),
        (
            vec![
                ("version", "0.10.1"),
                ("when", "at"),
                ("at", "2999-01-01T03:00"),
            ],
            bad_time,
        ),
    ] {
        let res = web.post(&url, &form).await;
        assert_eq!(res.header("location"), back, "{form:?}");
        let page = web.get(&back).await.body;
        assert!(page.contains(said), "{form:?}: {page}");
    }
    assert_eq!(queued().await, 0);
    let page = server_page(&mut web, id).await;
    assert!(page.contains("Update the agent to 0.10.1"), "{page}");
    // A set time that is fine.
    let soon = (jiff::Timestamp::now() + std::time::Duration::from_secs(2 * 3600)).to_string();
    let res = web
        .post(
            &url,
            &[("version", "0.10.1"), ("when", "at"), ("at", &soon)],
        )
        .await;
    assert!(res.header("location").starts_with("/runs/"), "{soon}");
    assert_eq!(
        app.count(
            "SELECT COUNT(*) FROM runs WHERE kind = 'agent_update' AND scheduled_for IS NOT NULL"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn the_updates_page_points_the_agent_to_its_server() {
    let (app, id, token) = setup().await;
    send_inventory_with(&app, &token, "wren-agent", AGENT_IMAGE).await;
    set_remote(&app, AGENT_IMAGE, D).await;
    let mut web = Web::new(&app, "ad", "admin").await;
    let page = web.get("/updates").await.body;
    assert!(
        page.contains(&format!("href=\"/servers/{id}#agent-update\""))
            && page.contains("Update it from its server page"),
        "{page}"
    );
    let tid = app
        .count("SELECT id FROM targets WHERE name = 'wren-agent'")
        .await;
    let page = web.get(&format!("/targets/{tid}")).await.body;
    assert!(
        page.contains(&format!("href=\"/servers/{id}#agent-update\"")),
        "{page}"
    );
    // An agent that can't update itself has nothing to point to.
    beat(&app, &token, "0.9.2", "boot-a", None).await;
    let page = web.get("/updates").await.body;
    assert!(!page.contains("Update it from its server page"), "{page}");
}

#[tokio::test]
async fn the_servers_list_shows_the_next_version() {
    let (app, _, _) = setup().await;
    let mut web = Web::new(&app, "ed", "editor").await;
    let page = web.get("/servers").await.body;
    assert!(
        page.contains("0.10.0 → 0.10.1") && page.contains("Agent update"),
        "{page}"
    );
}

#[tokio::test]
async fn a_never_connected_server_has_no_false_agent_card() {
    let (app, _, _) = setup().await;
    let (new_id, _) = app.add_server("Fresh").await;
    let mut web = Web::new(&app, "ad", "admin").await;
    let page = server_page(&mut web, new_id).await;
    assert!(
        !page.contains("id=\"agent-update\"") && !page.contains("look up its own agent release"),
        "{page}"
    );
    // Wren's own missing release is still said, for an agent that reported.
    let (id, token) = app.add_server("Known").await;
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, ""))).await;
    *app.wren.agent_release.write().unwrap() = None;
    let page = server_page(&mut web, id).await;
    assert!(page.contains("look up its own agent release"), "{page}");
}

#[tokio::test]
async fn a_report_of_off_says_the_self_update_is_off() {
    let (app, id, token) = setup().await;
    let mut web = Web::new(&app, "ad", "admin").await;
    beat(&app, &token, "0.10.0", "boot-a", Some(su(true, "off"))).await;
    let page = server_page(&mut web, id).await;
    assert!(page.contains("WW_AGENT_SELF_UPDATE=1"), "{page}");
}

#[tokio::test]
async fn an_agent_update_run_page_goes_back_to_its_server() {
    let (app, id, _) = setup().await;
    let mut web = Web::new(&app, "ad", "admin").await;
    let res = web
        .post(
            &format!("/servers/{id}/agent-update"),
            &[("version", "0.10.1"), ("when", "now")],
        )
        .await;
    let page = web.get(res.header("location")).await.body;
    let link = format!("href=\"/servers/{id}#agent-update\"");
    let at = page.find(&link).unwrap_or_else(|| panic!("{page}"));
    assert!(page[at..].contains("Tower"), "the server's name: {page}");
}
