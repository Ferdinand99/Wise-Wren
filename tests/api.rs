//! REST v1 and MCP cover what the web UI does (design §9), with the pages'
//! roles, write tokens only for writes and no secret in any answer (§8).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{App, Web, api_token, bearer};
use serde_json::{Value, json};
use vole_kit::auth::TokenScope;

async fn call(app: &App, method: &str, path: &str, token: &str, body: Value) -> common::Res {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.send(req).await
}

async fn token(app: &App, name: &'static str, role: &'static str, scope: TokenScope) -> String {
    let id = common::add_user(app, name, role).await;
    api_token(app, id, scope).await
}

/// A server with one container target; the target's id.
async fn with_target(app: &App) -> (i64, i64, String) {
    let (server, agent) = app.add_server("LANBox").await;
    let inv = json!({"containers": [{"name": "ww-test-a", "image": "localhost:5056/ww-test/app:latest",
        "image_id": "sha256:1", "repo_digests": [], "state": "running", "labels": {}}]});
    let res = call(app, "POST", "/api/agent/v1/inventory", &agent, inv).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let id = app
        .count("SELECT id FROM targets WHERE name = 'ww-test-a'")
        .await;
    (server, id, agent)
}

#[tokio::test]
async fn api_servers_tokens_and_roles() {
    let app = App::new().await;
    let admin = token(&app, "admin", "admin", TokenScope::Write).await;
    let editor = token(&app, "editor", "editor", TokenScope::Write).await;
    let beat = json!({"agent_version": "0.1.0"});

    let res = call(
        &app,
        "POST",
        "/api/v1/servers",
        &editor,
        json!({"name": "Tower"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = call(
        &app,
        "POST",
        "/api/v1/servers",
        &admin,
        json!({"name": "Tower"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let v = res.json();
    let (id, first) = (
        v["server_id"].as_i64().unwrap(),
        v["token"].as_str().unwrap().to_string(),
    );
    assert!(first.starts_with("wwa_"));
    assert!(v["docker_run"]["unraid"].as_str().unwrap().contains(&first));
    let res = call(
        &app,
        "POST",
        "/api/v1/servers",
        &admin,
        json!({"name": "Tower"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "name taken");
    assert_eq!(
        app.heartbeat(Some(&first), &beat).await.status,
        StatusCode::OK
    );

    // A new token: the old one stops at once.
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/servers/{id}/token"),
        &admin,
        json!({}),
    )
    .await;
    let second = res.json()["token"].as_str().unwrap().to_string();
    assert_eq!(
        app.heartbeat(Some(&first), &beat).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.heartbeat(Some(&second), &beat).await.status,
        StatusCode::OK
    );
    // Nobody reads a token back.
    let list = bearer(&app, "GET", "/api/v1/servers", &editor).await;
    assert!(!list.body.contains(&second[4..]), "{}", list.body);
    assert_eq!(list.json()["servers"][0]["mode"], "approve");

    let res = call(
        &app,
        "POST",
        &format!("/api/v1/servers/{id}/revoke"),
        &editor,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/servers/{id}/revoke"),
        &admin,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        app.heartbeat(Some(&second), &beat).await.status,
        StatusCode::UNAUTHORIZED
    );

    let res = call(
        &app,
        "DELETE",
        &format!("/api/v1/servers/{id}"),
        &admin,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(app.count("SELECT COUNT(*) FROM servers").await, 0);
}

#[tokio::test]
async fn api_writes_refuse_session_cookies_and_read_tokens() {
    let app = App::new().await;
    let mut web = Web::new(&app, "owner", "admin").await;
    // Same-origin and htmx: still no write with a cookie on /api/v1.
    let req = Request::post("/api/v1/servers")
        .header(header::ORIGIN, format!("http://{}", common::HOST))
        .header("hx-request", "true")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"name":"Tower"}"#))
        .unwrap();
    assert_eq!(web.send(req).await.status, StatusCode::FORBIDDEN);
    let req = Request::post("/api/v1/registries")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"host":"ghcr.io","username":"u","password":"p"}"#,
        ))
        .unwrap();
    assert_eq!(web.send(req).await.status, StatusCode::FORBIDDEN);
    // Reads with the cookie are fine.
    assert_eq!(web.get("/api/v1/servers").await.status, StatusCode::OK);
    // An admin's read token can't write either.
    let read = token(&app, "reader", "admin", TokenScope::Read).await;
    let res = call(&app, "POST", "/api/v1/servers", &read, json!({"name": "x"})).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(app.count("SELECT COUNT(*) FROM servers").await, 0);
}

#[tokio::test]
async fn api_registries_and_vv_keep_secrets() {
    let app = App::new().await;
    let admin = token(&app, "admin", "admin", TokenScope::Write).await;
    let editor = token(&app, "editor", "editor", TokenScope::Write).await;
    let res = call(
        &app,
        "POST",
        "/api/v1/registries",
        &admin,
        json!({"host": "https://GHCR.io/", "username": "owner", "password": "s3cret-pass"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let id = res.json()["id"].as_i64().unwrap();
    assert_eq!(res.json()["host"], "ghcr.io");
    let list = bearer(&app, "GET", "/api/v1/registries", &admin).await;
    assert!(!list.body.contains("s3cret"), "{}", list.body);
    assert_eq!(list.json()["registries"][0]["has_password"], true);
    assert_eq!(
        bearer(&app, "GET", "/api/v1/registries", &editor)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // Changing the user name keeps the password.
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/registries/{id}"),
        &admin,
        json!({"username": "me"}),
    )
    .await;
    assert_eq!(res.json()["username"], "me");
    let creds = wise_wren::registries::credentials(&app.wren.core).await;
    assert_eq!(
        creds["ghcr.io"],
        ("me".to_string(), "s3cret-pass".to_string())
    );
    let res = call(
        &app,
        "DELETE",
        &format!("/api/v1/registries/{id}"),
        &admin,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Vigilant Vole: the token is write-only.
    let res = call(
        &app,
        "POST",
        "/api/v1/vigilant-vole",
        &admin,
        json!({"url": "http://127.0.0.1:9", "token": "vv_topsecret"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let got = bearer(&app, "GET", "/api/v1/vigilant-vole", &admin).await;
    assert!(!got.body.contains("topsecret"), "{}", got.body);
    assert_eq!(got.json()["connected"], true);
    let res = call(
        &app,
        "POST",
        "/api/v1/vigilant-vole",
        &admin,
        json!({"url": "http://127.0.0.1:10"}),
    )
    .await;
    assert_eq!(
        res.json()["token_saved"],
        true,
        "a missing token keeps the saved one"
    );
    let test = call(
        &app,
        "POST",
        "/api/v1/vigilant-vole/test",
        &admin,
        json!({}),
    )
    .await;
    assert_eq!(test.json()["ok"], false);
    assert!(!test.body.contains("topsecret"));
    let res = call(&app, "DELETE", "/api/v1/vigilant-vole", &editor, json!({})).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = call(&app, "DELETE", "/api/v1/vigilant-vole", &admin, json!({})).await;
    assert_eq!(res.json()["connected"], false);
}

#[tokio::test]
async fn api_targets_checks_intervals_links() {
    let app = App::new().await;
    let admin = token(&app, "admin", "admin", TokenScope::Write).await;
    let editor = token(&app, "editor", "editor", TokenScope::Write).await;
    let viewer = token(&app, "viewer", "viewer", TokenScope::Write).await;
    let (server, target, _) = with_target(&app).await;

    let res = call(
        &app,
        "POST",
        &format!("/api/v1/servers/{server}/check"),
        &viewer,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/targets/{target}/check"),
        &editor,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM targets WHERE check_requested = 1")
            .await,
        1
    );
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/servers/{server}/interval"),
        &editor,
        json!({"hours": 12}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/targets/{target}/interval"),
        &editor,
        json!({"hours": 200}),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/targets/{target}/interval"),
        &editor,
        json!({"hours": 24}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let t = bearer(&app, "GET", &format!("/api/v1/targets/{target}"), &viewer)
        .await
        .json();
    assert_eq!(t["check_interval_hours"], 24);
    assert_eq!(t["effective_mode"], "approve");

    // Skip, then show again.
    call(
        &app,
        "POST",
        &format!("/api/v1/targets/{target}/skip"),
        &editor,
        json!({}),
    )
    .await;
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/targets/{target}/resume"),
        &editor,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        bearer(&app, "GET", &format!("/api/v1/targets/{target}"), &viewer)
            .await
            .json()["skipped"],
        false
    );

    // Monitor links: admins.
    let link = json!({"tag": "media"});
    let path = format!("/api/v1/targets/{target}/monitors");
    assert_eq!(
        call(&app, "POST", &path, &editor, link.clone())
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let res = call(&app, "POST", &path, &admin, link).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let link_id = res.json()["monitors"][0]["id"].as_i64().unwrap();
    let t = bearer(&app, "GET", &format!("/api/v1/targets/{target}"), &viewer)
        .await
        .json();
    assert_eq!(t["monitors"][0]["ref"], "media");
    let res = call(
        &app,
        "DELETE",
        &format!("{path}/{link_id}"),
        &admin,
        json!({}),
    )
    .await;
    assert_eq!(res.json()["monitors"], json!([]));

    // A rollback of something that isn't a run.
    let res = call(
        &app,
        "POST",
        "/api/v1/runs/999/rollback",
        &editor,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_notification_channels_redacted() {
    let app = App::new().await;
    let editor = token(&app, "editor", "editor", TokenScope::Write).await;
    let viewer = token(&app, "viewer", "viewer", TokenScope::Read).await;
    let body = json!({"kind": "ntfy", "name": "Phone",
        "config": {"server": "https://ntfy.example.com", "topic": "wren", "access_token": "tk_secret123"}});
    let res = call(&app, "POST", "/api/v1/notification-channels", &editor, body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(!res.body.contains("tk_secret123"), "{}", res.body);
    let id = res.json()["id"].as_i64().unwrap();
    assert_eq!(res.json()["config"]["server"], "https://ntfy.example.com");
    let typo = json!({"kind": "ntfy", "name": "x", "config": {"topic": "wren", "server_url": "https://a.example"}});
    let res = call(&app, "POST", "/api/v1/notification-channels", &editor, typo).await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "unknown settings are refused"
    );
    let list = bearer(&app, "GET", "/api/v1/notification-channels", &editor).await;
    assert!(!list.body.contains("tk_secret123"), "{}", list.body);
    assert_eq!(
        bearer(&app, "GET", "/api/v1/notification-channels", &viewer)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // A rename keeps the secret; a new destination needs it again.
    let path = format!("/api/v1/notification-channels/{id}/update");
    let res = call(&app, "POST", &path, &editor, json!({"name": "Phone 2"})).await;
    assert_eq!(res.json()["name"], "Phone 2");
    let res = call(
        &app,
        "POST",
        &path,
        &editor,
        json!({"config": {"server": "https://evil.example", "topic": "wren"}}),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let stored = app
        .wren
        .core
        .with_conn(|c| {
            c.query_row("SELECT config FROM notification_channels", [], |r| {
                r.get::<_, String>(0)
            })
        })
        .await
        .unwrap();
    assert!(stored.contains("tk_secret123") && stored.contains("ntfy.example.com"));
    let res = call(
        &app,
        "POST",
        &format!("/api/v1/notification-channels/{id}/active"),
        &editor,
        json!({"active": false}),
    )
    .await;
    assert_eq!(res.json()["active"], false);
    let providers = bearer(&app, "GET", "/api/v1/notification-providers", &editor).await;
    assert!(providers.body.contains("\"ntfy\""));
    let res = call(
        &app,
        "DELETE",
        &format!("/api/v1/notification-channels/{id}"),
        &editor,
        json!({}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
}

async fn mcp(app: &App, token: &str, method: &str, params: Value) -> Value {
    let req = Request::post("/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
        ))
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.json()
}

async fn tool_names(app: &App, token: &str) -> Vec<String> {
    mcp(app, token, "tools/list", json!({})).await["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn mcp_tools_by_role() {
    let app = App::new().await;
    let (_, target, _) = with_target(&app).await;
    let viewer = token(&app, "viewer", "viewer", TokenScope::Read).await;
    let editor = token(&app, "editor", "editor", TokenScope::Write).await;
    let admin = token(&app, "admin", "admin", TokenScope::Write).await;
    let v = tool_names(&app, &viewer).await;
    assert!(
        v.contains(&"get_target".into()) && !v.contains(&"check_now".into()),
        "{v:?}"
    );
    let e = tool_names(&app, &editor).await;
    assert!(
        e.contains(&"rollback_run".into()) && !e.contains(&"link_monitor".into()),
        "{e:?}"
    );
    let a = tool_names(&app, &admin).await;
    assert!(a.contains(&"link_monitor".into()), "{a:?}");
    for secret_tool in ["add_server", "agent_token", "registries", "notification"] {
        assert!(!a.iter().any(|n| n.contains(secret_tool)), "{a:?}");
    }
    // A viewer can't act through MCP even by naming the tool.
    let res = mcp(
        &app,
        &viewer,
        "tools/call",
        json!({"name": "check_now", "arguments": {"target_id": target}}),
    )
    .await;
    assert!(res.to_string().contains("needs a write token"), "{res}");
    let res = mcp(
        &app,
        &editor,
        "tools/call",
        json!({"name": "check_now", "arguments": {"target_id": target}}),
    )
    .await;
    assert_eq!(
        res["result"]["structuredContent"]["checking"], true,
        "{res}"
    );
    let res = mcp(
        &app,
        &viewer,
        "tools/call",
        json!({"name": "get_target", "arguments": {"target_id": target}}),
    )
    .await;
    assert_eq!(
        res["result"]["structuredContent"]["name"], "ww-test-a",
        "{res}"
    );
}

/// Every route the API serves is in the reference (and so in OpenAPI), and
/// the reference names nothing it doesn't serve.
#[test]
fn every_route_is_documented() {
    const SERVED: &[(&str, &str)] = &[
        ("GET", "/api/v1/servers"),
        ("POST", "/api/v1/servers"),
        ("DELETE", "/api/v1/servers/{id}"),
        ("POST", "/api/v1/servers/{id}/token"),
        ("POST", "/api/v1/servers/{id}/revoke"),
        ("POST", "/api/v1/servers/{id}/check"),
        ("POST", "/api/v1/servers/{id}/interval"),
        ("POST", "/api/v1/servers/{id}/mode"),
        ("GET", "/api/v1/targets"),
        ("GET", "/api/v1/targets/{id}"),
        ("POST", "/api/v1/targets/{id}/approve"),
        ("POST", "/api/v1/targets/{id}/schedule"),
        ("POST", "/api/v1/targets/{id}/skip"),
        ("POST", "/api/v1/targets/{id}/snooze"),
        ("POST", "/api/v1/targets/{id}/resume"),
        ("POST", "/api/v1/targets/{id}/mode"),
        ("POST", "/api/v1/targets/{id}/check"),
        ("POST", "/api/v1/targets/{id}/interval"),
        ("POST", "/api/v1/targets/{id}/monitors"),
        ("DELETE", "/api/v1/targets/{id}/monitors/{link}"),
        ("GET", "/api/v1/updates"),
        ("GET", "/api/v1/runs"),
        ("GET", "/api/v1/runs/{id}"),
        ("POST", "/api/v1/runs/{id}/reschedule"),
        ("POST", "/api/v1/runs/{id}/cancel"),
        ("POST", "/api/v1/runs/{id}/rollback"),
        ("GET", "/api/v1/schedules"),
        ("GET", "/api/v1/registries"),
        ("POST", "/api/v1/registries"),
        ("POST", "/api/v1/registries/{id}"),
        ("DELETE", "/api/v1/registries/{id}"),
        ("GET", "/api/v1/vigilant-vole"),
        ("POST", "/api/v1/vigilant-vole"),
        ("DELETE", "/api/v1/vigilant-vole"),
        ("POST", "/api/v1/vigilant-vole/test"),
        ("GET", "/api/v1/vigilant-vole/monitors"),
        ("GET", "/api/v1/notification-channels"),
        ("POST", "/api/v1/notification-channels"),
        ("GET", "/api/v1/notification-providers"),
        ("GET", "/api/v1/notification-channels/{id}"),
        ("DELETE", "/api/v1/notification-channels/{id}"),
        ("POST", "/api/v1/notification-channels/{id}/update"),
        ("POST", "/api/v1/notification-channels/{id}/active"),
        ("POST", "/api/v1/notification-channels/{id}/test"),
    ];
    let documented: Vec<(&str, &str)> = wise_wren::api::DOCS
        .routes
        .iter()
        .filter(|r| r.path.starts_with("/api/v1/"))
        .map(|r| (r.method, r.path))
        .collect();
    for route in SERVED {
        assert!(documented.contains(route), "{route:?} is not documented");
    }
    for route in &documented {
        assert!(
            SERVED.contains(route),
            "{route:?} is documented but not served"
        );
    }
}
