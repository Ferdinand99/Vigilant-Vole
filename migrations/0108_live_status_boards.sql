-- 0108: Discord live status boards.
--
-- A board keeps one message (or a few, when Discord's limits require) in a
-- channel up to date through the channel's webhook, editing in place.
--
-- `webhook_url` is a secret: it is stored sealed (kit `sealed`), shown
-- nowhere after it was saved and left out of the API, exports and logs.
-- `platform` is derived from the webhook's host ('discord' or 'fluxer').
--
-- `source` is either 'page' (follows one status page: its sections, order,
-- names and visibility) or 'items' (the monitors and groups of
-- `live_board_items`, in `position` order). Deleting the status page of a
-- 'page' board leaves the board in place, paused with an error.
--
-- `state` is 'active' or 'paused'. A board that fails repeatedly pauses
-- itself (`failures`, `last_error`) until someone retries it. `known_down`
-- is a JSON array of the monitor ids that were down at the last refresh,
-- so "post a separate message when something goes down" fires once per
-- outage, also across restarts.
CREATE TABLE live_boards (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    webhook_url TEXT NOT NULL,
    platform TEXT NOT NULL DEFAULT 'discord' CHECK (platform IN ('discord', 'fluxer')),
    source TEXT NOT NULL CHECK (source IN ('page', 'items')),
    status_page_id INTEGER REFERENCES status_pages(id) ON DELETE SET NULL,
    show_uptime_24h INTEGER NOT NULL DEFAULT 1 CHECK (show_uptime_24h IN (0, 1)),
    show_uptime_30d INTEGER NOT NULL DEFAULT 1 CHECK (show_uptime_30d IN (0, 1)),
    show_response INTEGER NOT NULL DEFAULT 0 CHECK (show_response IN (0, 1)),
    show_incidents INTEGER NOT NULL DEFAULT 1 CHECK (show_incidents IN (0, 1)),
    title TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    color TEXT NOT NULL DEFAULT '',
    refresh_seconds INTEGER NOT NULL DEFAULT 300 CHECK (refresh_seconds >= 60),
    notify_down INTEGER NOT NULL DEFAULT 0 CHECK (notify_down IN (0, 1)),
    mention TEXT NOT NULL DEFAULT '',
    lang TEXT NOT NULL DEFAULT 'en',
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'paused')),
    failures INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    last_ok_at TEXT,
    known_down TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_live_boards_page ON live_boards(status_page_id);

-- The monitors and groups of an 'items' board.
CREATE TABLE live_board_items (
    board_id INTEGER NOT NULL REFERENCES live_boards(id) ON DELETE CASCADE,
    monitor_id INTEGER NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (board_id, monitor_id)
);

-- The messages a board keeps, by `slot` (0 = the first). `content_hash` is
-- what was last sent to that message, so an unchanged board sends nothing.
CREATE TABLE live_board_messages (
    board_id INTEGER NOT NULL REFERENCES live_boards(id) ON DELETE CASCADE,
    slot INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    content_hash TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (board_id, slot)
);
