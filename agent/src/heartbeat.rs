//! The heartbeat: what the agent reports to Wren, and the client that posts
//! it (`POST /api/agent/v1/heartbeat` with `Authorization: Bearer wwa_…`).

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::docker;

pub const PATH: &str = "/api/agent/v1/heartbeat";
/// This agent's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// This agent process: a random id and when it started. Wren closes a run
/// handed to another process (the agent restarted mid-run: nothing will
/// finish it).
pub struct Boot {
    pub id: String,
    started: Instant,
}

impl Boot {
    pub fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

/// This process's [`Boot`], made on the first call (at start).
pub fn boot() -> &'static Boot {
    static BOOT: OnceLock<Boot> = OnceLock::new();
    BOOT.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // Randomly keyed per process: what the clock doesn't add.
        let salt = RandomState::new().hash_one(nanos);
        Boot {
            id: format!("{nanos:x}-{:x}-{salt:x}", std::process::id()),
            started: Instant::now(),
        }
    })
}

/// What one heartbeat reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Heartbeat {
    pub agent_version: String,
    pub hostname: String,
    pub os: String,
    pub docker_version: String,
    pub docker_error: String,
    pub unraid: bool,
    /// This process ([`boot`]).
    pub boot_id: String,
    pub uptime_seconds: u64,
    /// Managed settings (0.8.0+): the version in force, whether the agent is
    /// locked to `agent.toml`, the folders mounted into it. Filled in by the
    /// agent loop; older agents send nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<crate::managed::Report>,
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
        boot_id: boot().id.clone(),
        uptime_seconds: boot().uptime_seconds(),
        settings: None,
    }
}

/// Wren's answer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Reply {
    pub server: ServerInfo,
    pub interval_seconds: u64,
    /// Wren asks for a fresh inventory ("Check now"): the only thing it
    /// can ask.
    #[serde(default)]
    pub scan: bool,
    /// Settings an admin made in Wren (0.8.0+; absent from older servers,
    /// and while the agent is locked or already runs this version). Data
    /// only: [`crate::managed`] checks it before anything is used.
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
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

/// The most the agent reads of Wren's answer to a heartbeat.
pub const MAX_HEARTBEAT_BYTES: usize = 64 << 10;
/// ... of a job (or the news that there is none).
pub const MAX_JOB_BYTES: usize = 256 << 10;
/// ... of a monitor verdict or a step's answer.
pub const MAX_VERDICT_BYTES: usize = 64 << 10;
/// ... of the body of an error answer (shown in the log).
pub const MAX_ERROR_BYTES: usize = 4 << 10;

