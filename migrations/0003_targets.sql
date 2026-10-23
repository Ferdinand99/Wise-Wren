-- Discovery and version checks (docs/design.md §4, phase 3).
--
-- How often a server's targets are checked (hours; a target may override
-- it), whether "Check now" asked its agent for a fresh scan (the heartbeat
-- answers with the fixed flag "scan"; the next inventory clears it), and
-- when the last inventory came.
ALTER TABLE servers ADD COLUMN check_interval_hours INTEGER NOT NULL DEFAULT 6;
ALTER TABLE servers ADD COLUMN scan_requested_at TEXT;
ALTER TABLE servers ADD COLUMN inventory_at TEXT;
ALTER TABLE servers ADD COLUMN inventory_error TEXT NOT NULL DEFAULT '';

-- What Wren watches on a server: a container, an Unraid template
-- container, a Compose stack or a script stack (kind). Rebuilt from every
-- inventory; what Wren learned itself (checks, notes, what it notified) stays.
-- status is current, update or unknown, derived from the images' local
-- digests, remote_digests and, for script stacks, the versions.
-- available_key identifies the available version (one notification each).
CREATE TABLE targets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    server_id INTEGER NOT NULL REFERENCES servers (id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    working_dir TEXT NOT NULL DEFAULT '',
    files_reachable INTEGER NOT NULL DEFAULT 1,
    source_url TEXT NOT NULL DEFAULT '',
    version_source TEXT NOT NULL DEFAULT '',
    installed_version TEXT NOT NULL DEFAULT '',
    latest_version TEXT NOT NULL DEFAULT '',
    latest_url TEXT NOT NULL DEFAULT '',
    version_error TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'unknown',
    status_detail TEXT NOT NULL DEFAULT '',
    available TEXT NOT NULL DEFAULT '',
    available_key TEXT NOT NULL DEFAULT '',
    check_interval_hours INTEGER,
    check_requested INTEGER NOT NULL DEFAULT 0,
    last_checked_at TEXT,
    notes_url TEXT NOT NULL DEFAULT '',
    notes_title TEXT NOT NULL DEFAULT '',
    notified_key TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (server_id, kind, name)
);

-- The containers of a target with the image each runs: image is
-- .Config.Image (never docker ps), local_digest the RepoDigests entry for
-- its repository ('' when there is none). status, detail and remote_digest
-- are derived, like the target's.
CREATE TABLE target_images (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id INTEGER NOT NULL REFERENCES targets (id) ON DELETE CASCADE,
    container TEXT NOT NULL,
    service TEXT NOT NULL DEFAULT '',
    image TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT '',
    local_digest TEXT NOT NULL DEFAULT '',
    remote_digest TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'unknown',
    detail TEXT NOT NULL DEFAULT '',
    UNIQUE (target_id, container)
);

-- The registry's last answer per image reference (registry/repo:tag),
-- shared by every target that uses it. error is set when the check failed.
CREATE TABLE remote_digests (
    image TEXT PRIMARY KEY,
    digest TEXT NOT NULL DEFAULT '',
    media_type TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    checked_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Credentials per registry (host as in image references, docker.io for
-- Docker Hub). secret is the password or token, sealed with AES-256-GCM
-- under a key derived from the instance's key; never shown again.
CREATE TABLE registries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    host TEXT NOT NULL COLLATE NOCASE UNIQUE,
    username TEXT NOT NULL,
    secret BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- A channel with digest on gets one message per check run.
ALTER TABLE notification_channels ADD COLUMN digest INTEGER NOT NULL DEFAULT 0;
