-- vv:foreign_keys=off
--
-- 0097: SQL Server and MongoDB monitor types. The CHECK list also allows the
-- types planned for the same release (tailscale_ping, game_server,
-- grpc_keyword, real_browser) and `grpc_proto` is added now, so the table isn't rebuilt
-- again for them.
--
-- Same procedure as 0095 (SQLite cannot alter a CHECK constraint): build the
-- table with the longer monitor_type list, copy every row and column, drop
-- the old table, rename, recreate the indexes. The columns are those of 0095
-- followed by the HTTP authentication columns 0096 added, in that order and
-- with the same definitions, then the new `grpc_proto`.
CREATE TABLE new_monitors (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    monitor_type TEXT NOT NULL CHECK (monitor_type IN (
        'http', 'keyword', 'json_query', 'tcp', 'ping', 'dns', 'push', 'docker',
        'websocket', 'domain_expiry', 'ntp', 'steam', 'tls', 'smtp', 'redis',
        'postgres', 'mysql', 'mqtt', 'kafka', 'rabbitmq', 'grpc', 'radius', 'snmp', 'sqlserver',
        'mongodb', 'tailscale_ping', 'game_server', 'grpc_keyword',
        'real_browser', 'group')),
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
    secret TEXT,                               -- RADIUS shared secret, SNMP community
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
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    auth_method TEXT NOT NULL DEFAULT 'none',
    oauth_token_url TEXT,
    oauth_client_id TEXT,
    oauth_client_secret TEXT,
    oauth_scope TEXT,
    oauth_audience TEXT,
    oauth_client_auth TEXT NOT NULL DEFAULT 'client_secret_basic',
    tls_client_cert TEXT,
    tls_client_key TEXT,
    tls_ca_cert TEXT,
    grpc_proto TEXT                            -- for the upcoming gRPC keyword type (unused yet)
);
INSERT INTO new_monitors (id, name, description, monitor_type, target, parent_id, interval_seconds, retry_interval_seconds, timeout_seconds, retries, active, upside_down, http_method, http_headers, http_body, http_body_encoding, accepted_status_codes, max_redirects, ignore_tls, basic_auth_user, basic_auth_pass, secret, proxy_url, keyword, keyword_invert, json_path, json_operator, json_expected, dns_record_type, dns_resolver, dns_expected, docker_host, push_token, expiry_notify, expiry_days, resend_interval, escalation_minutes, flap_detection, run_on_agents, confirm_with_agents, source, external_id, sort_order, created_at, updated_at, auth_method, oauth_token_url, oauth_client_id, oauth_client_secret, oauth_scope, oauth_audience, oauth_client_auth, tls_client_cert, tls_client_key, tls_ca_cert)
    SELECT id, name, description, monitor_type, target, parent_id, interval_seconds, retry_interval_seconds, timeout_seconds, retries, active, upside_down, http_method, http_headers, http_body, http_body_encoding, accepted_status_codes, max_redirects, ignore_tls, basic_auth_user, basic_auth_pass, secret, proxy_url, keyword, keyword_invert, json_path, json_operator, json_expected, dns_record_type, dns_resolver, dns_expected, docker_host, push_token, expiry_notify, expiry_days, resend_interval, escalation_minutes, flap_detection, run_on_agents, confirm_with_agents, source, external_id, sort_order, created_at, updated_at, auth_method, oauth_token_url, oauth_client_id, oauth_client_secret, oauth_scope, oauth_audience, oauth_client_auth, tls_client_cert, tls_client_key, tls_ca_cert FROM monitors;
DROP TABLE monitors;
ALTER TABLE new_monitors RENAME TO monitors;
CREATE INDEX idx_monitors_parent ON monitors (parent_id);
CREATE UNIQUE INDEX idx_monitors_external ON monitors (source, external_id) WHERE external_id IS NOT NULL;
