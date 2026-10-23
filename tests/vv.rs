//! Phase 5: Vigilant Vole windows and monitor health against a fake VV,
//! scheduled approvals, auto mode with windows and exclusions, and Wren's
//! own images (design §2, §5, §6).

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, patch};
use axum::{Json, Router};
use common::{App, Web, api_token, bearer};
use jiff::Timestamp;
use serde_json::{Value, json};
use vole_kit::auth::TokenScope;
use wise_wren::{runs, schedule, targets, vv};

const OLD: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

// ---------------------------------------------------------------------------
// A fake Vigilant Vole
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Vv {
    /// Monitors as VV's API gives them.
    monitors: Vec<Value>,
    /// Heartbeats per monitor id.
    beats: Vec<(i64, Value)>,
    /// Every write: (method, path, body).
    writes: Vec<(String, String, Value)>,
    next: i64,
}

type Shared = Arc<Mutex<Vv>>;

async fn fake_vv(raw: bool) -> (String, Shared) {
    let mut m = json!({"id": 1, "name": "web", "active": true, "status": "up",
        "tags": [{"id": 1, "name": "ww-test", "color": "#000", "value": ""}]});
    if raw {
        m["raw_status"] = json!("up");
        m["raw_status_since"] = Value::Null;
        m["raw_message"] = Value::Null;
    }
    let state: Shared = Arc::new(Mutex::new(Vv {
        monitors: vec![m],
        next: 30,
        ..Vv::default()
    }));
    async fn me() -> Json<Value> {
        Json(json!({"username": "wren", "role": "editor", "can_edit": true}))
    }
    async fn health() -> Json<Value> {
        Json(json!({"status": "ok", "version": "0.9.0"}))
    }
    async fn monitors(State(s): State<Shared>) -> Json<Value> {
        Json(json!({"data": s.lock().unwrap().monitors, "next_cursor": null}))
    }
    async fn beats(State(s): State<Shared>, Path(id): Path<i64>) -> Json<Value> {
        let data: Vec<Value> = s
            .lock()
            .unwrap()
            .beats
            .iter()
            .filter(|(m, _)| *m == id)
            .map(|(_, b)| b.clone())
            .collect();
        Json(json!({"data": data, "next_cursor": null}))
    }
    async fn pages() -> Json<Value> {
        Json(json!({"data": [{"id": 5, "title": "Status"}], "next_cursor": null}))
    }
    async fn page() -> Json<Value> {
        Json(json!({"id": 5, "sections": [{"monitors": [{"id": 1, "name": "web"}]}]}))
    }
    async fn create(State(s): State<Shared>, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
        let mut s = s.lock().unwrap();
        s.next += 1;
        let id = s.next;
        s.writes
            .push(("POST".into(), "/api/v1/maintenance".into(), b));
        (StatusCode::CREATED, Json(json!({"id": id})))
    }
    async fn change(
        State(s): State<Shared>,
        Path(id): Path<i64>,
        method: axum::http::Method,
        body: axum::body::Bytes,
    ) -> Json<Value> {
        let b = serde_json::from_slice(&body).unwrap_or(Value::Null);
        s.lock()
            .unwrap()
            .writes
            .push((method.to_string(), format!("/api/v1/maintenance/{id}"), b));
        Json(json!({"id": id}))
    }
    let app = Router::new()
        .route("/api/v1/me", get(me))
        .route("/api/v1/health", get(health))
        .route("/api/v1/monitors", get(monitors))
        .route("/api/v1/monitors/{id}/heartbeats", get(beats))
        .route("/api/v1/status-pages", get(pages))
        .route("/api/v1/status-pages/5", get(page))
        .route("/api/v1/maintenance", axum::routing::post(create))
        .route("/api/v1/maintenance/{id}", patch(change).delete(change))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state)
}

fn writes(vv: &Shared) -> Vec<(String, String, Value)> {
    vv.lock().unwrap().writes.clone()
}

