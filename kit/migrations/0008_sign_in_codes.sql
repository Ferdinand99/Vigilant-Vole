-- vole-kit 0.8: one-time sign-in codes and users' email addresses
-- (KitFeatures::sign_in_codes).
--
-- `sign_in_codes`: a code an admin hands to a user who can't sign in, so
-- that they can choose a new password. Only the SHA-256 of the code is
-- stored (`code_hash`, 32 bytes); the code is shown once, when it is
-- created. A code works once (`used_at`), until `expires_at`, and not after
-- it was revoked (`revoked_at`; creating a new code for the user revokes the
-- earlier ones). Timestamps are UTC, in SQLite's `datetime('now')` form.
--
-- `user_emails`: an optional email address per user, where the code is sent
-- when the instance has mail settings. `verified_at` is for a product that
-- confirms addresses; the kit leaves it empty. The `users` table is not
-- changed.
--
-- Not part of the kit's core migrations: a product that turns the feature
-- on runs it (`config::install` checks).

CREATE TABLE sign_in_codes (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash BLOB NOT NULL UNIQUE,
    created_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT NOT NULL,
    used_at TEXT,
    revoked_at TEXT
);

CREATE INDEX idx_sign_in_codes_user ON sign_in_codes (user_id) WHERE used_at IS NULL AND revoked_at IS NULL;

CREATE TABLE user_emails (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    email TEXT NOT NULL,
    verified_at TEXT
);
