//! The heartbeat: what the agent reports to Wren, and the client that posts
//! it (`POST /api/agent/v1/heartbeat` with `Authorization: Bearer wwa_…`).

use std::path::Path;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::docker;

pub const PATH: &str = "/api/agent/v1/heartbeat";
/// This agent's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What one heartbeat reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Heartbeat {
    pub agent_version: String,
    pub hostname: String,
    pub os: String,
    pub docker_version: String,
    pub docker_error: String,
    pub unraid: bool,
}

/// Reads Docker on `socket` and looks for Unraid under `root`.
pub async fn collect(socket: &str, root: &Path) -> Heartbeat {
    let facts = docker::facts(socket).await;
    Heartbeat {
        agent_version: VERSION.to_string(),
        hostname: facts.hostname,
        os: facts.os,
        docker_version: facts.version,
        docker_error: facts.error,
        unraid: docker::is_unraid(root),
    }
}

/// Wren's answer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Reply {
    pub server: ServerInfo,
    pub interval_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServerInfo {
    pub id: i64,
    pub name: String,
}

/// Why a heartbeat didn't arrive.
#[derive(Debug)]
pub enum SendError {
    /// Wren refused the token (401/403): revoked, replaced or wrong.
    Refused(String),
    /// Anything else (network, Wren down, 429, 5xx): try again later.
    Failed(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Refused(m) | SendError::Failed(m) => f.write_str(m),
        }
    }
}

/// Posts heartbeats to one Wren.
pub struct Client {
    http: reqwest::Client,
    url: String,
}

impl Client {
    pub fn new(server: &str, token: &str) -> Result<Client, String> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "the token has characters a header can't carry")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        let http = reqwest::Client::builder()
            .user_agent(concat!("WiseWrenAgent/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Client {
            http,
            url: format!("{}{PATH}", server.trim_end_matches('/')),
        })
    }

    pub async fn send(&self, beat: &Heartbeat) -> Result<Reply, SendError> {
        let res = self
            .http
            .post(&self.url)
            .json(beat)
            .send()
            .await
            .map_err(|e| SendError::Failed(format!("can't reach Wren: {e}")))?;
        let status = res.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(SendError::Refused(format!(
                "Wren refused the token ({status}): it was revoked or replaced; copy the new one from the server's page"
            )));
        }
        if !status.is_success() {
            let body: String = res
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            return Err(SendError::Failed(format!("Wren answered {status}: {body}")));
        }
        res.json()
            .await
            .map_err(|e| SendError::Failed(format!("Wren's answer wasn't understood: {e}")))
    }
}