// ---------------------------------------------------------------------------
// Wren set-up
// ---------------------------------------------------------------------------

async fn post_json(app: &App, path: &str, token: &str, body: Value) -> common::Res {
    let req = Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.send(req).await
}

fn container(name: &str, image: &str) -> Value {
    let repo = image.rsplit_once(':').map_or(image, |(r, _)| r);
    json!({"name": name, "image": image, "image_id": "sha256:1",
        "repo_digests": [format!("{repo}@{OLD}")], "state": "running", "labels": {}})
}

/// LANBox with two test containers and Wren's own agent, each with a new
/// digest.
async fn setup() -> (App, i64, String) {
    let app = App::new().await;
    let (id, token) = app.add_server("LANBox").await;
    let inv = json!({"containers": [
        container("ww-test-a", "localhost:5056/ww-test/app:a"),
        container("ww-test-b", "localhost:5056/ww-test/app:b"),
        container("wren-agent", "ghcr.io/ferdinand99/wise-wren-agent:latest"),
    ]});
    let res = post_json(&app, "/api/agent/v1/inventory", &token, inv).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    for image in [
        "localhost:5056/ww-test/app:a",
        "localhost:5056/ww-test/app:b",
        "ghcr.io/ferdinand99/wise-wren-agent:latest",
    ] {
        app.wren
            .core
            .with_conn(move |c| {
                targets::set_remote(
                    c,
                    image,
                    &Ok(wise_wren::registry::Remote {
                        digest: NEW.into(),
                        media_type: String::new(),
                    }),
                )?;
                targets::refresh_all(c)
            })
            .await
            .unwrap();
    }
    (app, id, token)
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
        .unwrap()
}

async fn run(app: &App, id: i64) -> runs::Run {
    app.wren
        .core
        .with_conn(move |c| runs::get(c, id))
        .await
        .unwrap()
        .unwrap()
}

async fn steps(app: &App, id: i64) -> Vec<(String, String, String)> {
    app.wren
        .core
        .with_conn(move |c| runs::steps(c, id))
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.step, s.status, s.log))
        .collect()
}