/// At most `limit` bytes of a response's body, and whether more followed.
/// Reads chunk by chunk and stops at the limit: a server can't make the
/// agent buffer a body of any size.
pub async fn read_head(
    mut res: reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        let room = limit - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// Reads Wren's answer as JSON, refusing one larger than `limit` bytes (by
/// its `Content-Length` before reading, else when the body passes it).
pub async fn read_json<T: serde::de::DeserializeOwned>(
    res: reqwest::Response,
    limit: usize,
) -> Result<T, String> {
    let too_big = || format!("Wren's answer is larger than {} KiB", limit >> 10);
    if res.content_length().is_some_and(|n| n > limit as u64) {
        return Err(too_big());
    }
    let (body, more) = read_head(res, limit)
        .await
        .map_err(|e| format!("Wren's answer wasn't understood: {e}"))?;
    if more {
        return Err(too_big());
    }
    serde_json::from_slice(&body).map_err(|e| format!("Wren's answer wasn't understood: {e}"))
}

/// Posts heartbeats and inventories to one Wren.
pub struct Client {
    http: reqwest::Client,
    url: String,
    inventory_url: String,
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
            // An answer is the answer: a redirect would carry the bodies
            // (the inventory) somewhere else.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let server = server.trim_end_matches('/');
        Ok(Client {
            http,
            url: format!("{server}{PATH}"),
            inventory_url: format!("{server}{}", crate::discover::PATH),
        })
    }

    pub async fn send(&self, beat: &Heartbeat) -> Result<Reply, SendError> {
        let res = self.post(&self.url, beat).await?;
        read_json(res, MAX_HEARTBEAT_BYTES)
            .await
            .map_err(SendError::Failed)
    }

    /// Sends what runs on this server.
    pub async fn inventory(&self, inv: &crate::discover::Inventory) -> Result<(), SendError> {
        self.post(&self.inventory_url, inv).await.map(|_| ())
    }

    async fn post(&self, url: &str, body: &impl Serialize) -> Result<reqwest::Response, SendError> {
        let res = self
            .http
            .post(url)
            .json(body)
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
            // Only a little of it, without control characters: it is logged.
            let (head, _) = read_head(res, MAX_ERROR_BYTES).await.unwrap_or_default();
            let body: String = String::from_utf8_lossy(&head)
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect();
            return Err(SendError::Failed(format!("Wren answered {status}: {body}")));
        }
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// Answers the first connection with `response` and closes; the address.
    async fn serve(response: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            // The reader may stop before the end: not an error here.
            let _ = sock.write_all(&response).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}")
    }

    fn response(head: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {head}\r\nConnection: close\r\n\r\n").into_bytes();
        out.extend_from_slice(body);
        out
    }

    async fn get(url: &str) -> reqwest::Response {
        reqwest::get(url).await.unwrap()
    }

    #[tokio::test]
    async fn answers_within_the_limit_are_read() {
        let body = br#"{"a": 1}"#;
        let head = format!("200 OK\r\nContent-Length: {}", body.len());
        let url = serve(response(&head, body)).await;
        let v: serde_json::Value = read_json(get(&url).await, 1024).await.unwrap();
        assert_eq!(v["a"], 1);
    }

    #[tokio::test]
    async fn a_big_content_length_is_refused_up_front() {
        let url = serve(response("200 OK\r\nContent-Length: 10000000", b"{}")).await;
        let err = read_json::<serde_json::Value>(get(&url).await, 64 << 10)
            .await
            .unwrap_err();
        assert_eq!(err, "Wren's answer is larger than 64 KiB");
    }

    #[tokio::test]
    async fn a_body_without_a_length_stops_at_the_limit() {
        let big = vec![b' '; 300 << 10];
        let url = serve(response("200 OK", &big)).await;
        let err = read_json::<serde_json::Value>(get(&url).await, 64 << 10)
            .await
            .unwrap_err();
        assert!(err.contains("larger than 64 KiB"), "{err}");
        // An error body is read up to its limit only.
        let url = serve(response("500 Internal Server Error", &big)).await;
        let (head, more) = read_head(get(&url).await, MAX_ERROR_BYTES).await.unwrap();
        assert_eq!((head.len(), more), (MAX_ERROR_BYTES, true));
    }

    #[test]
    fn settings_in_wrens_answer_are_optional() {
        // An older Wren sends none; a newer one may send more than this agent knows.
        let old: Reply =
            serde_json::from_str(r#"{"server":{"id":1,"name":"a"},"interval_seconds":30}"#)
                .unwrap();
        assert!(old.settings.is_none() && !old.scan);
        let newer: Reply = serde_json::from_str(
            r#"{"server":{"id":1,"name":"a"},"interval_seconds":30,"scan":true,"settings":{"version":2,"future":[]},"other":1}"#,
        )
        .unwrap();
        assert_eq!(newer.settings.unwrap()["version"], 2);
        // A heartbeat without a settings report serializes as before.
        let json = serde_json::to_value(Heartbeat::default()).unwrap();
        assert!(json.get("settings").is_none());
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let url = serve(response(
            "307 Temporary Redirect\r\nLocation: http://127.0.0.1:9/elsewhere\r\nContent-Length: 0",
            b"",
        ))
        .await;
        let client = Client::new(&url, "wwa_x").unwrap();
        let Err(SendError::Failed(msg)) = client.send(&Heartbeat::default()).await else {
            panic!("a redirect is not an answer");
        };
        assert!(msg.contains("307"), "{msg}");
    }
}
