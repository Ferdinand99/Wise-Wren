//! Wren's tables and its migration set. The pool, the runner and the core
//! tables (users, API tokens, settings) are vole-kit's.

use rusqlite::{Connection, OptionalExtension, Row, params};
use vole_kit::auth::tokens;
use vole_kit::db::migrate::{self, MigrationError, MigrationSet};

use crate::product::AGENT_TOKEN_PREFIX;

/// Wren's own migrations, sorted by name. They run after the kit's core set.
const MIGRATION_FILES: &[(&str, &str)] = &[
    (
        "0001_servers.sql",
        include_str!("../migrations/0001_servers.sql"),
    ),
    (
        "0002_channels.sql",
        include_str!("../migrations/0002_channels.sql"),
    ),
    (
        "0003_targets.sql",
        include_str!("../migrations/0003_targets.sql"),
    ),
    ("0004_runs.sql", include_str!("../migrations/0004_runs.sql")),
    (
        "0005_vv_scheduling.sql",
        include_str!("../migrations/0005_vv_scheduling.sql"),
    ),
    (
        "0006_agent_restarts.sql",
        include_str!("../migrations/0006_agent_restarts.sql"),
    ),
    (
        "0007_policy_snapshots.sql",
        include_str!("../migrations/0007_policy_snapshots.sql"),
    ),
];

fn migrations() -> Vec<(String, String)> {
    MIGRATION_FILES
        .iter()
        .map(|(name, sql)| ((*name).to_string(), (*sql).to_string()))
        .collect()
}

fn no_fixups(_: &mut Connection) -> Result<(), MigrationError> {
    Ok(())
}

/// The kit's core set first, then Wren's (a fresh product: nothing to adopt).
pub static MIGRATIONS: MigrationSet = MigrationSet {
    core: migrate::CORE,
    adopt_core: None,
    migrations,
    fk_off_marker: "-- ww:foreign_keys=off",
    baseline: None,
    after: no_fixups,
};

/// How long after its last heartbeat an agent still counts as online: three
/// missed beats of [`crate::agent_api::INTERVAL_SECONDS`].
pub const ONLINE_WINDOW_SECONDS: i64 = 90;

/// How often a server's targets are checked unless changed (hours).
pub const DEFAULT_CHECK_HOURS: i64 = 6;

// ---------------------------------------------------------------------------
// Agent tokens
// ---------------------------------------------------------------------------

/// A new agent token: show `token` once, keep `hash` and `prefix`.
#[derive(Clone)]
pub struct NewToken {
    pub token: String,
    pub hash: String,
    pub prefix: String,
}

/// A random `wwa_…` token: the kit's generator (43 base62 characters,
/// ≈256 bits) with the agent prefix instead of the API token prefix.
pub fn new_agent_token() -> NewToken {
    let api = tokens::generate();
    let body = api
        .strip_prefix(crate::product::TOKEN_PREFIX)
        .unwrap_or(&api);
    let token = format!("{AGENT_TOKEN_PREFIX}{body}");
    NewToken {
        hash: tokens::hash(&token),
        prefix: token.chars().take(AGENT_TOKEN_PREFIX.len() + 6).collect(),
        token,
    }
}

/// Cheap shape check before touching the database.
pub fn is_agent_token(token: &str) -> bool {
    token.strip_prefix(AGENT_TOKEN_PREFIX).is_some_and(|body| {
        body.len() == tokens::TOKEN_BODY_LEN && body.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

// ---------------------------------------------------------------------------
// Servers
// ---------------------------------------------------------------------------

/// Where a server's agent stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// A heartbeat within [`ONLINE_WINDOW_SECONDS`].
    Online,
    /// It connected before, but not lately.
    Offline,
    /// It has a token but never connected.
    Waiting,
    /// Its token was revoked: it can't connect.
    NoToken,
}

impl Status {
    /// The design system's status class (`status-…`).
    pub fn class(self) -> &'static str {
        match self {
            Status::Online => "up",
            Status::Offline => "down",
            Status::Waiting => "pending",
            Status::NoToken => "paused",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Online => "online",
            Status::Offline => "offline",
            Status::Waiting => "waiting",
            Status::NoToken => "no_token",
        }
    }

    /// The translation key of its label.
    pub fn label_key(self) -> &'static str {
        match self {
            Status::Online => "servers.status_online",
            Status::Offline => "servers.status_offline",
            Status::Waiting => "servers.status_waiting",
            Status::NoToken => "servers.status_no_token",
        }
    }
}

/// A server with what its agent last reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub id: i64,
    pub name: String,
    pub has_token: bool,
    pub token_prefix: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub agent_version: String,
    pub hostname: String,
    pub os: String,
    pub docker_version: String,
    pub docker_error: String,
    pub unraid: bool,
    /// A heartbeat within [`ONLINE_WINDOW_SECONDS`].
    pub recent: bool,
}

impl Server {
    pub fn status(&self) -> Status {
        if !self.has_token {
            Status::NoToken
        } else if self.recent {
            Status::Online
        } else if self.last_seen_at.is_none() {
            Status::Waiting
        } else {
            Status::Offline
        }
    }
}

fn columns() -> String {
    format!(
        "id, name, token_hash IS NOT NULL, token_prefix, created_at, last_seen_at, agent_version,
         hostname, os, docker_version, docker_error, unraid,
         COALESCE(last_seen_at >= datetime('now', '-{ONLINE_WINDOW_SECONDS} seconds'), 0)"
    )
}

