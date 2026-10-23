-- Servers (docs/design.md §3, §8): one per Docker host, each with one agent.
-- The agent authenticates with its own token (wwa_…); only its SHA-256 is
-- kept (token_hash, NULL once revoked) with the visible start
-- (token_prefix). The last heartbeat's facts are copied in as they came:
-- what the agent reports about the host, read from Docker's /version and
-- /info. Online/offline is not stored: it is "a heartbeat in the last 90
-- seconds" (last_seen_at, UTC, datetime('now') format).
CREATE TABLE servers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL COLLATE NOCASE UNIQUE,
    token_hash TEXT UNIQUE,
    token_prefix TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT,
    agent_version TEXT NOT NULL DEFAULT '',
    hostname TEXT NOT NULL DEFAULT '',
    os TEXT NOT NULL DEFAULT '',
    docker_version TEXT NOT NULL DEFAULT '',
    docker_error TEXT NOT NULL DEFAULT '',
    unraid INTEGER NOT NULL DEFAULT 0
);
