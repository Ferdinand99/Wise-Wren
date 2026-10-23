//! Phase 3: inventories become targets, check runs mark updates against a
//! fake registry and a fake GitHub, notifications go out once per version,
//! registry credentials stay sealed, and the pages, REST and MCP list it all.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use common::{App, Web, api_token, bearer};
use serde_json::{Value, json};
use vole_kit::auth::TokenScope;
use wise_wren::checks;
use wise_wren::targets::{self, Filter, Target};

/// A registry, GitHub and a webhook receiver in one: `digests` answers
/// manifest HEADs by repository, `releases` GitHub's latest release by
/// `owner/repo`, and `/hook` collects webhook bodies.
#[derive(Clone, Default)]
struct Fake {
    digests: Arc<Mutex<HashMap<String, String>>>,
    releases: Arc<Mutex<HashMap<String, String>>>,
    seen: Arc<Mutex<Vec<String>>>,
    hooks: Arc<Mutex<Vec<Value>>>,
}

impl Fake {
    async fn start() -> (Fake, String) {
        let fake = Fake::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, addr)
    }

    fn set_digest(&self, repo: &str, digest: &str) {
        self.digests
            .lock()
            .unwrap()
            .insert(repo.into(), digest.into());
    }

    fn count(&self, prefix: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with(prefix))
            .count()
    }
}

