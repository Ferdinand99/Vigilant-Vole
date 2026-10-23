-- vole-kit: passkeys (WebAuthn credentials) and their sign-in challenges.
--
-- A passkey signs a user in on its own: the authenticator checked the user
-- (fingerprint, face, PIN), so it counts as both factors. The password and
-- two-factor sign-in stay as they were.

CREATE TABLE passkeys (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    credential_id BLOB NOT NULL UNIQUE,        -- as the authenticator chose it
    public_key BLOB NOT NULL,                  -- COSE_Key, as the authenticator sent it
    alg INTEGER NOT NULL,                      -- COSE algorithm: -7 ES256, -8 EdDSA, -257 RS256
    user_handle BLOB NOT NULL,                 -- random, the same for all of a user's passkeys
    name TEXT NOT NULL,
    sign_count INTEGER NOT NULL DEFAULT 0,
    transports TEXT NOT NULL DEFAULT '',       -- comma-separated hints from the browser
    backup_eligible INTEGER NOT NULL DEFAULT 0,
    backed_up INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_used_at TEXT
);

CREATE INDEX idx_passkeys_user ON passkeys (user_id);

-- Random, single use, about five minutes. Sign-in challenges have no user.
CREATE TABLE passkey_challenges (
    challenge TEXT PRIMARY KEY,                -- base64url
    purpose TEXT NOT NULL CHECK (purpose IN ('register', 'sign_in')),
    user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    user_handle BLOB,                          -- registration: the handle offered
    expires_at INTEGER NOT NULL                -- unix seconds
) WITHOUT ROWID;
