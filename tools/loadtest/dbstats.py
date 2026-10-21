"""Database probes for the load test. Standard library only.

Runs next to the database: inside a sidecar container that mounts the data
volume (Docker mode) or with the local interpreter (local mode). Opens the
database read-only, except for ``backfill``, which must run while the server
is stopped.

    python dbstats.py stats    DB SINCE_EPOCH LOOKBACK_SECONDS EXCLUDE_TAG [SERVER_START_EPOCH]
    python dbstats.py backfill DB DAYS
    python dbstats.py explain  DB          (query plans of hot queries)

Every command prints one JSON document.
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import time


def connect_ro(db: str) -> sqlite3.Connection:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=10)
    con.execute("PRAGMA busy_timeout = 10000")
    return con


def file_sizes(db: str) -> dict[str, int]:
    out = {}
    for suffix, key in (("", "db_bytes"), ("-wal", "wal_bytes"), ("-shm", "shm_bytes")):
        try:
            out[key] = os.path.getsize(db + suffix)
        except OSError:
            out[key] = 0
    return out


def quantiles(values: list[float]) -> dict[str, float | int | None]:
    if not values:
        return {"n": 0, "p50": None, "p95": None, "p99": None, "max": None, "over5": 0}
    v = sorted(values)

    def q(p: float) -> float:
        return v[min(len(v) - 1, int(p * len(v)))]

    return {
        "n": len(v),
        "p50": q(0.50),
        "p95": q(0.95),
        "p99": q(0.99),
        "max": v[-1],
        "over5": sum(1 for x in v if x > 5),
    }


# Gap between consecutive local heartbeats of a monitor minus its interval.
# Only pairs where the earlier result was up/maintenance (so the next check was
# due one `interval_seconds` later), only active-check types, and not the
# monitors the bulk editor keeps changing. The gap after a monitor's first check
# since the server started is skipped too: that is where the scheduler moves a
# check onto the monitor's phase slot, up to half an interval later on purpose.
LATENESS_SQL = """
WITH recent AS (
    SELECT h.id, h.monitor_id, h.status, CAST(strftime('%s', h.checked_at) AS INTEGER) AS t
    FROM heartbeats h
    WHERE h.agent_id IS NULL AND h.checked_at >= datetime(?1, 'unixepoch')
), gaps AS (
    SELECT monitor_id, t, status,
           LAG(t) OVER w AS pt, LAG(status) OVER w AS ps, LAG(id) OVER w AS pid
    FROM recent
    WINDOW w AS (PARTITION BY monitor_id ORDER BY id)
)
SELECT g.t - g.pt - m.interval_seconds
FROM gaps g JOIN monitors m ON m.id = g.monitor_id
WHERE g.pt IS NOT NULL AND g.t >= ?2
  AND g.ps IN ('up', 'maintenance') AND g.status IN ('up', 'maintenance')
  AND m.active = 1
  AND m.monitor_type IN ('http', 'keyword', 'json_query', 'tcp', 'dns')
  AND m.id NOT IN (
      SELECT mt.monitor_id FROM monitor_tags mt JOIN tags t ON t.id = mt.tag_id WHERE t.name = ?3
  )
  AND g.pid IS NOT (
      SELECT MIN(h2.id) FROM heartbeats h2
      WHERE h2.monitor_id = g.monitor_id AND h2.agent_id IS NULL AND h2.checked_at >= datetime(?4, 'unixepoch')
  )
"""


def stats(db: str, since: int, lookback: int, exclude_tag: str, server_start: int = 0) -> dict:
    out: dict = file_sizes(db)
    con = connect_ro(db)
    try:
        started = time.perf_counter()
        out["page_size"] = con.execute("PRAGMA page_size").fetchone()[0]
        out["page_count"] = con.execute("PRAGMA page_count").fetchone()[0]
        out["freelist_count"] = con.execute("PRAGMA freelist_count").fetchone()[0]
        out["heartbeats"] = con.execute("SELECT COUNT(*) FROM heartbeats").fetchone()[0]
        out["heartbeat_daily"] = con.execute("SELECT COUNT(*) FROM heartbeat_daily").fetchone()[0]
        out["monitor_events"] = con.execute("SELECT COUNT(*) FROM monitor_events").fetchone()[0]
        row = con.execute("SELECT value FROM settings WHERE key = 'engine_rollup_watermark'").fetchone()
        out["watermark"] = row[0] if row else None
        out["count_ms"] = round((time.perf_counter() - started) * 1000, 1)
        started = time.perf_counter()
        gaps = [r[0] for r in con.execute(LATENESS_SQL, (since - lookback, since, exclude_tag, server_start))]
        out["lateness"] = quantiles([float(g) for g in gaps])
        out["lateness_ms"] = round((time.perf_counter() - started) * 1000, 1)
    finally:
        con.close()
    return out


# Same aggregation as the server's hourly rollup (src/db/rollups.rs).
ROLLUP_BEFORE_YESTERDAY = """
INSERT INTO heartbeat_daily
    (monitor_id, day, up_count, down_count, pending_count, maintenance_count, avg_response_ms)
