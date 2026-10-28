-- vole-kit 0.7: push from the servers through the Hedgerow push relay.
--
-- `push_devices`: the app installs that registered a handle (`hrh_...`) with
-- this server. The handle is a secret: only its SHA-256 (for lookups and
-- duplicate detection) is readable; the handle itself is sealed. A device
-- belongs to a user, and to the API token it registered with when it did
-- so with a token: deleting either removes it.
--
-- `push_registrations`: when each user registered a new handle, to limit how
-- many they can in an hour. Kept apart from `push_devices` so that deleting
-- a device and adding it again doesn't reset the count.
--
-- `push_events`: the text of a push, kept for 24 hours under a random `ref`
-- that the push carries. The app fetches it from this server after the
-- push arrives; the relay and Apple never see it.

CREATE TABLE push_devices (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_id INTEGER REFERENCES api_tokens(id) ON DELETE CASCADE,
    handle_sealed TEXT NOT NULL,
    handle_hash TEXT NOT NULL,                  -- sha256 hex of the handle
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_used_at TEXT,
    UNIQUE (user_id, handle_hash)
);

CREATE INDEX idx_push_devices_token ON push_devices (token_id);
CREATE INDEX idx_push_devices_hash ON push_devices (handle_hash);

CREATE TABLE push_events (
    ref TEXT PRIMARY KEY,                       -- 16 random bytes, base64url
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    kind TEXT NOT NULL,                         -- problem, recovered, ...
    level TEXT NOT NULL,                        -- critical, warning, ok, info
    occurred_at TEXT NOT NULL,                  -- RFC 3339, UTC
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
) WITHOUT ROWID;

CREATE INDEX idx_push_events_created ON push_events (created_at);

CREATE TABLE push_registrations (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_push_registrations_user ON push_registrations (user_id, created_at);
