-- Docker label discovery: monitors that discovery paused because their
-- container disappeared. Only these are resumed automatically when the
-- container comes back; a monitor a user paused by hand stays paused.
CREATE TABLE IF NOT EXISTS discovery_paused (
    monitor_id INTEGER PRIMARY KEY REFERENCES monitors(id) ON DELETE CASCADE,
    paused_at TEXT NOT NULL DEFAULT (datetime('now'))
);
