-- vv:foreign_keys=off
--
-- 0002: full data model (Kuma parity).
--
-- Runs inside one transaction with foreign key enforcement disabled (see the
-- marker above and db::apply_one); the runner executes PRAGMA foreign_key_check
-- before committing. Existing tables are rebuilt with the SQLite
-- "create new_X / copy / drop X / rename new_X -> X" procedure so every row is
-- preserved. Heartbeats whose monitor no longer exists (only possible if the
-- database was ever written with foreign keys disabled) are dropped so the
-- final foreign_key_check passes.
-- All timestamps are UTC text 'YYYY-MM-DD HH:MM:SS'.

-- ---------------------------------------------------------------------------
-- users (rebuilt): roles, 2FA, activity. Existing users become admins.
-- ---------------------------------------------------------------------------
CREATE TABLE new_users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'viewer' CHECK (role IN ('admin', 'editor', 'viewer')),
    display_name TEXT NOT NULL DEFAULT '',
    totp_secret TEXT,
    totp_enabled INTEGER NOT NULL DEFAULT 0,
    recovery_codes TEXT,                       -- JSON array of argon2 hashes
    active INTEGER NOT NULL DEFAULT 1,
    last_login_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO new_users (id, username, password_hash, role, created_at)
    SELECT id, username, password_hash, 'admin', created_at FROM users;
DROP TABLE users;
ALTER TABLE new_users RENAME TO users;

