-- Vigilant Vole integration and scheduling (docs/design.md §5, §6, phase 5).
--
-- The auto window of a server, as local HH:MM in the site's time zone
-- (General settings). NULL = no window: nothing on the server is updated
-- automatically, whatever its mode. servers.mode keeps its default
-- 'approve'; only an admin sets 'auto'.
ALTER TABLE servers ADD COLUMN auto_start TEXT;
ALTER TABLE servers ADD COLUMN auto_end TEXT;

-- 1: a run of this target is cancelled before anything stops when the
-- Vigilant Vole maintenance window can't be opened.
ALTER TABLE targets ADD COLUMN require_vv INTEGER NOT NULL DEFAULT 0;

-- What started a run: 'user' (approved now), 'schedule' (approved for
-- scheduled_for, UTC) or 'auto' (the server's auto window).
ALTER TABLE runs ADD COLUMN trigger TEXT NOT NULL DEFAULT 'user';
ALTER TABLE runs ADD COLUMN scheduled_for TEXT;
-- The run's maintenance window in Vigilant Vole and where it stands:
-- '' (none yet), planned (created ahead of a scheduled run), replan (the
-- time changed), open (the run is in it), closed (ended or deleted), none
-- (not needed or not possible; see the run's maintenance step).
ALTER TABLE runs ADD COLUMN vv_window_id INTEGER;
ALTER TABLE runs ADD COLUMN vv_state TEXT NOT NULL DEFAULT '';
CREATE INDEX runs_scheduled ON runs (status, scheduled_for);

-- The Vigilant Vole monitors whose results count for a target's health and
-- go into its maintenance windows: by monitor id or by tag name.
CREATE TABLE target_monitors (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id INTEGER NOT NULL REFERENCES targets (id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    UNIQUE (target_id, kind, ref)
);
