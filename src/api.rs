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

use crate::db;

const ERROR_EXAMPLE: &str = r#"{"error": {"code": "not_found", "message": "Not found."}}"#;

const SERVERS_EXAMPLE: &str = r#"{"servers": [{"id": 1, "name": "Tower", "status": "online", "last_seen_at": "2026-10-23 10:00:00", "hostname": "Tower", "os": "Unraid OS 7.4", "docker_version": "29.8.2", "agent_version": "0.1.0", "unraid": true}]}"#;

const HEARTBEAT_EXAMPLE: &str = r#"{"agent_version": "0.1.0", "hostname": "Tower", "os": "Unraid OS 7.4", "docker_version": "29.8.2", "docker_error": "", "unraid": true}"#;

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
        r("POST", "/api/agent/v1/heartbeat", "agent token", "Agents", "An agent reports that it runs, every 30 seconds. Wren counts a server as online while heartbeats arrive (at most 90 seconds apart). 429 with `Retry-After` above 20 a minute.")
            .req(HEARTBEAT_EXAMPLE)
            .res(r#"{"server": {"id": 1, "name": "Tower"}, "interval_seconds": 30}"#),
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
    Router::new().route("/api/v1/servers", get(servers))
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

/// MCP with no tools yet: enough for clients to connect and see Wren.
struct Mcp {
    can_write: bool,
}

impl Handler for Mcp {
    fn can_write(&self) -> bool {
        self.can_write
    }

    fn instructions(&self) -> String {
        "Wise Wren keeps the Docker containers on the user's servers up to date. It has no tools yet.".into()
    }

    fn tools(&self) -> Vec<Value> {
        Vec::new()
    }

    async fn call_tool(&self, name: &str, _args: Map<String, Value>) -> RpcResult {
        Err((-32602, format!("Unknown tool {name}.")))
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
            can_write: caller.can_write,
        },
        &body,
    )
    .await
}
