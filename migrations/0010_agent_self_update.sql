-- Agent self-update (0.10.0): what the agent's last heartbeat said about it (JSON; NULL before 0.10).
ALTER TABLE servers ADD COLUMN self_update_report TEXT;
-- Automatic agent updates inside the server's auto window (admin; 0.11).
ALTER TABLE servers ADD COLUMN agent_auto_update INTEGER NOT NULL DEFAULT 0;
-- The agent version whose "agent update available" notice went out ('' = none yet).
ALTER TABLE servers ADD COLUMN agent_update_announced TEXT NOT NULL DEFAULT '';
