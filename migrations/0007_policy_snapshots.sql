-- What the agent allows, snapshots that count, coded run texts (0.4.0).
--
-- The agent's policy from its last inventory, as JSON: its [updates] allow
-- and deny globs and its roots ({"allow": [...] | null, "deny": [...],
-- "roots": [...]}). NULL: an agent before 0.4.0 that doesn't report it, so
-- Wren can't tell what it would refuse and blocks nothing.
ALTER TABLE servers ADD COLUMN policy TEXT;
-- The host folders the agent's inventory showed (bind mounts and Compose
-- folders), as a JSON list: the agent.toml helper suggests roots from them.
ALTER TABLE servers ADD COLUMN folders TEXT NOT NULL DEFAULT '[]';

-- 1: a run of this target stops before anything is touched when no data
-- path can be snapshotted.
ALTER TABLE targets ADD COLUMN require_snapshot INTEGER NOT NULL DEFAULT 0;

-- A run's result as a code and its values (JSON), shown in the reader's
-- language; detail keeps the English for codes Wren doesn't know.
ALTER TABLE runs ADD COLUMN detail_code TEXT NOT NULL DEFAULT '';
ALTER TABLE runs ADD COLUMN detail_params TEXT NOT NULL DEFAULT '';

-- What the agent noted about a step, as JSON [{"code", "params"}]: for the
-- snapshot, why each path was left out and whether nothing was taken.
ALTER TABLE run_steps ADD COLUMN notes TEXT NOT NULL DEFAULT '';
