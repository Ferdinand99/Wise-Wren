//! What the agent reads from Docker over the Unix socket, `GET`s only:
//! `/version`, `/info`, the container list and container and image inspect
//! (phase 3 discovery). A tiny HTTP/1.0 client keeps the
//! agent free of a Docker client crate: the daemon answers HTTP/1.0 with a
//! plain body and closes the connection.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

/// How long one request to Docker may take.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// The most the agent reads of one answer.
const MAX_RESPONSE: usize = 16 << 20;

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

/// `POST path` with a JSON body on the socket (the agent's only API write:
/// creating a container with the configuration of the one it replaces).
/// Any 2xx answer is fine; its JSON (or null).
pub async fn post_json(socket: &str, path: &str, body: &Value) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let raw = tokio::time::timeout(
        Duration::from_secs(60),
        request(socket, "POST", path, Some(&bytes)),
    )
    .await
    .map_err(|_| "Docker didn't answer within 60 s".to_string())??;
    let (status, body) = split_response(&raw)?;
    if !status
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .starts_with('2')
    {
        return Err(format!(
            "Docker answered {}: {}",
            status.trim(),
            String::from_utf8_lossy(&body).trim()
        ));
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&body).map_err(|e| format!("Docker sent no JSON for {path}: {e}"))
}

/// Runs the `docker` CLI (argv, never a shell) against `socket`; its
/// output on success, else the exit status and the end of what it printed.
pub async fn cli(socket: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let host = format!("unix://{socket}");
    let mut all = vec!["-H", host.as_str()];
    all.extend_from_slice(args);
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(&all)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::process::scrub(&mut cmd);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "docker {} took longer than {} s",
                args.first().unwrap_or(&""),
                timeout.as_secs()
            )
        })?
        .map_err(|e| format!("can't start docker: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text.trim().to_string())
    } else {
        Err(format!(
            "docker {} failed ({}): {}",
            args.first().unwrap_or(&""),
            out.status,
            crate::snapshot::tail(text.trim(), 600)
        ))
    }
}

/// `GET path` on the socket, as JSON.
pub async fn get_json(socket: &str, path: &str) -> Result<Value, String> {
    let raw = tokio::time::timeout(TIMEOUT, request(socket, "GET", path, None))
        .await
        .map_err(|_| format!("Docker didn't answer within {} s", TIMEOUT.as_secs()))??;
    let body = parse_response(&raw)?;
    serde_json::from_slice(&body).map_err(|e| format!("Docker sent no JSON for {path}: {e}"))
}

#[cfg(unix)]
async fn request(
    socket: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| format!("can't open {socket}: {e}"))?;
    let mut req = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.0\r\nHost: docker\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            b.len()
        )
        .into_bytes(),
        None => {
            format!("{method} {path} HTTP/1.0\r\nHost: docker\r\nConnection: close\r\n\r\n")
                .into_bytes()
        }
    };
    if let Some(b) = body {
        req.extend_from_slice(b);
    }
    stream
        .write_all(&req)
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
async fn request(
    socket: &str,
    _method: &str,
    _path: &str,
    _body: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    Err(format!("{socket}: Unix sockets need Linux"))
}

/// The body of a `200` answer (chunked or not).
pub fn parse_response(raw: &[u8]) -> Result<Vec<u8>, String> {
    let (status, body) = split_response(raw)?;
    if status.split_whitespace().nth(1).unwrap_or_default() != "200" {
        return Err(format!("Docker answered {}", status.trim()));
    }
    Ok(body)
}

/// The status line and the (de-chunked) body of any answer.
fn split_response(raw: &[u8]) -> Result<(String, Vec<u8>), String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("Docker sent an incomplete answer")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default().to_string();
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok((status, body))
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
