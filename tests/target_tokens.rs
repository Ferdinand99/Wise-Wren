//! Target tokens: an API token that may read and approve one target and
//! nothing else (src/target_tokens.rs). Every allowed route works for the
//! token's own target; every other route, other targets and other targets'
//! runs are refused, even for a user who may do more.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{App, Web, add_user, api_token, bearer, post_json, set_remote};
use serde_json::{Value, json};
use vole_kit::auth::{TokenScope, tokens};
use wise_wren::target_tokens::SCOPE_NAME;

const OLD: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn container(name: &str, image: &str) -> Value {
    let repo = image.rsplit_once(':').map_or(image, |(r, _)| r);
    json!({
        "name": name, "image": image, "image_id": "sha256:1",
        "repo_digests": [format!("{repo}@{OLD}")], "state": "running", "labels": {},
    })
}

struct World {
    app: App,
    /// The three targets, each with an update.
    a: i64,
    b: i64,
    c: i64,
    /// Their version keys.
    key_a: String,
    key_b: String,
    key_c: String,
    /// An admin's write token, for setting things up.
    admin: String,
}

async fn world() -> World {
    let app = App::new().await;
    let (_, agent) = app.add_server("LANBox").await;
    let inv = json!({"containers": [
        container("ww-test-a", "localhost:5056/ww-test/a:latest"),
        container("ww-test-b", "localhost:5056/ww-test/b:latest"),
        container("ww-test-c", "localhost:5056/ww-test/c:latest"),
    ]});
    let res = post_json(&app, "/api/agent/v1/inventory", &agent, inv).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    for image in [
        "localhost:5056/ww-test/a:latest",
        "localhost:5056/ww-test/b:latest",
        "localhost:5056/ww-test/c:latest",
    ] {
        set_remote(&app, image, NEW).await;
    }
    let id = |name: &'static str| {
        let app = &app;
        async move {
            app.count_sql(format!("SELECT id FROM targets WHERE name = '{name}'"))
                .await
        }
    };
    let (a, b, c) = (
        id("ww-test-a").await,
        id("ww-test-b").await,
        id("ww-test-c").await,
    );
    let admin_id = add_user(&app, "admin", "admin").await;
    let admin = api_token(&app, admin_id, TokenScope::Write).await;
    let mut keys = Vec::new();
    for t in [a, b, c] {
        let res = bearer(&app, "GET", &format!("/api/v1/targets/{t}"), &admin).await;
        keys.push(res.json()["version_key"].as_str().unwrap().to_string());
    }
    let (key_c, key_b, key_a) = (
        keys.pop().unwrap(),
        keys.pop().unwrap(),
        keys.pop().unwrap(),
    );
    World {
        app,
        a,
        b,
        c,
        key_a,
        key_b,
        key_c,
        admin,
    }
}

