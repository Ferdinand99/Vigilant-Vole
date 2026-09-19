CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS monitors (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    monitor_type TEXT NOT NULL CHECK (monitor_type IN ('http', 'tcp', 'ping')),
    target TEXT NOT NULL,
    interval_seconds INTEGER NOT NULL DEFAULT 60,
    timeout_seconds INTEGER NOT NULL DEFAULT 10,
    retries INTEGER NOT NULL DEFAULT 0,
    active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS heartbeats (
    id INTEGER PRIMARY KEY,
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    status TEXT NOT NULL CHECK (status IN ('up', 'down', 'pending')),
    response_time_ms INTEGER,
    message TEXT,
    checked_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_heartbeats_monitor_id ON heartbeats (monitor_id, checked_at);

CREATE TABLE IF NOT EXISTS notification_channels (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('discord', 'slack', 'telegram', 'webhook')),
    config TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1
);