async fn step(app: &App, token: &str, run: i64, step: &str, status: &str) -> Value {
    let res = post_json(
        app,
        &format!("/api/agent/v1/runs/{run}/steps"),
        token,
        json!({"step": step, "status": status}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.json()
}

async fn poll_run(app: &App, token: &str) -> Option<i64> {
    let res = bearer(app, "GET", "/api/agent/v1/jobs?wait=0", token).await;
    (res.status == StatusCode::OK).then(|| res.json()["job"]["run_id"].as_i64().unwrap())
}

async fn connect(app: &App, url: &str) {
    vv::save(&app.wren.core, url, Some("vv_test"))
        .await
        .unwrap();
}

async fn link(app: &App, target_id: i64, kind: &'static str, r: &'static str) {
    app.wren
        .core
        .with_conn(move |c| vv::add_link(c, target_id, kind, r, r, "admin"))
        .await
        .unwrap();
}

async fn approve(app: &App, target_id: i64, at: Option<Timestamp>) -> Result<i64, runs::Refusal> {
    app.wren
        .core
        .with_conn(move |c| runs::approve_at(c, target_id, None, "ed", at))
        .await
        .unwrap()
}

/// `HH:MM` in UTC `minutes` from now (the site time zone is UTC here).
fn hhmm(minutes: i64) -> String {
    let t = Timestamp::now() + jiff::SignedDuration::from_mins(minutes);
    t.strftime("%H:%M").to_string()
}

async fn set_window(app: &App, server: i64, mode: &'static str, from: i64, to: i64) {
    let w = Some(Some((hhmm(from), hhmm(to))));
    app.wren
        .core
        .with_conn(move |c| runs::set_server_mode(c, server, mode, w, "admin", true))
        .await
        .unwrap()
        .unwrap();
}

async fn set_target_mode(
    app: &App,
    id: i64,
    mode: Option<&'static str>,
    admin: bool,
) -> Result<(), runs::Refusal> {
    app.wren
        .core
        .with_conn(move |c| {
            runs::set_target_policy(
                c,
                id,
                &runs::TargetSettings {
                    mode: mode.map(str::to_string),
                    health_seconds: Some(60),
                    require_vv: false,
                },
                "someone",
                admin,
            )
        })
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Anti-goals: auto off by default, never outside a window, exclusions, own
// images
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_default_stays_approve_and_only_admins_turn_auto_on() {
    let (app, server, _) = setup().await;
    assert_eq!(
        app.count("SELECT COUNT(*) FROM servers WHERE mode != 'approve'")
            .await,
        0
    );
    assert_eq!(
        app.count("SELECT COUNT(*) FROM targets WHERE mode IS NOT NULL")
            .await,
        0
    );
    // Nothing is picked without auto.
    let picked = app
        .wren
        .core
        .with_conn(|c| schedule::pick_auto(c, Timestamp::now()))
        .await
        .unwrap();
    assert!(picked.is_empty());
    let a = target(&app, "ww-test-a").await;
    // Editors can't, and auto needs a window.
    let editor = |mode, window| {
        let w = window;
        app.wren
            .core
            .with_conn(move |c| runs::set_server_mode(c, server, mode, w, "ed", false))
    };
    assert_eq!(
        editor("auto", None).await.unwrap(),
        Err(runs::Refusal::AdminOnly)
    );
    assert_eq!(
        app.wren
            .core
            .with_conn(move |c| runs::set_server_mode(c, server, "auto", None, "admin", true))
            .await
            .unwrap(),
        Err(runs::Refusal::NoWindow)
    );
    assert_eq!(
        set_target_mode(&app, a.id, Some("auto"), true).await,
        Err(runs::Refusal::NoWindow)
    );
    set_window(&app, server, "approve", -60, 60).await;
    assert_eq!(
        set_target_mode(&app, a.id, Some("auto"), false).await,
        Err(runs::Refusal::AdminOnly)
    );
    assert_eq!(
        set_target_mode(&app, a.id, Some("auto"), true).await,
        Ok(())
    );
    // Following an auto server is turning auto on too.
    let b = target(&app, "ww-test-b").await;
    set_window(&app, server, "auto", -60, 60).await;
    set_target_mode(&app, b.id, Some("approve"), true)
        .await
        .unwrap();
    assert_eq!(
        set_target_mode(&app, b.id, None, false).await,
        Err(runs::Refusal::AdminOnly)
    );
    // Logged.
    assert!(
        app.count("SELECT COUNT(*) FROM actions WHERE action = 'server_mode'")
            .await
            >= 2
    );
}

#[tokio::test]
async fn auto_runs_only_inside_the_window_and_skip_exclusions() {
    let (app, server, token) = setup().await;
    let (a, b) = (
        target(&app, "ww-test-a").await,
        target(&app, "ww-test-b").await,
    );
    // An auto server; b is excluded (approve); Wren's own agent follows the
    // server but is never updated.
    set_window(&app, server, "auto", 60, 120).await;
    set_target_mode(&app, b.id, Some("approve"), true)
        .await
        .unwrap();
    let pick = || {
        app.wren
            .core
            .with_conn(|c| schedule::pick_auto(c, Timestamp::now()))
    };
    assert!(pick().await.unwrap().is_empty(), "the window isn't open");
    set_window(&app, server, "auto", -60, 60).await;
    let ids = pick().await.unwrap();
    assert_eq!(ids.len(), 1, "only ww-test-a");
    let r = run(&app, ids[0]).await;
    assert_eq!(
        (r.target_name.as_str(), r.trigger.as_str()),
        ("ww-test-a", "auto")
    );
    assert_eq!(r.to_key, a.available_key);
    assert!(pick().await.unwrap().is_empty(), "one run per target");
    // The window closes before the agent picks it up: it lapses.
    set_window(&app, server, "auto", 60, 120).await;
    assert_eq!(poll_run(&app, &token).await, None);
    let r = run(&app, ids[0]).await;
    assert_eq!(r.status, "lapsed");
    assert!(r.detail.contains("auto window"), "{}", r.detail);
    // A version whose auto run failed isn't tried again on its own.
    app.exec(format!(
        "UPDATE runs SET status = 'rolled_back' WHERE id = {}",
        ids[0]
    ))
    .await;
    set_window(&app, server, "auto", -60, 60).await;
    assert!(pick().await.unwrap().is_empty());
    // The schedules API shows which targets update on their own.
    let uid = common::add_user(&app, "viewer", "viewer").await;
    let tok = api_token(&app, uid, TokenScope::Read).await;
    let s = bearer(&app, "GET", "/api/v1/schedules", &tok).await.json();
    let names: Vec<&str> = s["servers"][0]["auto_targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["ww-test-a"], "{s}");
}

#[tokio::test]
async fn own_images_are_never_updated() {
    let (app, server, _) = setup().await;
    let own = target(&app, "wren-agent").await;
    assert_eq!(own.status, "update");
    assert_eq!(
        approve(&app, own.id, None).await,
        Err(runs::Refusal::OwnImage)
    );
    let later = Timestamp::now() + jiff::SignedDuration::from_hours(1);
    assert_eq!(
        approve(&app, own.id, Some(later)).await,
        Err(runs::Refusal::OwnImage)
    );
    set_window(&app, server, "auto", -60, 60).await;
    let ids = app
        .wren
        .core
        .with_conn(|c| schedule::pick_auto(c, Timestamp::now()))
        .await
        .unwrap();
    for id in ids {
        assert_ne!(run(&app, id).await.target_name, "wren-agent");
    }
    let mut web = Web::new(&app, "adm", "admin").await;
    let page = web.get(&format!("/targets/{}", own.id)).await;
    assert!(page.body.contains("Not updated by Wren"), "{}", page.body);
    let page = web.get(&format!("/servers/{server}")).await;
    assert!(page.body.contains("data-own=\"1\""));
    assert!(page.body.contains("data-auto=\"1\""));
}

// ---------------------------------------------------------------------------
// Scheduled approvals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scheduled_run_waits_for_its_time_and_can_move() {
    let (app, _, token) = setup().await;
    let a = target(&app, "ww-test-a").await;
    let past = Timestamp::now() - jiff::SignedDuration::from_mins(5);
    assert_eq!(
        approve(&app, a.id, Some(past)).await,
        Err(runs::Refusal::BadTime)
    );
    let later = Timestamp::now() + jiff::SignedDuration::from_hours(2);
    let id = approve(&app, a.id, Some(later)).await.unwrap();
    let r = run(&app, id).await;
    assert_eq!(r.trigger, "schedule");
    assert_eq!(r.to_key, a.available_key);
    assert_eq!(poll_run(&app, &token).await, None, "not before its time");
    // Move it (API, editor token), then let its time come.
    let uid = common::add_user(&app, "ed", "editor").await;
    let tok = api_token(&app, uid, TokenScope::Write).await;
    let res = post_json(
        &app,
        &format!("/api/v1/runs/{id}/reschedule"),
        &tok,
        json!({"at": "2099-01-01T00:00:00Z"}),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "too far ahead: {}",
        res.body
    );
    let soon = schedule::db_time(Timestamp::now() + jiff::SignedDuration::from_hours(3));
    let res = post_json(
        &app,
        &format!("/api/v1/runs/{id}/reschedule"),
        &tok,
        json!({"at": soon.replace(' ', "T") + "Z"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        run(&app, id).await.scheduled_for.as_deref(),
        Some(soon.as_str())
    );
    app.exec(format!(
        "UPDATE runs SET scheduled_for = datetime('now', '-1 minute') WHERE id = {id}"
    ))
    .await;
    assert_eq!(poll_run(&app, &token).await, Some(id));
    // Schedule through the API, then cancel it.
    let b = target(&app, "ww-test-b").await;
    let res = post_json(
        &app,
        &format!("/api/v1/targets/{}/schedule", b.id),
        &tok,
        json!({"version_key": b.available_key, "at": soon.replace(' ', "T") + "Z"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let rid = res.json()["run_id"].as_i64().unwrap();
    let res = post_json(&app, &format!("/api/v1/runs/{rid}/cancel"), &tok, json!({})).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(run(&app, rid).await.status, "cancelled");
    // Mode changes through the API: auto needs an admin.
    let res = post_json(
        &app,
        &format!("/api/v1/targets/{}/mode", b.id),
        &tok,
        json!({"mode": "auto"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = post_json(
        &app,
        &format!("/api/v1/targets/{}/mode", b.id),
        &tok,
        json!({"mode": "notify"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

// ---------------------------------------------------------------------------
// Vigilant Vole windows (steps 2 and 8) and planned windows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_opens_and_closes_a_window() {
    let (url, fake) = fake_vv(true).await;
    let (app, _, token) = setup().await;
    connect(&app, &url).await;
    let a = target(&app, "ww-test-a").await;
    link(&app, a.id, "tag", "ww-test").await;
    let id = approve(&app, a.id, None).await.unwrap();
    assert_eq!(poll_run(&app, &token).await, Some(id));
    assert!(writes(&fake).is_empty(), "nothing before the preflight");
    let ans = step(&app, &token, id, "preflight", "ok").await;
    assert_eq!(ans["cancel"], false);
    let w = writes(&fake);
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0].0, "POST");
    assert_eq!(w[0].2["monitor_ids"], json!([1]));
    assert_eq!(w[0].2["status_page_ids"], json!([5]));
    assert_eq!(w[0].2["strategy"], "once");
    let r = run(&app, id).await;
    assert_eq!((r.vv_state.as_str(), r.vv_window_id), ("open", Some(31)));
    // The run rolls back: the window still closes at the end.
    let res = post_json(
        &app,
        &format!("/api/agent/v1/runs/{id}/finish"),
        &token,
        json!({"outcome": "rolled_back", "detail": "broken"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    vv::sync(&app.wren).await;
    let w = writes(&fake);
    assert_eq!(w.len(), 2, "{w:?}");
    assert_eq!(
        (w[1].0.as_str(), w[1].1.as_str()),
        ("PATCH", "/api/v1/maintenance/31")
    );
    assert!(w[1].2["end_at"].is_string());
    assert_eq!(run(&app, id).await.vv_state, "closed");
    let s = steps(&app, id).await;
    assert!(
        s.iter().any(|(n, st, _)| n == "maintenance" && st == "ok"),
        "{s:?}"
    );
    assert!(
        s.iter().any(|(n, st, _)| n == "close" && st == "ok"),
        "{s:?}"
    );
    // Nothing more to do.
    vv::sync(&app.wren).await;
    assert_eq!(writes(&fake).len(), 2);
}

#[tokio::test]
async fn scheduled_run_plans_a_window_and_activates_it() {
    let (url, fake) = fake_vv(true).await;
    let (app, _, token) = setup().await;
    connect(&app, &url).await;
    let a = target(&app, "ww-test-a").await;
    link(&app, a.id, "monitor", "1").await;
    let at = Timestamp::now() + jiff::SignedDuration::from_hours(5);
    let id = approve(&app, a.id, Some(at)).await.unwrap();
    vv::sync(&app.wren).await;
    let w = writes(&fake);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].2["start_at"], schedule::db_time(at));
    assert_eq!(run(&app, id).await.vv_state, "planned");
    // Moved: the planned window follows.
    let later = at + jiff::SignedDuration::from_hours(1);
    app.wren
        .core
        .with_conn(move |c| runs::reschedule(c, id, later, "ed"))
        .await
        .unwrap()
        .unwrap();
    vv::sync(&app.wren).await;
    let w = writes(&fake);
    assert_eq!(
        (w[1].0.as_str(), w[1].1.as_str()),
        ("PATCH", "/api/v1/maintenance/31")
    );
    assert_eq!(w[1].2["start_at"], schedule::db_time(later));
    // Its time comes: the planned window becomes active (no second window).
    app.exec(format!(
        "UPDATE runs SET scheduled_for = datetime('now', '-1 minute') WHERE id = {id}"
    ))
    .await;
    assert_eq!(poll_run(&app, &token).await, Some(id));
    step(&app, &token, id, "preflight", "ok").await;
    let w = writes(&fake);
    assert_eq!(w.len(), 3);
    assert_eq!(w[2].0, "PATCH");
    assert_eq!(run(&app, id).await.vv_state, "open");
    // A cancelled schedule deletes its planned window.
    let b = target(&app, "ww-test-b").await;
    link(&app, b.id, "monitor", "1").await;
    let rid = approve(&app, b.id, Some(at)).await.unwrap();
    vv::sync(&app.wren).await;
    app.wren
        .core
        .with_conn(move |c| runs::cancel(c, rid, "ed"))
        .await
        .unwrap()
        .unwrap();
    vv::sync(&app.wren).await;
    let w = writes(&fake);
    assert_eq!(w.last().unwrap().0, "DELETE", "{w:?}");
}

#[tokio::test]
async fn vv_down_continues_or_cancels_with_require_vv() {
    let (app, _, token) = setup().await;
    connect(&app, "http://127.0.0.1:9").await;
    let a = target(&app, "ww-test-a").await;
    link(&app, a.id, "monitor", "1").await;
    let id = approve(&app, a.id, None).await.unwrap();
    assert_eq!(poll_run(&app, &token).await, Some(id));
    assert_eq!(
        step(&app, &token, id, "preflight", "ok").await["cancel"],
        false
    );
    let s = steps(&app, id).await;
    let m = s.iter().find(|(n, _, _)| n == "maintenance").unwrap();
    assert_eq!(m.1, "failed");
    assert!(m.2.contains("without a maintenance window"), "{}", m.2);
    // Required: the run stops before anything stops.
    app.exec(format!(
        "UPDATE runs SET status = 'succeeded' WHERE id = {id}"
    ))
    .await;
    app.exec(format!(
        "UPDATE targets SET require_vv = 1 WHERE id = {}",
        a.id
    ))
    .await;
    let b = target(&app, "ww-test-b").await;
    link(&app, b.id, "monitor", "1").await;
    app.exec(format!(
        "UPDATE targets SET require_vv = 1 WHERE id = {}",
        b.id
    ))
    .await;
    let id = approve(&app, b.id, None).await.unwrap();
    assert_eq!(poll_run(&app, &token).await, Some(id));
    assert_eq!(
        step(&app, &token, id, "preflight", "ok").await["cancel"],
        true
    );
    let res = post_json(
        &app,
        &format!("/api/agent/v1/runs/{id}/finish"),
        &token,
        json!({"outcome": "cancelled", "detail": ""}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let r = run(&app, id).await;
    assert_eq!(r.status, "cancelled");
    assert!(r.detail.contains("requires Vigilant Vole"), "{}", r.detail);
}

// ---------------------------------------------------------------------------
// Monitor health (step 6)
// ---------------------------------------------------------------------------

async fn ask(app: &App, token: &str, id: i64) -> Value {
    let res = post_json(
        app,
        &format!("/api/agent/v1/runs/{id}/monitors"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.json()
}

#[tokio::test]
async fn monitors_endpoint_uses_raw_status() {
    let (url, fake) = fake_vv(true).await;
    let (app, _, token) = setup().await;
    connect(&app, &url).await;
    let a = target(&app, "ww-test-a").await;
    let b = target(&app, "ww-test-b").await;
    // No links: the container checks decide.
    let id = approve(&app, b.id, None).await.unwrap();
    assert_eq!(poll_run(&app, &token).await, Some(id));
    assert_eq!(ask(&app, &token, id).await["verdict"], "skipped");
    app.exec(format!(
        "UPDATE runs SET status = 'succeeded' WHERE id = {id}"
    ))
    .await;

    link(&app, a.id, "monitor", "1").await;
    let id = approve(&app, a.id, None).await.unwrap();
    assert_eq!(poll_run(&app, &token).await, Some(id));
    step(&app, &token, id, "health", "running").await;
    // The health step began 70 s ago (period 60 s for this target).
    app.exec(format!(
        "UPDATE run_steps SET started_at = datetime('now', '-70 seconds') WHERE run_id = {id} AND step = 'health';
         UPDATE runs SET job = '{{\"health_seconds\": 60}}' WHERE id = {id};"
    ))
    .await;
    let ago = |s: i64| schedule::db_time(Timestamp::now() - jiff::SignedDuration::from_secs(s));
    // In maintenance publicly, but really down after the period: failed.
    fake.lock().unwrap().beats = vec![
        (
            1,
            json!({"checked_at": ago(60), "status": "maintenance", "raw_status": "up"}),
        ),
        (
            1,
            json!({"checked_at": ago(2), "status": "maintenance", "raw_status": "down"}),
        ),
    ];
    assert_eq!(ask(&app, &token, id).await["verdict"], "failed");
    // Really up since the start: ok.
    fake.lock().unwrap().beats = vec![
        (
            1,
            json!({"checked_at": ago(65), "status": "maintenance", "raw_status": "up"}),
        ),
        (
            1,
            json!({"checked_at": ago(5), "status": "maintenance", "raw_status": "up"}),
        ),
    ];
    assert_eq!(ask(&app, &token, id).await["verdict"], "ok");
    // Down while starting, up for only 20 s: wait.
    fake.lock().unwrap().beats = vec![
        (
            1,
            json!({"checked_at": ago(65), "status": "maintenance", "raw_status": "down"}),
        ),
        (
            1,
            json!({"checked_at": ago(20), "status": "maintenance", "raw_status": "up"}),
        ),
    ];
    assert_eq!(ask(&app, &token, id).await["verdict"], "wait");
    let s = steps(&app, id).await;
    assert!(s.iter().any(|(n, _, _)| n == "monitors"), "{s:?}");
}

#[tokio::test]
async fn old_vv_falls_back_to_container_checks() {
    let (url, _fake) = fake_vv(false).await;
    let (app, _, token) = setup().await;
    connect(&app, &url).await;
    let a = target(&app, "ww-test-a").await;
    link(&app, a.id, "monitor", "1").await;
    let id = approve(&app, a.id, None).await.unwrap();
    assert_eq!(poll_run(&app, &token).await, Some(id));
    step(&app, &token, id, "health", "running").await;
    let v = ask(&app, &token, id).await;
    assert_eq!(v["verdict"], "skipped");
    assert!(v["log"].as_str().unwrap().contains("too old"), "{v}");
    // Test connection says so too.
    let conn = vv::connection(&app.wren.core).await.unwrap();
    let t = vv::test(&app.wren.core, conn).await;
    assert!(t.ok && t.too_old, "{t:?}");
    // The settings page is for admins, with the token masked.
    let mut web = Web::new(&app, "adm", "admin").await;
    let page = web.get("/settings/vigilant-vole").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("vv_test"));
    let mut ed = Web::new(&app, "ed", "editor").await;
    assert_eq!(
        ed.get("/settings/vigilant-vole").await.status,
        StatusCode::FORBIDDEN
    );
    let page = web.get("/schedules").await;
    assert_eq!(page.status, StatusCode::OK);
}
