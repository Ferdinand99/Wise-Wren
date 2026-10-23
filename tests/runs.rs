//! Update runs on the server: approvals, the job channel, step reports,
//! lapses, cancels, notifications, pages and the API (design §5, §6).

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{App, Web, api_token, bearer};
use serde_json::{Value, json};
use vole_kit::auth::TokenScope;
use wise_wren::{runs, targets};

const OLD: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const NEWER: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn container(name: &str, image: &str, labels: Value) -> Value {
    let repo = image.rsplit_once(':').map_or(image, |(r, _)| r);
    json!({
        "name": name, "image": image, "image_id": "sha256:1",
        "repo_digests": [format!("{repo}@{OLD}")], "state": "running", "labels": labels,
    })
}

async fn post_json(app: &App, path: &str, token: &str, body: Value) -> common::Res {
    let req = Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.send(req).await
}

/// LANBox with two plain containers and a Compose stack whose files the
/// agent can't reach, each with a new digest in the registry.
async fn setup() -> (App, i64, String) {
    let app = App::new().await;
    let (id, token) = app.add_server("LANBox").await;
    send_inventory(&app, &token, &[OLD]).await;
    set_remote(&app, "localhost:5056/ww-test/app:latest", NEW).await;
    set_remote(&app, "localhost:5056/ww-test/other:latest", NEW).await;
    set_remote(&app, "localhost:5056/ww-test/db:1", NEW).await;
    (app, id, token)
}

