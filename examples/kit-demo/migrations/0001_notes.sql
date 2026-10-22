-- Kit Demo's own table, after the kit's core set (users, api_tokens,
-- settings).
CREATE TABLE notes (
    id INTEGER PRIMARY KEY,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
