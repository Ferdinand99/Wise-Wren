//! Agent settings managed in Wren: the settings page and who may use it,
//! versions and what the agent reports back, the history, widening changes
//! and their notification, the first import from an agent's agent.toml, and
//! agents that can't or mustn't take settings (older, locked).

mod common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use common::{App, Web};
use serde_json::{Value, json};

const MOUNTS: [&str; 3] = ["/mnt/user/appdata", "/srv", "/boot/config"];

/// What `agent.toml` of a fresh agent says, as the agent reports it.
fn toml_settings() -> Value {
    json!({
        "version": 0,
        "updates": {"allow": ["*"], "deny": [], "min_health_seconds": 30, "require_snapshot": false},
        "roots": ["/mnt/user/appdata"],
        "retention": {"rollback_images": 1, "snapshots": null},
        "script_stacks": [],
    })
}

/// The `settings` part of a heartbeat from an agent 0.8.
fn report(version: Option<i64>, current: Option<Value>) -> Value {
    json!({
        "supported": true, "locked": false, "version": version,
        "error": "", "error_version": null,
        "mounts": MOUNTS, "presets": ["fluxer"], "custom_stacks": ["mine"],
        "current": current,
    })
}

fn beat(settings: Option<Value>) -> Value {
    let mut b = json!({
        "agent_version": "0.8.0", "hostname": "tower", "os": "Unraid", "docker_version": "29.0",
        "docker_error": "", "unraid": true, "boot_id": "boot-1", "uptime_seconds": 10,
    });
    if let Some(s) = settings {
        b["settings"] = s;
    }
    b
}

async fn send(app: &App, token: &str, settings: Option<Value>) -> Value {
    let res = app.heartbeat(Some(token), &beat(settings)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.json()
}

/// A server whose agent has imported its agent.toml (version 1, running).
async fn imported(app: &App) -> (i64, String) {
    let (id, token) = app.add_server("LANBox").await;
    let reply = send(app, &token, Some(report(None, Some(toml_settings())))).await;
    assert_eq!(reply["settings"]["version"], 1);
    send(app, &token, Some(report(Some(1), Some(toml_settings())))).await;
    (id, token)
}

/// A settings form: the defaults, with the extras added (or, for a single
/// value such as `min_health`, instead of the default).
fn form<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    const SINGLE: [&str; 5] = [
        "updates",
        "min_health",
        "rollback_images",
        "snapshots",
        "stacks",
    ];
    let mut f = vec![
        ("updates", "list"),
        ("allow", "nextcloud"),
        ("min_health", "30"),
        ("rollback_images", "1"),
        ("snapshots", ""),
        ("root", "/mnt/user/appdata"),
        ("stacks", "0"),
    ];
    f.retain(|(k, _)| !(SINGLE.contains(k) && extra.iter().any(|(e, _)| e == k)));
    f.extend_from_slice(extra);
    f
}

#[tokio::test]
async fn an_agent_before_0_8_sends_nothing_and_is_sent_nothing() {
    let app = App::new().await;
    let (id, token) = app.add_server("LANBox").await;
    // The heartbeat of an agent 0.7: no `settings` at all.
    let reply = send(&app, &token, None).await;
    assert!(reply.get("settings").is_none(), "{reply}");
    assert_eq!(reply["scan"], false);
    assert_eq!(app.count("SELECT COUNT(*) FROM agent_settings").await, 0);
    let mut web = Web::new(&app, "owner", "admin").await;
    let page = web.get(&format!("/servers/{id}/settings")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains("data-settings-status=\"unsupported\""),
        "{}",
        page.body
    );
    assert!(page.body.contains("can’t take settings from Wren"));
    assert!(
        !page.body.contains("id=\"settings-form\""),
        "no form for it"
    );
    // The agent.toml helper is still there, and linked.
    assert!(page.body.contains(&format!("/servers/{id}/agent-toml")));
    // Saving is refused for it.
    let res = web
        .post(&format!("/servers/{id}/settings"), &form(&[]))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // A server whose agent never connected says so.
    let (never, _) = app.add_server("Other").await;
    let page = web.get(&format!("/servers/{never}/settings")).await;
    assert!(page.body.contains("data-settings-status=\"unknown\""));
}

