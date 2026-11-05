//! The agent's self-update on the server: the heartbeat report, its storage
//! and what an admin may do about it (design: Agent self-update).

mod common;

use axum::http::StatusCode;
use common::{App, add_user, api_token, poll_as, post_json};
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
    let app = App::new().await;
    let (id, token) = app.add_server("Tower").await;
    *app.wren.agent_release.write().unwrap() = Some(Release {
        version: "0.10.1".into(),
        digest: D.into(),
        newest_wren: None,
    });
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

/// An inventory with one container.
async fn send_inventory_with(app: &App, token: &str, name: &str, image: &str) {
    let inv = json!({"containers": [{
        "name": name, "image": image, "image_id": "sha256:1",
        "repo_digests": [], "state": "running", "labels": {},
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
