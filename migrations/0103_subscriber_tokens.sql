-- Status page subscribers, second pass:
--
-- * `token` becomes `token_hash`: only the SHA-256 of a subscriber's link
--   token is stored. The token itself is derived from `nonce` with the
--   server's secret key (see `src/subscribers.rs`), so a copy of the
--   database alone can't confirm or unsubscribe anybody. Rows created by
--   0102 still hold their plain `vv_…` token here; the server hashes those
--   right after migrating (`db::subscribers::hash_legacy_tokens`), so links
--   that were already sent keep working.
-- * `base_url` goes: links are built from the configured public URL (or the
--   page's custom domain) when a message is sent, never from the Host header
--   of whoever signed up.
CREATE TABLE status_page_subscribers_new (
    id INTEGER PRIMARY KEY,
    page_id INTEGER NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('email', 'webhook')),
    target TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    nonce TEXT NOT NULL DEFAULT '',
    lang TEXT NOT NULL DEFAULT 'en',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    confirmed_at TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    UNIQUE (page_id, kind, target)
);
INSERT INTO status_page_subscribers_new
    (id, page_id, kind, target, token_hash, nonce, lang, created_at, confirmed_at, failures)
SELECT id, page_id, kind, target, token, '', lang, created_at, confirmed_at, failures
FROM status_page_subscribers;
DROP TABLE status_page_subscribers;
ALTER TABLE status_page_subscribers_new RENAME TO status_page_subscribers;
CREATE INDEX idx_status_page_subscribers_page ON status_page_subscribers(page_id);
