-- vv:foreign_keys=off
--
-- 0095: more monitor types (tls, smtp, redis, postgres, mysql, mqtt, kafka, rabbitmq,
-- grpc, radius, snmp).
--
-- SQLite cannot alter a CHECK constraint, so `monitors` is rebuilt with the
-- create new / copy / drop / rename procedure (foreign keys are off for this
-- migration, see db::apply_one). The columns are those of 0002; only
-- the monitor_type list grows and `secret` is new. Indexes are recreated
-- afterwards.
CREATE TABLE new_monitors (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    monitor_type TEXT NOT NULL CHECK (monitor_type IN (
        'http', 'keyword', 'json_query', 'tcp', 'ping', 'dns', 'push', 'docker',
        'websocket', 'domain_expiry', 'ntp', 'steam', 'tls', 'smtp', 'redis',
        'postgres', 'mysql', 'mqtt', 'kafka', 'rabbitmq', 'grpc', 'radius', 'snmp', 'group')),
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
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO new_monitors (id, name, description, monitor_type, target, parent_id, interval_seconds, retry_interval_seconds, timeout_seconds, retries, active, upside_down, http_method, http_headers, http_body, http_body_encoding, accepted_status_codes, max_redirects, ignore_tls, basic_auth_user, basic_auth_pass, proxy_url, keyword, keyword_invert, json_path, json_operator, json_expected, dns_record_type, dns_resolver, dns_expected, docker_host, push_token, expiry_notify, expiry_days, resend_interval, escalation_minutes, flap_detection, run_on_agents, confirm_with_agents, source, external_id, sort_order, created_at, updated_at)
    SELECT id, name, description, monitor_type, target, parent_id, interval_seconds, retry_interval_seconds, timeout_seconds, retries, active, upside_down, http_method, http_headers, http_body, http_body_encoding, accepted_status_codes, max_redirects, ignore_tls, basic_auth_user, basic_auth_pass, proxy_url, keyword, keyword_invert, json_path, json_operator, json_expected, dns_record_type, dns_resolver, dns_expected, docker_host, push_token, expiry_notify, expiry_days, resend_interval, escalation_minutes, flap_detection, run_on_agents, confirm_with_agents, source, external_id, sort_order, created_at, updated_at FROM monitors;
DROP TABLE monitors;
ALTER TABLE new_monitors RENAME TO monitors;
CREATE INDEX idx_monitors_parent ON monitors (parent_id);
CREATE UNIQUE INDEX idx_monitors_external ON monitors (source, external_id) WHERE external_id IS NOT NULL;
