-- vv:foreign_keys=off
--
-- 0100: maintenance windows that repeat every N days, on days of the month,
-- or by cron expression. SQLite cannot alter a CHECK constraint, so the table
-- is rebuilt (same procedure as 0095): create it with the longer strategy
-- list and the new columns, copy every row, drop the old table, rename.
-- `interval_days` and `month_days` go with the time-of-day columns; `cron`
-- and `duration_minutes` stand alone.
CREATE TABLE new_maintenance_windows (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    strategy TEXT NOT NULL DEFAULT 'manual'
        CHECK (strategy IN ('manual', 'once', 'daily', 'weekly', 'interval', 'monthly', 'cron')),
    active INTEGER NOT NULL DEFAULT 1,
    start_at TEXT,                              -- 'once': window start (UTC); recurring: first valid day
    end_at TEXT,
    weekdays TEXT,                              -- 'weekly': e.g. '1,3,5' (ISO, Mon = 1)
    time_from TEXT,                             -- 'HH:MM' in `timezone`
    time_to TEXT,
    timezone TEXT NOT NULL DEFAULT 'UTC',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    skip_until TEXT,
    interval_days INTEGER,                      -- 'interval': every N days, counted from start_at
    month_days TEXT,                            -- 'monthly': e.g. '1,15,-1' (negative = from the end)
    cron TEXT,                                  -- 'cron': five fields, evaluated in `timezone`
    duration_minutes INTEGER                    -- 'cron': length of each occurrence
);
INSERT INTO new_maintenance_windows
    (id, title, description, strategy, active, start_at, end_at, weekdays, time_from, time_to, timezone, created_at, skip_until)
SELECT id, title, description, strategy, active, start_at, end_at, weekdays, time_from, time_to, timezone, created_at, skip_until
FROM maintenance_windows;
DROP TABLE maintenance_windows;
ALTER TABLE new_maintenance_windows RENAME TO maintenance_windows;
