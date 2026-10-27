//! 0.7.0: a target's settings survive an inventory that misses it (30
//! days), a target never seen before on an `auto` server waits for an
//! admin, and an agent without an `[updates]` allow list shows as blocked.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{App, Web};
use serde_json::{Value, json};
use wise_wren::policy::Blocked;
use wise_wren::{runs, targets};

async fn send_inventory(app: &App, token: &str, names: &[&str], extra: Value) -> common::Res {
    let containers: Vec<Value> = names
        .iter()
        .map(|n| {
            json!({
                "name": n, "image": format!("ghcr.io/x/{n}:latest"), "image_id": "sha256:1",
                "repo_digests": [], "state": "running", "labels": {},
            })
        })
        .collect();
    let mut body = json!({ "containers": containers });
    if let Value::Object(map) = extra {
        for (k, v) in map {
            body[k] = v;
        }
    }
    let req = Request::post("/api/agent/v1/inventory")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res
}

async fn target(app: &App, name: &'static str) -> Option<targets::Target> {
    app.wren
        .core
        .with_conn(move |c| {
            Ok(targets::list(c, targets::Filter::default())?
                .into_iter()
                .find(|t| t.name == name))
        })
        .await
        .unwrap()
}

/// `(mode, health_seconds, require_vv, require_snapshot, auto_pending)`.
async fn settings(
    app: &App,
    name: &'static str,
) -> (Option<String>, Option<i64>, bool, bool, bool) {
    app.wren
        .core
        .with_conn(move |c| {
            c.query_row(
                "SELECT mode, health_seconds, require_vv, require_snapshot, auto_pending
                 FROM targets WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        })
        .await
        .unwrap()
}

async fn set_auto_server(app: &App, server_id: i64) {
    app.exec(format!(
        "UPDATE servers SET mode = 'auto', auto_start = '03:00', auto_end = '05:00' WHERE id = {server_id}"
    ))
    .await;
}

#[tokio::test]
async fn settings_and_monitor_links_return_with_the_target() {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    send_inventory(&app, &token, &["db", "web"], json!({})).await;
    let db = target(&app, "db").await.unwrap();
    app.wren
        .core
        .with_conn(move |c| {
            let s = runs::TargetSettings {
                mode: Some("notify".into()),
                health_seconds: Some(600),
                require_vv: true,
                require_snapshot: true,
            };
            runs::set_target_policy(c, db.id, &s, "admin", true)?.unwrap();
            c.execute(
                "INSERT INTO target_monitors (target_id, kind, ref, label) VALUES (?1, 'tag', 'db', 'db')",
                [db.id],
            )?;
            c.execute("UPDATE targets SET skipped_key = 'k' WHERE id = ?1", [db.id])
        })
        .await
        .unwrap();

    // One inventory misses it: the target goes, its settings wait.
    send_inventory(&app, &token, &["web"], json!({})).await;
    assert!(target(&app, "db").await.is_none());
    assert_eq!(app.count("SELECT COUNT(*) FROM target_memory").await, 1);
    assert_eq!(app.count("SELECT COUNT(*) FROM target_monitors").await, 0);

    // It comes back: mode, health period, requirements, skip and monitor
    // links with it, and the memory is used up.
    send_inventory(&app, &token, &["db", "web"], json!({})).await;
    assert_eq!(
        settings(&app, "db").await,
        (Some("notify".into()), Some(600), true, true, false)
    );
    assert_eq!(
        app.count("SELECT COUNT(*) FROM targets WHERE name = 'db' AND skipped_key = 'k'")
            .await,
        1
    );
    assert_eq!(
        app.count("SELECT COUNT(*) FROM target_monitors WHERE kind = 'tag' AND ref = 'db'")
            .await,
        1
    );
    assert_eq!(app.count("SELECT COUNT(*) FROM target_memory").await, 0);
    // The other target never had settings and still has none.
    assert_eq!(
        settings(&app, "web").await,
        (None, None, false, false, false)
    );
}

#[tokio::test]
async fn settings_are_kept_for_thirty_days_and_then_purged() {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    send_inventory(&app, &token, &["db", "old"], json!({})).await;
    for name in ["db", "old"] {
        app.exec(format!(
            "UPDATE targets SET mode = 'notify' WHERE name = '{name}'"
        ))
        .await;
    }
    send_inventory(&app, &token, &["keep"], json!({})).await;
    assert_eq!(app.count("SELECT COUNT(*) FROM target_memory").await, 2);

    // 29 days: still there. 31 days: purged by the next inventory.
    app.exec(
        "UPDATE target_memory SET gone_at = datetime('now', '-29 days') WHERE name = 'db'".into(),
    )
    .await;
    app.exec(
        "UPDATE target_memory SET gone_at = datetime('now', '-31 days') WHERE name = 'old'".into(),
    )
    .await;
    send_inventory(&app, &token, &["db", "old", "keep"], json!({})).await;
    assert_eq!(settings(&app, "db").await.0.as_deref(), Some("notify"));
    assert_eq!(
        settings(&app, "old").await.0,
        None,
        "forgotten after 30 days"
    );
    assert_eq!(app.count("SELECT COUNT(*) FROM target_memory").await, 0);
}

#[tokio::test]
async fn a_failed_docker_read_forgets_nothing() {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    send_inventory(&app, &token, &["db"], json!({})).await;
    send_inventory(&app, &token, &[], json!({"error": "docker is down"})).await;
    assert!(target(&app, "db").await.is_some());
    assert_eq!(app.count("SELECT COUNT(*) FROM target_memory").await, 0);
}

#[tokio::test]
async fn new_targets_on_an_auto_server_wait_for_an_admin() {
    let app = App::new().await;
    let (server_id, token) = app.add_server("Tower").await;
    send_inventory(&app, &token, &["web"], json!({})).await;
    // An approve server: nothing special about new targets.
    assert_eq!(
        settings(&app, "web").await,
        (None, None, false, false, false)
    );

    set_auto_server(&app, server_id).await;
    send_inventory(&app, &token, &["web", "db", "cache"], json!({})).await;
    // Known before: follows the server, as it did. New ones: approve first.
    assert_eq!(settings(&app, "web").await.0, None);
    for name in ["db", "cache"] {
        let (mode, _, _, _, pending) = settings(&app, name).await;
        assert_eq!(
            (mode.as_deref(), pending),
            (Some("approve"), true),
            "{name}"
        );
    }
    let db = target(&app, "db").await.unwrap();
    assert!(db.auto_pending);
    let policy = app
        .wren
        .core
        .with_conn(move |c| runs::policy(c, db.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(policy.effective(), "approve");
    assert!(!policy.updates_itself());

    // The pages say so; only the admin gets the buttons.
    let mut editor = Web::new(&app, "ed", "editor").await;
    let page = editor.get(&format!("/servers/{server_id}")).await.body;
    assert!(page.contains("data-awaiting-auto=\"1\""), "{page}");
    assert!(page.contains("New: approve first"), "{page}");
    assert!(!page.contains("/allow-auto"), "{page}");
    let denied = editor
        .post(&format!("/targets/{}/allow-auto", db.id), &[])
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let denied = editor
        .post(&format!("/servers/{server_id}/allow-auto"), &[])
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(settings(&app, "db").await.4);

    let mut admin = Web::new(&app, "root", "admin").await;
    let page = admin.get(&format!("/servers/{server_id}")).await.body;
    assert!(
        page.contains(&format!("/servers/{server_id}/allow-auto")),
        "{page}"
    );
    let page = admin.get(&format!("/targets/{}", db.id)).await.body;
    assert!(
        page.contains(&format!("/targets/{}/allow-auto", db.id)),
        "{page}"
    );

    // One target: it follows the server from now on.
    admin
        .post(&format!("/targets/{}/allow-auto", db.id), &[])
        .await;
    assert_eq!(
        settings(&app, "db").await,
        (None, None, false, false, false)
    );
    let policy = app
        .wren
        .core
        .with_conn(move |c| runs::policy(c, db.id))
        .await
        .unwrap()
        .unwrap();
    assert!(policy.updates_itself());
    assert!(settings(&app, "cache").await.4, "the other one still waits");

    // All new targets of the server.
    admin
        .post(&format!("/servers/{server_id}/allow-auto"), &[])
        .await;
    assert_eq!(
        settings(&app, "cache").await,
        (None, None, false, false, false)
    );
    let page = admin.get(&format!("/servers/{server_id}")).await.body;
    assert!(!page.contains("data-awaiting-auto"), "{page}");
    assert_eq!(
        app.count("SELECT COUNT(*) FROM actions WHERE action = 'allow_auto'")
            .await,
        2
    );
}

#[tokio::test]
async fn a_returning_target_does_not_start_over_as_new() {
    let app = App::new().await;
    let (server_id, token) = app.add_server("Tower").await;
    set_auto_server(&app, server_id).await;
    send_inventory(&app, &token, &["db"], json!({})).await;
    let db = target(&app, "db").await.unwrap();
    app.wren
        .core
        .with_conn(move |c| runs::allow_auto(c, db.id, "root", true))
        .await
        .unwrap()
        .unwrap();
    // It drops out and returns: still confirmed, not "approve" again.
    send_inventory(&app, &token, &[], json!({})).await;
    send_inventory(&app, &token, &["db"], json!({})).await;
    assert_eq!(
        settings(&app, "db").await,
        (None, None, false, false, false)
    );
    // And a target that was still waiting keeps waiting.
    send_inventory(&app, &token, &["web"], json!({})).await;
    send_inventory(&app, &token, &[], json!({})).await;
    send_inventory(&app, &token, &["web"], json!({})).await;
    let (mode, _, _, _, pending) = settings(&app, "web").await;
    assert_eq!((mode.as_deref(), pending), (Some("approve"), true));
}

#[tokio::test]
async fn saving_a_pending_targets_policy_ends_the_wait_without_turning_auto_on() {
    let app = App::new().await;
    let (server_id, token) = app.add_server("Tower").await;
    set_auto_server(&app, server_id).await;
    send_inventory(&app, &token, &["db"], json!({})).await;
    let mut editor = Web::new(&app, "ed", "editor").await;
    let id = target(&app, "db").await.unwrap().id;
    editor
        .post(&format!("/targets/{id}/policy"), &[("mode", "approve")])
        .await;
    assert_eq!(settings(&app, "db").await.0.as_deref(), Some("approve"));
    assert!(!settings(&app, "db").await.4);
    // Asking for the server's mode (auto) is still an admin's call.
    let res = editor
        .post(&format!("/targets/{id}/policy"), &[("mode", "")])
        .await;
    assert!(res.status.is_redirection() || res.status.is_success());
    assert_eq!(settings(&app, "db").await.0.as_deref(), Some("approve"));
}

#[tokio::test]
async fn an_agent_without_an_updates_list_shows_every_target_as_blocked() {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    let policy = json!({"allow": [], "deny": [], "roots": ["/mnt/user/appdata"]});
    send_inventory(&app, &token, &["web"], json!({ "policy": policy })).await;
    let web = target(&app, "web").await.unwrap();
    assert_eq!(web.blocked, Some(Blocked::NoUpdates));
    assert_eq!(
        web.blocked.as_ref().unwrap().english(),
        "agent.toml has no [updates] allow list"
    );
    let mut viewer = Web::new(&app, "vi", "viewer").await;
    let page = viewer.get(&format!("/targets/{}", web.id)).await.body;
    assert!(
        page.contains("agent.toml has no [updates] allow list"),
        "{page}"
    );

    // An older agent reports nothing (or no allow key): nothing is blocked.
    let policy = json!({"allow": null, "deny": [], "roots": ["/mnt/user/appdata"]});
    send_inventory(&app, &token, &["web"], json!({ "policy": policy })).await;
    assert_eq!(target(&app, "web").await.unwrap().blocked, None);
    // A real list that leaves the target out is the old reason.
    let policy = json!({"allow": ["other"], "deny": [], "roots": []});
    send_inventory(&app, &token, &["web"], json!({ "policy": policy })).await;
    assert_eq!(
        target(&app, "web").await.unwrap().blocked,
        Some(Blocked::NotAllowed)
    );
}

#[tokio::test]
async fn the_agents_coded_refusals_show_in_the_readers_language() {
    use std::collections::BTreeMap;
    wise_wren::install();
    let params: BTreeMap<String, String> = [
        ("target".to_string(), "db".to_string()),
        ("run".to_string(), "7".to_string()),
        ("newest".to_string(), "9".to_string()),
        ("minutes".to_string(), "12".to_string()),
        ("error".to_string(), "disk full".to_string()),
    ]
    .into();
    for code in [
        "refused.updates_required",
        "refused.run_old",
        "refused.rollback_unknown",
        "refused.rollback_used",
        "refused.too_soon",
        "refused.ledger_write",
    ] {
        let text = wise_wren::texts::coded(code, &params).unwrap_or_else(|| panic!("{code}"));
        assert!(text.starts_with("Refused by the agent"), "{code}: {text}");
        assert!(!text.contains('{'), "{code}: {text}");
    }
    assert!(wise_wren::texts::is_config_refusal(
        "refused.updates_required",
        ""
    ));
    assert!(!wise_wren::texts::is_config_refusal("refused.run_old", ""));
}
