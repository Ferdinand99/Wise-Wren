//! What the agent reads from Docker: `GET /version` and `GET /info` over the
//! Unix socket, nothing else (phase 2). A tiny HTTP/1.0 client keeps the
//! agent free of a Docker client crate: the daemon answers HTTP/1.0 with a
//! plain body and closes the connection.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

/// How long one request to Docker may take.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// The most the agent reads of one answer.
const MAX_RESPONSE: usize = 1 << 20;

/// What Docker says about its host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// The host's name (`/info` `Name`).
    pub hostname: String,
    /// The host's operating system (`/info` `OperatingSystem`).
    pub os: String,
    /// The daemon's version (`/version` `Version`).
    pub version: String,
    /// Why Docker couldn't be read; empty when it could.
    pub error: String,
}

/// Reads `/version` and `/info`; a failure ends up in [`Facts::error`].
pub async fn facts(socket: &str) -> Facts {
    let version = match get_json(socket, "/version").await {
        Ok(v) => v,
        Err(error) => {
            return Facts {
                error,
                ..Facts::default()
            };
        }
    };
    let info = get_json(socket, "/info").await;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let mut facts = Facts {
        version: text(&version, "Version"),
        ..Facts::default()
    };
    match info {
        Ok(info) => {
            facts.hostname = text(&info, "Name");
            facts.os = text(&info, "OperatingSystem");
        }
        Err(error) => facts.error = error,
    }
    facts
}

/// `GET path` on the socket, as JSON.
pub async fn get_json(socket: &str, path: &str) -> Result<Value, String> {
    let raw = tokio::time::timeout(TIMEOUT, request(socket, path))
        .await
        .map_err(|_| format!("Docker didn't answer within {} s", TIMEOUT.as_secs()))??;
    let body = parse_response(&raw)?;
    serde_json::from_slice(&body).map_err(|e| format!("Docker sent no JSON for {path}: {e}"))
}

#[cfg(unix)]
async fn request(socket: &str, path: &str) -> Result<Vec<u8>, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| format!("can't open {socket}: {e}"))?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: docker\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("can't write to {socket}: {e}"))?;
    let mut out = Vec::new();
    (&mut stream)
        .take(MAX_RESPONSE as u64)
        .read_to_end(&mut out)
        .await
        .map_err(|e| format!("can't read from {socket}: {e}"))?;
    Ok(out)
}

#[cfg(not(unix))]
async fn request(socket: &str, _path: &str) -> Result<Vec<u8>, String> {
    Err(format!("{socket}: Unix sockets need Linux"))
}

/// The body of a `200` answer (chunked or not).
pub fn parse_response(raw: &[u8]) -> Result<Vec<u8>, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("Docker sent an incomplete answer")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default();
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    if code != "200" {
        return Err(format!("Docker answered {}", status.trim()));
    }
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if chunked {
        dechunk(body)
    } else {
        Ok(body.to_vec())
    }
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("bad chunked answer")?;
        let size = std::str::from_utf8(&body[..end]).map_err(|e| e.to_string())?;
        let size = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "bad chunk size")?;
        body = &body[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size {
            return Err("short chunk".into());
        }
        out.extend_from_slice(&body[..size]);
        body = body.get(size + 2..).unwrap_or_default();
    }
}

/// Whether this looks like Unraid: `/boot/config` with the dockerMan
/// templates and Unraid's Docker manager plugin, under `root` (`/` in the
/// agent; mounted from the host as the spike's decision says).
pub fn is_unraid(root: &Path) -> bool {
    root.join("boot/config/plugins/dockerMan").is_dir()
        && root
            .join("usr/local/emhttp/plugins/dynamix.docker.manager")
            .is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_chunked_answers() {
        let plain =
            b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"Version\":\"29.8.2\"}";
        assert_eq!(parse_response(plain).unwrap(), b"{\"Version\":\"29.8.2\"}");
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), b"{\"a\":1}");
        let err = parse_response(b"HTTP/1.0 403 Forbidden\r\n\r\n").unwrap_err();
        assert!(err.contains("403"), "{err}");
        assert!(parse_response(b"HTTP/1.0 200").is_err());
    }

    #[test]
    fn unraid_detected_from_paths() {
        let root = std::env::temp_dir().join(format!("wren-agent-unraid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("boot/config/plugins/dockerMan")).unwrap();
        assert!(!is_unraid(&root));
        std::fs::create_dir_all(root.join("usr/local/emhttp/plugins/dynamix.docker.manager"))
            .unwrap();
        assert!(is_unraid(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn docker_facts_over_a_unix_socket() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let path =
            std::env::temp_dir().join(format!("wren-agent-docker-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 1024];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                assert!(req.contains("HTTP/1.0"), "{req}");
                let body = if req.starts_with("GET /version ") {
                    r#"{"Version":"29.8.2","ApiVersion":"1.52"}"#
                } else if req.starts_with("GET /info ") {
                    r#"{"Name":"LANBox","OperatingSystem":"Unraid OS 7.4 x86_64"}"#
                } else {
                    panic!("unexpected request {req}");
                };
                let res =
                    format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}");
                s.write_all(res.as_bytes()).await.unwrap();
            }
        });
        let f = facts(path.to_str().unwrap()).await;
        assert_eq!(
            f,
            Facts {
                hostname: "LANBox".into(),
                os: "Unraid OS 7.4 x86_64".into(),
                version: "29.8.2".into(),
                error: String::new(),
            }
        );
        let _ = std::fs::remove_file(&path);
        let missing = facts("/nonexistent/docker.sock").await;
        assert!(missing.error.contains("can't open"), "{missing:?}");
    }
}
