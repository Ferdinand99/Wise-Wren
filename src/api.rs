//! The REST API v1 and MCP (design §9): the API reference (the kit's pages
//! from [`DOCS`]), servers, targets, updates, runs, approvals (now or for a
//! chosen time), schedules and modes. Writes need an API token with write
//! access of an editor; only admins turn automatic updates on.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Map, Value, json};
use vole_kit::auth::CurrentUser;
use vole_kit::docs::{BearerScheme, Credential, DocsSpec, RATE_LIMITED, r};
use vole_kit::http::ApiResult;
use vole_kit::mcp::{Handler, RpcResult};
use vole_kit::state::Core;

use axum::extract::{Extension, Path, Query};
use axum::routing::post;
use vole_kit::auth::AuthMethod;
use vole_kit::http::ApiError;

use crate::runs::{self, Run};

use crate::db;
use crate::targets::{self, Filter, Target};

const ERROR_EXAMPLE: &str = r#"{"error": {"code": "not_found", "message": "Not found."}}"#;

const SERVERS_EXAMPLE: &str = r#"{"servers": [{"id": 1, "name": "Tower", "status": "online", "last_seen_at": "2026-10-23 10:00:00", "hostname": "Tower", "os": "Unraid OS 7.4", "docker_version": "29.8.2", "agent_version": "0.1.0", "unraid": true}]}"#;

const HEARTBEAT_EXAMPLE: &str = r#"{"agent_version": "0.1.0", "hostname": "Tower", "os": "Unraid OS 7.4", "docker_version": "29.8.2", "docker_error": "", "unraid": true}"#;

const TARGETS_EXAMPLE: &str = r#"{"targets": [{"id": 7, "server": {"id": 1, "name": "LANBox"}, "kind": "compose", "name": "postiz", "image": "ghcr.io/gitroomhq/postiz-app:latest", "images": 6, "status": "update", "status_detail": "", "available": "2/6", "version_key": "postiz=sha256:5c0d…,postiz-postgres=sha256:a1b2…", "installed_version": "", "latest_version": "", "version_source": null, "version_error": "", "working_dir": "/boot/config/plugins/compose.manager/projects/postiz", "files_reachable": true, "source_url": "https://github.com/gitroomhq/postiz-app", "release_notes": {"url": "https://github.com/gitroomhq/postiz-app/releases/tag/v2.1.0", "title": "v2.1.0"}, "check_interval_hours": 6, "last_checked_at": "2026-10-23 10:00:00"}]}"#;

const RUNS_EXAMPLE: &str = r#"{"runs": [{"id": 12, "server": {"id": 1, "name": "LANBox"}, "target": {"id": 7, "kind": "container", "name": "ww-test-container"}, "kind": "update", "rollback_of": null, "from_key": "ww-test-container=sha256:02d8…", "to_key": "ww-test-container=sha256:4f90…", "version": "4f90b33d4a1c", "status": "succeeded", "detail": "", "requested_by": "owner", "created_at": "2026-10-23 10:00:00", "started_at": "2026-10-23 10:00:01", "finished_at": "2026-10-23 10:03:20"}]}"#;

const RUN_EXAMPLE: &str = r#"{"id": 12, "status": "rolled_back", "detail": "ww-test-container is unhealthy", "steps": [{"step": "preflight", "status": "ok", "started_at": "2026-10-23 10:00:01", "duration_ms": 812, "log": "pulled …"}, {"step": "health", "status": "failed", "started_at": "2026-10-23 10:00:05", "duration_ms": 9000, "log": "ww-test-container is unhealthy"}]}"#;

const JOB_EXAMPLE: &str = r#"{"job": {"run_id": 12, "kind": "update", "target": {"kind": "container", "name": "ww-test-container"}, "from_key": "ww-test-container=sha256:02d8…", "to_key": "ww-test-container=sha256:4f90…", "images": [{"container": "ww-test-container", "image": "localhost:5056/ww-test/app:latest", "from": "sha256:02d8…", "to": "sha256:4f90…"}], "health_seconds": 180, "keep_snapshots": 2, "rollback_of": null}}"#;

const SCHEDULES_EXAMPLE: &str = r#"{"time_zone": "Europe/Oslo", "scheduled": [{"id": 14, "server": {"id": 1, "name": "LANBox"}, "target": {"id": 7, "kind": "container", "name": "ww-test-container"}, "kind": "update", "trigger": "schedule", "scheduled_for": "2026-10-24 01:00:00", "status": "queued", "maintenance": {"window_id": 31, "state": "planned"}}], "servers": [{"id": 1, "name": "LANBox", "mode": "approve", "auto_window": {"start": "03:00", "end": "05:00", "open_now": false}, "auto_targets": [{"id": 9, "name": "ww-test-compose"}]}]}"#;

