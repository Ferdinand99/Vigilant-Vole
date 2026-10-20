-- 0099: real-browser monitors.
--
-- `remote_browsers` are Chrome DevTools Protocol endpoints (an external
-- headless Chromium such as browserless or chromedp/headless-shell) that
-- `real_browser` monitors drive. `url` is ws(s)://host:port/devtools/browser/<id>,
-- http(s)://host:port (discovered through /json/version) or a browserless-style
-- wss://host?token=…; a token or password in it is a secret and is masked
-- wherever it leaves the database.
--
-- The monitor_type CHECK already allows 'real_browser' (0097), so the monitor
-- columns are plain ALTER TABLE ADD COLUMN (no table rebuild):
--   remote_browser_id  the browser to use (NULL = the first configured one)
--   browser_selector   CSS selector that must match (instead of `keyword`)
--   browser_wait       'load' | 'network_idle'
--   browser_viewport   'desktop' (1280×800) | 'mobile' (390×844)
--   browser_js_errors  1 = an uncaught JavaScript exception fails the check
CREATE TABLE remote_browsers (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    url TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

ALTER TABLE monitors ADD COLUMN remote_browser_id INTEGER REFERENCES remote_browsers(id) ON DELETE SET NULL;
ALTER TABLE monitors ADD COLUMN browser_selector TEXT;
ALTER TABLE monitors ADD COLUMN browser_wait TEXT NOT NULL DEFAULT 'load';
ALTER TABLE monitors ADD COLUMN browser_viewport TEXT NOT NULL DEFAULT 'desktop';
ALTER TABLE monitors ADD COLUMN browser_js_errors INTEGER NOT NULL DEFAULT 0;

-- The latest failure screenshot per monitor (JPEG, size-capped by the app,
-- overwritten on every new failure). Never shown on public pages.
CREATE TABLE monitor_screenshots (
    monitor_id INTEGER PRIMARY KEY REFERENCES monitors(id) ON DELETE CASCADE,
    image BLOB NOT NULL,
    mime TEXT NOT NULL DEFAULT 'image/jpeg',
    message TEXT,
    taken_at TEXT NOT NULL DEFAULT (datetime('now'))
);
