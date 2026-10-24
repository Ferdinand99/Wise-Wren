//! 0.4.0: what the agent allows (blocked approvals), snapshot warnings,
//! the agent's coded texts, who started a run, the Overview's order and the
//! agent.toml helper.

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{App, Web, api_token};
use serde_json::{Value, json};
use vole_kit::auth::TokenScope;
use vole_kit::i18n::Lang;
use wise_wren::{runs, targets};

const OLD: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TOKEN: &str = "wwa_0123456789abcdefghijABCDEFGHIJ0123456789xyz";

async fn post_json(app: &App, path: &str, token: &str, body: Value) -> common::Res {
    let req = Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.send(req).await
}

fn container(name: &str, image: &str, labels: Value, binds: &[&str]) -> Value {
    let repo = image.rsplit_once(':').map_or(image, |(r, _)| r);
    json!({
        "name": name, "image": image, "image_id": "sha256:1",
        "repo_digests": [format!("{repo}@{OLD}")], "state": "running", "labels": labels,
        "binds": binds,
    })
}

/// Tower after its first night: the agent allows only `fluxer`; sonarr and
/// a Compose Manager stack have updates.
async fn tower(policy: Option<Value>) -> (App, String) {
    let app = App::new().await;
    let (_, token) = app.add_server("Tower").await;
    let mut inv = json!({
        "containers": [
            container("sonarr", "lscr.io/linuxserver/sonarr:latest", json!({}),
                      &["/mnt/user/appdata/sonarr", "/mnt/user/plex_data"]),
            container("sylo-docs-web-1", "ghcr.io/x/sylo-docs:latest",
                      json!({"com.docker.compose.project": "sylo-docs"}), &[]),
        ],
        "compose_projects": [{"name": "sylo-docs", "files_reachable": true,
            "working_dir": "/boot/config/plugins/compose.manager/projects/sylo-docs"}],
    });
    if let Some(p) = policy {
        inv["policy"] = p;
    }
    let res = post_json(&app, "/api/agent/v1/inventory", &token, inv).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    for image in [
        "lscr.io/linuxserver/sonarr:latest",
        "ghcr.io/x/sylo-docs:latest",
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
    (app, token)
}

fn tower_policy() -> Value {
    json!({"allow": ["fluxer"], "deny": [],
           "roots": ["/mnt/cache/appdata", "/mnt/user/appdata"]})
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

#[tokio::test]
async fn approvals_the_agent_would_refuse_are_blocked() {
    let (app, _) = tower(Some(tower_policy())).await;
    let sonarr = target(&app, "sonarr").await;
    assert_eq!(sonarr.blocked, Some(wise_wren::policy::Blocked::NotAllowed));
    // The page shows the badge and no working Approve.
    let mut web = Web::new(&app, "ed", "editor").await;
    let page = web.get("/updates").await.body;
    assert!(page.contains("data-blocked=\"1\""), "{page}");
    assert!(
        page.contains("Not allowed by [updates] in agent.toml"),
        "{page}"
    );
    assert!(!page.contains(&format!("formaction=\"/targets/{}/approve\"", sonarr.id)));
    let page = web.get(&format!("/targets/{}", sonarr.id)).await.body;
    assert!(
        page.contains("disabled title=\"Not allowed by [updates]"),
        "{page}"
    );
    let server = web
        .get(&format!("/servers/{}", sonarr.server_id))
        .await
        .body;
    assert!(server.contains("badge-blocked"), "{server}");

    // A form post anyway: refused with the reason, nothing queued.
    let res = web
        .post(
            &format!("/targets/{}/approve", sonarr.id),
            &[("key", &sonarr.available_key)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    // Bulk approval skips it and says how many.
    let docs = target(&app, "sylo-docs").await;
    let (a, b) = (sonarr.id.to_string(), docs.id.to_string());
    let (ka, kb) = (format!("key_{a}"), format!("key_{b}"));
    web.post(
        "/updates/approve",
        &[
            ("ids", &a),
            (&ka, &sonarr.available_key),
            ("ids", &b),
            (&kb, &docs.available_key),
        ],
    )
    .await;
    let page = web.get("/updates").await.body;
    assert!(
        page.contains("2 updates skipped: the agent would refuse them."),
        "{page}"
    );
    assert_eq!(app.count("SELECT COUNT(*) FROM runs").await, 0);

    // REST: blocked_reason, and approve answers 409 with it.
    let id = common::add_user(&app, "api", "editor").await;
    let token = api_token(&app, id, TokenScope::Write).await;
    let list = common::bearer(&app, "GET", "/api/v1/updates", &token)
        .await
        .json();
    let reasons: Vec<&str> = list["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["blocked_reason"].as_str())
        .collect();
    assert!(
        reasons.contains(&"not allowed by [updates] in agent.toml"),
        "{list}"
    );
    let req = Request::post(format!("/api/v1/targets/{}/approve", sonarr.id))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body.contains("not allowed by [updates]"),
        "{}",
        res.body
    );
}

#[tokio::test]
async fn a_stack_outside_the_roots_is_blocked_and_old_agents_block_nothing() {
    let all = json!({"allow": null, "deny": [], "roots": ["/mnt/user/appdata"]});
    let (app, _) = tower(Some(all)).await;
    assert_eq!(target(&app, "sonarr").await.blocked, None);
    assert_eq!(
        target(&app, "sylo-docs").await.blocked,
        Some(wise_wren::policy::Blocked::NotUnderRoots(
            "/boot/config/plugins/compose.manager/projects/sylo-docs".into()
        ))
    );
    // An agent before 0.4.0 reports no policy: nothing is blocked by it.
    let (app, _) = tower(None).await;
    assert_eq!(target(&app, "sonarr").await.blocked, None);
    assert_eq!(target(&app, "sylo-docs").await.blocked, None);
}

async fn hooks() -> (String, Arc<Mutex<Vec<Value>>>) {
    let got: Arc<Mutex<Vec<Value>>> = Arc::default();
    let g = got.clone();
    let hook = axum::Router::new().fallback(move |body: axum::body::Bytes| {
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
    tokio::spawn(async move { axum::serve(listener, hook).await.unwrap() });
    (format!("http://{addr}/hook"), got)
}

/// A run of sonarr handed to the agent (the policy allows everything).
async fn running(app: &App, token: &str) -> i64 {
    let t = target(app, "sonarr").await;
    let id = app
        .wren
        .core
        .with_conn(move |c| runs::approve(c, t.id, Some(&t.available_key), "ferdinand99"))
        .await
        .unwrap()
        .unwrap();
    let res = common::bearer(app, "GET", "/api/agent/v1/jobs?wait=0", token).await;
    assert_eq!(res.json()["job"]["run_id"], id, "{}", res.body);
    id
}

#[tokio::test]
async fn coded_results_translate_and_config_refusals_stay_quiet() {
    let (app, token) = tower(Some(json!({"allow": null, "deny": [], "roots": []}))).await;
    let (url, got) = hooks().await;
    let config = json!({"url": url, "method": "POST"}).to_string();
    app.wren
        .core
        .with_conn(move |c| {
            c.execute(
                "INSERT INTO notification_channels (name, kind, config, language) VALUES ('hook', 'webhook', ?1, 'nb')",
                [config],
            )
        })
        .await
        .unwrap();
    // Refused by the agent's agent.toml: shown, not notified.
    let id = running(&app, &token).await;
    let res = post_json(
        &app,
        &format!("/api/agent/v1/runs/{id}/finish"),
        &token,
        json!({"outcome": "refused",
               "detail": "refused by agent config: sonarr is not allowed by [updates] in agent.toml",
               "code": "refused.not_allowed", "params": {"target": "sonarr"}}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let run = app
        .wren
        .core
        .with_conn(move |c| runs::get(c, id))
        .await
        .unwrap()
        .unwrap();
    assert!(run.config_refusal());
    wise_wren::run_notify::send_result(&app.wren, &run).await;
    // An older agent's English says the same.
    let mut old = run.clone();
    old.detail_code.clear();
    assert!(old.config_refusal());
    // The detail in the reader's language, the English as a fallback.
    let nb = vole_kit::i18n::scope(Lang::NB, async { run.detail_text() }).await;
    let en = vole_kit::i18n::scope(Lang::EN, async { run.detail_text() }).await;
    assert!(nb.contains("sonarr") && nb != en, "{nb} / {en}");
    assert_eq!(
        vole_kit::i18n::scope(Lang::NB, async { old.detail_text() }).await,
        old.detail
    );
    assert!(
        !got.lock()
            .unwrap()
            .iter()
            .any(|m| m["event"] == "run_refused"),
        "no notification for it"
    );

    // A run whose snapshot took nothing: a warning with the reasons, on the
    // page and in the notification (in the channel's language).
    let id = running(&app, &token).await;
    post_json(
        &app,
        &format!("/api/agent/v1/runs/{id}/steps"),
        &token,
        json!({"step": "snapshot", "status": "ok", "log": "…", "duration_ms": 1, "notes": [
            {"code": "snapshot.none", "params": {}},
            {"code": "path.shared", "params": {"path": "/mnt/user/appdata/sonarr", "with": "rclone"}},
            {"code": "no.such.code", "params": {}},
        ]}),
    )
    .await;
    post_json(
        &app,
        &format!("/api/agent/v1/runs/{id}/finish"),
        &token,
        json!({"outcome": "succeeded", "detail": ""}),
    )
    .await;
    let mut web = Web::new(&app, "ed", "editor").await;
    let page = web.get(&format!("/runs/{id}")).await.body;
    assert!(page.contains("data-no-snapshot=\"1\""), "{page}");
    assert!(page.contains("No data snapshot was taken"), "{page}");
    assert!(
        page.contains("/mnt/user/appdata/sonarr is also used by rclone"),
        "{page}"
    );
    assert!(page.contains("data-status=\"warning\""), "{page}");
    // Who started it, not "Approved now".
    assert!(page.contains("Approved by ferdinand99"), "{page}");
    // The finish sends the result (a check may also announce the version).
    let run_results = || -> Vec<Value> {
        got.lock()
            .unwrap()
            .iter()
            .filter(|m| m["event"].as_str().is_some_and(|e| e.starts_with("run_")))
            .cloned()
            .collect()
    };
    for _ in 0..50 {
        if !run_results().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let sent = run_results();
    assert_eq!(sent.len(), 1, "{sent:?}");
    let text = sent[0].to_string();
    let none_nb = vole_kit::i18n::catalog::get(Lang::NB, "snap.none").unwrap();
    assert!(text.contains(none_nb), "{text}");
    assert!(text.contains("rclone"), "{text}");
}

#[tokio::test]
async fn require_snapshot_goes_into_the_job() {
    let (app, token) = tower(Some(json!({"allow": null, "deny": [], "roots": []}))).await;
    let t = target(&app, "sonarr").await;
    let mut web = Web::new(&app, "ed", "editor").await;
    web.post(
        &format!("/targets/{}/policy", t.id),
        &[
            ("mode", ""),
            ("health_seconds", ""),
            ("require_snapshot", "true"),
        ],
    )
    .await;
    let page = web.get(&format!("/targets/{}", t.id)).await.body;
    assert!(page.contains("Require a snapshot"), "{page}");
    app.wren
        .core
        .with_conn(move |c| runs::approve(c, t.id, Some(&t.available_key), "ed"))
        .await
        .unwrap()
        .unwrap();
    let job = common::bearer(&app, "GET", "/api/agent/v1/jobs?wait=0", &token)
        .await
        .json();
    assert_eq!(job["job"]["require_snapshot"], true, "{job}");
}

#[tokio::test]
async fn overview_shows_running_then_queued_then_finished() {
    let (app, _) = tower(None).await;
    app.exec(
        "INSERT INTO runs (id, server_id, target_kind, target_name, status, requested_by)
         VALUES (1, 1, 'container', 'a', 'succeeded', 'x'), (2, 1, 'container', 'b', 'queued', 'x'),
                (3, 1, 'container', 'c', 'failed', 'x'), (4, 1, 'container', 'd', 'running', 'x'),
                (5, 1, 'container', 'e', 'queued', 'x'), (6, 1, 'container', 'f', 'refused', 'x');"
            .into(),
    )
    .await;
    let ids: Vec<i64> = app
        .wren
        .core
        .with_conn(|c| runs::overview(c, 2))
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, [4, 2, 5, 6, 3]);
}

#[tokio::test]
async fn the_agent_toml_helper_builds_a_config_the_agent_takes() {
    let (app, _) = tower(Some(tower_policy())).await;
    let server_id = target(&app, "sonarr").await.server_id;
    let mut viewer = Web::new(&app, "vi", "viewer").await;
    let res = viewer
        .get(&format!("/servers/{server_id}/agent-toml"))
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let mut admin = Web::new(&app, "ad", "admin").await;
    let page = admin.get(&format!("/servers/{server_id}")).await.body;
    assert!(page.contains("/agent-toml"), "admins see the link");
    // First look: the agent's own roots, appdata and Compose Manager
    // ticked; media folders offered but not ticked.
    let page = admin
        .get(&format!("/servers/{server_id}/agent-toml"))
        .await
        .body;
    assert!(page.contains("value=\"/mnt/user/plex_data\""), "{page}");
    assert!(
        page.contains("value=\"/boot/config/plugins/compose.manager/projects\" checked"),
        "{page}"
    );
    assert!(
        !page.contains("value=\"/mnt/user/plex_data\" checked"),
        "{page}"
    );
    // Allow sonarr, the Fluxer preset.
    let q = "go=1&updates=list&allow=sonarr&root=%2Fmnt%2Fuser%2Fappdata\
             &root=%2Fboot%2Fconfig%2Fplugins%2Fcompose.manager%2Fprojects&stack=fluxer&stack_was=";
    let page = admin
        .get(&format!("/servers/{server_id}/agent-toml?{q}"))
        .await
        .body;
    let start = page.find("<code id=\"agent-toml\">").unwrap() + "<code id=\"agent-toml\">".len();
    let end = start + page[start..].find("</code>").unwrap();
    let text = page[start..end]
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    let full = format!("server = \"http://wren\"\ntoken = \"{TOKEN}\"\n{text}");
    let config = wren_agent::config::Config::from_parts(Some(&full), |_| None)
        .unwrap_or_else(|e| panic!("{e}\n{full}"));
    assert!(config.updates.allows("sonarr") && config.updates.allows("fluxer"));
    assert!(!config.updates.allows("immich"));
    assert_eq!(config.script_stacks[0].name, "fluxer");
    assert!(config.script_stacks[0].handles_own_backup);
    assert!(
        config.script_stacks[0]
            .check
            .as_deref()
            .is_some_and(|c| c.contains("rexctl")),
        "{full}"
    );
    assert!(config.compose_manager_hint(true).is_none());
    assert!(
        config
            .roots
            .iter()
            .any(|r| r.as_os_str() == "/mnt/cache/appdata/fluxer"),
        "the stack's folder joins the roots: {:?}",
        config.roots
    );
}