-- ---------------------------------------------------------------------------
-- remote agents (needed before heartbeats, which reference it)
-- ---------------------------------------------------------------------------
CREATE TABLE agents (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    location TEXT NOT NULL DEFAULT '',
    prefix TEXT NOT NULL,                      -- first chars of the token, for display/lookup
    token_hash TEXT NOT NULL UNIQUE,           -- sha256 hex
    version TEXT,
    last_seen_at TEXT,
    active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- ---------------------------------------------------------------------------
-- monitors (rebuilt)
-- ---------------------------------------------------------------------------
CREATE TABLE new_monitors (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    monitor_type TEXT NOT NULL CHECK (monitor_type IN (
        'http', 'keyword', 'json_query', 'tcp', 'ping', 'dns', 'push', 'docker',
        'websocket', 'domain_expiry', 'ntp', 'steam', 'group')),
    target TEXT NOT NULL DEFAULT '',
    parent_id INTEGER REFERENCES monitors(id) ON DELETE SET NULL,  -- must be a 'group' (app-validated)
    interval_seconds INTEGER NOT NULL DEFAULT 60,
    retry_interval_seconds INTEGER,            -- NULL = same as interval_seconds
    timeout_seconds INTEGER NOT NULL DEFAULT 10,
    retries INTEGER NOT NULL DEFAULT 0,
    active INTEGER NOT NULL DEFAULT 1,
    upside_down INTEGER NOT NULL DEFAULT 0,
    http_method TEXT NOT NULL DEFAULT 'GET',
    http_headers TEXT,                         -- JSON object
    http_body TEXT,
    http_body_encoding TEXT NOT NULL DEFAULT 'json' CHECK (http_body_encoding IN ('json', 'form', 'raw')),
    accepted_status_codes TEXT NOT NULL DEFAULT '200-399',
    max_redirects INTEGER NOT NULL DEFAULT 10,
    ignore_tls INTEGER NOT NULL DEFAULT 0,
    basic_auth_user TEXT,
    basic_auth_pass TEXT,
    proxy_url TEXT,
    keyword TEXT,
    keyword_invert INTEGER NOT NULL DEFAULT 0,
    json_path TEXT,
    json_operator TEXT NOT NULL DEFAULT '==' CHECK (json_operator IN ('==', '!=', 'contains', '<', '<=', '>', '>=')),
    json_expected TEXT,
    dns_record_type TEXT NOT NULL DEFAULT 'A',
    dns_resolver TEXT,
    dns_expected TEXT,
    docker_host TEXT,
    push_token TEXT UNIQUE,
    expiry_notify INTEGER NOT NULL DEFAULT 1,
    expiry_days INTEGER NOT NULL DEFAULT 14,
    resend_interval INTEGER NOT NULL DEFAULT 0,      -- re-notify every N down checks; 0 = off
    escalation_minutes INTEGER NOT NULL DEFAULT 0,   -- 0 = off
    flap_detection INTEGER NOT NULL DEFAULT 1,
    run_on_agents INTEGER NOT NULL DEFAULT 0,
    confirm_with_agents INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('manual', 'docker', 'config')),
    external_id TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO new_monitors (id, name, monitor_type, target, interval_seconds, timeout_seconds,
                          retries, active, created_at, updated_at)
    SELECT id, name, monitor_type, target, interval_seconds, timeout_seconds,
           retries, active, created_at, created_at
    FROM monitors;
DROP TABLE monitors;
ALTER TABLE new_monitors RENAME TO monitors;
CREATE INDEX idx_monitors_parent ON monitors (parent_id);
CREATE UNIQUE INDEX idx_monitors_external ON monitors (source, external_id) WHERE external_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- heartbeats (rebuilt): agent_id, 'maintenance' status
-- ---------------------------------------------------------------------------
CREATE TABLE new_heartbeats (
    id INTEGER PRIMARY KEY,
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    agent_id INTEGER REFERENCES agents(id) ON DELETE CASCADE,   -- NULL = checked locally
    status TEXT NOT NULL CHECK (status IN ('up', 'down', 'pending', 'maintenance')),
    response_time_ms INTEGER,
    message TEXT,
    checked_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO new_heartbeats (id, monitor_id, status, response_time_ms, message, checked_at)
    SELECT id, monitor_id, status, response_time_ms, message, checked_at
    FROM heartbeats
    WHERE monitor_id IN (SELECT id FROM monitors);
DROP TABLE heartbeats;
ALTER TABLE new_heartbeats RENAME TO heartbeats;
CREATE INDEX idx_heartbeats_monitor_id ON heartbeats (monitor_id, checked_at);
CREATE INDEX idx_heartbeats_checked_at ON heartbeats (checked_at);
CREATE INDEX idx_heartbeats_agent ON heartbeats (agent_id, monitor_id, checked_at);

-- Daily rollups so long-range uptime survives heartbeat retention.
CREATE TABLE heartbeat_daily (
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    day TEXT NOT NULL,                          -- 'YYYY-MM-DD' (UTC)
    up_count INTEGER NOT NULL DEFAULT 0,
    down_count INTEGER NOT NULL DEFAULT 0,
    pending_count INTEGER NOT NULL DEFAULT 0,
    maintenance_count INTEGER NOT NULL DEFAULT 0,
    avg_response_ms REAL,
    PRIMARY KEY (monitor_id, day)
) WITHOUT ROWID;

-- Latest known TLS cert / domain expiry per monitor.
CREATE TABLE monitor_expiry (
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('cert', 'domain')),
    subject TEXT,
    issuer TEXT,
    expires_at TEXT NOT NULL,
    checked_at TEXT NOT NULL DEFAULT (datetime('now')),
    notified_at TEXT,
    PRIMARY KEY (monitor_id, kind)
) WITHOUT ROWID;

-- State-change history (incidents per monitor).
CREATE TABLE monitor_events (
    id INTEGER PRIMARY KEY,
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('down', 'up', 'maintenance', 'paused', 'resumed',
                                       'cert_expiring', 'domain_expiring', 'flapping')),
    message TEXT,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at TEXT,
    duration_seconds INTEGER
);
CREATE INDEX idx_monitor_events_monitor ON monitor_events (monitor_id, started_at);

-- ---------------------------------------------------------------------------
-- tags
-- ---------------------------------------------------------------------------
CREATE TABLE tags (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    color TEXT NOT NULL DEFAULT ''
);
CREATE TABLE monitor_tags (
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    value TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (monitor_id, tag_id)
) WITHOUT ROWID;
CREATE INDEX idx_monitor_tags_tag ON monitor_tags (tag_id);

