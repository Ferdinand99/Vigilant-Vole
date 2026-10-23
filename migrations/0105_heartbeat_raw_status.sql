-- The real check result next to the recorded one. During a maintenance
-- window `status` is 'maintenance' (what status pages, badges, uptime and
-- alerting see) while `raw_status` keeps what the check actually found, so
-- the API can show whether a monitor is really up during the window.
-- Outside maintenance both are equal. NULL means unknown: rows recorded
-- during maintenance before this migration.
ALTER TABLE heartbeats ADD COLUMN raw_status TEXT CHECK (raw_status IN ('up', 'down', 'pending'));
