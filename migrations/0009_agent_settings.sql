-- Agent settings managed in Wren (0.8.0).
--
-- What an admin chose for a server's agent (docs/agent.md, "Managed
-- settings"), as the JSON the agent receives, with a version that only counts
-- up. The agent keeps its own copy (managed.json) and reports the version it
-- runs.
CREATE TABLE agent_settings (
    server_id INTEGER PRIMARY KEY REFERENCES servers (id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    settings TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Every change: who, when, what (a JSON list of {code, value, widening}).
-- source is 'user' or 'import' (the first start from the agent's agent.toml).
-- widening: the change lets the agent do more than before; admins were told.
CREATE TABLE agent_settings_history (
    id INTEGER PRIMARY KEY,
    server_id INTEGER NOT NULL REFERENCES servers (id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    at TEXT NOT NULL DEFAULT (datetime('now')),
    changed_by TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'user',
    changes TEXT NOT NULL DEFAULT '[]',
    widening INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX agent_settings_history_server ON agent_settings_history (server_id, id);

-- The settings part of the agent's last heartbeat (JSON): NULL for an agent
-- that doesn't report one (before 0.8.0).
ALTER TABLE servers ADD COLUMN settings_report TEXT;