/// A target token of `user_id` for `target_id`, as the Target tokens page
/// makes it.
async fn target_token(app: &App, user_id: i64, target_id: i64, name: &'static str) -> String {
    let token = tokens::generate();
    let (hash, prefix) = (tokens::hash(&token), tokens::display_prefix(&token));
    app.wren
        .core
        .with_conn(move |c| {
            let id = vole_kit::db::api_tokens::insert(
                c,
                user_id,
                name,
                &prefix,
                &hash,
                TokenScope::Read,
                None,
            )?;
            vole_kit::db::api_tokens::add_scope(c, id, SCOPE_NAME)?;
            c.execute(
                "INSERT INTO api_token_targets (token_id, target_id) VALUES (?1, ?2)",
                rusqlite::params![id, target_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    token
}

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

#[tokio::test]
async fn every_allowed_route_works_for_the_tokens_own_target() {
    let w = world().await;
    let editor = add_user(&w.app, "ed", "editor").await;
    let token = target_token(&w.app, editor, w.a, "Sylo Update now").await;

    // The target itself.
    let res = bearer(&w.app, "GET", &format!("/api/v1/targets/{}", w.a), &token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.json()["name"], "ww-test-a");
    assert_eq!(res.json()["version_key"], w.key_a);

    // The updates: its own target only, though all three have one.
    let all = bearer(&w.app, "GET", "/api/v1/updates", &w.admin).await;
    assert_eq!(all.json()["targets"].as_array().unwrap().len(), 3);
    let res = bearer(&w.app, "GET", "/api/v1/updates", &token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let list = res.json();
    let list = list["targets"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["id"], w.a);

    // Approve (for the version it saw).
    let res = call(
        &w.app,
        "POST",
        &format!("/api/v1/targets/{}/approve", w.a),
        &token,
        json!({"version_key": w.key_a}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let run = res.json()["run_id"].as_i64().unwrap();
    assert_eq!(res.json()["status"], "queued");

    // The log and the run name the token.
    let who = r#"ed via token "Sylo Update now""#;
    assert_eq!(
        w.app
            .count_sql(format!(
                "SELECT COUNT(*) FROM actions WHERE action = 'approve' AND target_id = {} AND username = '{}'",
                w.a,
                who.replace('\'', "''")
            ))
            .await,
        1
    );
    let res = bearer(&w.app, "GET", &format!("/api/v1/runs/{run}"), &token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.json()["requested_by"], who);

    // Schedule another target's worth: its own, later.
    let token_c = target_token(&w.app, editor, w.c, "Sylo C").await;
    let soon = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_hours(24))
        .unwrap()
        .to_string();
    let res = call(
        &w.app,
        "POST",
        &format!("/api/v1/targets/{}/schedule", w.c),
        &token_c,
        json!({"version_key": w.key_c, "at": soon}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.json()["scheduled_for"].is_string());
    let run_c = res.json()["run_id"].as_i64().unwrap();
    assert_eq!(
        w.app
            .count_sql(format!(
                "SELECT COUNT(*) FROM actions WHERE action = 'schedule' AND username = 'ed via token \"Sylo C\"' AND run_id = {run_c}"
            ))
            .await,
        1
    );
}

#[tokio::test]
async fn everything_else_is_refused() {
    let w = world().await;
    let editor = add_user(&w.app, "ed", "editor").await;
    let token = target_token(&w.app, editor, w.a, "Sylo").await;
    // A run of the other target, and one of this.
    let res = call(
        &w.app,
        "POST",
        &format!("/api/v1/targets/{}/approve", w.b),
        &w.admin,
        json!({"version_key": w.key_b}),
    )
    .await;
    let run_b = res.json()["run_id"].as_i64().unwrap();
    let res = call(
        &w.app,
        "POST",
        &format!("/api/v1/targets/{}/approve", w.a),
        &token,
        json!({"version_key": w.key_a}),
    )
    .await;
    let run_a = res.json()["run_id"].as_i64().unwrap();
    let runs_before = w.app.count("SELECT COUNT(*) FROM runs").await;

    let (a, b) = (w.a, w.b);
    let refused: Vec<(&str, String, Value)> = vec![
        // Other targets: reading, approving, scheduling.
        ("GET", format!("/api/v1/targets/{b}"), json!({})),
        (
            "POST",
            format!("/api/v1/targets/{b}/approve"),
            json!({"version_key": w.key_b}),
        ),
        (
            "POST",
            format!("/api/v1/targets/{b}/schedule"),
            json!({"version_key": w.key_b, "at": "2099-01-01T00:00:00Z"}),
        ),
        ("GET", "/api/v1/targets/999".into(), json!({})),
        // The lists.
        ("GET", "/api/v1/targets".into(), json!({})),
        ("GET", "/api/v1/runs".into(), json!({})),
        ("GET", "/api/v1/schedules".into(), json!({})),
        // Runs: another target's, one that doesn't exist, and every action.
        ("GET", format!("/api/v1/runs/{run_b}"), json!({})),
        ("GET", "/api/v1/runs/999".into(), json!({})),
        ("POST", format!("/api/v1/runs/{run_a}/cancel"), json!({})),
        ("POST", format!("/api/v1/runs/{run_a}/rollback"), json!({})),
        (
            "POST",
            format!("/api/v1/runs/{run_a}/reschedule"),
            json!({"at": "2099-01-01T00:00:00Z"}),
        ),
        // Its own target's other actions.
        ("POST", format!("/api/v1/targets/{a}/skip"), json!({})),
        (
            "POST",
            format!("/api/v1/targets/{a}/snooze"),
            json!({"hours": 1}),
        ),
        ("POST", format!("/api/v1/targets/{a}/resume"), json!({})),
        (
            "POST",
            format!("/api/v1/targets/{a}/mode"),
            json!({"mode": "notify"}),
        ),
        ("POST", format!("/api/v1/targets/{a}/check"), json!({})),
        (
            "POST",
            format!("/api/v1/targets/{a}/interval"),
            json!({"hours": 1}),
        ),
        (
            "POST",
            format!("/api/v1/targets/{a}/monitors"),
            json!({"tag": "x"}),
        ),
        (
            "DELETE",
            format!("/api/v1/targets/{a}/monitors/1"),
            json!({}),
        ),
        // Servers, settings, notifications, the API's own pages.
        ("GET", "/api/v1/servers".into(), json!({})),
        ("GET", "/api/v1/servers/1".into(), json!({})),
        ("POST", "/api/v1/servers".into(), json!({"name": "x"})),
        ("POST", "/api/v1/servers/1/token".into(), json!({})),
        ("POST", "/api/v1/servers/1/check".into(), json!({})),
        (
            "POST",
            "/api/v1/servers/1/mode".into(),
            json!({"mode": "approve"}),
        ),
        ("GET", "/api/v1/registries".into(), json!({})),
        ("GET", "/api/v1/vigilant-vole".into(), json!({})),
        ("GET", "/api/v1/notification-channels".into(), json!({})),
        ("GET", "/api/v1/notification-providers".into(), json!({})),
        ("GET", "/api/v1/openapi.json".into(), json!({})),
        ("POST", "/api/v1/push/devices".into(), json!({})),
        ("GET", "/api/v1/push/events/x".into(), json!({})),
        // Odd spellings of the allowed paths.
        ("GET", format!("/api/v1/targets/%3{a}"), json!({})),
        ("GET", format!("/api/v1/targets/+{a}"), json!({})),
    ];
    for (method, path, body) in refused {
        let res = call(&w.app, method, &path, &token, body).await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "{method} {path}: {}",
            res.body
        );
    }

    // Paths no route serves never reach a handler (405 or 404), the token or not.
    for (method, path) in [
        ("POST", format!("/api/v1/targets/{a}")),
        ("POST", "/api/v1/updates".to_string()),
        ("GET", format!("/api/v1/targets/{a}/")),
        ("GET", format!("/api/v1/runs/{run_a}/")),
    ] {
        let res = call(&w.app, method, &path, &token, json!({})).await;
        assert!(
            [
                StatusCode::NOT_FOUND,
                StatusCode::METHOD_NOT_ALLOWED,
                StatusCode::FORBIDDEN
            ]
            .contains(&res.status),
            "{method} {path}: {}",
            res.status
        );
    }

    // MCP is out of reach, too.
    let req = Request::post("/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}).to_string(),
        ))
        .unwrap();
    assert_eq!(w.app.send(req).await.status, StatusCode::FORBIDDEN);

    // Nothing changed: no new run, no skip, no mode.
    assert_eq!(w.app.count("SELECT COUNT(*) FROM runs").await, runs_before);
    let res = bearer(&w.app, "GET", &format!("/api/v1/targets/{a}"), &w.admin).await;
    assert_eq!(res.json()["skipped"], false);
    assert_eq!(res.json()["mode"], Value::Null);

    // The allowed run is still there for it, the other's isn't.
    let res = bearer(&w.app, "GET", &format!("/api/v1/runs/{run_a}"), &token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // A session cookie never works as a target token, and a plain read
    // token of the same user still reads everything (not a target token).
    let read = api_token(&w.app, editor, TokenScope::Read).await;
    assert_eq!(
        bearer(&w.app, "GET", &format!("/api/v1/targets/{b}"), &read)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn approving_needs_an_editor() {
    let w = world().await;
    let viewer = add_user(&w.app, "vi", "viewer").await;
    let token = target_token(&w.app, viewer, w.a, "Sylo").await;
    // It reads…
    let res = bearer(&w.app, "GET", &format!("/api/v1/targets/{}", w.a), &token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    // …but the user may not approve, so neither may it.
    let runs = w.app.count("SELECT COUNT(*) FROM runs").await;
    let res = call(
        &w.app,
        "POST",
        &format!("/api/v1/targets/{}/approve", w.a),
        &token,
        json!({"version_key": w.key_a}),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(w.app.count("SELECT COUNT(*) FROM runs").await, runs);

    // An editor who is demoted later loses the approval at once.
    let editor = add_user(&w.app, "ed", "editor").await;
    let token_c = target_token(&w.app, editor, w.c, "Sylo C").await;
    w.app
        .exec(format!(
            "UPDATE users SET role = 'viewer' WHERE id = {editor}"
        ))
        .await;
    let res = call(
        &w.app,
        "POST",
        &format!("/api/v1/targets/{}/approve", w.c),
        &token_c,
        json!({"version_key": w.key_c}),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // A token of an admin approves as an editor and no more (no mode change).
    let admin_id = w
        .app
        .count("SELECT id FROM users WHERE username = 'admin'")
        .await;
    let admin_token = target_token(&w.app, admin_id, w.b, "Admin's").await;
    let res = call(
        &w.app,
        "POST",
        &format!("/api/v1/targets/{}/mode", w.b),
        &admin_token,
        json!({"mode": "auto"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = call(
        &w.app,
        "POST",
        &format!("/api/v1/targets/{}/approve", w.b),
        &admin_token,
        json!({"version_key": w.key_b}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn a_token_without_a_target_is_refused_everywhere() {
    let w = world().await;
    let editor = add_user(&w.app, "ed", "editor").await;
    // What the plain API tokens page makes when someone picks the scope:
    // the scope, but no target.
    let token = tokens::generate();
    let (hash, prefix) = (tokens::hash(&token), tokens::display_prefix(&token));
    w.app
        .wren
        .core
        .with_conn(move |c| {
            let id = vole_kit::db::api_tokens::insert(
                c,
                editor,
                "no target",
                &prefix,
                &hash,
                TokenScope::Read,
                None,
            )?;
            vole_kit::db::api_tokens::add_scope(c, id, SCOPE_NAME)
        })
        .await
        .unwrap();
    for (method, path) in [
        ("GET", format!("/api/v1/targets/{}", w.a)),
        ("GET", "/api/v1/updates".to_string()),
        ("POST", format!("/api/v1/targets/{}/approve", w.a)),
    ] {
        let res = call(&w.app, method, &path, &token, json!({})).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{method} {path}");
    }

    // When the target goes, the token goes dead with it; revoking removes
    // the binding.
    let token = target_token(&w.app, editor, w.b, "b").await;
    assert_eq!(
        bearer(&w.app, "GET", &format!("/api/v1/targets/{}", w.b), &token)
            .await
            .status,
        StatusCode::OK
    );
    w.app
        .exec(format!("DELETE FROM targets WHERE id = {}", w.b))
        .await;
    assert_eq!(
        w.app.count("SELECT COUNT(*) FROM api_token_targets").await,
        0,
        "the binding goes with the target"
    );
    assert_eq!(
        bearer(&w.app, "GET", &format!("/api/v1/targets/{}", w.b), &token)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let token = target_token(&w.app, editor, w.a, "a").await;
    w.app.exec("DELETE FROM api_tokens".to_string()).await;
    assert_eq!(
        w.app.count("SELECT COUNT(*) FROM api_token_targets").await,
        0
    );
    assert_eq!(
        bearer(&w.app, "GET", &format!("/api/v1/targets/{}", w.a), &token)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_target_tokens_page_makes_revokes_and_hides_them() {
    let w = world().await;
    let mut ed = Web::new(&w.app, "ed", "editor").await;
    let mut other = Web::new(&w.app, "ed2", "editor").await;
    let mut viewer = Web::new(&w.app, "vi", "viewer").await;
    assert_eq!(
        viewer.get("/settings/target-tokens").await.status,
        StatusCode::FORBIDDEN
    );

    let page = ed.get("/settings/target-tokens").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    for name in ["ww-test-a", "ww-test-b", "ww-test-c"] {
        assert!(page.body.contains(name), "{name} is offered");
    }
    // Nothing to pick: refused, nothing made.
    for form in [
        vec![("name", "x"), ("target", ""), ("expires", "90")],
        vec![("name", "x"), ("target", "999"), ("expires", "90")],
        vec![("name", ""), ("target", "1"), ("expires", "90")],
        vec![("name", "x"), ("target", "1"), ("expires", "1")],
    ] {
        let res = ed.post("/settings/target-tokens", &form).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{form:?}");
    }
    assert_eq!(
        w.app.count("SELECT COUNT(*) FROM api_token_targets").await,
        0
    );
    let a = w.a.to_string();
    let res = viewer
        .post(
            "/settings/target-tokens",
            &[("name", "x"), ("target", &a), ("expires", "90")],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    let res = ed
        .post(
            "/settings/target-tokens",
            &[("name", "Sylo"), ("target", &a), ("expires", "90")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.header("cache-control"), "no-store");
    let token = res
        .body
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .find(|w| w.starts_with("ww_") && w.len() > 20)
        .expect("the token is shown once")
        .to_string();
    assert!(
        res.body
            .contains(&format!("/api/v1/targets/{}/approve", w.a))
    );
    // It is a target token for the picked target, read-only, expiring.
    assert_eq!(
        w.app
            .count_sql(format!(
                "SELECT COUNT(*) FROM api_token_targets b JOIN api_tokens t ON t.id = b.token_id
                 JOIN api_token_scopes s ON s.token_id = t.id
                 WHERE b.target_id = {} AND t.scope = 'read' AND t.expires_at IS NOT NULL
                   AND s.scope = 'target:approve'",
                w.a
            ))
            .await,
        1
    );
    let res = bearer(&w.app, "GET", &format!("/api/v1/targets/{}", w.a), &token).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = bearer(&w.app, "GET", &format!("/api/v1/targets/{}", w.b), &token).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // The list shows it to the user who made it (and the admin), not to another editor.
    let page = ed.get("/settings/target-tokens").await;
    assert!(page.body.contains("Sylo") && page.body.contains("LANBox / ww-test-a"));
    let page = other.get("/settings/target-tokens").await;
    assert!(
        !page.body.contains("Sylo"),
        "someone else's token is hidden"
    );
    let mut admin = Web::anonymous(&w.app);
    let res = admin
        .post(
            "/login",
            &[("username", "admin"), ("password", common::PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(
        admin
            .get("/settings/target-tokens")
            .await
            .body
            .contains("LANBox / ww-test-a")
    );
    // The API tokens page lists it with its scope's name.
    let page = ed.get("/settings/tokens").await;
    assert!(page.body.contains("Approve one target"), "{}", page.body);

    // Revoking (the API tokens page's button) kills it.
    let id = w
        .app
        .count("SELECT id FROM api_tokens WHERE name = 'Sylo'")
        .await;
    let res = ed.delete(&format!("/settings/tokens/{id}")).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = bearer(&w.app, "GET", &format!("/api/v1/targets/{}", w.a), &token).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_reference_lists_the_scope() {
    let w = world().await;
    let res = bearer(&w.app, "GET", "/api/v1/openapi.json", &w.admin).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let doc = res.json();
    assert!(
        doc["info"]["description"]
            .as_str()
            .unwrap()
            .contains("target:approve")
    );
    for (path, method) in [
        ("/api/v1/targets/{id}", "get"),
        ("/api/v1/targets/{id}/approve", "post"),
        ("/api/v1/targets/{id}/schedule", "post"),
        ("/api/v1/updates", "get"),
        ("/api/v1/runs/{id}", "get"),
    ] {
        let summary = doc["paths"][path][method]["summary"]
            .as_str()
            .unwrap_or_default();
        let description = doc["paths"][path][method]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            summary.contains("target token") || description.contains("target token"),
            "{method} {path} doesn't mention target tokens"
        );
    }
}
