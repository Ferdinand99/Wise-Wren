//! The agent protocol, modelled on Vigilant Vole's remote agents: the agent
//! connects **out** to Wren over plain HTTPS with its own token
//! (`Authorization: Bearer wwa_…`), so servers open no ports and it works
//! behind NAT and any reverse proxy.
//!
//! Phase 2 has one endpoint, versioned by URL:
//!
//! - `POST /api/agent/v1/heartbeat` with [`Heartbeat`] (JSON) every
//!   [`INTERVAL_SECONDS`] → `200 {"server": {"id", "name"}, "interval_seconds"}`;
//!   401 for a missing, unknown or revoked token, 429 with `Retry-After`
//!   over [`RATE`], 400 for a body that isn't a heartbeat.
//!
//! - `POST /api/agent/v1/inventory` ([`crate::inventory`]): what runs on
//!   the server, sent on the agent's own schedule and when a heartbeat's
//!   answer says `"scan": true` ("Check now").
//!
//! - `GET /api/agent/v1/jobs` and `POST /api/agent/v1/runs/{id}/steps|finish`
//!   ([`crate::runs`]): approved updates and rollbacks as named, typed jobs
//!   (data, never a command line), fetched with a long-poll and reported
//!   step by step. The agent checks each one against its own `agent.toml`
//!   (`[updates]` allow/deny) and Docker before it acts (§8).
//!
//! [`snippets`] builds the `docker run` lines from the spike's decision
//! (docs/spike-findings.md, "Decision: agent runtime form").

use std::time::Instant;

use axum::Json;
use axum::body::Bytes;
use axum::extract::Extension;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use vole_kit::auth::tokens;
use vole_kit::http::ApiError;
use vole_kit::http::ratelimit::{Rate, too_many_requests};

use crate::Wren;
use crate::db::{self, Facts};

pub const HEARTBEAT_PATH: &str = "/api/agent/v1/heartbeat";
/// How often agents send a heartbeat.
pub const INTERVAL_SECONDS: u64 = 30;
/// Heartbeats one server may send: 20 at once, 20 a minute (restarts and
/// retries included; a healthy agent sends two).
pub const RATE: Rate = Rate::per_minute(20);
/// Longest value kept of each reported fact (characters).
pub const MAX_FACT_CHARS: usize = 200;
/// The agent's image.
pub const AGENT_IMAGE: &str = "ghcr.io/ferdinand99/wise-wren-agent:latest";

/// What an agent reports. Unknown fields are ignored, so newer agents can
/// report more.
#[derive(Debug, Default, Deserialize)]
pub struct Heartbeat {
    #[serde(default)]
    pub agent_version: String,
    /// The host's name, as Docker reports it.
    #[serde(default)]
    pub hostname: String,
    /// The host's operating system, as Docker reports it.
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub docker_version: String,
    /// Why the agent couldn't read Docker, if it couldn't.
    #[serde(default)]
    pub docker_error: String,
    /// Unraid's `/boot/config` and dockerMan paths are there.
    #[serde(default)]
    pub unraid: bool,
    /// A random id of the agent process (0.2.1+). Another one than the
    /// process a running run was handed to means the agent restarted.
    #[serde(default)]
    pub boot_id: String,
    /// Seconds since the agent process started (0.2.1+): runs handed out
    /// before it started can't finish any more.
    #[serde(default)]
    pub uptime_seconds: Option<u64>,
}

/// A boot id as Wren keeps it: letters, digits, `-` and `_`, at most 64;
/// `None` when the agent sent none (an agent before 0.2.1).
pub fn boot_id(raw: &str) -> Option<String> {
    let id: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    (!id.is_empty()).then_some(id)
}

fn clip(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_FACT_CHARS)
        .collect()
}

impl Heartbeat {
    pub fn facts(&self) -> Facts {
        Facts {
            agent_version: clip(&self.agent_version),
            hostname: clip(&self.hostname),
            os: clip(&self.os),
            docker_version: clip(&self.docker_version),
            docker_error: clip(&self.docker_error),
            unraid: self.unraid,
        }
    }
}

/// The server whose agent token `headers` carry: `(id, name)`, or the 401
/// error to answer (401).
pub async fn authenticate(wren: &Wren, headers: &HeaderMap) -> Result<(i64, String), ApiError> {
    let Some(token) = tokens::token_from_headers(headers).filter(|t| db::is_agent_token(t)) else {
        return Err(ApiError::Unauthorized);
    };
    let hash = tokens::hash(&token);
    match wren
        .core
        .with_conn(move |c| db::by_token_hash(c, &hash))
        .await
    {
        Ok(Some(found)) => Ok(found),
        Ok(None) => Err(ApiError::Unauthorized),
        Err(err) => Err(err),
    }
}

