-- vole-kit 0.8: invitation links (KitFeatures::invites).
--
-- `invites`: a link an admin hands out so that someone can create their own
-- account with a given role. Only the SHA-256 of the link's token is
-- stored (`token_hash`, 32 bytes); the token itself is shown once, when the
-- invitation is created. An invitation works once (`used_at`/`used_by`),
-- until `expires_at`, and not after it was revoked (`revoked_at`).
-- Timestamps are UTC, in SQLite's `datetime('now')` form.
--
-- Not part of the kit's core migrations: a product that turns the feature
-- on runs it (`config::install` checks).

CREATE TABLE invites (
    id INTEGER PRIMARY KEY,
    token_hash BLOB NOT NULL UNIQUE,
    role TEXT NOT NULL CHECK (role IN ('admin', 'editor', 'viewer')),
    display_name TEXT NOT NULL DEFAULT '',
    created_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT NOT NULL,
    used_at TEXT,
    used_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    revoked_at TEXT
);

CREATE INDEX idx_invites_open ON invites (expires_at) WHERE used_at IS NULL AND revoked_at IS NULL;
