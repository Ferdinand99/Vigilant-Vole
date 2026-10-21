-- Status page subscribers: visitors can subscribe to incident and
-- maintenance announcements by email (double opt-in) or by webhook.
--
-- `subscribe_channel_id` is the SMTP notification channel used to send the
-- mails; NULL (or a missing/inactive channel) turns email subscriptions off.
-- `subscribe_webhooks` allows webhook subscriptions.
ALTER TABLE status_pages ADD COLUMN subscribe_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE status_pages ADD COLUMN subscribe_channel_id INTEGER REFERENCES notification_channels(id) ON DELETE SET NULL;
ALTER TABLE status_pages ADD COLUMN subscribe_webhooks INTEGER NOT NULL DEFAULT 0;

-- `target` is an address or a URL. `token` is the unguessable secret in
-- confirm/unsubscribe links. `confirmed_at` stays NULL until the owner of the
-- address clicked the link (unconfirmed rows are purged after a few days).
-- `failures` counts consecutive failed deliveries; a subscriber is dropped
-- after too many.
CREATE TABLE status_page_subscribers (
    id INTEGER PRIMARY KEY,
    page_id INTEGER NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('email', 'webhook')),
    target TEXT NOT NULL,
    token TEXT NOT NULL UNIQUE,
    lang TEXT NOT NULL DEFAULT 'en',
    base_url TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    confirmed_at TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    UNIQUE (page_id, kind, target)
);
CREATE INDEX idx_status_page_subscribers_page ON status_page_subscribers(page_id);
