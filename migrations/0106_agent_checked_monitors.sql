-- 0106: agent-checked monitors.
--
-- `check_from_agent_id` names the one remote agent that checks the monitor
-- instead of this server. NULL = the server checks it (as before). That
-- agent's results are the monitor's own status, history and uptime; when it
-- stops reporting the monitor goes down. Deleting the agent falls back to
-- local checks (ON DELETE SET NULL).
ALTER TABLE monitors ADD COLUMN check_from_agent_id INTEGER REFERENCES agents(id) ON DELETE SET NULL;
