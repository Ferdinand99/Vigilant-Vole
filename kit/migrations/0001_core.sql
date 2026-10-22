-- vole-kit core schema: users, API tokens and the key/value settings.
--
-- Every statement is written exactly as Vigilant Vole's migrations left it in
-- sqlite_master (users was rebuilt and then grew three columns, hence the
-- quoted name and the odd last line), so a product that starts from this file
-- and Vigilant Vole, which adopted it (0104_adopt_kit_core.sql), have the same
-- tables. Vigilant Vole's tests compare the two.

CREATE TABLE "users" (
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
, session_version INTEGER NOT NULL DEFAULT 1, totp_last_step INTEGER, language TEXT);

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
