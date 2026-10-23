-- Update runs (docs/design.md §5, §6, phase 4).
--
-- mode: what Wren may do with a server's targets. 'approve' (the default):
-- an editor approves each version; 'notify': Wren only tells. 'auto' comes
-- in phase 5. A target's mode NULL means the server's.
ALTER TABLE servers ADD COLUMN mode TEXT NOT NULL DEFAULT 'approve';
-- When the "agent offline" notification went out for the current outage
-- (cleared, with a recovery message, when the agent is back).
ALTER TABLE servers ADD COLUMN offline_notified_at TEXT;

ALTER TABLE targets ADD COLUMN mode TEXT;
-- How long the target must run (and be healthy) after an update before it
-- counts as good; NULL = 180 seconds.
ALTER TABLE targets ADD COLUMN health_seconds INTEGER;
-- "Skip this version": the available_key skipped; a newer one shows again.
ALTER TABLE targets ADD COLUMN skipped_key TEXT NOT NULL DEFAULT '';
-- "Snooze": hidden from the Updates list until then (UTC).
ALTER TABLE targets ADD COLUMN snoozed_until TEXT;

-- One update or rollback of one target. A run exists only because someone
-- approved it (requested_by); target_* are copied so the history outlives
-- the target. status: queued (approved, waiting for the agent), running,
-- then succeeded, rolled_back, failed, rollback_failed, refused, cancelled
-- or lapsed (a newer version appeared before it started).
CREATE TABLE runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    server_id INTEGER NOT NULL REFERENCES servers (id) ON DELETE CASCADE,
    target_id INTEGER REFERENCES targets (id) ON DELETE SET NULL,
    target_kind TEXT NOT NULL,
    target_name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'update',
    rollback_of INTEGER REFERENCES runs (id) ON DELETE SET NULL,
    from_key TEXT NOT NULL DEFAULT '',
    to_key TEXT NOT NULL DEFAULT '',
    shown TEXT NOT NULL DEFAULT '',
    job TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'queued',
    detail TEXT NOT NULL DEFAULT '',
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    requested_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    started_at TEXT,
    finished_at TEXT,
    last_report_at TEXT
);
CREATE INDEX runs_target ON runs (target_id, id);
CREATE INDEX runs_server_status ON runs (server_id, status);

-- Each step of a run as the agent reported it; log is the end of its output.
CREATE TABLE run_steps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id INTEGER NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
    step TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    duration_ms INTEGER NOT NULL DEFAULT 0,
    log TEXT NOT NULL DEFAULT '',
    UNIQUE (run_id, step)
);

-- Who did what: approve, skip, snooze, cancel, rollback, mode, ...
CREATE TABLE actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL DEFAULT (datetime('now')),
    username TEXT NOT NULL,
    action TEXT NOT NULL,
    target_id INTEGER,
    run_id INTEGER,
    detail TEXT NOT NULL DEFAULT ''
);