async fn handle(State(fake): State<Fake>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    fake.seen
        .lock()
        .unwrap()
        .push(format!("{} {path}", req.method()));
    if let Some(rest) = path.strip_prefix("/v2/") {
        let repo = rest.split("/manifests/").next().unwrap_or("");
        return match fake.digests.lock().unwrap().get(repo) {
            Some(d) => Response::builder()
                .status(200)
                .header("docker-content-digest", d.as_str())
                .body(Body::empty())
                .unwrap(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if let Some(rest) = path.strip_prefix("/repos/") {
        let repo = rest.trim_end_matches("/releases/latest");
        return match fake.releases.lock().unwrap().get(repo) {
            Some(tag) => axum::Json(json!({
                "tag_name": tag,
                "name": format!("Release {tag}"),
                "html_url": format!("https://github.com/{repo}/releases/tag/{tag}"),
            }))
            .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if path == "/version.json" {
        return axum::Json(json!({ "release": { "version": "2.0.0" } })).into_response();
    }
    if path == "/hook" {
        let body = axum::body::to_bytes(req.into_body(), usize::MAX)
            .await
            .unwrap();
        fake.hooks
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
        return StatusCode::OK.into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

fn container(name: &str, image: &str, digests: &[&str], labels: Value) -> Value {
    json!({
        "name": name, "image": image, "image_id": "sha256:1",
        "repo_digests": digests, "state": "running", "labels": labels,
    })
}

async fn inventory(app: &App, token: &str, body: Value) -> common::Res {
    let req = axum::http::Request::post("/api/agent/v1/inventory")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.send(req).await
}

async fn all(app: &App) -> Vec<Target> {
    app.wren
        .core
        .with_conn(|c| targets::list(c, Filter::default()))
        .await
        .unwrap()
}

fn by_name<'a>(rows: &'a [Target], name: &str) -> &'a Target {
    rows.iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no {name}"))
}

/// LANBox with a container per case on the fake registry at `reg`.
fn lanbox(reg: &str) -> Value {
    json!({
        "containers": [
            container("same", &format!("{reg}/team/same:1"), &[&format!("{reg}/team/same@sha256:1111")], json!({})),
            container("old", &format!("{reg}/team/old:1"), &[&format!("{reg}/team/old@sha256:aaaa")],
                json!({"org.opencontainers.image.source": "https://github.com/team/old"})),
            container("pinned", "redis@sha256:1009f15c", &["redis@sha256:1009f15c"], json!({})),
            container("built", "wren-local:1", &[], json!({})),
            container("Newt", &format!("{reg}/team/same:1"), &[&format!("{reg}/team/same@sha256:1111")], json!({})),
            container("pz-db", &format!("{reg}/team/old:1"), &[&format!("{reg}/team/old@sha256:aaaa")],
                json!({"com.docker.compose.project": "postiz", "com.docker.compose.project.working_dir": "/boot/x/postiz"})),
            container("pz-app", &format!("{reg}/team/same:1"), &[&format!("{reg}/team/same@sha256:1111")],
                json!({"com.docker.compose.project": "postiz"})),
            container("stack-web", &format!("{reg}/team/same:1"), &[&format!("{reg}/team/same@sha256:1111")],
                json!({"com.docker.compose.project": "ww-test-stack"})),
        ],
        "unraid_templates": ["Newt"],
        "compose_projects": [{"name": "postiz", "working_dir": "/boot/x/postiz", "config_files": "compose.yaml", "files_reachable": false}],
        "script_stacks": [{"name": "ww-test-stack", "compose_project": "ww-test-stack", "working_dir": "/mnt/user/appdata/ww-test-stack",
            "files_reachable": true, "version": {"source": {"github": "traefik/whoami", "command": false}, "installed": "v1.10.3", "latest": "", "error": ""}}],
        "error": "",
    })
}

async fn setup() -> (App, Fake, String, i64, String) {
    let app = App::new().await;
    let (fake, reg) = Fake::start().await;
    fake.set_digest("team/same", "sha256:1111");
    fake.set_digest("team/old", "sha256:2222");
    fake.releases
        .lock()
        .unwrap()
        .insert("traefik/whoami".into(), "v1.12.0".into());
    fake.releases
        .lock()
        .unwrap()
        .insert("team/old".into(), "v3.0.0".into());
    app.wren.github.set_api(&format!("http://{reg}"));
    let (id, token) = app.add_server("LANBox").await;
    (app, fake, reg, id, token)
}

#[tokio::test]
async fn inventory_builds_targets() {
    let (app, _fake, reg, id, token) = setup().await;
    let res = inventory(&app, &token, lanbox(&reg)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.json()["targets"], 7);
    let rows = all(&app).await;
    let kinds: Vec<(&str, &str)> = rows
        .iter()
        .map(|t| (t.kind.as_str(), t.name.as_str()))
        .collect();
    for want in [
        ("container", "same"),
        ("container", "pinned"),
        ("unraid", "Newt"),
        ("compose", "postiz"),
        ("script", "ww-test-stack"),
    ] {
        assert!(kinds.contains(&want), "{want:?} in {kinds:?}");
    }
    let postiz = by_name(&rows, "postiz");
    assert!(!postiz.files_reachable);
    assert_eq!(postiz.image_count, 2);
    assert_eq!(by_name(&rows, "ww-test-stack").image_count, 1);
    // Nothing is checked yet: unknown, with why.
    assert_eq!(by_name(&rows, "same").status_detail, "not_checked");
    assert_eq!(by_name(&rows, "pinned").status_detail, "pinned");
    assert_eq!(by_name(&rows, "built").status_detail, "local");

    // A container that went away goes away; a failed Docker read keeps all.
    let mut less = lanbox(&reg);
    less["containers"].as_array_mut().unwrap().remove(3);
    inventory(&app, &token, less).await;
    assert!(all(&app).await.iter().all(|t| t.name != "built"));
    let res = inventory(
        &app,
        &token,
        json!({"error": "can't open /var/run/docker.sock"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(all(&app).await.len(), 6);
    let err = app
        .count("SELECT COUNT(*) FROM servers WHERE inventory_error LIKE 'can''t open%'")
        .await;
    assert_eq!(err, 1);
    let _ = id;
}

#[tokio::test]
async fn inventory_needs_an_agent_token() {
    let (app, _fake, reg, _id, _token) = setup().await;
    let admin = common::add_user(&app, "admin", "admin").await;
    let api = api_token(&app, admin, TokenScope::Write).await;
    for token in ["", "wwa_unknown", api.as_str()] {
        let res = inventory(&app, token, lanbox(&reg)).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{token}");
    }
    assert_eq!(app.count("SELECT COUNT(*) FROM targets").await, 0);
}

#[tokio::test]
async fn check_now_sets_the_scan_flag() {
    let (app, _fake, reg, id, token) = setup().await;
    let beat = json!({"agent_version": "0.1.0"});
    assert_eq!(
        app.heartbeat(Some(&token), &beat).await.json()["scan"],
        false
    );
    let mut viewer = Web::new(&app, "viewer", "viewer").await;
    let res = viewer.post(&format!("/servers/{id}/check"), &[]).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let mut editor = Web::new(&app, "editor", "editor").await;
    let res = editor.post(&format!("/servers/{id}/check"), &[]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(
        app.heartbeat(Some(&token), &beat).await.json()["scan"],
        true
    );
    // While the online agent scans, its targets wait (at most a minute).
    inventory(&app, &token, lanbox(&reg)).await;
    assert_eq!(
        app.heartbeat(Some(&token), &beat).await.json()["scan"],
        false
    );
    editor.post(&format!("/servers/{id}/check"), &[]).await;
    assert_eq!(checks::run_due(&app.wren).await.targets, 0);
    inventory(&app, &token, lanbox(&reg)).await;
    assert_eq!(checks::run_due(&app.wren).await.targets, 7);
}

#[tokio::test]
async fn check_run_marks_updates() {
    let (app, fake, reg, _id, token) = setup().await;
    inventory(&app, &token, lanbox(&reg)).await;
    let report = checks::run_due(&app.wren).await;
    assert_eq!(report.targets, 7);
    // Two references, one HEAD each, shared by every target; none for the
    // pinned or local images; never a GET.
    assert_eq!(report.images, 2);
    assert_eq!(fake.count("HEAD /v2/"), 2);
    assert_eq!(fake.count("GET /v2/"), 0);
    let rows = all(&app).await;
    let status = |n: &str| by_name(&rows, n).status.clone();
    assert_eq!(status("same"), "current");
    assert_eq!(status("Newt"), "current");
    assert_eq!(status("old"), "update");
    assert_eq!(by_name(&rows, "old").available, "2222");
    assert_eq!(status("pinned"), "unknown");
    assert_eq!(status("built"), "unknown");
    assert_eq!(status("postiz"), "update");
    assert_eq!(by_name(&rows, "postiz").available, "1/2");
    // Release notes from the source label, only for the update.
    assert_eq!(
        by_name(&rows, "old").notes_url,
        "https://github.com/team/old/releases/tag/v3.0.0"
    );
    // Nothing is due right after a run.
    assert_eq!(checks::run_due(&app.wren).await.targets, 0);
    assert_eq!(fake.count("HEAD /v2/"), 2);

    // The user updated "old": the next inventory says current at once.
    let mut updated = lanbox(&reg);
    updated["containers"][1]["repo_digests"] = json!([format!("{reg}/team/old@sha256:2222")]);
    inventory(&app, &token, updated).await;
    assert_eq!(by_name(&all(&app).await, "old").status, "current");
}

#[tokio::test]
async fn intervals_decide_what_is_due() {
    let (app, _fake, reg, id, token) = setup().await;
    inventory(&app, &token, lanbox(&reg)).await;
    checks::run_due(&app.wren).await;
    app.exec("UPDATE targets SET last_checked_at = datetime('now', '-5 hours')".into())
        .await;
    assert_eq!(checks::run_due(&app.wren).await.targets, 0);
    app.exec("UPDATE targets SET check_interval_hours = 4 WHERE name = 'same'".into())
        .await;
    assert_eq!(checks::run_due(&app.wren).await.targets, 1);
    let mut editor = Web::new(&app, "editor", "editor").await;
    let res = editor
        .post(&format!("/servers/{id}/interval"), &[("hours", "200")])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let res = editor
        .post(&format!("/servers/{id}/interval"), &[("hours", "3")])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.count("SELECT check_interval_hours FROM servers").await,
        3
    );
    // 5 hours ago is past the server's new 3 hours.
    assert_eq!(checks::run_due(&app.wren).await.targets, 6);
}

#[tokio::test]
async fn script_stack_versions() {
    let (app, _fake, reg, _id, token) = setup().await;
    let mut inv = lanbox(&reg);
    inv["script_stacks"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({"name": "by-url", "compose_project": "none", "working_dir": "/x", "files_reachable": true,
                "version": {"source": {"url": format!("http://{reg}/version.json"), "json_pointer": "/release/version"}, "installed": "2.0.0"}}),
            json!({"name": "by-command", "compose_project": "none", "working_dir": "/y", "files_reachable": true,
                "version": {"source": {"command": true}, "installed": "1.0", "latest": "1.1"}}),
        ]);
    inventory(&app, &token, inv).await;
    checks::run_due(&app.wren).await;
    let rows = all(&app).await;
    let stack = by_name(&rows, "ww-test-stack");
    assert_eq!(stack.latest_version, "v1.12.0");
    assert_eq!(stack.status, "update");
    assert_eq!(stack.available, "v1.12.0");
    assert_eq!(
        stack.notes_url,
        "https://github.com/traefik/whoami/releases/tag/v1.12.0"
    );
    let url = by_name(&rows, "by-url");
    assert_eq!(
        (url.latest_version.as_str(), url.status.as_str()),
        ("2.0.0", "current")
    );
    let cmd = by_name(&rows, "by-command");
    assert_eq!(
        (cmd.latest_version.as_str(), cmd.status.as_str()),
        ("1.1", "update")
    );
}

async fn add_webhook(app: &App, reg: &str, name: &str, digest: bool) {
    let config = json!({"url": format!("http://{reg}/hook"), "method": "POST"}).to_string();
    let (name, digest) = (name.to_string(), digest);
    app.wren
        .core
        .with_conn(move |c| {
            c.execute(
                "INSERT INTO notification_channels (name, kind, config, digest) VALUES (?1, 'webhook', ?2, ?3)",
                rusqlite::params![name, config, digest],
            )
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn new_version_notifies_once() {
    let (app, fake, reg, id, token) = setup().await;
    add_webhook(&app, &reg, "one by one", false).await;
    inventory(&app, &token, lanbox(&reg)).await;
    let report = checks::run_due(&app.wren).await;
    // old, postiz and the script stack.
    assert_eq!(report.notifications, 3);
    let hooks = fake.hooks.lock().unwrap().clone();
    assert_eq!(hooks.len(), 3);
    let text = hooks.iter().map(Value::to_string).collect::<String>();
    assert!(text.contains("New version for old on LANBox"), "{text}");

    // Checked again: same versions, nothing sent.
    app.wren
        .core
        .with_conn(move |c| targets::request_check(c, id, None))
        .await
        .unwrap();
    app.exec("UPDATE servers SET scan_requested_at = NULL".into())
        .await;
    assert_eq!(checks::run_due(&app.wren).await.notifications, 0);
    // A newer version of "old": one more for each target running it.
    fake.set_digest("team/old", "sha256:3333");
    app.wren
        .core
        .with_conn(move |c| targets::request_check(c, id, None))
        .await
        .unwrap();
    app.exec("UPDATE servers SET scan_requested_at = NULL".into())
        .await;
    assert_eq!(checks::run_due(&app.wren).await.notifications, 2);
    assert_eq!(fake.hooks.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn digest_channel_gets_one_message() {
    let (app, fake, reg, _id, token) = setup().await;
    add_webhook(&app, &reg, "digest", true).await;
    inventory(&app, &token, lanbox(&reg)).await;
    assert_eq!(checks::run_due(&app.wren).await.notifications, 1);
    let hooks = fake.hooks.lock().unwrap().clone();
    assert!(hooks[0].to_string().contains("3 new versions"), "{hooks:?}");
}

#[tokio::test]
async fn registry_credentials_are_encrypted_and_masked() {
    let app = App::new().await;
    let mut admin = Web::new(&app, "admin", "admin").await;
    let res = admin
        .post(
            "/settings/registries",
            &[
                ("host", "https://GHCR.io/"),
                ("username", "me"),
                ("password", "s3cret-token"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let stored: Vec<u8> = app
        .wren
        .core
        .with_conn(|c| c.query_row("SELECT secret FROM registries", [], |r| r.get(0)))
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&stored).contains("s3cret"));
    let creds = wise_wren::registries::credentials(&app.wren.core).await;
    assert_eq!(
        creds["ghcr.io"],
        ("me".to_string(), "s3cret-token".to_string())
    );
    let list = admin.get("/settings/registries").await;
    assert!(list.body.contains("ghcr.io") && !list.body.contains("s3cret"));
    let edit = admin.get("/settings/registries/1").await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(!edit.body.contains("s3cret") && edit.body.contains("••••••"));
    // Empty keeps the password; a new one replaces it.
    admin
        .post(
            "/settings/registries/1",
            &[("username", "me2"), ("password", "")],
        )
        .await;
    let creds = wise_wren::registries::credentials(&app.wren.core).await;
    assert_eq!(
        creds["ghcr.io"],
        ("me2".to_string(), "s3cret-token".to_string())
    );
    admin
        .post(
            "/settings/registries/1",
            &[("username", "me2"), ("password", "new")],
        )
        .await;
    let creds = wise_wren::registries::credentials(&app.wren.core).await;
    assert_eq!(creds["ghcr.io"].1, "new");
    let res = admin
        .post(
            "/settings/registries",
            &[("host", "ghcr.io"), ("username", "x"), ("password", "y")],
        )
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        admin.delete("/settings/registries/1").await.status,
        StatusCode::OK
    );
    assert_eq!(app.count("SELECT COUNT(*) FROM registries").await, 0);
}

#[tokio::test]
async fn only_admins_manage_registries() {
    let app = App::new().await;
    let mut editor = Web::new(&app, "editor", "editor").await;
    assert_eq!(
        editor.get("/settings/registries").await.status,
        StatusCode::FORBIDDEN
    );
    let res = editor
        .post(
            "/settings/registries",
            &[("host", "ghcr.io"), ("username", "x"), ("password", "y")],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn pages_show_targets_and_updates() {
    let (app, _fake, reg, id, token) = setup().await;
    inventory(&app, &token, lanbox(&reg)).await;
    checks::run_due(&app.wren).await;
    let mut viewer = Web::new(&app, "viewer", "viewer").await;
    let updates = viewer.get("/updates").await;
    assert_eq!(updates.status, StatusCode::OK);
    assert!(updates.body.contains("ww-test-stack") && updates.body.contains("postiz"));
    assert!(!updates.body.contains(">same<"), "only updates");
    let server = viewer.get(&format!("/servers/{id}")).await;
    assert!(
        server.body.contains("Files not reachable"),
        "postiz isn't mounted"
    );
    assert!(server.body.contains("Unraid template"));
    assert!(!server.body.contains("Check now"), "viewers can't check");
    let old = by_name(&all(&app).await, "old").id;
    let page = viewer.get(&format!("/targets/{old}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Release v3.0.0"));
    let overview = viewer.get("/").await;
    assert!(overview.body.contains("Updates available"));
    let res = viewer
        .post(&format!("/targets/{old}/interval"), &[("hours", "2")])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn api_and_mcp_list_targets() {
    let (app, _fake, reg, id, token) = setup().await;
    inventory(&app, &token, lanbox(&reg)).await;
    checks::run_due(&app.wren).await;
    let viewer = common::add_user(&app, "viewer", "viewer").await;
    let api = api_token(&app, viewer, TokenScope::Read).await;
    let res = bearer(&app, "GET", "/api/v1/updates", &api).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let names: Vec<String> = res.json()["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["old", "postiz", "ww-test-stack"]);
    let res = bearer(
        &app,
        "GET",
        &format!("/api/v1/targets?server_id={id}"),
        &api,
    )
    .await;
    assert_eq!(res.json()["targets"].as_array().unwrap().len(), 7);
    assert_eq!(
        bearer(&app, "GET", "/api/v1/targets", &token).await.status,
        StatusCode::UNAUTHORIZED
    );
    let req = axum::http::Request::post("/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {api}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_updates","arguments":{}}}"#,
        ))
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("ww-test-stack"), "{}", res.body);
}
