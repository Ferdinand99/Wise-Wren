//! Wren in-process: the kit's pages in Wren's names, servers and agent
//! tokens, heartbeats and status, the live pages, notification channels and
//! the API stubs.

mod common;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use common::{App, Web, api_token, bearer, next_event};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt as _;
use vole_kit::auth::TokenScope;

fn beat() -> Value {
    json!({
        "agent_version": "0.1.0",
        "hostname": "LANBox",
        "os": "Unraid OS 7.4 x86_64",
        "docker_version": "29.8.2",
        "docker_error": "",
        "unraid": true,
        "something_newer": [1, 2]
    })
}

/// Words from Vigilant Vole that must never show in Wren's pages.
fn assert_wren_branded(path: &str, body: &str) {
    for foreign in [
        "Vigilant", "Vole", "vv_", "VV_", "vva_", "Kit Demo", "Kestrel",
    ] {
        assert!(!body.contains(foreign), "{path}: contains {foreign:?}");
    }
}

#[tokio::test]
async fn setup_then_sign_in() {
    let app = App::new().await;
    let mut web = Web::anonymous(&app);
    assert_eq!(web.get("/").await.header("location"), "/setup");
    let page = web.get("/setup").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Wise Wren"), "{}", page.body);
    assert!(page.body.contains("Home lab updates…"));
    assert_wren_branded("/setup", &page.body);
    let res = web
        .post(
            "/setup",
            &[
                ("username", "admin"),
                ("password", common::PASSWORD),
                ("confirm", common::PASSWORD),
                ("site_name", ""),
                ("timezone", "Europe/Oslo"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(web.cookies.contains_key("ww_session"), "{:?}", web.cookies);
    assert!(web.cookies.keys().all(|k| k.starts_with("ww_")));
    let home = web.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body.contains("No servers yet"), "{}", home.body);
    assert!(home.body.contains("/static/wren.css"));

    // Signed out, sign-in works with the password.
    let mut other = Web::anonymous(&app);
    let login = other.get("/login").await;
    assert!(login.body.contains("Log in to see your servers"));
    let res = other
        .post(
            "/login",
            &[("username", "admin"), ("password", common::PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(other.cookies.contains_key("ww_session"));
}

#[tokio::test]
async fn pages_name_wren_not_vole() {
    let app = App::new().await;
    let mut web = Web::new(&app, "admin", "admin").await;
    for path in [
        "/",
        "/servers",
        "/servers/new",
        "/settings/general",
        "/settings/users",
        "/settings/security",
        "/settings/tokens",
        "/settings/notifications",
        "/api-docs",
        "/api-docs.md",
        "/llms.txt",
    ] {
        let res = web.get(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}: {}", res.body);
        assert_wren_branded(path, &res.body);
        if !path.starts_with("/api-docs.md") && path != "/llms.txt" {
            assert!(res.body.contains("Wise Wren"), "{path}");
        }
    }
    assert_eq!(
        web.get("/settings").await.header("location"),
        "/settings/general"
    );
    // The nav: Overview, Servers, Settings.
    let home = web.get("/").await;
    for href in ["href=\"/\"", "href=\"/servers\"", "href=\"/settings\""] {
        assert!(home.body.contains(href), "{href}");
    }
}

#[tokio::test]
async fn healthz_and_static() {
    let app = App::new().await;
    let mut web = Web::anonymous(&app);
    let res = web.get("/healthz").await;
    assert_eq!(res.status, StatusCode::OK);
    for path in [
        "/static/style.css",
        "/static/wren.css",
        "/static/favicon.svg",
        "/static/site.webmanifest",
    ] {
        assert_eq!(web.get(path).await.status, StatusCode::OK, "{path}");
    }
    let css = web.get("/static/wren.css").await;
    assert!(css.body.contains("#ff6482"));
}

#[tokio::test]
async fn admin_adds_a_server_and_sees_the_token_once() {
    let app = App::new().await;
    let mut web = Web::new(&app, "admin", "admin").await;
    let res = web.post("/servers", &[("name", "LANBox")]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let at = res.body.find("wwa_").expect("the token is shown");
    let token: String = res.body[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    assert_eq!(token.len(), 4 + 43, "{token}");
    assert!(res.body.contains(&format!("-e WW_AGENT_TOKEN={token}")));
    assert!(res.body.contains("-e WW_SERVER_URL=http://wren.test:3003"));
    assert!(
        res.body
            .contains("-v /var/run/docker.sock:/var/run/docker.sock")
    );
    assert!(res.body.contains("Waiting for the agent"));

    // Only its hash is stored, and the page never shows it again.
    let stored = app
        .count(
            "SELECT COUNT(*) FROM servers WHERE token_hash IS NOT NULL AND length(token_hash) = 64",
        )
        .await;
    assert_eq!(stored, 1);
    let hash = vole_kit::auth::tokens::hash(&token);
    let found = app
        .wren
        .core
        .with_conn(move |c| wise_wren::db::by_token_hash(c, &hash))
        .await
        .unwrap();
    let (id, name) = found.unwrap();
    assert_eq!(name, "LANBox");
    let again = web.get(&format!("/servers/{id}")).await;
    assert_eq!(again.status, StatusCode::OK);
    assert!(!again.body.contains(&token));
    assert!(again.body.contains(&token[..10]), "the visible start");

    // A new token replaces the old one at once.
    let fresh = web
        .post_with(&format!("/servers/{id}/token"), &[], true)
        .await;
    assert_eq!(fresh.status, StatusCode::OK);
    assert!(fresh.body.contains("id=\"agent-card\""));
    assert!(!fresh.body.contains(&token));
    assert_eq!(
        app.heartbeat(Some(&token), &beat()).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn viewers_and_editors_cannot_manage_servers() {
    let app = App::new().await;
    let (id, _) = app.add_server("Tower").await;
    for role in ["viewer", "editor"] {
        let name = if role == "viewer" { "vic" } else { "eddie" };
        let mut web = Web::new(&app, name, role).await;
        assert_eq!(web.get("/servers").await.status, StatusCode::OK);
        let page = web.get(&format!("/servers/{id}")).await;
        assert_eq!(page.status, StatusCode::OK);
        assert!(!page.body.contains("/token\""), "{role} sees token actions");
        assert_eq!(web.get("/servers/new").await.status, StatusCode::FORBIDDEN);
        for path in [
            "/servers".to_string(),
            format!("/servers/{id}/token"),
            format!("/servers/{id}/revoke"),
        ] {
            let res = web.post_with(&path, &[("name", "X")], true).await;
            assert_eq!(res.status, StatusCode::FORBIDDEN, "{role} {path}");
        }
        let res = web.delete(&format!("/servers/{id}")).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN);
    }
    assert_eq!(app.count("SELECT COUNT(*) FROM servers").await, 1);
}

#[tokio::test]
async fn duplicate_and_empty_names() {
    let app = App::new().await;
    app.add_server("Tower").await;
    let mut web = Web::new(&app, "admin", "admin").await;
    let res = web.post("/servers", &[("name", "tower")]).await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        res.body
            .contains("There is already a server with this name.")
    );
    let res = web.post("/servers", &[("name", "  ")]).await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(res.body.contains("Enter a name."));
    assert_eq!(app.count("SELECT COUNT(*) FROM servers").await, 1);
}

#[tokio::test]
async fn revoke_and_delete() {
    let app = App::new().await;
    let (id, token) = app.add_server("Tower").await;
    let mut web = Web::new(&app, "admin", "admin").await;
    let res = web
        .post_with(&format!("/servers/{id}/revoke"), &[], true)
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.header("hx-redirect"), format!("/servers/{id}"));
    assert_eq!(
        app.heartbeat(Some(&token), &beat()).await.status,
        StatusCode::UNAUTHORIZED
    );
    let page = web.get(&format!("/servers/{id}")).await;
    assert!(page.body.contains("No token"), "{}", page.body);
    let res = web.delete(&format!("/servers/{id}")).await;
    assert_eq!(res.header("hx-redirect"), "/servers");
    assert_eq!(app.count("SELECT COUNT(*) FROM servers").await, 0);
}

#[tokio::test]
async fn heartbeat_marks_online_and_records_facts() {
    let app = App::new().await;
    let (id, token) = app.add_server("LANBox").await;
    let res = app.heartbeat(Some(&token), &beat()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.json(),
        json!({"server": {"id": id, "name": "LANBox"}, "interval_seconds": 30})
    );
    // Machine headers on the agent endpoint.
    assert_eq!(
        res.header("content-security-policy"),
        "default-src 'none'; frame-ancestors 'none'"
    );
    let mut web = Web::new(&app, "vic", "viewer").await;
    let page = web.get(&format!("/servers/{id}")).await;
    for fact in [
        "Online",
        "LANBox",
        "Unraid OS 7.4 x86_64",
        "29.8.2",
        "0.1.0",
        "Unraid",
    ] {
        assert!(page.body.contains(fact), "{fact}");
    }
    let overview = web.get("/").await;
    assert!(overview.body.contains("data-status=\"online\""));
    let docker_down = json!({"agent_version": "0.1.0", "docker_error": "can't open /var/run/docker.sock: denied"});
    assert_eq!(
        app.heartbeat(Some(&token), &docker_down).await.status,
        StatusCode::OK
    );
    let page = web.get(&format!("/servers/{id}")).await;
    assert!(page.body.contains("Not reachable: can"), "{}", page.body);
}

#[tokio::test]
async fn bad_tokens_are_refused() {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    let admin = common::add_user(&app, "admin", "admin").await;
    let api = api_token(&app, admin, TokenScope::Write).await;
    let unknown = wise_wren::db::new_agent_token().token;
    for t in [
        None,
        Some(api.as_str()),
        Some(unknown.as_str()),
        Some("wwa_short"),
    ] {
        let res = app.heartbeat(t, &beat()).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{t:?}");
    }
    // A body that isn't a heartbeat.
    let req = Request::post("/api/agent/v1/heartbeat")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("nope"))
        .unwrap();
    assert_eq!(app.send(req).await.status, StatusCode::BAD_REQUEST);
    // Agent tokens can't use the API, and API tokens can.
    assert_eq!(
        bearer(&app, "GET", "/api/v1/servers", &token).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.count("SELECT COUNT(*) FROM servers WHERE last_seen_at IS NOT NULL")
            .await,
        0
    );
}

#[tokio::test]
async fn heartbeats_are_rate_limited() {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    for _ in 0..20 {
        assert_eq!(
            app.heartbeat(Some(&token), &beat()).await.status,
            StatusCode::OK
        );
    }
    let res = app.heartbeat(Some(&token), &beat()).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!res.header("retry-after").is_empty());
    // Another server has its own budget.
    let (_, other) = app.add_server("LANBox").await;
    assert_eq!(
        app.heartbeat(Some(&other), &beat()).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn stale_heartbeat_is_offline() {
    let app = App::new().await;
    let (id, token) = app.add_server("Tower").await;
    app.heartbeat(Some(&token), &beat()).await;
    let mut last = None;
    assert_eq!(
        wise_wren::servers::check_online(&app.wren, &mut last).await,
        Some(false)
    );
    app.exec(format!(
        "UPDATE servers SET last_seen_at = datetime('now', '-91 seconds') WHERE id = {id}"
    ))
    .await;
    // The watcher notices the agent went quiet.
    assert_eq!(
        wise_wren::servers::check_online(&app.wren, &mut last).await,
        Some(true)
    );
    let mut web = Web::new(&app, "vic", "viewer").await;
    let page = web.get(&format!("/servers/{id}")).await;
    assert!(
        page.body.contains("data-status=\"offline\""),
        "{}",
        page.body
    );
    let api = api_token(&app, 1, TokenScope::Read).await;
    let list = bearer(&app, "GET", "/api/v1/servers", &api).await;
    assert_eq!(list.json()["servers"][0]["status"], "offline");
}

#[tokio::test]
async fn live_view_pushes_on_heartbeat() {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    let web = Web::new(&app, "vic", "viewer").await;
    for (path, event) in [
        ("/servers/live", "servers-updated"),
        ("/live", "overview-updated"),
    ] {
        let mut req = Request::get(path)
            .header(header::HOST, common::HOST)
            .header(header::COOKIE, web.cookie_header())
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        ));
        let res = app.router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut body = res.into_body().into_data_stream();
        assert_eq!(
            app.heartbeat(Some(&token), &beat()).await.status,
            StatusCode::OK
        );
        let got = next_event(&mut body).await;
        assert!(got.starts_with(&format!("event: {event}\n")), "{got}");
        assert!(got.contains("data-status=\"online\""), "{got}");
    }
}

#[tokio::test]
async fn channel_add_edit_delete() {
    let app = App::new().await;
    let mut web = Web::new(&app, "eddie", "editor").await;
    let form = web.get("/settings/notifications/form?kind=webhook").await;
    assert_eq!(form.status, StatusCode::OK);
    let res = web
        .post_with(
            "/settings/notifications",
            &[
                ("kind", "webhook"),
                ("name", "Ops hook"),
                ("c_url", "https://hooks.example.com/SECRET"),
                ("c_method", "POST"),
            ],
            true,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("Ops hook"), "{}", res.body);
    let id = app.count("SELECT id FROM notification_channels").await;
    let edit = web.get(&format!("/settings/notifications/{id}/edit")).await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(!edit.body.contains("SECRET"), "secrets stay masked");
    let res = web.delete(&format!("/settings/notifications/{id}")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM notification_channels")
            .await,
        0
    );
}

#[tokio::test]
async fn viewers_cannot_open_channels() {
    let app = App::new().await;
    let mut web = Web::new(&app, "vic", "viewer").await;
    assert_eq!(
        web.get("/settings/notifications").await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn api_lists_servers_with_a_token() {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    app.heartbeat(Some(&token), &beat()).await;
    let admin = common::add_user(&app, "admin", "admin").await;
    let api = api_token(&app, admin, TokenScope::Read).await;
    let res = bearer(&app, "GET", "/api/v1/servers", &api).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let s = &res.json()["servers"][0];
    assert_eq!(s["name"], "Tower");
    assert_eq!(s["status"], "online");
    assert_eq!(s["docker_version"], "29.8.2");
    assert_eq!(s["unraid"], true);
    assert!(s.get("token_hash").is_none());
}

#[tokio::test]
async fn openapi_and_mcp_say_wren() {
    let app = App::new().await;
    let admin = common::add_user(&app, "admin", "admin").await;
    let api = api_token(&app, admin, TokenScope::Write).await;
    let doc = bearer(&app, "GET", "/api/v1/openapi.json", &api).await;
    assert_eq!(doc.status, StatusCode::OK);
    let doc = doc.json();
    assert_eq!(doc["info"]["title"], "Wise Wren API");
    assert_eq!(doc["info"]["version"], wise_wren::VERSION);
    assert!(doc["components"]["securitySchemes"]["agentToken"].is_object());
    let req = Request::post("/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {api}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        ))
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("\"name\":\"wren\""), "{}", res.body);
    assert!(!res.body.contains("Vole"));
}