const INVENTORY_EXAMPLE: &str = r#"{"containers": [{"name": "temporal-postgresql", "image": "postgres:16", "image_id": "sha256:066167ff39c2…", "repo_digests": ["postgres@sha256:a902560563d3…"], "state": "running", "labels": {"com.docker.compose.project": "postiz"}}], "unraid_templates": ["Newt"], "compose_projects": [{"name": "postiz", "working_dir": "/boot/config/plugins/compose.manager/projects/postiz", "config_files": "…/compose.yaml", "files_reachable": true}], "script_stacks": [], "error": ""}"#;

fn no_param(_: &vole_kit::docs::RouteDoc, _: &str) -> Option<String> {
    None
}

fn intro(out: &mut String, base: &str) {
    out.push_str(&format!(
        "# Wise Wren API\n\nBase URL: {base}/api/v1. Send `Authorization: Bearer ww_…`.\n\n"
    ));
}

fn nothing(_: &mut String) {}

pub static DOCS: DocsSpec = DocsSpec {
    routes: &[
        r("GET", "/api/v1/servers", "viewer", "Servers", "List the servers with their agents' status (online, offline, waiting for the agent, no token) and what each agent last reported.")
            .res(SERVERS_EXAMPLE),
        r("GET", "/api/v1/targets", "viewer", "Targets", "List what Wren watches on every server (or `?server_id=`): containers, Unraid template containers, Compose stacks and script stacks, with the current and available version and the status `current`, `update` or `unknown`.")
            .res(TARGETS_EXAMPLE),
        r("GET", "/api/v1/updates", "viewer", "Targets", "List the targets with a new version (status `update`), with release notes when Wren found them.")
            .res(TARGETS_EXAMPLE),
        r("GET", "/api/v1/runs", "viewer", "Runs", "List update runs, newest first (`?target_id=`, `?server_id=`, `?limit=`, at most 500). Status: `queued` (approved, waiting for the agent), `running`, `succeeded`, `rolled_back` (the update failed and was undone), `failed` (nothing changed), `rollback_failed` (needs a person), `refused` (by the agent's own `agent.toml`), `cancelled` or `lapsed` (a newer version appeared before it started).")
            .res(RUNS_EXAMPLE),
        r("GET", "/api/v1/runs/{id}", "viewer", "Runs", "One run with each step (`preflight`, `stop`, `snapshot`, `update`, `health`, `rollback`): status, duration and the end of its log.")
            .res(RUN_EXAMPLE),
        r("POST", "/api/v1/targets/{id}/approve", "editor", "Runs", "Approve the target's available update now. Send `{\"version_key\": \"…\"}` (the target's `version_key`) to approve exactly the version you saw: a different one is refused, and if a newer version appears before the run starts, the approval lapses. Refused for `notify` targets and stacks whose files the agent can't reach. Needs an API token with write access.")
            .req(r#"{"version_key": "ww-test-container=sha256:4f90b33d…"}"#)
            .res(r#"{"run_id": 12, "status": "queued"}"#),
        r("POST", "/api/v1/targets/{id}/schedule", "editor", "Schedules", "Approve the target's available update for a later time: `at` is RFC 3339 with an offset, or `YYYY-MM-DD HH:MM:SS` in UTC; at most 90 days ahead. Send `version_key` as for approve: the approval is for exactly that version and lapses if a newer one appears first. When the target has linked Vigilant Vole monitors, Wren creates a planned maintenance window in VV ahead of time and moves or deletes it with the schedule.")
            .req(r#"{"version_key": "ww-test-container=sha256:4f90b33d…", "at": "2026-10-24T03:00:00+02:00"}"#)
            .res(r#"{"run_id": 14, "status": "queued", "scheduled_for": "2026-10-24 01:00:00"}"#),
        r("POST", "/api/v1/runs/{id}/reschedule", "editor", "Schedules", "Move a scheduled run that hasn't started (`at` as for schedule). Its planned window in Vigilant Vole moves with it.")
            .req(r#"{"at": "2026-10-25T03:00:00+02:00"}"#)
            .res(r#"{"run_id": 14, "scheduled_for": "2026-10-25 01:00:00"}"#),
        r("POST", "/api/v1/runs/{id}/cancel", "editor", "Runs", "Cancel a run before anything stops: a queued or scheduled one at once (its planned window in Vigilant Vole is deleted), a running one when the agent reports its preflight.")
            .res(r#"{"cancelled": true}"#),
        r("GET", "/api/v1/schedules", "viewer", "Schedules", "Upcoming scheduled updates (soonest first) with their Vigilant Vole window, and per server its mode, auto window (local times in the site's time zone) and the targets that update on their own inside it.")
            .res(SCHEDULES_EXAMPLE),
        r("POST", "/api/v1/targets/{id}/mode", "editor", "Schedules", "Set a target's mode: `approve`, `notify`, `auto` or `null` (the server's). Turning automatic updates on (`auto`, or `null` on an `auto` server) needs an admin and the server's auto window; it is logged. Optional: `health_seconds`, `require_vv` (cancel a run when the Vigilant Vole window can't be opened).")
            .req(r#"{"mode": "auto", "require_vv": false}"#)
            .res(r#"{"mode": "auto", "effective_mode": "auto", "updates_itself": true}"#),
        r("POST", "/api/v1/servers/{id}/mode", "editor", "Schedules", "Set a server's mode (`approve`, the default, `notify` or `auto`) and its auto window (`auto_start`, `auto_end` as local `HH:MM`, both empty for none). `auto` and any window change need an admin; `auto` needs a window. Auto updates only start inside the window, for targets whose effective mode is `auto`; a target set to `approve` or `notify` is excluded.")
            .req(r#"{"mode": "auto", "auto_start": "03:00", "auto_end": "05:00"}"#)
            .res(r#"{"mode": "auto", "auto_window": {"start": "03:00", "end": "05:00"}}"#),
        r("POST", "/api/v1/targets/{id}/skip", "editor", "Runs", "Skip the available version; a newer one shows again.")
            .res(r#"{"skipped": true}"#),
        r("POST", "/api/v1/targets/{id}/snooze", "editor", "Runs", "Hide the available update for `hours` (default 24, at most 90 days).")
            .req(r#"{"hours": 168}"#)
            .res(r#"{"snoozed_until": "2026-10-30 10:00:00"}"#),
        r("GET", "/api/agent/v1/jobs", "agent token", "Agents", "The agent waits here (up to 25 s) for its next job: an approved update or rollback of one of its targets, as data (target kind and name, version keys, per image the container, the reference it runs and the two digests), never a command. `204` when there is none. One run at a time per server.")
            .res(JOB_EXAMPLE),
        r("POST", "/api/agent/v1/runs/{id}/steps", "agent token", "Agents", "The agent reports a step of its running run. The answer to the `preflight` result is the last moment a run can be cancelled: nothing has stopped yet.")
            .req(r#"{"step": "preflight", "status": "ok", "log": "pulled localhost:5056/ww-test/app:4f90…", "duration_ms": 812}"#)
            .res(r#"{"cancel": false}"#),
        r("POST", "/api/agent/v1/runs/{id}/monitors", "agent token", "Agents", "After its container checks passed, the agent asks whether the target's linked Vigilant Vole monitors are up: `ok`, `wait` (ask again in 10 s), `failed` (roll back) or `skipped` (no monitors, VV unreachable or too old: the container checks decide). Wren reads the monitors' real results during maintenance (`raw_status`) since the health step began.")
            .res(r#"{"verdict": "wait", "log": "web: up for 40 of 180 s"}"#),
        r("POST", "/api/agent/v1/runs/{id}/finish", "agent token", "Agents", "The agent reports how its run ended: `succeeded`, `rolled_back`, `failed`, `rollback_failed`, `refused` or `cancelled`.")
            .req(r#"{"outcome": "rolled_back", "detail": "ww-test-container is unhealthy"}"#)
            .res(r#"{}"#),
        r("POST", "/api/agent/v1/heartbeat", "agent token", "Agents", "An agent reports that it runs, every 30 seconds. Wren counts a server as online while heartbeats arrive (at most 90 seconds apart). `scan: true` in the answer asks the agent for a fresh inventory (\"Check now\"); it is the only thing Wren can ask. 429 with `Retry-After` above 20 a minute.")
            .req(HEARTBEAT_EXAMPLE)
            .res(r#"{"server": {"id": 1, "name": "Tower"}, "interval_seconds": 30, "scan": false}"#),
        r("POST", "/api/agent/v1/inventory", "agent token", "Agents", "An agent reports its server's containers (with `.Config.Image`, `RepoDigests` and labels), the Unraid template names, the Compose projects and its script stacks with what their version commands printed. Wren replaces the server's targets with it. 429 with `Retry-After` above 6 a minute.")
            .req(INVENTORY_EXAMPLE)
            .res(r#"{"targets": 12}"#),
    ],
    openapi_title: "Wise Wren API",
    openapi_description: "REST API for Wise Wren. Authenticate with `Authorization: Bearer ww_…`.",
    bearer_description: "API token (ww_…)",
    credentials: &[Credential {
        access: "agent token",
        words: "the server's agent token (`wwa_…`), not an API token",
        bearer: Some(BearerScheme {
            name: "agentToken",
            description: "Agent token (wwa_…)",
            variable: "AGENT_TOKEN",
        }),
        header: None,
    }],
    error_example: ERROR_EXAMPLE,
    error_codes: &[RATE_LIMITED],
    config_document: None,
    curl_path_param: no_param,
    markdown_intro: intro,
    markdown_middle: nothing,
    markdown_outro: nothing,
    page_subtitle: "tokens.sub",
    openapi_path: "/api/v1/openapi.json",
    api_base: "/api/v1",
    example_path: "/api/v1/servers",
    metrics_path: None,
    fallback_host: Some("localhost:3003"),
    param_types: &[],
    strict_access: true,
};

pub fn router() -> Router<Core> {
    Router::new()
        .route("/api/v1/servers", get(servers))
        .route("/api/v1/targets", get(targets_route))
        .route("/api/v1/updates", get(updates_route))
        .route("/api/v1/runs", get(runs_route))
        .route("/api/v1/runs/{id}", get(run_route))
        .route("/api/v1/targets/{id}/{action}", post(target_action))
        .route("/api/v1/runs/{id}/{action}", post(run_action))
        .route("/api/v1/servers/{id}/mode", post(server_mode_route))
        .route("/api/v1/schedules", get(schedules_route))
}

/// A target as the API and MCP give it.
pub fn target_json(t: &Target) -> Value {
    let source: Value = serde_json::from_str(&t.version_source).unwrap_or(Value::Null);
    json!({
        "id": t.id,
        "server": { "id": t.server_id, "name": t.server_name },
        "kind": t.kind,
        "name": t.name,
        "image": t.image,
        "images": t.image_count,
        "status": t.status,
        "status_detail": t.status_detail,
        "available": t.available,
        "version_key": t.available_key,
        "installed_version": t.installed_version,
        "latest_version": t.latest_version,
        "version_source": source,
        "version_error": t.version_error,
        "working_dir": t.working_dir,
        "files_reachable": t.files_reachable,
        "source_url": t.source_url,
        "release_notes": if t.notes_url.is_empty() { Value::Null } else { json!({ "url": t.notes_url, "title": t.notes_title }) },
        "check_interval_hours": t.check_interval_hours.unwrap_or(t.server_interval_hours),
        "last_checked_at": t.last_checked_at,
    })
}

/// Targets as the API and MCP give them.
pub async fn target_list(core: &Core, filter: Filter) -> ApiResult<Value> {
    let rows = core.with_conn(move |c| targets::list(c, filter)).await?;
    Ok(json!({ "targets": rows.iter().map(target_json).collect::<Vec<_>>() }))
}

#[derive(serde::Deserialize)]
struct TargetsQuery {
    server_id: Option<i64>,
}

async fn targets_route(
    State(core): State<Core>,
    _user: CurrentUser,
    Query(q): Query<TargetsQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        target_list(
            &core,
            Filter {
                server_id: q.server_id,
                updates_only: false,
            },
        )
        .await?,
    ))
}

async fn updates_route(State(core): State<Core>, _user: CurrentUser) -> ApiResult<Json<Value>> {
    Ok(Json(
        target_list(
            &core,
            Filter {
                server_id: None,
                updates_only: true,
            },
        )
        .await?,
    ))
}

/// The servers as the API and MCP give them.
pub async fn server_list(core: &Core) -> ApiResult<Value> {
    let rows = core.with_conn(|c| db::servers(c)).await?;
    let servers: Vec<Value> = rows
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "status": s.status().as_str(),
                "last_seen_at": s.last_seen_at,
                "hostname": s.hostname,
                "os": s.os,
                "docker_version": s.docker_version,
                "docker_error": s.docker_error,
                "agent_version": s.agent_version,
                "unraid": s.unraid,
            })
        })
        .collect();
    Ok(json!({ "servers": servers }))
}

async fn servers(State(core): State<Core>, _user: CurrentUser) -> ApiResult<Json<Value>> {
    Ok(Json(server_list(&core).await?))
}

/// MCP: read-only tools that list targets and updates.
struct Mcp {
    core: Core,
    wren: crate::Wren,
    user: String,
    can_write: bool,
    is_admin: bool,
}

fn tool(name: &str, description: &str, properties: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "additionalProperties": false },
        "annotations": { "readOnlyHint": true },
    })
}

impl Handler for Mcp {
    fn can_write(&self) -> bool {
        self.can_write
    }

    fn instructions(&self) -> String {
        "Wise Wren keeps the Docker containers on the user's servers up to date. Use list_updates for what has a new version, list_targets for everything Wren watches, list_runs for update runs, list_schedules for scheduled updates and auto windows, approve_update (write tokens) to approve one exact version now or at a time, and set_mode to change a target's or server's mode (auto needs an admin).".into()
    }

    fn tools(&self) -> Vec<Value> {
        let mut tools = vec![
            tool(
                "list_targets",
                "List the containers, Unraid template containers, Compose stacks and script stacks Wren watches, with current and available version and status (current, update, unknown).",
                json!({ "server_id": { "type": "integer", "description": "Only this server's targets." } }),
            ),
            tool(
                "list_updates",
                "List the targets that have a new version, with release notes when Wren found them.",
                json!({}),
            ),
            tool(
                "list_schedules",
                "List upcoming scheduled updates with their Vigilant Vole planned window, and per server its mode, auto window and the targets that update on their own.",
                json!({}),
            ),
            tool(
                "list_runs",
                "List update runs (newest first) with their status: queued, running, succeeded, rolled_back, failed, rollback_failed (needs a person), refused, cancelled or lapsed.",
                json!({ "target_id": { "type": "integer" }, "server_id": { "type": "integer" }, "limit": { "type": "integer" } }),
            ),
        ];
        if self.can_write {
            tools.push(json!({
                "name": "approve_update",
                "description": "Approve a target's available update now (it runs at once, on the agent, with snapshot, health check and automatic rollback). Pass version_key (the target's available_key from list_updates) so the approval is for exactly that version; it is refused when the target is notify-only or a newer version appeared.",
                "inputSchema": { "type": "object", "properties": { "target_id": { "type": "integer" }, "version_key": { "type": "string" }, "at": { "type": "string", "description": "Run later instead: RFC 3339 with an offset, or YYYY-MM-DD HH:MM:SS in UTC (at most 90 days ahead). Creates a planned maintenance window in Vigilant Vole." } }, "required": ["target_id"], "additionalProperties": false },
                "annotations": { "readOnlyHint": false, "destructiveHint": false },
            }));
            tools.push(json!({
                "name": "set_mode",
                "description": "Set the update mode of a target (target_id) or a server (server_id): approve (the default), notify, auto, or for a target \"server\" (follow the server). auto needs an admin and the server's auto window (auto_start/auto_end as local HH:MM, admins only). A target set to approve or notify is never updated automatically.",
                "inputSchema": { "type": "object", "properties": { "target_id": { "type": "integer" }, "server_id": { "type": "integer" }, "mode": { "type": "string", "enum": ["approve", "notify", "auto", "server"] }, "auto_start": { "type": "string" }, "auto_end": { "type": "string" } }, "required": ["mode"], "additionalProperties": false },
                "annotations": { "readOnlyHint": false, "destructiveHint": false },
            }));
        }
        tools
    }

    async fn call_tool(&self, name: &str, args: Map<String, Value>) -> RpcResult {
        let value = match name {
            "list_runs" => Some(
                run_list(
                    &self.core,
                    runs::Filter {
                        target_id: args.get("target_id").and_then(Value::as_i64),
                        server_id: args.get("server_id").and_then(Value::as_i64),
                        limit: args
                            .get("limit")
                            .and_then(Value::as_i64)
                            .unwrap_or(50)
                            .clamp(1, 500),
                    },
                )
                .await
                .map_err(|_| (-32603, "Wren couldn't read its runs.".to_string()))?,
            ),
            "approve_update" => {
                if !self.can_write {
                    return Err((
                        -32602,
                        "approve_update needs a write token of an editor or admin.".into(),
                    ));
                }
                let id = args
                    .get("target_id")
                    .and_then(Value::as_i64)
                    .ok_or((-32602, "target_id is required.".to_string()))?;
                let body = Body {
                    version_key: args
                        .get("version_key")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    at: args.get("at").and_then(Value::as_str).map(str::to_string),
                    ..Body::default()
                };
                let action = if body.at.is_some() {
                    "schedule"
                } else {
                    "approve"
                };
                Some(
                    act(&self.wren, &self.user, self.is_admin, action, id, body)
                        .await
                        .map_err(|e| (-32602, e.to_string()))?,
                )
            }
            "set_mode" => {
                if !self.can_write {
                    return Err((
                        -32602,
                        "set_mode needs a write token of an editor or admin.".into(),
                    ));
                }
                let mode = args
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let body = Body {
                    mode: Some(mode),
                    auto_start: args
                        .get("auto_start")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    auto_end: args
                        .get("auto_end")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ..Body::default()
                };
                let res = match (
                    args.get("target_id").and_then(Value::as_i64),
                    args.get("server_id").and_then(Value::as_i64),
                ) {
                    (Some(id), None) => {
                        act(&self.wren, &self.user, self.is_admin, "mode", id, body).await
                    }
                    (None, Some(id)) => {
                        server_mode(&self.wren, &self.user, self.is_admin, id, body).await
                    }
                    _ => {
                        return Err((-32602, "Give target_id or server_id.".to_string()));
                    }
                };
                Some(res.map_err(|e| (-32602, e.to_string()))?)
            }
            "list_schedules" => Some(
                schedules_json(&self.core)
                    .await
                    .map_err(|_| (-32603, "Wren couldn't read its schedules.".to_string()))?,
            ),
            _ => None,
        };
        if let Some(value) = value {
            return Ok(json!({
                "content": [{ "type": "text", "text": value.to_string() }],
                "structuredContent": value,
            }));
        }
        let filter = match name {
            "list_targets" => Filter {
                server_id: args.get("server_id").and_then(Value::as_i64),
                updates_only: false,
            },
            "list_updates" => Filter {
                server_id: None,
                updates_only: true,
            },
            _ => return Err((-32602, format!("Unknown tool {name}."))),
        };
        let value = target_list(&self.core, filter)
            .await
            .map_err(|_| (-32603, "Wren couldn't read its targets.".to_string()))?;
        Ok(json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "structuredContent": value,
        }))
    }

    async fn list_resources(&self) -> RpcResult {
        Ok(json!({ "resources": [] }))
    }

    fn resource_templates(&self) -> Value {
        json!([])
    }

    async fn read_resource(&self, uri: &str) -> RpcResult {
        Err((-32002, format!("Unknown resource {uri}.")))
    }
}

/// `POST /mcp` with an API token.
pub async fn mcp(
    State(core): State<Core>,
    Extension(wren): Extension<crate::Wren>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let caller = match vole_kit::mcp::authenticate(&core, &headers, 0).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    vole_kit::mcp::serve(
        &Mcp {
            core: core.clone(),
            wren,
            user: caller.user.username.clone(),
            can_write: caller.can_write,
            is_admin: caller.can_write && caller.user.is_admin(),
        },
        &body,
    )
    .await
}

// ---------------------------------------------------------------------------
// Runs and approvals
// ---------------------------------------------------------------------------

/// A run as the API and MCP give it.
pub fn run_json(r: &Run) -> Value {
    json!({
        "id": r.id,
        "server": { "id": r.server_id, "name": r.server_name },
        "target": { "id": r.target_id, "kind": r.target_kind, "name": r.target_name },
        "kind": r.kind,
        "rollback_of": r.rollback_of,
        "from_key": r.from_key,
        "to_key": r.to_key,
        "version": r.shown,
        "status": r.status,
        "detail": r.detail,
        "requested_by": r.requested_by,
        "trigger": r.trigger,
        "scheduled_for": r.scheduled_for,
        "maintenance": { "window_id": r.vv_window_id, "state": r.vv_state },
        "created_at": r.created_at,
        "started_at": r.started_at,
        "finished_at": r.finished_at,
    })
}

/// Upcoming scheduled runs and the servers' auto windows.
pub async fn schedules_json(core: &Core) -> ApiResult<Value> {
    let (scheduled, servers, tz) = core
        .with_conn(|c| {
            Ok((
                runs::scheduled(c)?,
                crate::schedule::servers_auto(c, jiff::Timestamp::now())?,
                crate::schedule::site_tz_name(c)?,
            ))
        })
        .await?;
    Ok(json!({
        "time_zone": tz,
        "scheduled": scheduled.iter().map(run_json).collect::<Vec<_>>(),
        "servers": servers.iter().map(|s| json!({
            "id": s.id,
            "name": s.name,
            "mode": s.mode,
            "auto_window": s.window.as_ref().map(|(a, b)| json!({"start": a, "end": b, "open_now": s.open_now})),
            "auto_targets": s.targets.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    }))
}

async fn schedules_route(State(core): State<Core>, _user: CurrentUser) -> ApiResult<Json<Value>> {
    Ok(Json(schedules_json(&core).await?))
}

#[derive(serde::Deserialize, Default)]
struct RunsQuery {
    target_id: Option<i64>,
    server_id: Option<i64>,
    limit: Option<i64>,
}

pub async fn run_list(core: &Core, f: runs::Filter) -> ApiResult<Value> {
    let rows = core.with_conn(move |c| runs::list(c, f)).await?;
    Ok(json!({ "runs": rows.iter().map(run_json).collect::<Vec<_>>() }))
}

async fn runs_route(
    State(core): State<Core>,
    _user: CurrentUser,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        run_list(
            &core,
            runs::Filter {
                target_id: q.target_id,
                server_id: q.server_id,
                limit: q.limit.unwrap_or(50).clamp(1, 500),
            },
        )
        .await?,
    ))
}

async fn run_route(
    State(core): State<Core>,
    _user: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let (run, steps) = core
        .with_conn(move |c| Ok((runs::get(c, id)?, runs::steps(c, id)?)))
        .await?;
    let mut v = run_json(&run.ok_or(ApiError::NotFound)?);
    v["steps"] = json!(steps
        .iter()
        .map(|s| json!({"step": s.step, "status": s.status, "started_at": s.started_at, "duration_ms": s.duration_ms, "log": s.log}))
        .collect::<Vec<_>>());
    Ok(Json(v))
}

/// API writes need an API token (no session cookie: these paths skip the
/// CSRF check) of an editor or admin with write access.
fn writer(user: &CurrentUser, method: &AuthMethod) -> Result<(), ApiError> {
    if !matches!(method, AuthMethod::Token { .. }) {
        return Err(ApiError::Forbidden);
    }
    user.require_editor()
}

/// What the write endpoints read (each uses its own fields).
#[derive(serde::Deserialize, Default)]
pub struct Body {
    #[serde(default)]
    pub version_key: Option<String>,
    #[serde(default)]
    pub hours: Option<i64>,
    /// When to run: RFC 3339 with an offset, or `YYYY-MM-DD HH:MM:SS` UTC.
    #[serde(default)]
    pub at: Option<String>,
    /// `approve`, `notify`, `auto`; for a target `null`/`"server"` too.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub health_seconds: Option<i64>,
    #[serde(default)]
    pub require_vv: Option<bool>,
    #[serde(default)]
    pub auto_start: Option<String>,
    #[serde(default)]
    pub auto_end: Option<String>,
}

fn body_of(body: &Bytes) -> Result<Body, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Body::default());
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("not JSON: {e}")))
}

fn time_of(raw: Option<&str>) -> Result<jiff::Timestamp, ApiError> {
    raw.and_then(|a| crate::schedule::parse_at(a, &jiff::tz::TimeZone::UTC))
        .ok_or_else(|| ApiError::bad_request(runs::Refusal::BadTime.message()))
}

fn changed(wren: &crate::Wren) {
    wren.jobs.notify_waiters();
    wren.vv_sync.notify_one();
    let _ = wren.changes.send(());
}

/// Approves (now or at a time), skips, snoozes or sets the mode of a
/// target; shared by the API and MCP. The JSON answer.
pub async fn act(
    wren: &crate::Wren,
    user: &str,
    is_admin: bool,
    action: &str,
    target_id: i64,
    b: Body,
) -> ApiResult<Value> {
    let at = match action {
        "schedule" => Some(time_of(b.at.as_deref())?),
        _ => None,
    };
    let who = user.to_string();
    let action_name = action.to_string();
    let res: Result<Value, runs::Refusal> = wren
        .core
        .with_conn(move |c| {
            Ok(match action_name.as_str() {
                "approve" | "schedule" => {
                    runs::approve_at(c, target_id, b.version_key.as_deref(), &who, at)?.map(
                        |id| json!({ "run_id": id, "status": "queued", "scheduled_for": at.map(crate::schedule::db_time) }),
                    )
                }
                "skip" => runs::skip(c, target_id, &who)?.map(|()| json!({ "skipped": true })),
                "mode" => {
                    let mode = match b.mode.as_deref() {
                        None | Some("" | "server") => None,
                        Some(m) => Some(m.to_string()),
                    };
                    let old = runs::policy(c, target_id)?;
                    let settings = runs::TargetSettings {
                        mode,
                        health_seconds: b
                            .health_seconds
                            .or(old.as_ref().and_then(|p| p.health_seconds)),
                        require_vv: b
                            .require_vv
                            .unwrap_or(old.as_ref().is_some_and(|p| p.require_vv)),
                    };
                    match runs::set_target_policy(c, target_id, &settings, &who, is_admin)? {
                        Ok(()) => {
                            let p = runs::policy(c, target_id)?.unwrap_or_default();
                            Ok(json!({
                                "mode": p.mode,
                                "effective_mode": p.effective(),
                                "updates_itself": p.updates_itself(),
                                "require_vv": p.require_vv,
                            }))
                        }
                        Err(r) => Err(r),
                    }
                }
                _ => runs::snooze(c, target_id, b.hours.unwrap_or(24), &who)?
                    .map(|until| json!({ "snoozed_until": until })),
            })
        })
        .await?;
    let v = res.map_err(ApiError::from)?;
    changed(wren);
    Ok(v)
}

/// Sets a server's mode and auto window; shared by the API and MCP.
pub async fn server_mode(
    wren: &crate::Wren,
    user: &str,
    is_admin: bool,
    server_id: i64,
    b: Body,
) -> ApiResult<Value> {
    let mode = crate::run_pages::parse_mode(b.mode.as_deref().unwrap_or_default())
        .ok()
        .flatten()
        .ok_or_else(|| ApiError::bad_request(vole_kit::i18n::t("runs.err_mode")))?;
    let window = match (b.auto_start.as_deref(), b.auto_end.as_deref()) {
        (None, None) => None,
        (Some(""), Some("")) => Some(None),
        (s, e) => match (
            s.and_then(crate::schedule::normal_hhmm),
            e.and_then(crate::schedule::normal_hhmm),
        ) {
            (Some(s), Some(e)) if s != e => Some(Some((s, e))),
            _ => {
                return Err(ApiError::bad_request(vole_kit::i18n::t("runs.err_window")));
            }
        },
    };
    let who = user.to_string();
    let res = wren
        .core
        .with_conn(move |c| {
            Ok(
                match runs::set_server_mode(c, server_id, mode, window, &who, is_admin)? {
                    Ok(()) => Ok(crate::schedule::window(c, server_id)?),
                    Err(r) => Err(r),
                },
            )
        })
        .await?;
    let w = res.map_err(ApiError::from)?;
    changed(wren);
    Ok(json!({
        "mode": mode,
        "auto_window": w.map(|(a, b)| json!({"start": a, "end": b})),
    }))
}

async fn server_mode_route(
    Extension(wren): Extension<crate::Wren>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    writer(&user, &method)?;
    let b = body_of(&body)?;
    Ok(Json(
        server_mode(&wren, &user.username, user.is_admin(), id, b).await?,
    ))
}

/// `POST /api/v1/runs/{id}/{reschedule,cancel}`.
async fn run_action(
    Extension(wren): Extension<crate::Wren>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path((id, action)): Path<(i64, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    writer(&user, &method)?;
    let b = body_of(&body)?;
    let who = user.username.clone();
    let v = match action.as_str() {
        "reschedule" => {
            let at = time_of(b.at.as_deref())?;
            wren.core
                .with_conn(move |c| runs::reschedule(c, id, at, &who))
                .await?
                .map_err(ApiError::from)?;
            json!({ "run_id": id, "scheduled_for": crate::schedule::db_time(at) })
        }
        "cancel" => {
            wren.core
                .with_conn(move |c| runs::cancel(c, id, &who))
                .await?
                .map_err(ApiError::from)?;
            json!({ "cancelled": true })
        }
        _ => return Err(ApiError::NotFound),
    };
    changed(&wren);
    Ok(Json(v))
}

async fn target_action(
    Extension(wren): Extension<crate::Wren>,
    user: CurrentUser,
    Extension(method): Extension<AuthMethod>,
    Path((id, action)): Path<(i64, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    writer(&user, &method)?;
    if !matches!(
        action.as_str(),
        "approve" | "schedule" | "skip" | "snooze" | "mode"
    ) {
        return Err(ApiError::NotFound);
    }
    let b = body_of(&body)?;
    Ok(Json(
        act(&wren, &user.username, user.is_admin(), &action, id, b).await?,
    ))
}
