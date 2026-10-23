-- Kit Demo's store: raw envelopes as clients sent them. Disposable; it
-- lives in its own file with its own migrations.
CREATE TABLE envelopes (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    body BLOB NOT NULL,
    received_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX envelopes_project ON envelopes (project);