/// The inventory of [`setup`], with these repo digests for ww-test-a.
async fn send_inventory(app: &App, token: &str, a_digests: &[&str]) {
    let mut a = container("ww-test-a", "localhost:5056/ww-test/app:latest", json!({}));
    a["repo_digests"] = a_digests
        .iter()
        .map(|d| json!(format!("localhost:5056/ww-test/app@{d}")))
        .collect();
    let inv = json!({
        "containers": [
            a,
            container("ww-test-b", "localhost:5056/ww-test/other:latest", json!({})),
            container("pz-db", "localhost:5056/ww-test/db:1", json!({"com.docker.compose.project": "postiz"})),
        ],
        "compose_projects": [{"name": "postiz", "working_dir": "/boot/x/postiz", "files_reachable": false}],
    });
    let res = post_json(app, "/api/agent/v1/inventory", token, inv).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

async fn set_remote(app: &App, image: &'static str, digest: &'static str) {
    app.wren
        .core
        .with_conn(move |c| {
            targets::set_remote(
                c,
                image,
                &Ok(wise_wren::registry::Remote {
                    digest: digest.into(),
                    media_type: String::new(),
                }),
            )?;
            targets::refresh_all(c)
        })
        .await
        .unwrap();
}

async fn target(app: &App, name: &'static str) -> targets::Target {
    app.wren
        .core
        .with_conn(move |c| {
            Ok(targets::list(c, targets::Filter::default())?
                .into_iter()
                .find(|t| t.name == name))
        })
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no target {name}"))
}

async fn run(app: &App, id: i64) -> runs::Run {
    app.wren
        .core
        .with_conn(move |c| runs::get(c, id))
        .await
        .unwrap()
        .unwrap()
}

/// The agent's long poll, without waiting.
async fn poll(app: &App, token: &str) -> common::Res {
    bearer(app, "GET", "/api/agent/v1/jobs?wait=0", token).await
}

async fn step(app: &App, token: &str, run: i64, step: &str, status: &str) -> common::Res {
    post_json(
        app,
        &format!("/api/agent/v1/runs/{run}/steps"),
        token,
        json!({"step": step, "status": status, "log": "…", "duration_ms": 120}),
    )
    .await
}

async fn finish(app: &App, token: &str, run: i64, outcome: &str) -> common::Res {
    post_json(
        app,
        &format!("/api/agent/v1/runs/{run}/finish"),
        token,
        json!({"outcome": outcome, "detail": "x"}),
    )
    .await
}

async fn approve(web: &mut Web<'_>, id: i64, key: &str) -> common::Res {
    web.post(&format!("/targets/{id}/approve"), &[("key", key)])
        .await
}

fn run_id_of(job: &Value) -> i64 {
    job["job"]["run_id"].as_i64().unwrap()
}

#[tokio::test]
async fn approval_runs_once_for_that_version() {
    let (app, server_id, token) = setup().await;
    let t = target(&app, "ww-test-a").await;
    assert_eq!(t.status, "update");
    // Nothing to do before anyone approves.
    assert_eq!(poll(&app, &token).await.status, StatusCode::NO_CONTENT);
    let mut web = Web::new(&app, "ed", "editor").await;
    let res = approve(&mut web, t.id, &t.available_key).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);

    let res = poll(&app, &token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let job = res.json();
    let id = run_id_of(&job);
    // Data only: target, keys, images with both digests; no command, no path.
    assert_eq!(job["job"]["kind"], "update");
    assert_eq!(
        job["job"]["target"],
        json!({"kind": "container", "name": "ww-test-a"})
    );
    assert_eq!(
        job["job"]["images"],
        json!([{"container": "ww-test-a", "image": "localhost:5056/ww-test/app:latest", "from": OLD, "to": NEW}])
    );
    assert_eq!(job["job"]["to_key"], t.available_key);
    for never in ["command", "script", "path", "cmd"] {
        assert!(!res.body.contains(never), "{never} in {}", res.body);
    }
    assert_eq!(run(&app, id).await.status, "running");
    // One job: the poll is empty while it runs.
    assert_eq!(poll(&app, &token).await.status, StatusCode::NO_CONTENT);

    for (s, st) in [
        ("preflight", "running"),
        ("preflight", "ok"),
        ("stop", "ok"),
        ("snapshot", "ok"),
        ("update", "ok"),
        ("health", "ok"),
    ] {
        let res = step(&app, &token, id, s, st).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(res.json()["cancel"], false);
    }
    assert_eq!(
        step(&app, &token, id, "shell", "ok").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        finish(&app, &token, id, "succeeded").await.status,
        StatusCode::OK
    );
    let r = run(&app, id).await;
    assert_eq!(
        (r.status.as_str(), r.requested_by.as_str()),
        ("succeeded", "ed")
    );
    // A fresh inventory and check are asked for; nothing else is queued.
    assert_eq!(
        app.count("SELECT COUNT(*) FROM servers WHERE scan_requested_at IS NOT NULL")
            .await,
        1
    );
    assert_eq!(poll(&app, &token).await.status, StatusCode::NO_CONTENT);
    // A finished run takes no more reports.
    assert_eq!(
        finish(&app, &token, id, "failed").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.count("SELECT COUNT(*) FROM actions WHERE action = 'approve' AND username = 'ed'")
            .await,
        1
    );
    let _ = server_id;
}

#[tokio::test]
async fn newer_version_lapses_the_approval() {
    let (app, _, token) = setup().await;
    let t = target(&app, "ww-test-a").await;
    let mut web = Web::new(&app, "ed", "editor").await;
    approve(&mut web, t.id, &t.available_key).await;
    // A newer digest before the agent picked it up.
    set_remote(&app, "localhost:5056/ww-test/app:latest", NEWER).await;
    assert_eq!(poll(&app, &token).await.status, StatusCode::NO_CONTENT);
    let r = run(&app, 1).await;
    assert_eq!(r.status, "lapsed");
    assert!(r.detail.contains("newer version"), "{}", r.detail);
    // Approving what the person saw before is refused as well.
    approve(&mut web, t.id, &t.available_key).await;
    assert_eq!(app.count("SELECT COUNT(*) FROM runs").await, 1);
}

#[tokio::test]
async fn notify_targets_and_viewers_cannot_approve() {
    let (app, _, token) = setup().await;
    let t = target(&app, "ww-test-a").await;
    let mut viewer = Web::new(&app, "vi", "viewer").await;
    assert_eq!(
        approve(&mut viewer, t.id, &t.available_key).await.status,
        StatusCode::FORBIDDEN
    );
    let mut web = Web::new(&app, "ed", "editor").await;
    // Notify only (the target's mode).
    web.post(
        &format!("/targets/{}/policy", t.id),
        &[("mode", "notify"), ("health_seconds", "")],
    )
    .await;
    approve(&mut web, t.id, &t.available_key).await;
    // The server's mode.
    let b = target(&app, "ww-test-b").await;
    web.post(
        &format!("/servers/{}/mode", b.server_id),
        &[("mode", "notify")],
    )
    .await;
    approve(&mut web, b.id, &b.available_key).await;
    // Files the agent can't reach.
    web.post(
        &format!("/servers/{}/mode", b.server_id),
        &[("mode", "approve")],
    )
    .await;
    let pz = target(&app, "postiz").await;
    approve(&mut web, pz.id, &pz.available_key).await;
    assert_eq!(app.count("SELECT COUNT(*) FROM runs").await, 0);
    assert_eq!(poll(&app, &token).await.status, StatusCode::NO_CONTENT);
    // No "auto" mode yet.
    web.post(&format!("/targets/{}/policy", t.id), &[("mode", "auto")])
        .await;
    assert_eq!(
        app.count("SELECT COUNT(*) FROM targets WHERE mode = 'auto'")
            .await,
        0
    );
}

#[tokio::test]
async fn one_run_per_server_and_bulk_approval() {
    let (app, _, token) = setup().await;
    let (a, b) = (
        target(&app, "ww-test-a").await,
        target(&app, "ww-test-b").await,
    );
    let mut web = Web::new(&app, "ed", "editor").await;
    let (ida, idb) = (a.id.to_string(), b.id.to_string());
    let (ka, kb) = (format!("key_{ida}"), format!("key_{idb}"));
    let res = web
        .post(
            "/updates/approve",
            &[
                ("ids", &ida),
                (&ka, &a.available_key),
                ("ids", &idb),
                (&kb, &b.available_key),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM runs WHERE status = 'queued'")
            .await,
        2
    );
    let first = run_id_of(&poll(&app, &token).await.json());
    assert_eq!(poll(&app, &token).await.status, StatusCode::NO_CONTENT);
    finish(&app, &token, first, "rolled_back").await;
    let second = run_id_of(&poll(&app, &token).await.json());
    assert_ne!(first, second);
}

#[tokio::test]
async fn cancel_before_stop() {
    let (app, _, token) = setup().await;
    let (a, b) = (
        target(&app, "ww-test-a").await,
        target(&app, "ww-test-b").await,
    );
    let mut web = Web::new(&app, "ed", "editor").await;
    approve(&mut web, a.id, &a.available_key).await;
    approve(&mut web, b.id, &b.available_key).await;
    // Queued: cancelled at once.
    web.post("/runs/2/cancel", &[]).await;
    assert_eq!(run(&app, 2).await.status, "cancelled");
    // Running, preflight under way: the answer to its result says cancel.
    let id = run_id_of(&poll(&app, &token).await.json());
    step(&app, &token, id, "preflight", "running").await;
    web.post(&format!("/runs/{id}/cancel"), &[]).await;
    assert_eq!(
        step(&app, &token, id, "preflight", "ok").await.json()["cancel"],
        true
    );
    finish(&app, &token, id, "cancelled").await;
    assert_eq!(run(&app, id).await.status, "cancelled");
    // After the preflight it is too late.
    approve(&mut web, a.id, &a.available_key).await;
    let id = run_id_of(&poll(&app, &token).await.json());
    step(&app, &token, id, "preflight", "ok").await;
    web.post(&format!("/runs/{id}/cancel"), &[]).await;
    assert!(!run(&app, id).await.cancel_requested);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM actions WHERE action = 'cancel'")
            .await,
        2
    );
}

#[tokio::test]
async fn skip_and_snooze_hide_until_newer() {
    let (app, _, _) = setup().await;
    let (a, b) = (
        target(&app, "ww-test-a").await,
        target(&app, "ww-test-b").await,
    );
    let mut web = Web::new(&app, "ed", "editor").await;
    web.post(&format!("/targets/{}/skip", a.id), &[]).await;
    web.post(&format!("/targets/{}/snooze", b.id), &[("hours", "24")])
        .await;
    let page = web.get("/updates").await.body;
    let held = &page[page.find("id=\"held\"").expect("a held section")..];
    assert!(
        held.contains("ww-test-a") && held.contains("ww-test-b"),
        "{held}"
    );
    // A newer version of a: no longer skipped.
    set_remote(&app, "localhost:5056/ww-test/app:latest", NEWER).await;
    let page = web.get("/updates").await.body;
    let held = &page[page.find("id=\"held\"").unwrap()..];
    assert!(
        !held.contains("ww-test-a") && held.contains("ww-test-b"),
        "{held}"
    );
    web.post(&format!("/targets/{}/resume", b.id), &[]).await;
    assert!(!web.get("/updates").await.body.contains("id=\"held\""));
}

#[tokio::test]
async fn job_for_another_servers_run_is_refused() {
    let (app, _, token) = setup().await;
    let (_, other) = app.add_server("Tower").await;
    let a = target(&app, "ww-test-a").await;
    let mut web = Web::new(&app, "ed", "editor").await;
    approve(&mut web, a.id, &a.available_key).await;
    // Tower's agent gets nothing of LANBox.
    assert_eq!(poll(&app, &other).await.status, StatusCode::NO_CONTENT);
    let id = run_id_of(&poll(&app, &token).await.json());
    assert_eq!(
        step(&app, &other, id, "preflight", "ok").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        finish(&app, &other, id, "succeeded").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        poll(&app, "wwa_nope").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(run(&app, id).await.status, "running");
}

#[tokio::test]
async fn silent_agent_fails_the_run() {
    let (app, id, token) = setup().await;
    let a = target(&app, "ww-test-a").await;
    let mut web = Web::new(&app, "ed", "editor").await;
    approve(&mut web, a.id, &a.available_key).await;
    let run_id = run_id_of(&poll(&app, &token).await.json());
    // Online agent: a long step is fine.
    app.exec(format!("UPDATE runs SET last_report_at = datetime('now', '-30 minutes'); UPDATE servers SET last_seen_at = datetime('now') WHERE id = {id}")).await;
    let closed = app
        .wren
        .core
        .with_conn(|c| runs::close_silent(c))
        .await
        .unwrap();
    assert!(closed.is_empty());
    // Offline and silent: needs a person; never retried.
    app.exec(format!(
        "UPDATE servers SET last_seen_at = datetime('now', '-1 hour') WHERE id = {id}"
    ))
    .await;
    let closed = app
        .wren
        .core
        .with_conn(|c| runs::close_silent(c))
        .await
        .unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(run(&app, run_id).await.status, "rollback_failed");
    assert_eq!(poll(&app, &token).await.status, StatusCode::NO_CONTENT);
}

/// A webhook receiver: what it got.
async fn hooks() -> (String, Arc<Mutex<Vec<Value>>>) {
    let got: Arc<Mutex<Vec<Value>>> = Arc::default();
    let g = got.clone();
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let g = g.clone();
        async move {
            g.lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            StatusCode::OK
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/hook"), got)
}

async fn add_hook(app: &App, url: String) {
    let config = json!({"url": url, "method": "POST"}).to_string();
    app.wren
        .core
        .with_conn(move |c| {
            c.execute(
                "INSERT INTO notification_channels (name, kind, config) VALUES ('hook', 'webhook', ?1)",
                [config],
            )
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn run_results_notify() {
    let (app, _, _) = setup().await;
    let (url, got) = hooks().await;
    add_hook(&app, url).await;
    let mut r = runs::Run {
        id: 4,
        server_id: 1,
        server_name: "LANBox".into(),
        target_id: Some(1),
        target_kind: "container".into(),
        target_name: "ww-test-a".into(),
        kind: "update".into(),
        rollback_of: None,
        from_key: String::new(),
        to_key: String::new(),
        shown: "bbbbbbbbbbbb".into(),
        status: "succeeded".into(),
        detail: String::new(),
        requested_by: "ed".into(),
        created_at: String::new(),
        started_at: None,
        finished_at: None,
        cancel_requested: false,
        trigger: "user".into(),
        scheduled_for: None,
        vv_window_id: None,
        vv_state: String::new(),
    };
    for status in ["succeeded", "rolled_back", "rollback_failed", "cancelled"] {
        r.status = status.into();
        wise_wren::run_notify::send_result(&app.wren, &r).await;
    }
    let hits = got.clone();
    let got = got.lock().unwrap().clone();
    assert_eq!(got.len(), 3, "a cancelled run says nothing");
    let text: Vec<String> = got.iter().map(Value::to_string).collect();
    assert!(
        text[0].contains("ww-test-a on LANBox is updated"),
        "{}",
        text[0]
    );
    assert!(
        text[1].contains("failed and was rolled back"),
        "{}",
        text[1]
    );
    assert!(text[2].contains("needs you"), "{}", text[2]);
    assert_eq!(got[2]["event"], "run_rollback_failed");
    // Run results name what started them.
    assert!(text[0].contains("Approved now"), "{}", text[0]);
    r.status = "succeeded".into();
    r.trigger = "auto".into();
    wise_wren::run_notify::send_result(&app.wren, &r).await;
    let last = hits.lock().unwrap().last().unwrap().to_string();
    assert!(last.contains("Automatic (auto window)"), "{last}");
    assert!(!last.contains("Approved by"), "{last}");
}

#[tokio::test]
async fn agent_offline_once_and_back() {
    let (app, id, _) = setup().await;
    let (url, got) = hooks().await;
    add_hook(&app, url).await;
    app.exec(format!(
        "UPDATE servers SET last_seen_at = datetime('now', '-10 minutes') WHERE id = {id}"
    ))
    .await;
    wise_wren::run_notify::check_agents(&app.wren).await;
    wise_wren::run_notify::check_agents(&app.wren).await;
    assert_eq!(got.lock().unwrap().len(), 1, "once per outage");
    app.exec(format!(
        "UPDATE servers SET last_seen_at = datetime('now') WHERE id = {id}"
    ))
    .await;
    wise_wren::run_notify::check_agents(&app.wren).await;
    wise_wren::run_notify::check_agents(&app.wren).await;
    let got = got.lock().unwrap().clone();
    assert_eq!(got.len(), 2);
    assert!(got[0].to_string().contains("offline") && got[1].to_string().contains("back online"));
}

#[tokio::test]
async fn pages_show_runs_and_actions() {
    let (app, _, token) = setup().await;
    let a = target(&app, "ww-test-a").await;
    let mut web = Web::new(&app, "ed", "editor").await;
    let updates = web.get("/updates").await.body;
    assert!(
        updates.contains(&format!("formaction=\"/targets/{}/approve\"", a.id)),
        "{updates}"
    );
    assert!(updates.contains("status-update"));
    approve(&mut web, a.id, &a.available_key).await;
    let id = run_id_of(&poll(&app, &token).await.json());
    step(&app, &token, id, "preflight", "ok").await;
    post_json(
        &app,
        &format!("/api/agent/v1/runs/{id}/steps"),
        &token,
        json!({"step": "health", "status": "failed", "log": "ww-test-a is unhealthy", "duration_ms": 9000}),
    )
    .await;
    finish(&app, &token, id, "rolled_back").await;
    let page = web.get(&format!("/runs/{id}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains("ww-test-a is unhealthy") && page.body.contains("Rolled back"),
        "{}",
        page.body
    );
    let target_page = web.get(&format!("/targets/{}", a.id)).await.body;
    assert!(target_page.contains(&format!("/runs/{id}")) && target_page.contains("Run history"));
    let overview = web.get("/").await.body;
    assert!(overview.contains("Recent runs") && overview.contains(&format!("#{id}")));
    // A Compose stack's images read as a count.
    let server = web.get(&format!("/servers/{}", a.server_id)).await.body;
    assert!(server.contains("1 image"), "{server}");
    let css = web.get("/static/wren.css").await.body;
    assert!(css.contains(".status-update"));
    // Viewers see runs but no buttons.
    let mut viewer = Web::new(&app, "vi", "viewer").await;
    let page = viewer.get("/updates").await.body;
    assert!(!page.contains("formaction"));
}

#[tokio::test]
async fn api_and_mcp_runs_and_approve() {
    let (app, _, token) = setup().await;
    let a = target(&app, "ww-test-a").await;
    let editor = common::add_user(&app, "ed", "editor").await;
    let viewer = common::add_user(&app, "vi", "viewer").await;
    let write = api_token(&app, editor, TokenScope::Write).await;
    let read = api_token(&app, viewer, TokenScope::Read).await;
    // Read tokens can't approve; a stale key is refused.
    let path = format!("/api/v1/targets/{}/approve", a.id);
    assert_eq!(
        post_json(&app, &path, &read, json!({})).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post_json(&app, &path, &write, json!({"version_key": "x=sha256:old"}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let res = post_json(&app, &path, &write, json!({"version_key": a.available_key})).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let id = res.json()["run_id"].as_i64().unwrap();
    let res = bearer(&app, "GET", "/api/v1/runs", &read).await;
    assert_eq!(res.json()["runs"][0]["id"], id);
    assert_eq!(res.json()["runs"][0]["requested_by"], "ed");
    assert_eq!(
        bearer(&app, "GET", &format!("/api/v1/runs/{id}"), &read)
            .await
            .json()["status"],
        "queued"
    );
    let res = bearer(&app, "GET", "/api/v1/updates", &read).await;
    let listed = res.json()["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "ww-test-a")
        .cloned()
        .unwrap();
    assert_eq!(listed["version_key"], a.available_key);
    // MCP: approve_update for write tokens only.
    let b = target(&app, "ww-test-b").await;
    let call = |tok: &str, name: &str, args: Value| {
        Request::post("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string(),
            ))
            .unwrap()
    };
    let res = app
        .send(call(&read, "approve_update", json!({"target_id": b.id})))
        .await;
    assert!(res.body.contains("write token"), "{}", res.body);
    let res = app
        .send(call(
            &write,
            "approve_update",
            json!({"target_id": b.id, "version_key": b.available_key}),
        ))
        .await;
    assert!(res.body.contains("queued"), "{}", res.body);
    let res = app.send(call(&read, "list_runs", json!({}))).await;
    assert!(res.body.contains("ww-test-b"), "{}", res.body);
    let _ = token;
}

#[tokio::test]
async fn revoked_token_gets_no_job_from_a_waiting_poll() {
    let (app, server_id, token) = setup().await;
    let a = target(&app, "ww-test-a").await;
    // The agent is already waiting when its token is revoked.
    let router = app.router.clone();
    let t = token.clone();
    let waiting = tokio::spawn(async move {
        use tower::ServiceExt as _;
        let mut req = Request::get("/api/agent/v1/jobs?wait=10")
            .header(header::AUTHORIZATION, format!("Bearer {t}"))
            .header(header::HOST, common::HOST)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40000,
            ))));
        router.oneshot(req).await.unwrap().status()
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let key = a.available_key.clone();
    app.wren
        .core
        .with_conn(move |c| {
            wise_wren::db::set_token(c, server_id, None)?;
            let _ = runs::approve_at(c, a.id, Some(&key), "owner", None)?;
            Ok(())
        })
        .await
        .unwrap();
    app.wren.jobs.notify_waiters();
    assert_eq!(waiting.await.unwrap(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM runs WHERE status = 'running'")
            .await,
        0
    );
}

async fn heartbeat(app: &App, token: &str, beat: Value) {
    let res = post_json(app, "/api/agent/v1/heartbeat", token, beat).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// The job poll of the agent process `boot`, without waiting.
async fn poll_as(app: &App, token: &str, boot: &str) -> common::Res {
    bearer(
        app,
        "GET",
        &format!("/api/agent/v1/jobs?wait=0&boot={boot}"),
        token,
    )
    .await
}

async fn approve_both(app: &App) {
    let (a, b) = (
        target(app, "ww-test-a").await,
        target(app, "ww-test-b").await,
    );
    let mut web = Web::new(app, "ed", "editor").await;
    approve(&mut web, a.id, &a.available_key).await;
    approve(&mut web, b.id, &b.available_key).await;
    assert_eq!(
        app.count("SELECT COUNT(*) FROM runs WHERE status = 'queued'")
            .await,
        2
    );
}

#[tokio::test]
async fn agent_restart_mid_run_closes_it_and_the_next_one_goes() {
    let (app, _, token) = setup().await;
    let (url, got) = hooks().await;
    add_hook(&app, url).await;
    approve_both(&app).await;
    let first = run_id_of(&poll_as(&app, &token, "one").await.json());
    for (s, st) in [
        ("preflight", "ok"),
        ("stop", "ok"),
        ("snapshot", "ok"),
        ("update", "ok"),
        ("health", "running"),
    ] {
        assert_eq!(
            step(&app, &token, first, s, st).await.status,
            StatusCode::OK
        );
    }
    // The same process: its heartbeats and polls change nothing.
    heartbeat(
        &app,
        &token,
        json!({"boot_id": "one", "uptime_seconds": 600}),
    )
    .await;
    assert_eq!(
        poll_as(&app, &token, "one").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(run(&app, first).await.status, "running");

    // agent.toml was edited and the agent restarted in the health step: the
    // new process's first heartbeat closes the run for a person to check.
    heartbeat(&app, &token, json!({"boot_id": "two", "uptime_seconds": 2})).await;
    let r = run(&app, first).await;
    assert_eq!(r.status, "rollback_failed");
    assert!(
        r.detail.starts_with(
            "The agent restarted during the run; check ww-test-a: it may be on the new or the old version."
        ),
        "{}",
        r.detail
    );
    assert!(
        r.detail.contains(&format!("ww-test-a-wren-old-{first}")),
        "{}",
        r.detail
    );
    // The queue is free: the next approved run goes to the new process.
    let res = poll_as(&app, &token, "two").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let second = run_id_of(&res.json());
    assert_eq!(run(&app, second).await.target_name, "ww-test-b");
    heartbeat(
        &app,
        &token,
        json!({"boot_id": "two", "uptime_seconds": 40}),
    )
    .await;
    assert_eq!(run(&app, second).await.status, "running");
    // Refused by agent.toml's [updates]: closed at once, nothing waits.
    assert_eq!(
        finish(&app, &token, second, "refused").await.status,
        StatusCode::OK
    );
    assert_eq!(run(&app, second).await.status, "refused");
    assert_eq!(
        app.count("SELECT COUNT(*) FROM runs WHERE status IN ('queued', 'running')")
            .await,
        0
    );
    // The urgent "needs you" went out.
    for _ in 0..50 {
        if got
            .lock()
            .unwrap()
            .iter()
            .any(|v| v.to_string().contains("The agent restarted"))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let sent: Vec<String> = got.lock().unwrap().iter().map(Value::to_string).collect();
    assert!(
        sent.iter()
            .any(|t| t.contains("needs you") && t.contains("The agent restarted")),
        "{sent:?}"
    );
}

#[tokio::test]
async fn old_agents_and_runs_handed_out_before_the_update() {
    let (app, _, token) = setup().await;
    approve_both(&app).await;
    // An agent before 0.2.1 sends no boot id: nothing changes for it.
    let first = run_id_of(&poll(&app, &token).await.json());
    heartbeat(&app, &token, json!({"agent_version": "0.2.0"})).await;
    assert_eq!(run(&app, first).await.status, "running");
    assert_eq!(
        app.count("SELECT COUNT(*) FROM runs WHERE agent_boot IS NOT NULL")
            .await,
        0
    );
    app.exec(format!(
        "UPDATE runs SET started_at = datetime('now', '-2 minutes') WHERE id = {first}"
    ))
    .await;
    // A new agent up since before the run (updated before Wren was) is the
    // process the run was handed to: it keeps running, also through polls.
    heartbeat(
        &app,
        &token,
        json!({"boot_id": "new", "uptime_seconds": 3600}),
    )
    .await;
    assert_eq!(
        poll_as(&app, &token, "new").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(run(&app, first).await.status, "running");
    // An agent started after the run (Tower: restarted mid-run, then
    // updated): its first heartbeat closes it.
    heartbeat(
        &app,
        &token,
        json!({"boot_id": "newer", "uptime_seconds": 20}),
    )
    .await;
    let r = run(&app, first).await;
    assert_eq!(r.status, "rollback_failed");
    assert!(r.detail.contains("The agent restarted"), "{}", r.detail);
    // An old agent's poll still gets the next run.
    let second = run_id_of(&poll(&app, &token).await.json());
    assert_ne!(first, second);
}

#[tokio::test]
async fn any_repo_digest_of_the_image_is_current() {
    let (app, _, token) = setup().await;
    assert_eq!(target(&app, "ww-test-a").await.status, "update");
    // Pulled again by a new index digest: RepoDigests has both.
    send_inventory(&app, &token, &[OLD, NEW]).await;
    let a = target(&app, "ww-test-a").await;
    assert_eq!(
        (a.status.as_str(), a.available_key.as_str()),
        ("current", "")
    );
    let id = a.id;
    let images = app
        .wren
        .core
        .with_conn(move |c| targets::images(c, id))
        .await
        .unwrap();
    assert_eq!(images[0].local_digest, NEW);
    // A newer digest is an update again, from a digest the image has.
    set_remote(&app, "localhost:5056/ww-test/app:latest", NEWER).await;
    let a = target(&app, "ww-test-a").await;
    assert_eq!(a.status, "update");
    assert_eq!(a.available_key, format!("ww-test-a={NEWER}"));
    let mut web = Web::new(&app, "ed", "editor").await;
    approve(&mut web, a.id, &a.available_key).await;
    let job = poll(&app, &token).await.json();
    let from = job["job"]["images"][0]["from"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(from == OLD || from == NEW, "{job}");
    assert_eq!(job["job"]["images"][0]["to"], NEWER);
}

#[tokio::test]
async fn approval_of_the_version_already_running_succeeds_at_once() {
    let (app, _, token) = setup().await;
    approve_both(&app).await;
    // The approved digest arrived meanwhile under another repo digest of the
    // same image (Tower's arrgresql18 after run 1).
    send_inventory(&app, &token, &[OLD, NEW]).await;
    let res = poll(&app, &token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.json()["job"]["target"]["name"], "ww-test-b");
    let r = run(&app, 1).await;
    assert_eq!(r.status, "succeeded");
    assert!(r.detail.starts_with("already up to date"), "{}", r.detail);
    assert_eq!(r.started_at, None);
}