#[tokio::test]
async fn the_first_report_is_imported_as_version_1_and_offered_back_unchanged() {
    let app = App::new().await;
    let (id, token) = app.add_server("LANBox").await;
    let mut imported = toml_settings();
    imported["updates"]["allow"] = json!(["nextcloud", "media-*"]);
    imported["updates"]["deny"] = json!(["media-db*"]);
    imported["updates"]["min_health_seconds"] = json!(45);
    imported["script_stacks"] = json!([{
        "preset": "fluxer", "name": "fluxer", "compose_project": "fluxer",
        "working_dir": "/mnt/user/appdata/fluxer", "min_interval_seconds": 7200,
    }]);
    let reply = send(&app, &token, Some(report(None, Some(imported.clone())))).await;
    // The same values, as version 1.
    let mut want = imported.clone();
    want["version"] = json!(1);
    assert_eq!(reply["settings"], want);
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 1);
    // Told again until the agent reports having it.
    let again = send(&app, &token, Some(report(None, Some(imported.clone())))).await;
    assert_eq!(again["settings"]["version"], 1);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM agent_settings_history")
            .await,
        1
    );
    let quiet = send(&app, &token, Some(report(Some(1), Some(want.clone())))).await;
    assert!(quiet.get("settings").is_none(), "{quiet}");
    // The history says it was imported, by nobody, and widened nothing.
    assert_eq!(
        app.count("SELECT COUNT(*) FROM agent_settings_history WHERE source = 'import' AND widening = 0 AND changed_by = ''")
            .await,
        1
    );
    let mut web = Web::new(&app, "owner", "admin").await;
    let page = web.get(&format!("/servers/{id}/settings")).await;
    assert!(page.body.contains("data-settings-status=\"active\""));
    assert!(
        page.body.contains("Active on the agent (version 1)"),
        "{}",
        page.body
    );
    assert!(page.body.contains("Imported from agent.toml"));
    assert!(page.body.contains("Started from the agent’s agent.toml"));
    assert!(page.body.contains("media-db*"), "patterns stay editable");
    assert!(
        page.body
            .contains("Script stacks with commands of their own")
    );
    assert!(page.body.contains("mine"), "custom stacks are named");
    // A second import never happens once Wren holds settings.
    send(&app, &token, Some(report(None, Some(toml_settings())))).await;
    assert_eq!(
        app.count("SELECT COUNT(*) FROM agent_settings_history")
            .await,
        1
    );
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 1);
}

#[tokio::test]
async fn admins_edit_everyone_else_reads() {
    let app = App::new().await;
    let (id, _) = imported(&app).await;
    let path = format!("/servers/{id}/settings");
    let mut admin = Web::new(&app, "owner", "admin").await;
    let page = admin.get(&path).await;
    assert!(
        page.body
            .contains("<fieldset class=\"fieldset-plain stack\">"),
        "{}",
        page.body
    );
    assert!(page.body.contains("Save settings"));
    for role in ["editor", "viewer"] {
        let name = if role == "editor" { "ed" } else { "vi" };
        let mut web = Web::new(&app, name, role).await;
        let page = web.get(&path).await;
        assert_eq!(page.status, StatusCode::OK);
        // Shown, but disabled and without a save button.
        assert!(page.body.contains("id=\"settings-form\""));
        assert!(
            page.body
                .contains("<fieldset class=\"fieldset-plain stack\" disabled>"),
            "{role}"
        );
        assert!(!page.body.contains("Save settings"), "{role}");
        assert!(page.body.contains("Only admins can change these settings"));
        assert!(
            !page.body.contains("/agent-toml"),
            "the helper is for admins"
        );
        let res = web.post(&path, &form(&[])).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{role}: {}", res.body);
    }
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 1);
    // The Agent card on the server's page links the settings for all.
    let mut viewer = Web::new(&app, "vi2", "viewer").await;
    let card = viewer.get(&format!("/servers/{id}")).await;
    assert!(
        card.body
            .contains(&format!("href=\"/servers/{id}/settings\""))
    );
}

