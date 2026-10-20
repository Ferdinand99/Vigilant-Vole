-- Status page extras: logo and favicon uploads, password protection and
-- analytics (built-in counters plus an optional third-party provider).

-- Uploaded images live in the database so a backup stays one file. `kind` is
-- 'logo' | 'favicon'; `mime` is taken from the file's magic bytes (never from
-- the upload's name or Content-Type); `sha256` (hex) doubles as the ETag.
CREATE TABLE status_page_assets (
    page_id INTEGER NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('logo', 'favicon')),
    mime TEXT NOT NULL,
    bytes BLOB NOT NULL,
    sha256 TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (page_id, kind)
);

-- Password protection: an Argon2id PHC string, NULL = public. Every change or
-- removal bumps `password_version`, which revokes existing unlock cookies.
ALTER TABLE status_pages ADD COLUMN password_hash TEXT;
ALTER TABLE status_pages ADD COLUMN password_version INTEGER NOT NULL DEFAULT 0;

-- Built-in analytics (on by default) and a third-party provider:
-- 'none' | 'plausible' | 'umami' | 'matomo' | 'google'.
ALTER TABLE status_pages ADD COLUMN analytics_enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE status_pages ADD COLUMN analytics_unique INTEGER NOT NULL DEFAULT 1;
ALTER TABLE status_pages ADD COLUMN analytics_provider TEXT NOT NULL DEFAULT 'none';
ALTER TABLE status_pages ADD COLUMN analytics_site_id TEXT NOT NULL DEFAULT '';
ALTER TABLE status_pages ADD COLUMN analytics_url TEXT NOT NULL DEFAULT '';

-- Daily counters (UTC days). No cookies, IP addresses or user agents are
-- stored: unique visitors are told apart in memory with a salted hash whose
-- salt rotates, and is forgotten, every day.
CREATE TABLE status_page_views (
    page_id INTEGER NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    day TEXT NOT NULL,
    views INTEGER NOT NULL DEFAULT 0,
    visitors INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (page_id, day)
) WITHOUT ROWID;

CREATE TABLE status_page_referrers (
    page_id INTEGER NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    day TEXT NOT NULL,
    domain TEXT NOT NULL,
    views INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (page_id, day, domain)
) WITHOUT ROWID;