/// `POST /api/agent/v1/heartbeat`.
pub async fn heartbeat(
    Extension(wren): Extension<Wren>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (id, name) = match authenticate(&wren, &headers).await {
        Ok(found) => found,
        Err(err) => return err.into_response(),
    };
    if let Err(wait) = wren.budgets.take(&id, RATE, Instant::now()) {
        tracing::warn!(server_id = id, "agent over its heartbeat budget");
        return too_many_requests(wait);
    }
    let beat: Heartbeat = match serde_json::from_slice(&body) {
        Ok(beat) => beat,
        Err(err) => {
            return ApiError::bad_request(format!("not a heartbeat: {err}")).into_response();
        }
    };
    let facts = beat.facts();
    let boot = boot_id(&beat.boot_id);
    let uptime = beat.uptime_seconds;
    let (scan, interrupted) = match wren
        .core
        .with_conn(move |c| {
            db::record_heartbeat(c, id, &facts)?;
            // An agent that restarted mid-run never finishes that run.
            let interrupted = match &boot {
                Some(boot) => crate::runs::close_interrupted(c, id, boot, uptime)?,
                None => Vec::new(),
            };
            Ok((db::scan_requested(c, id)?, interrupted))
        })
        .await
    {
        Ok(found) => found,
        Err(err) => return err.into_response(),
    };
    let _ = wren.changes.send(());
    crate::runs::after_interrupted(&wren, interrupted);
    // `scan` is all Wren can ask for: a fresh inventory (§8).
    Json(json!({
        "server": { "id": id, "name": name },
        "interval_seconds": INTERVAL_SECONDS,
        "scan": scan,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Starting an agent
// ---------------------------------------------------------------------------

/// The commands that start a server's agent.
pub struct Snippets {
    pub unraid: String,
    pub linux: String,
}

/// `value` as one shell word: as it is when it is plainly safe, else in
/// single quotes. The URL can come from the request's `Host` header, so it
/// must never reach a shell unquoted.
pub fn sh_word(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// `docker run` for the agent. Every folder is mounted at the **same path**
/// inside the container (the daemon resolves Compose binds on the host);
/// no `--privileged` and no added capabilities. On Unraid also the paths
/// that let it run Unraid's own `update_container` later (§3).
pub fn docker_run(server_url: &str, token: &str, unraid: bool) -> String {
    let mut lines = vec![
        "docker run -d --name wren-agent --restart unless-stopped".to_string(),
        format!("-e {}", sh_word(&format!("WW_SERVER_URL={server_url}"))),
        format!("-e {}", sh_word(&format!("WW_AGENT_TOKEN={token}"))),
        "-v /var/run/docker.sock:/var/run/docker.sock".to_string(),
    ];
    if unraid {
        lines.extend(
            [
                "-v /mnt/user/appdata/wren-agent:/config",
                "-v /mnt/user/appdata:/mnt/user/appdata",
                "-v /mnt/cache/appdata:/mnt/cache/appdata",
                "-v /usr/local/emhttp:/usr/local/emhttp:ro",
                "-v /var/local/emhttp:/var/local/emhttp",
                "-v /boot/config:/boot/config",
                "-v /var/lib/docker:/var/lib/docker",
            ]
            .map(String::from),
        );
    } else {
        lines.push("-v /etc/wren-agent:/config".to_string());
    }
    lines.push(AGENT_IMAGE.to_string());
    // The agent.toml default, said where the agent is started; on Unraid
    // also the roots that cover appdata and Compose Manager's stacks.
    let roots = if unraid {
        "# Recommended roots in agent.toml: /mnt/user/appdata, /mnt/cache/appdata and\n\
         # /boot/config/plugins/compose.manager/projects (Compose Manager's stacks).\n"
    } else {
        ""
    };
    format!(
        "# agent.toml needs an [updates] allow list: without one the agent refuses every update and\n\
         # rollback. Put this in /config/agent.toml (a list of just the names you want updated is safer):\n\
         #   [updates]\n\
         #   allow = [\"*\"]\n{roots}{}",
        lines.join(" \\\n  ")
    )
}

/// Both commands for a server.
pub fn snippets(server_url: &str, token: &str) -> Snippets {
    Snippets {
        unraid: docker_run(server_url, token, true),
        linux: docker_run(server_url, token, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_run_has_the_spike_mounts() {
        let run = docker_run("http://10.0.3.26:3003", "wwa_abc", true);
        for part in [
            "-e WW_SERVER_URL=http://10.0.3.26:3003",
            "-e WW_AGENT_TOKEN=wwa_abc",
            "-v /var/run/docker.sock:/var/run/docker.sock",
            "-v /mnt/user/appdata:/mnt/user/appdata",
            "-v /usr/local/emhttp:/usr/local/emhttp:ro",
            "-v /var/local/emhttp:/var/local/emhttp",
            "-v /boot/config:/boot/config",
            "-v /var/lib/docker:/var/lib/docker",
            AGENT_IMAGE,
        ] {
            assert!(run.contains(part), "{part} missing in\n{run}");
        }
        assert!(run.contains("/boot/config/plugins/compose.manager/projects"));
        // The agent refuses everything without an [updates] allow list.
        assert!(run.contains("#   [updates]\n#   allow = [\"*\"]"), "{run}");
        for never in ["--privileged", "--cap-add", "--pid"] {
            assert!(!run.contains(never));
        }
        let linux = docker_run("https://wren.example.com", "wwa_abc", false);
        assert!(linux.contains("-v /etc/wren-agent:/config") && !linux.contains("emhttp"));
    }

    #[test]
    fn odd_urls_are_quoted() {
        assert_eq!(
            sh_word("WW_SERVER_URL=http://x:3003"),
            "WW_SERVER_URL=http://x:3003"
        );
        assert_eq!(sh_word("a$(reboot)"), "'a$(reboot)'");
        assert_eq!(sh_word("it's"), r"'it'\''s'");
        let run = docker_run("http://evil$(id)", "wwa_x", false);
        assert!(run.contains("-e 'WW_SERVER_URL=http://evil$(id)'"));
    }

    #[test]
    fn facts_are_trimmed_and_clipped() {
        let beat = Heartbeat {
            hostname: format!("  LAN\u{7}Box{}  ", "x".repeat(300)),
            ..Heartbeat::default()
        };
        let facts = beat.facts();
        assert!(facts.hostname.starts_with("LANBox"));
        assert_eq!(facts.hostname.chars().count(), MAX_FACT_CHARS);
    }
}