fn server_row(r: &Row<'_>) -> rusqlite::Result<Server> {
    Ok(Server {
        id: r.get(0)?,
        name: r.get(1)?,
        has_token: r.get(2)?,
        token_prefix: r.get(3)?,
        created_at: r.get(4)?,
        last_seen_at: r.get(5)?,
        agent_version: r.get(6)?,
        hostname: r.get(7)?,
        os: r.get(8)?,
        docker_version: r.get(9)?,
        docker_error: r.get(10)?,
        unraid: r.get(11)?,
        recent: r.get(12)?,
    })
}

/// Every server, by name.
pub fn servers(c: &Connection) -> rusqlite::Result<Vec<Server>> {
    c.prepare(&format!(
        "SELECT {} FROM servers ORDER BY name COLLATE NOCASE, id",
        columns()
    ))?
    .query_map([], server_row)?
    .collect()
}

pub fn server(c: &Connection, id: i64) -> rusqlite::Result<Option<Server>> {
    c.query_row(
        &format!("SELECT {} FROM servers WHERE id = ?1", columns()),
        [id],
        server_row,
    )
    .optional()
}

/// Whether another server already has this name (case-insensitive).
pub fn name_taken(c: &Connection, name: &str) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT EXISTS (SELECT 1 FROM servers WHERE name = ?1)",
        [name],
        |r| r.get(0),
    )
}

/// Adds a server with its first agent token; its id.
pub fn insert_server(c: &Connection, name: &str, token: &NewToken) -> rusqlite::Result<i64> {
    c.execute(
        "INSERT INTO servers (name, token_hash, token_prefix) VALUES (?1, ?2, ?3)",
        params![name, token.hash, token.prefix],
    )?;
    Ok(c.last_insert_rowid())
}

/// Replaces the server's agent token (`None`: revokes it). The old one stops
/// working at once.
pub fn set_token(c: &Connection, id: i64, token: Option<&NewToken>) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "UPDATE servers SET token_hash = ?2, token_prefix = ?3 WHERE id = ?1",
        params![
            id,
            token.map(|t| t.hash.as_str()),
            token.map_or("", |t| t.prefix.as_str())
        ],
    )? > 0)
}

pub fn delete_server(c: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(c.execute("DELETE FROM servers WHERE id = ?1", [id])? > 0)
}

/// The server whose agent token hashes to `hash`: `(id, name)`.
pub fn by_token_hash(c: &Connection, hash: &str) -> rusqlite::Result<Option<(i64, String)>> {
    c.query_row(
        "SELECT id, name FROM servers WHERE token_hash = ?1",
        [hash],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// What one heartbeat reports, already trimmed to size.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub agent_version: String,
    pub hostname: String,
    pub os: String,
    pub docker_version: String,
    pub docker_error: String,
    pub unraid: bool,
}

/// Records a heartbeat: now, and what the agent reported.
pub fn record_heartbeat(c: &Connection, id: i64, f: &Facts) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "UPDATE servers SET last_seen_at = datetime('now'), agent_version = ?2, hostname = ?3,
             os = ?4, docker_version = ?5, docker_error = ?6, unraid = ?7
         WHERE id = ?1",
        params![
            id,
            f.agent_version,
            f.hostname,
            f.os,
            f.docker_version,
            f.docker_error,
            f.unraid
        ],
    )? > 0)
}

/// Whether "Check now" asked this server's agent for a fresh inventory.
pub fn scan_requested(c: &Connection, id: i64) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT scan_requested_at IS NOT NULL FROM servers WHERE id = ?1",
        [id],
        |r| r.get(0),
    )
}

/// The ids of the servers that are online now (the watcher compares them).
pub fn online_ids(c: &Connection) -> rusqlite::Result<Vec<i64>> {
    c.prepare(&format!(
        "SELECT id FROM servers WHERE token_hash IS NOT NULL
             AND last_seen_at >= datetime('now', '-{ONLINE_WINDOW_SECONDS} seconds')
         ORDER BY id"
    ))?
    .query_map([], |r| r.get(0))?
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_tokens_have_their_own_prefix() {
        crate::product::install();
        let t = new_agent_token();
        assert!(t.token.starts_with("wwa_") && is_agent_token(&t.token));
        assert_eq!(t.token.len(), 4 + 43);
        assert_eq!(t.prefix, &t.token[..10]);
        assert_eq!(t.hash, tokens::hash(&t.token));
        assert_ne!(t.token, new_agent_token().token);
        assert!(!is_agent_token("ww_abc"));
        assert!(!is_agent_token(&tokens::generate()));
    }

    #[test]
    fn status_follows_token_and_heartbeats() {
        let mut s = Server {
            id: 1,
            name: "Tower".into(),
            has_token: true,
            token_prefix: String::new(),
            created_at: String::new(),
            last_seen_at: None,
            agent_version: String::new(),
            hostname: String::new(),
            os: String::new(),
            docker_version: String::new(),
            docker_error: String::new(),
            unraid: false,
            recent: false,
        };
        assert_eq!(s.status(), Status::Waiting);
        s.last_seen_at = Some("2026-10-23 10:00:00".into());
        assert_eq!(s.status(), Status::Offline);
        s.recent = true;
        assert_eq!(s.status(), Status::Online);
        s.has_token = false;
        assert_eq!(s.status(), Status::NoToken);
    }
}