-- ---------------------------------------------------------------------------
-- notification channels (rebuilt: kind is free text, validated in the app)
-- ---------------------------------------------------------------------------
CREATE TABLE new_notification_channels (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    config TEXT NOT NULL DEFAULT '{}',          -- JSON object
    active INTEGER NOT NULL DEFAULT 1,
    is_default INTEGER NOT NULL DEFAULT 0,      -- auto-attach to new monitors
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
-- Existing channels notified every monitor; keep that by making them defaults
-- (new monitors attach to them) and linking them to every existing monitor below.
INSERT INTO new_notification_channels (id, name, kind, config, active, is_default)
    SELECT id, name, kind, config, active, 1 FROM notification_channels;
DROP TABLE notification_channels;
ALTER TABLE new_notification_channels RENAME TO notification_channels;

CREATE TABLE monitor_notifications (
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    channel_id INTEGER NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
    escalation INTEGER NOT NULL DEFAULT 0,      -- 1 = only notified after escalation_minutes
    PRIMARY KEY (monitor_id, channel_id)
) WITHOUT ROWID;
CREATE INDEX idx_monitor_notifications_channel ON monitor_notifications (channel_id);
INSERT INTO monitor_notifications (monitor_id, channel_id, escalation)
    SELECT m.id, c.id, 0 FROM monitors m CROSS JOIN notification_channels c;

-- ---------------------------------------------------------------------------
-- status pages & incidents
-- ---------------------------------------------------------------------------
CREATE TABLE status_pages (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    custom_domain TEXT UNIQUE,
    published INTEGER NOT NULL DEFAULT 1,
    theme TEXT NOT NULL DEFAULT 'auto' CHECK (theme IN ('auto', 'light', 'dark')),
    show_tags INTEGER NOT NULL DEFAULT 0,
    show_uptime INTEGER NOT NULL DEFAULT 1,
    show_response INTEGER NOT NULL DEFAULT 1,
    footer_text TEXT,
    custom_css TEXT,
    refresh_seconds INTEGER NOT NULL DEFAULT 60,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE status_page_sections (
    id INTEGER PRIMARY KEY,
    status_page_id INTEGER NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    name TEXT NOT NULL DEFAULT '',
    sort_order INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_status_page_sections_page ON status_page_sections (status_page_id, sort_order);
CREATE TABLE status_page_monitors (
    section_id INTEGER NOT NULL REFERENCES status_page_sections(id) ON DELETE CASCADE,
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (section_id, monitor_id)
) WITHOUT ROWID;
CREATE INDEX idx_status_page_monitors_monitor ON status_page_monitors (monitor_id);

CREATE TABLE incidents (
    id INTEGER PRIMARY KEY,
    status_page_id INTEGER NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    severity TEXT NOT NULL DEFAULT 'info' CHECK (severity IN ('info', 'warning', 'danger', 'success')),
    pinned INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    resolved_at TEXT
);
CREATE INDEX idx_incidents_page ON incidents (status_page_id, created_at);

-- ---------------------------------------------------------------------------
-- maintenance windows
-- ---------------------------------------------------------------------------
CREATE TABLE maintenance_windows (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    strategy TEXT NOT NULL DEFAULT 'manual' CHECK (strategy IN ('manual', 'once', 'daily', 'weekly')),
    active INTEGER NOT NULL DEFAULT 1,
    start_at TEXT,                              -- 'once': window start (UTC); recurring: first valid day
    end_at TEXT,
    weekdays TEXT,                              -- 'weekly': e.g. '1,3,5' (ISO, Mon = 1)
    time_from TEXT,                             -- 'HH:MM' in `timezone`
    time_to TEXT,
    timezone TEXT NOT NULL DEFAULT 'UTC',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE maintenance_monitors (
    maintenance_id INTEGER NOT NULL REFERENCES maintenance_windows(id) ON DELETE CASCADE,
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    PRIMARY KEY (maintenance_id, monitor_id)
) WITHOUT ROWID;
CREATE INDEX idx_maintenance_monitors_monitor ON maintenance_monitors (monitor_id);
CREATE TABLE maintenance_status_pages (
    maintenance_id INTEGER NOT NULL REFERENCES maintenance_windows(id) ON DELETE CASCADE,
    status_page_id INTEGER NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    PRIMARY KEY (maintenance_id, status_page_id)
) WITHOUT ROWID;
CREATE INDEX idx_maintenance_status_pages_page ON maintenance_status_pages (status_page_id);

-- ---------------------------------------------------------------------------
-- API tokens & settings
-- ---------------------------------------------------------------------------
CREATE TABLE api_tokens (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,                       -- first chars of the token, for display
    token_hash TEXT NOT NULL UNIQUE,            -- sha256 hex
    scope TEXT NOT NULL DEFAULT 'read' CHECK (scope IN ('read', 'write')),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_used_at TEXT,
    expires_at TEXT
);
CREATE INDEX idx_api_tokens_user ON api_tokens (user_id);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