/// A webhook receiver: what it got.
async fn hooks() -> (String, Arc<Mutex<Vec<Value>>>) {
    let got: Arc<Mutex<Vec<Value>>> = Arc::default();
    let g = got.clone();
    let router = axum::Router::new().fallback(move |body: axum::body::Bytes| {
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
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
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
async fn saving_counts_up_is_sent_until_acknowledged_and_logged() {
    let app = App::new().await;
    let (id, token) = imported(&app).await;
    let path = format!("/servers/{id}/settings");
    let mut web = Web::new(&app, "owner", "admin").await;

    // Narrowing: a shorter allow list, a longer health period, snapshots required.
    let res = web
        .post(
            &path,
            &form(&[
                ("min_health", "90"),
                ("require_snapshot", "true"),
                ("deny", "media-db"),
            ]),
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 2);
    let reply = send(&app, &token, Some(report(Some(1), None))).await;
    let s = &reply["settings"];
    assert_eq!(s["version"], 2);
    assert_eq!(s["updates"]["allow"], json!(["nextcloud"]));
    assert_eq!(s["updates"]["deny"], json!(["media-db"]));
    assert_eq!(s["updates"]["min_health_seconds"], 90);
    assert_eq!(s["updates"]["require_snapshot"], true);
    assert_eq!(s["roots"], json!(["/mnt/user/appdata"]));
    // Pending until the agent says it runs version 2.
    let page = web.get(&path).await;
    assert!(
        page.body.contains("Version 2 is waiting for the agent"),
        "{}",
        page.body
    );
    let quiet = send(&app, &token, Some(report(Some(2), None))).await;
    assert!(quiet.get("settings").is_none());
    let page = web.get(&path).await;
    assert!(page.body.contains("Active on the agent (version 2)"));

    // Nothing changed: no new version.
    let res = web
        .post(
            &path,
            &form(&[
                ("min_health", "90"),
                ("require_snapshot", "true"),
                ("deny", "media-db"),
            ]),
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 2);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM agent_settings_history")
            .await,
        2
    );

    // History: who, what, and narrowing changes are not marked.
    let row: (String, String, bool) = app
        .wren
        .core
        .with_conn(|c| {
            c.query_row(
                "SELECT changed_by, changes, widening FROM agent_settings_history WHERE version = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(row.0, "owner");
    assert!(!row.2, "narrowing changes are only logged: {}", row.1);
    assert!(
        row.1.contains("allow_removed")
            && row.1.contains("deny_added")
            && row.1.contains("snapshot_on")
    );
    let page = web.get(&path).await;
    assert!(page.body.contains("No longer allowed: *"), "{}", page.body);
    assert!(page.body.contains("Shortest health period: 30 → 90 s"));
    assert!(!page.body.contains("data-widened"));
}

#[tokio::test]
async fn changes_that_widen_are_marked_and_told_to_the_channels() {
    let app = App::new().await;
    let (hook, got) = hooks().await;
    add_hook(&app, hook).await;
    let (id, token) = imported(&app).await;
    let path = format!("/servers/{id}/settings");
    let mut web = Web::new(&app, "owner", "admin").await;

    // Narrowing first: logged, nobody told.
    web.post(&path, &form(&[("min_health", "90")])).await;
    assert!(got.lock().unwrap().is_empty());
    send(&app, &token, Some(report(Some(2), None))).await;

    // A new root (from the mounts), a lower minimum, snapshots off: widening.
    let res = web
        .post(
            &path,
            &[
                ("updates", "all"),
                ("min_health", "10"),
                ("rollback_images", "1"),
                ("snapshots", ""),
                ("root", "/mnt/user/appdata"),
                ("root", "/srv"),
                ("stacks", "0"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let sent = got.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    let text = sent[0].to_string();
    assert!(text.contains("now allow more"), "{text}");
    assert!(text.contains("owner") && text.contains("LANBox"));
    assert!(text.contains("Folder added: /srv"), "{text}");
    assert!(text.contains("Allowed: *"), "{text}");
    assert!(text.contains("Shortest health period: 90 → 10 s"), "{text}");
    assert_eq!(
        app.count("SELECT COUNT(*) FROM agent_settings_history WHERE widening = 1")
            .await,
        1
    );
    let page = web.get(&path).await;
    assert!(page.body.contains("data-widened=\"1\""), "{}", page.body);
    assert!(page.body.contains("Allows more"));
    // Sent as data: roots, no command anywhere.
    let reply = send(&app, &token, Some(report(Some(2), None))).await;
    assert_eq!(
        reply["settings"]["roots"],
        json!(["/mnt/user/appdata", "/srv"])
    );
    assert_eq!(reply["settings"]["updates"]["allow"], json!(["*"]));
}

#[tokio::test]
async fn script_stacks_are_presets_and_new_ones_widen() {
    let app = App::new().await;
    let (hook, got) = hooks().await;
    add_hook(&app, hook).await;
    let (id, token) = imported(&app).await;
    let path = format!("/servers/{id}/settings");
    let mut web = Web::new(&app, "owner", "admin").await;
    let page = web.get(&path).await;
    assert!(page.body.contains("Add a script stack"), "{}", page.body);
    assert!(page.body.contains("Fluxer"));
    let res = web
        .post(
            &path,
            &form(&[
                ("stacks", "1"),
                ("stack_0_preset", "fluxer"),
                ("stack_0_name", "fluxer"),
                ("stack_0_project", "fluxer"),
                ("stack_0_dir", "/mnt/user/appdata/fluxer"),
                ("stack_0_minutes", "120"),
            ]),
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let reply = send(&app, &token, Some(report(Some(1), None))).await;
    let stacks = &reply["settings"]["script_stacks"];
    assert_eq!(
        stacks[0],
        json!({
            "preset": "fluxer", "name": "fluxer", "compose_project": "fluxer",
            "working_dir": "/mnt/user/appdata/fluxer", "min_interval_seconds": 7200,
        })
    );
    // No field of a stack can carry a command.
    assert_eq!(stacks[0].as_object().unwrap().len(), 5);
    assert_eq!(got.lock().unwrap().len(), 1, "a new script stack widens");
    assert!(
        got.lock().unwrap()[0]
            .to_string()
            .contains("Script stack added: fluxer (fluxer)")
    );
    send(&app, &token, Some(report(Some(2), None))).await;

    // A lower minimum interval widens; removing the stack does not.
    let again = |minutes: &'static str, remove: bool| {
        let mut f = vec![
            ("updates", "list"),
            ("allow", "nextcloud"),
            ("min_health", "30"),
            ("rollback_images", "1"),
            ("snapshots", ""),
            ("root", "/mnt/user/appdata"),
            ("stacks", "1"),
            ("stack_0_preset", "fluxer"),
            ("stack_0_name", "fluxer"),
            ("stack_0_project", "fluxer"),
            ("stack_0_dir", "/mnt/user/appdata/fluxer"),
            ("stack_0_minutes", minutes),
        ];
        if remove {
            f.push(("stack_0_remove", "true"));
        }
        f
    };
    web.post(&path, &again("5", false)).await;
    assert_eq!(got.lock().unwrap().len(), 2, "a lower interval widens");
    send(&app, &token, Some(report(Some(3), None))).await;
    web.post(&path, &again("5", true)).await;
    assert_eq!(got.lock().unwrap().len(), 2, "removing a stack narrows");
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 4);
}

#[tokio::test]
async fn what_the_form_may_choose_is_limited() {
    let app = App::new().await;
    let (id, token) = imported(&app).await;
    let path = format!("/servers/{id}/settings");
    let mut web = Web::new(&app, "owner", "admin").await;
    let page = web.get(&path).await;
    // Offered: the mounts (and Compose Manager's folder inside /boot/config);
    // never a system folder, never free text.
    assert!(page.body.contains("value=\"/srv\""), "{}", page.body);
    assert!(
        page.body
            .contains("value=\"/boot/config/plugins/compose.manager/projects\"")
    );
    assert!(!page.body.contains("value=\"/boot/config\""));
    assert!(!page.body.contains("name=\"roots_extra\""));
    for (extra, want) in [
        (vec![("root", "/etc")], "can’t be a root"),
        (vec![("root", "/home/me")], "can’t be a root"),
        (vec![("root", "/boot/config")], "can’t be a root"),
        (vec![("root", "relative")], "can’t be a root"),
        (vec![("min_health", "99999")], "from 0 to 3600"),
        (vec![("rollback_images", "0")], "from 1 to 10"),
        (
            vec![("allow", "bad name")],
            "isn’t a target name or pattern",
        ),
    ] {
        let f = form(&extra);
        let res = web.post(&path, &f).await;
        assert_eq!(
            res.status,
            StatusCode::BAD_REQUEST,
            "{extra:?}: {}",
            res.body
        );
        assert!(res.body.contains(want), "{extra:?}: {}", res.body);
        // What was typed comes back.
        assert!(res.body.contains("id=\"settings-form\""));
    }
    // A stack outside the roots, with a wrong preset, or a bad name.
    for (field, value) in [
        ("stack_0_dir", "/srv/elsewhere"),
        ("stack_0_preset", "shell"),
        ("stack_0_name", "Bad Name"),
        ("stack_0_minutes", "0"),
    ] {
        let mut f = form(&[
            ("stacks", "1"),
            ("stack_0_preset", "fluxer"),
            ("stack_0_name", "fluxer"),
            ("stack_0_project", "fluxer"),
            ("stack_0_dir", "/mnt/user/appdata/fluxer"),
            ("stack_0_minutes", "60"),
        ]);
        f.retain(|(k, _)| *k != field);
        f.push((field, value));
        let res = web.post(&path, &f).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{field}: {}", res.body);
        assert!(res.body.contains("Script stack “"), "{field}");
    }
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 1);
    // The agent still sends nothing new.
    let reply = send(&app, &token, Some(report(Some(1), None))).await;
    assert!(reply.get("settings").is_none());
}

#[tokio::test]
async fn a_refusal_by_the_agent_is_shown_and_a_restored_database_catches_up() {
    let app = App::new().await;
    let (id, token) = imported(&app).await;
    let path = format!("/servers/{id}/settings");
    let mut web = Web::new(&app, "owner", "admin").await;
    web.post(&path, &form(&[("min_health", "60")])).await;
    // The agent refuses version 2 and says why.
    let mut r = report(Some(1), None);
    r["error"] = json!("roots[0] \"/srv\": not in a folder mounted into the agent");
    r["error_version"] = json!(2);
    let reply = send(&app, &token, Some(r)).await;
    assert_eq!(
        reply["settings"]["version"], 2,
        "offered again until it is accepted"
    );
    let page = web.get(&path).await;
    assert!(
        page.body.contains("The agent refused version 2"),
        "{}",
        page.body
    );
    assert!(page.body.contains("not in a folder mounted into the agent"));
    assert!(page.body.contains("data-settings-status=\"refused\""));

    // A Wren database restored from a backup: the agent runs version 9.
    send(&app, &token, Some(report(Some(9), None))).await;
    let page = web.get(&path).await;
    assert!(
        page.body.contains("The agent runs version 9"),
        "{}",
        page.body
    );
    web.post(&path, &form(&[("min_health", "70")])).await;
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 10);
}

#[tokio::test]
async fn a_locked_agent_is_shown_read_only_and_never_sent_settings() {
    let app = App::new().await;
    let (id, token) = imported(&app).await;
    let path = format!("/servers/{id}/settings");
    let mut web = Web::new(&app, "owner", "admin").await;
    web.post(&path, &form(&[("min_health", "60")])).await;
    let mut r = report(None, None);
    r["locked"] = json!(true);
    let reply = send(&app, &token, Some(r.clone())).await;
    assert!(reply.get("settings").is_none(), "{reply}");
    let page = web.get(&path).await;
    assert!(page.body.contains("Locked on the agent"), "{}", page.body);
    assert!(page.body.contains("WW_AGENT_LOCKED"));
    assert!(!page.body.contains("id=\"settings-form\""), "no form");
    assert!(
        page.body.contains(&format!("/servers/{id}/agent-toml")),
        "the helper stays"
    );
    let res = web.post(&path, &form(&[("min_health", "70")])).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(app.count("SELECT version FROM agent_settings").await, 2);
    // A locked agent that was never seen unlocked is not imported.
    let (_, other) = app.add_server("Other").await;
    r["current"] = toml_settings();
    let reply = send(&app, &other, Some(r)).await;
    assert!(reply.get("settings").is_none());
    assert_eq!(app.count("SELECT COUNT(*) FROM agent_settings").await, 1);
}

#[tokio::test]
async fn a_report_that_doesnt_parse_or_is_oversized_does_no_harm() {
    let app = App::new().await;
    let (_, token) = app.add_server("LANBox").await;
    // Wrong types: the heartbeat still works, the report is ignored.
    let reply = send(&app, &token, Some(json!({"supported": "yes", "mounts": 5}))).await;
    assert!(reply.get("settings").is_none());
    // An import with more entries than allowed is not taken.
    let mut huge = toml_settings();
    huge["roots"] = json!((0..80).map(|i| format!("/srv/{i}")).collect::<Vec<_>>());
    let reply = send(&app, &token, Some(report(None, Some(huge)))).await;
    assert!(reply.get("settings").is_none());
    // Unknown fields in a report from a newer agent are ignored.
    let mut r = report(None, Some(toml_settings()));
    r["something_new"] = json!(true);
    let reply = send(&app, &token, Some(r)).await;
    assert_eq!(reply["settings"]["version"], 1);
}
