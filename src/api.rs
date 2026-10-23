//! The REST API v1 and MCP, as stubs for phase 2: the API reference (the
//! kit's pages from [`DOCS`]), `GET /api/v1/servers`, and an MCP server
//! with no tools yet. Design §9 grows both to everything the web UI does.

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

use axum::extract::Query;

use crate::db;
use crate::targets::{self, Filter, Target};

const ERROR_EXAMPLE: &str = r#"{"error": {"code": "not_found", "message": "Not found."}}"#;

const SERVERS_EXAMPLE: &str = r#"{"servers": [{"id": 1, "name": "Tower", "status": "online", "last_seen_at": "2026-10-23 10:00:00", "hostname": "Tower", "os": "Unraid OS 7.4", "docker_version": "29.8.2", "agent_version": "0.1.0", "unraid": true}]}"#;

const HEARTBEAT_EXAMPLE: &str = r#"{"agent_version": "0.1.0", "hostname": "Tower", "os": "Unraid OS 7.4", "docker_version": "29.8.2", "docker_error": "", "unraid": true}"#;

const TARGETS_EXAMPLE: &str = r#"{"targets": [{"id": 7, "server": {"id": 1, "name": "LANBox"}, "kind": "compose", "name": "postiz", "image": "ghcr.io/gitroomhq/postiz-app:latest", "images": 6, "status": "update", "status_detail": "", "available": "2/6", "installed_version": "", "latest_version": "", "version_source": null, "version_error": "", "working_dir": "/boot/config/plugins/compose.manager/projects/postiz", "files_reachable": true, "source_url": "https://github.com/gitroomhq/postiz-app", "release_notes": {"url": "https://github.com/gitroomhq/postiz-app/releases/tag/v2.1.0", "title": "v2.1.0"}, "check_interval_hours": 6, "last_checked_at": "2026-10-23 10:00:00"}]}"#;

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
    can_write: bool,
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
        "Wise Wren keeps the Docker containers on the user's servers up to date. Use list_updates for what has a new version, list_targets for everything Wren watches.".into()
    }

    fn tools(&self) -> Vec<Value> {
        vec![
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
        ]
    }

    async fn call_tool(&self, name: &str, args: Map<String, Value>) -> RpcResult {
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
pub async fn mcp(State(core): State<Core>, headers: HeaderMap, body: Bytes) -> Response {
    let caller = match vole_kit::mcp::authenticate(&core, &headers, 0).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    vole_kit::mcp::serve(
        &Mcp {
            core: core.clone(),
            can_write: caller.can_write,
        },
        &body,
    )
    .await
}
