//! The agent's self-update on the server: the heartbeat report, its storage
//! and what an admin may do about it (design: Agent self-update).

mod common;

use axum::http::StatusCode;
use common::{App, post_json};
use serde_json::{Value, json};
use wise_wren::agent_update::{self, Release};

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
