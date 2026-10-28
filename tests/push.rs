//! The Hedgerow app channel end to end: a channel, a device registered with
//! a read-only token, a notification Wren broadcasts, the push the (fake)
//! relay gets and the text the app fetches with its ref.
//!
//! This is the only test in its binary: the kit's push client is installed
//! once per process (by `start`), so a second Wren started in the same
//! process would take it over.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{App, add_user, api_token};
use serde_json::{Value, json};
use vole_kit::auth::TokenScope;

/// What the fake relay saw.
#[derive(Default)]
struct Seen {
    registrations: usize,
    pushes: Vec<Value>,
}

/// A relay that takes every registration (difficulty 0) and accepts every
/// handle. Returns its URL.
async fn fake_relay() -> (String, Arc<Mutex<Seen>>) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let app = Router::new()
        .route(
            "/v1/challenge",
            get(|| async {
                Json(json!({
                    "challenge": "challenge-1",
                    "difficulty": 0,
                    "expires_at": "2099-01-01T00:00:00Z",
                }))
            }),
        )
        .route(
            "/v1/instances",
            post(|State(seen): State<Arc<Mutex<Seen>>>| async move {
                seen.lock().unwrap().registrations += 1;
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "instance_credential": "hrk_registered0",
                        "tier": "new",
                        "limits": {"sends_per_hour": 600, "handles_per_request": 25, "unknown_handles_per_hour": 20},
                    })),
                )
            }),
        )
        .route(
            "/v1/push",
            post(
                |State(seen): State<Arc<Mutex<Seen>>>, Json(body): Json<Value>| async move {
                    let n = body["handles"].as_array().map_or(0, Vec::len);
                    seen.lock().unwrap().pushes.push(body);
                    let results: Vec<Value> =
                        (0..n).map(|_| json!({"status": "accepted"})).collect();
                    Json(json!({ "results": results }))
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, seen)
}

const HANDLE: &str = "hrh_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

async fn call(
    app: &App,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> common::Res {
    let mut req = Request::builder().method(method).uri(path);
    if !token.is_empty() {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    app.send(req.body(body).unwrap()).await
}

#[tokio::test]
async fn a_notification_pushes_a_ref_and_the_ref_opens_the_text() {
    let app = App::new().await;
    let (relay_url, seen) = fake_relay().await;
    // Point the push client at the fake relay, and give the instance an
    // address, so a deep link would exist if the text kept it.
    app.wren
        .core
        .with_conn(move |c| {
            vole_kit::db::settings::set(c, vole_kit::push::relay::SETTING_RELAY_URL, &relay_url)?;
            vole_kit::db::settings::set(c, vole_kit::db::settings::BASE_URL, "https://wren.test")
        })
        .await
        .unwrap();
    let user = add_user(&app, "ada", "viewer").await;
    let read = api_token(&app, user, TokenScope::Read).await;

    // Push is off until there is a Hedgerow app channel.
    let register = json!({ "handle": HANDLE });
    let res = call(
        &app,
        "POST",
        "/api/v1/push/devices",
        &read,
        Some(register.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.json()["error"]["code"], "push_disabled");

    // The channel has no settings of its own.
    app.exec(
        "INSERT INTO notification_channels (name, kind, config) VALUES ('Phones', 'hedgerow', '{}')"
            .into(),
    )
    .await;

    // A read-only token registers its own device.
    let res = call(&app, "POST", "/api/v1/push/devices", &read, Some(register)).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // An agent goes offline: one notification, one push.
    let (server_id, _token) = app.add_server("LANBox").await;
    let sent = wise_wren::run_notify::broadcast(&app.wren.core, |base| {
        wise_wren::run_notify::agent_alert(server_id, "LANBox", false, base)
    })
    .await;
    assert_eq!(sent, 1, "the channel counts it as sent");
    for _ in 0..100 {
        if !seen.lock().unwrap().pushes.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Give a wrong second push time to show up.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (registrations, mut pushes) = {
        let seen = seen.lock().unwrap();
        (seen.registrations, seen.pushes.clone())
    };
    assert_eq!(registrations, 1, "the server registered itself once");
    assert_eq!(pushes.len(), 1, "one push for one notification");
    let push = pushes.remove(0);
    assert_eq!(push["kind"], "alert");
    assert_eq!(push["handles"], json!([HANDLE]));
    let reference = push["ref"].as_str().expect("a ref").to_string();
    assert!(!reference.is_empty());
    // The relay and Apple get a ref and nothing else of the notification.
    let wire = push.to_string();
    for secret in ["LANBox", "offline", "wren.test"] {
        assert!(!wire.contains(secret), "{secret} went to the relay: {wire}");
    }

    // The app fetches the text from this server, with a read-only token.
    let res = call(
        &app,
        "GET",
        &format!("/api/v1/push/events/{reference}"),
        &read,
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let text = res.json();
    let title = text["title"].as_str().unwrap();
    let body = text["body"].as_str().unwrap_or_default();
    assert!(
        title.contains("LANBox") || body.contains("LANBox"),
        "the text names the server: {text}"
    );
    assert!(
        !title.contains("wren.test") && !body.contains("wren.test"),
        "the deep link isn't stored: {text}"
    );
    // Not without a token.
    let res = call(
        &app,
        "GET",
        &format!("/api/v1/push/events/{reference}"),
        "",
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    // An unknown ref is a plain 404.
    let res = call(&app, "GET", "/api/v1/push/events/nope", &read, None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}