SELECT monitor_id, date(checked_at),
       SUM(status = 'up'), SUM(status = 'down'), SUM(status = 'pending'), SUM(status = 'maintenance'),
       AVG(response_time_ms)
FROM heartbeats
WHERE agent_id IS NULL AND checked_at < date('now', '-1 day')
GROUP BY monitor_id, date(checked_at)
ON CONFLICT (monitor_id, day) DO UPDATE SET
    up_count = excluded.up_count, down_count = excluded.down_count, pending_count = excluded.pending_count,
    maintenance_count = excluded.maintenance_count, avg_response_ms = excluded.avg_response_ms
"""


def backfill(db: str, days: int) -> dict:
    """Synthetic history: one heartbeat per interval for every active
    check/push monitor from ``days`` days ago until now, in chunks so the
    WAL stays small. Days before yesterday are rolled up and the rollup
    watermark is set to yesterday, as on a server that has been running all
    along. The server must be stopped."""
    con = sqlite3.connect(db, timeout=30)
    con.execute("PRAGMA busy_timeout = 30000")
    monitors = con.execute(
        "SELECT id, interval_seconds FROM monitors WHERE active = 1 AND monitor_type <> 'group'"
    ).fetchall()
    now = int(time.time())
    start = now - days * 86400
    inserted = 0
    started = time.perf_counter()
    batch: list[tuple] = []

    def flush() -> None:
        nonlocal inserted
        con.executemany(
            "INSERT INTO heartbeats (monitor_id, status, response_time_ms, message, checked_at) "
            "VALUES (?, ?, ?, ?, datetime(?, 'unixepoch'))",
            batch,
        )
        con.commit()
        inserted += len(batch)
        batch.clear()

    for mid, interval in monitors:
        interval = max(int(interval), 20)
        t = start + (mid * 7919) % interval
        while t < now - 120:
            down = (t // 3600 + mid) % 97 == 0  # ~1% of hours down
            batch.append((mid, "down" if down else "up", None if down else 40 + (t % 37), None, t))
            t += interval
            if len(batch) >= 50_000:
                flush()
    if batch:
        flush()
    con.execute(ROLLUP_BEFORE_YESTERDAY)
    con.execute(
        "INSERT INTO settings (key, value) VALUES ('engine_rollup_watermark', date('now', '-1 day')) "
        "ON CONFLICT (key) DO UPDATE SET value = excluded.value"
    )
    con.commit()
    con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    con.close()
    return {"inserted": inserted, "seconds": round(time.perf_counter() - started, 1), **file_sizes(db)}


EXPLAIN = {
    "rollup": "SELECT monitor_id, date(checked_at), SUM(status = 'up') FROM heartbeats "
    "WHERE agent_id IS NULL AND checked_at >= '2026-01-01' AND checked_at < date('now', '+1 day') "
    "GROUP BY monitor_id, date(checked_at)",
    "retention_delete": "SELECT id FROM heartbeats WHERE checked_at < '2026-01-01' LIMIT 5000",
    "latest_local": "SELECT * FROM heartbeats WHERE monitor_id = 1 AND agent_id IS NULL "
    "ORDER BY checked_at DESC, id DESC LIMIT 1",
    "agent_latest": "SELECT id FROM heartbeats WHERE agent_id = 1 AND monitor_id = 1 "
    "ORDER BY checked_at DESC, id DESC LIMIT 1",
    "chart": "SELECT COUNT(*) FROM heartbeats WHERE monitor_id = 1 AND agent_id IS NULL "
    "AND checked_at >= datetime('now', '-1 day')",
}


def explain(db: str) -> dict:
    con = connect_ro(db)
    try:
        return {
            name: [r[3] for r in con.execute("EXPLAIN QUERY PLAN " + sql)] for name, sql in EXPLAIN.items()
        }
    finally:
        con.close()


def main(argv: list[str]) -> None:
    cmd, db = argv[1], argv[2]
    if cmd == "stats":
        result = stats(db, int(argv[3]), int(argv[4]), argv[5], int(argv[6]) if len(argv) > 6 else 0)
    elif cmd == "backfill":
        result = backfill(db, int(argv[3]))
    elif cmd == "explain":
        result = explain(db)
    else:
        raise SystemExit(f"unknown command {cmd}")
    print(json.dumps(result))


if __name__ == "__main__":
    main(sys.argv)
