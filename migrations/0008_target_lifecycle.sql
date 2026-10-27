-- What happens to a target's settings and to new targets (0.7.0).
--
-- 1: a target Wren saw for the first time on a server in 'auto' mode starts
-- with the explicit mode 'approve', and this flag, until an admin confirms
-- that it may update automatically ("Allow auto"). Setting the target's
-- policy by hand also clears it.
ALTER TABLE targets ADD COLUMN auto_pending INTEGER NOT NULL DEFAULT 0;

-- The settings of a target that dropped out of an inventory, kept for 30
-- days under (server, kind, name). When the same target comes back inside
-- that time its settings and monitor links return with it, so a container
-- that is re-created (or left out of one scan) doesn't fall back to the
-- server's mode. Older rows are purged by the next inventory. monitors is a
-- JSON list of {kind, ref, label}.
CREATE TABLE target_memory (
    server_id INTEGER NOT NULL REFERENCES servers (id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    mode TEXT,
    health_seconds INTEGER,
    require_vv INTEGER NOT NULL DEFAULT 0,
    require_snapshot INTEGER NOT NULL DEFAULT 0,
    check_interval_hours INTEGER,
    skipped_key TEXT NOT NULL DEFAULT '',
    snoozed_until TEXT,
    auto_pending INTEGER NOT NULL DEFAULT 0,
    monitors TEXT NOT NULL DEFAULT '[]',
    gone_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (server_id, kind, name)
);
