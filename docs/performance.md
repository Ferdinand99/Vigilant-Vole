# Performance and load testing

`tools/loadtest/` is a load and soak test harness for Vigilant Vole. It starts the release Docker image
(or a local binary, or targets a server you already run) with a fresh data directory, runs **fake targets on
the same machine** (nothing goes to the internet), seeds monitors, channels, status pages and maintenance
through the REST API, applies dashboard, status-page, API, push and remote-agent load, and samples resource
use and latencies every 10 seconds. It writes a CSV, a Markdown summary and a JSON summary, and exits non-zero
when a threshold is exceeded.

- [Running it](#running-it)
- [What it does](#what-it-does)
- [What is measured](#what-is-measured)
- [Results](#results)
- [What the load test found](#what-the-load-test-found)
- [Sizing notes](#sizing-notes)
- [CI](#ci)

## Running it

Python 3.10 or newer and Docker:

```bash
python -m venv .venv && . .venv/bin/activate      # Windows: .venv\Scripts\activate
pip install -r tools/loadtest/requirements.txt
docker build -t vv-load .
docker pull python:3.12-alpine                     # database probe sidecar

python tools/loadtest/loadtest.py --mode smoke --image vv-load          # 200 monitors, 5 min
python tools/loadtest/loadtest.py --mode load  --image vv-load          # 1000 monitors, 30 min
python tools/loadtest/loadtest.py --mode soak  --image vv-load --hours 2
```

| Mode | Monitors | Measured | History written first | Retention | Purpose |
|---|---|---|---|---|---|
| `smoke` | 200 | 5 min | 1 day | 90 days | CI, weekly on amd64 and arm64 |
| `load` | 1000 | 30 min | 2 days | 90 days | Capacity on a small VPS |
| `soak` | 1000 | `--hours` (2 h default) | 3 days | 1 day | Memory growth, retention and rollups, DB size plateau |

Every mode runs 60 s of warm-up load first (`--warmup`), which is not part of the reported numbers.

Useful options (`--help` lists all):

| Option | Default | |
|---|---|---|
| `--image NAME` / `--binary PATH` / `--url URL` | – | What to test: the Docker image (with `--cpus 1 --memory 512m`, change with `--cpus`/`--memory`), a local binary with a temporary `DATA_DIR`, or a running, freshly installed server (process and database probes are then unavailable; use `--target-host` for how that server reaches this machine) |
| `--monitors`, `--duration`, `--hours` | by mode | Size and length |
| `--sse-clients` | 50 | Open live dashboards (`/events`) |
| `--status-rps` | 20 | Status-page visitors: page, auto-refresh fragment, JSON |
| `--api-rps` | 10 | REST API reads with an API token |
| `--ui-rps` | 0.5 | Dashboard and monitor pages with a session |
| `--bulk-every` | 60 s | A bulk edit (`POST /api/v1/monitors/bulk`) of the `lt-bulk` monitors |
| `--backfill-days` | by mode | Synthetic history written before the load (0 = empty database) |
| `--retention-days` | by mode | The retention setting |
| `--max-*`, `--no-thresholds` | by mode | Pass/fail limits (see `tools/loadtest/report.py`) |
| `--keep` | off | Leave the container, volume or data directory behind for inspection |

Output goes to `tools/loadtest/results/<mode>-<time>/` (ignored by git), or `--out DIR`:

- `samples.csv`: one row per 10 s sample (columns below),
- `summary.md` and `summary.json`: run-wide numbers, thresholds, retention runs seen in the log, warnings
  and errors the server logged, and the query plans of the hot heartbeat queries,
- `server.log`: the server's log.

The reference runs below are kept in [`docs/performance/`](performance/).

## What it does

1. **Starts the server.** Docker mode creates a named volume, runs the image with VPS-like limits and
   `--add-host host.docker.internal:host-gateway`, and starts a `python:3.12-alpine` sidecar on the same volume
   for read-only database probes. Everything is removed afterwards unless `--keep`.
2. **Fake targets** in a separate process: an HTTP server with per-request latency (5–300 ms), keyword and
   JSON bodies, slow (1.5–4 s), failing and **flapping** endpoints (90 s down every 5 min), a webhook sink that
   records every notification, a TCP listener and a small DNS responder for `dns` monitors.
3. **Seeds through the API** (setup wizard, login, an API token and a remote agent created in the UI):
   a realistic mix of http (38 %), keyword (12 %), json-query (10 %), tcp (10 %), dns (5 %), push (5 %),
   remote-agent http (6 %), bulk-edited http (5 %), slow (3 %), failing (2 %), flapping (2 %) and group (2 %)
   monitors, at 20 s–5 min intervals (half at 60 s), 1–3 tags each, a few as group children; 5 webhook channels;
   3 status pages (5×10, 10×30 and 4×25 monitors); a maintenance window every 10 minutes.
4. **Writes history** (`--backfill-days`): with the server stopped, the sidecar inserts one heartbeat per
   interval for every monitor, rolls up the days before yesterday and sets the rollup watermark, as on a server
   that has been running for that long. Then the server starts again. Without this, every query looks fast:
   most of the cost of dashboards, status pages and `/metrics` grows with the heartbeats of the last day(s).
5. **Applies load** at fixed rates (open loop, so slow responses don't slow the load down; at most 500 requests
   in flight): SSE dashboards with different filters, status-page visitors, API readers, the dashboard and
   monitor pages, push clients (twice per interval), a fake remote agent posting results, bulk edits, and a
   Prometheus scrape of `/metrics`.

## What is measured

Every 10 s (`samples.csv`):

| Column(s) | Source |
|---|---|
| `rss_mb`, `rss_peak_mb`, `threads`, `fds`, `cpu_pct` | `/proc/1` in the container (`docker exec`), or `/proc/<pid>` for a local binary on Linux |
| `cgroup_mb`, `cgroup_file_mb` | The container's memory cgroup (includes the page cache of the database) |
| `db_mb`, `wal_mb`, `free_pages`, `heartbeats`, `heartbeat_daily`, `watermark` | The database file and WAL size and row counts, from the sidecar |
| `checks_per_s` | `vigilant_checks_total` from `/metrics` |
| `delay_p50_s`, `delay_p95_s`, `delay_p99_s`, `delay_over1s` | **Check lateness**: how long after its due time a scheduled check started, from the `vigilant_check_delay_seconds` histogram (timer lag plus the wait for a probe slot) |
| `gap_late_*` | Lateness as seen in the heartbeats: gap between consecutive heartbeats of a monitor minus its interval (up → up pairs of active-check monitors, 1 s resolution) |
| `<class>_n`, `_p50`, `_p95`, `_p99`, `_err` | Latency (ms) and errors per endpoint class: `status_page`, `status_fragment`, `status_json`, `api_list`, `api_monitor`, `api_heartbeats`, `api_uptime`, `ui_dashboard`, `ui_monitor`, `push`, `agent_poll`, `agent_results`, `bulk_edit`, `metrics`, `sse_connect` |
| `sse_connected`, `sse_events`, `sse_mb` | Open dashboards, live updates and bytes received |
| `deliveries`, `notif_lat_p95_s`, `detect_p95_s`, `notif_sent`, `notif_failed` | Notifications: webhook deliveries, event time → sink latency, flap → notification latency, and the server's own counters |

## Results

**Test machine.** AMD Ryzen 7 5800X (8 cores, 16 threads), 80 GB RAM, Windows 11 with Docker Desktop 29.9
(WSL 2, Linux 6.18); the server is the amd64 release image built from this branch, run with
`--cpus 1 --memory 512m`. The load generator and fake targets run on the same machine, outside the container's
CPU limit. One Ryzen core is faster than a typical VPS vCPU, so read the absolute numbers as optimistic; the
before/after comparison is what matters. Some runs overlapped in time (each server is limited to one CPU; the
host had cores to spare).

**Before** is the code this branch started from (v0.4.0 + parity round 2) plus only the
`vigilant_check_delay_seconds` metric, so that both sides report the same scheduler numbers. **After** is this
branch; the `load` "after" run predates only the WAL limit and the history "after" run also the phase-spread checks, neither of which matters for those runs (their WAL stayed under 10 MB). The raw data of each run (`summary.md`, `samples.csv`) is in [`docs/performance/`](performance/).

### `load`: 1000 monitors, 2 days of history, 30 minutes

50 live dashboards, 20 status-page requests/s, 10 API requests/s, 0.5 UI requests/s, a bulk edit every minute,
a `/metrics` scrape every 10 s, 50 push monitors, a remote agent with 60 monitors; ~20 checks/s.

| | Before | After |
|---|---|---|
| Server CPU (1 core = 100 %), average | 100 % (saturated) | 61 % |
| Peak RSS | 123 MB | 107 MB |
| Requests failed (60 s client timeout) | 62 % | 0 |
| Status page p50 / p95 | 33 s / 60 s | 15 / 339 ms |
| Status fragment (auto-refresh) p50 / p95 | 33 s / 60 s | 15 / 342 ms |
| `GET /api/v1/monitors/{id}` p50 / p95 | 59 s / 61 s | 5 / 329 ms |
| `GET /api/v1/monitors?limit=100` p50 / p95 | 58 s / 60 s | 108 / 485 ms |
| Dashboard page p50 / p95 | all timed out | 128 / 1016 ms |
| `/metrics` p50 / p95 | all timed out | 606 / 1132 ms |
| Push p95 | all timed out | 206 ms |
| Live dashboard updates received | 0.7/s | 20.5/s |
| Check start delay p95 / p99 | – (`/metrics` never answered) | 0.05 / 0.15 s |
| Heartbeat gaps more than 5 s over the interval | 15 356 | 89 (the one-off phase move after the restart) |
| Notification delivery, event → webhook p95 | 100 s | 0.04 s |
| Flapping target → notification p95 (30 s interval) | 149 s | 29.8 s |
| DB growth / WAL peak | 6.6 MB/h (fewer checks got recorded) / 5.7 MB | 13.5 MB/h / 4.7 MB |

The fixes one at a time (same scenario; the first column used an API mix with more list calls):

| | +shared SSE renders, cached 24 h uptime, single-flight status views | +engine pool, rebalanced API mix | +uptime ranges, rendered-HTML cache, immediate transactions | +phase-spread checks (= After) |
|---|---|---|---|---|
| CPU average | 96 % | 81 % | 72 % | 61 % |
| Status page p95 | 12.1 s | 1.46 s | 728 ms | 339 ms |
| API monitor p95 | 19.2 s | 1.85 s | 857 ms | 329 ms |
| Check delay p99 | 2.6 s | 0.44 s | 0.24 s | 0.15 s |
| Failed requests | 0.21 % | 0.01 % (bulk edits: "database is locked") | 0 | 0 |

### 30 days of history: 200 monitors, 15 minutes

The same load against 200 monitors with 30 days of heartbeats (9.5 million rows, 1.3 GB), which is what a
long-running instance looks like:

| | Before | After |
|---|---|---|
| Server CPU average | 100 % (saturated) | 40 % |
| Requests failed | 82 % | 0 |
| Status page p95 | 56 s | 138 ms |
| API monitor p95 | 61 s | 65 ms |
| `/metrics` p95 | all timed out | 341 ms |
| Dashboard p95 | all timed out | 299 ms |
| Check delay p99 | – | 0.07 s |
| Peak RSS | 114 MB | 78 MB |

### `soak`: 1000 monitors, retention 1 day, 3 days of history, 2 hours

The first retention run (about a minute after start) has to roll up and delete the two oldest days
(2.3 million heartbeats).

| | Before (2 h) | After, 2 h run* | After, clean re-run** (first 13 min) |
|---|---|---|---|
| Server CPU average | 100 % (saturated) | 68 % | ~60 % |
| RSS start / end / peak | 107 / 114 / 128 MB | 73 / 93 / 114 MB (10-minute averages 92–97 MB throughout) | 86 MB at 13 min |
| Requests failed | 81 % | 0 | 0 |
| Status page p95 | 60 s | 907 ms | – |
| API monitor p95 | 61 s | 1.26 s | – |
| Check delay p95 / p99 | – | 0.09 / 0.23 s | 0.03–0.06 s (samples) |
| Notifications delivered / failed | 4 868 with p95 102 s | 4 920 / 0, p95 0.07 s | – |
| First retention run (2.1–2.3 M deletes) | never finished in 2 h | 236 s | 76 s |
| Later hourly runs | – | 6.1 s, 6.6 s | – |
| Heartbeat rows | 4.94 M → 3.88 M (deletes still running) | 4.91 M → 2.78 M | 4.94 M → 2.82 M |
| Database file | 663 MB, flat | 664 MB, flat (freed pages reused) | 664 MB, flat |
| WAL after the retention burst | 22 MB (burst never ran) | **646 MB, stayed** | peaked at 319 MB, **back to 64 MB** |

\* The 2-hour "after" run used the image before the last two fixes (phase-spread checks and the WAL limit),
and for part of its run it shared the machine with a Docker build and another load test, so its latencies are
pessimistic. Memory stayed flat (no leak), no request failed and every notification was delivered.

\** Re-run with every fix; stopped by the owner after 13 minutes of measurement. Its numbers are only the WAL
and retention evidence above (samples in the run's log; not kept in `docs/performance/`).

### `smoke`: 200 monitors, 1 day of history, 5 minutes

What CI runs every week. Even at 200 monitors, one day of history was enough to saturate the old code.

| | Before | After |
|---|---|---|
| Server CPU average | 100 % | 29 % |
| Peak RSS | 100 MB | 75 MB |
| Requests failed | 9.2 % | 0 |
| Status page p95 | 35.7 s | 72 ms |
| API monitor p95 | 52.8 s | 23 ms |
| Dashboard p95 | 58.1 s | 39 ms |
| `/metrics` p95 | all timed out | 206 ms |
| Check delay p95 / p99 | – | 0.010 / 0.034 s |
| Notification delivery p95 | 58 s | 0.004 s |
| Thresholds | FAIL | PASS |

## What the load test found

Each fix has a regression test that fails without it. In order of impact:

1. **Live dashboards rendered once per open tab.** Every change signal woke all `/events` streams and each
   rendered the full monitor list itself: 50 dashboards on 1000 monitors were 50 identical renders of ~1.8 MB
   per update, which alone saturated the CPU. Streams that show the same view (same filters, permission,
   language) now share one render per update, single-flight; a render is only reused if it started after the
   change it is meant to show. `src/api/events.rs` (`shared_render`); test
   `api::events::tests::streams_woken_by_one_update_share_a_render`.
2. **The 24 h uptime of every monitor was recomputed from raw heartbeats on every dashboard render**
   (~1.6 million rows a day at 1000 monitors; 70 % of a 460 ms render). The dashboard and `/metrics` now reuse
   each monitor's 24 h uptime for up to a minute (`db::rollups::uptime_24h_cached`; monitors without data are
   not cached, so a new monitor shows up at once), and the API list, `/api/v1/health` and bulk-edit filters,
   which never showed it, no longer compute it (`db::monitors::list_monitors_with_latest`). Unloaded, on the
   test machine: dashboard 464 → 72 ms, `GET /api/v1/monitors?limit=100` 466 → 86 ms. Tests
   `db::rollups::tests::cached_24h_uptime_is_reused_but_new_monitors_show_up_at_once`,
   `in_memory_databases_are_never_cached`, `db::monitors::tests::dashboard_query_ignores_agent_heartbeats`.
3. **Checks and alerts waited behind the web UI for a database connection.** The pool has
   2 × CPUs connections, so 2 on a 1-vCPU VPS, shared first-come-first-served by page renders and heartbeat
   writes. Under load, checks were recorded minutes late (heartbeat gaps up to 590 s over the interval), and
   alerts left 90–120 s after the event. The check engine now has its own pool of 4 connections
   (`db::build_engine_pool`, wired in `server::prepare`; `Scheduler::shutdown` closes it). Test
   `tests/db_concurrency.rs::heartbeats_are_recorded_while_the_web_pool_is_exhausted`.
4. **Long-range uptime read every retained heartbeat of the window.** `uptime_pct` combines daily rollups
   with raw heartbeats for the partial first day and the days after the rollup watermark, but fetched that raw
   part as one index range over the whole window with a filter, so a 30-day uptime (every `/metrics` scrape,
   badges, the API) or 90-day uptime (every status page build) read every raw heartbeat still retained. It now
   reads the two uncovered ranges only: per monitor, 30 days 5.4 → 0.4 ms, 90 days 14 → 0.4 ms (warm cache;
   cold 66 → 0.6 and 145 → 0.7 ms). With 30 days of history (200 monitors) the old build saturated and timed
   out; the new one ran at 40 % CPU. `src/db/rollups.rs` (`uptime_pct_at`, `RAW_UP_DOWN`); test
   `long_range_uptime_skips_rolled_up_raw_rows` (counts SQLite VM steps).
5. **Status pages: concurrent cache misses each rebuilt the view, and every hit re-rendered the HTML.** The
   view of a public page is cached for 5 s, but when it expired, every request in flight rebuilt it (up to 8
   builds of ~300 ms for a 300-monitor page in the test), and each hit re-rendered megabytes of HTML. Builds are
   now single-flight per page, and the rendered page and fragment are kept with the cached view for anonymous
   visitors (editors and previews still render their own). `src/api/public_status.rs` (`cached_view`,
   `RenderedHtml`, `render_once`); tests `concurrent_cache_misses_build_the_view_once`,
   `anonymous_page_html_is_rendered_once_per_cached_view`.
6. **Monitors created or edited together checked in lockstep forever.** After a reload a monitor's first
   check runs within 1.5 s and then every interval from there, so monitors created by the API, an import, a
   config file or a bulk edit fired together every minute (the check rate swung between 10 and 37/s and page
   p95 spiked to 1.2 s at each burst). The next check after a success now goes to the monitor's own phase slot
   in its interval (once, 0.5–1.5 intervals later), then exactly one interval apart; a late start no longer
   shifts later checks. Failing monitors keep their retry interval. `src/monitor/mod.rs` (`next_due`); tests
   `checks_keep_their_phase_one_interval_apart`, `monitors_created_together_spread_over_the_interval`.
7. **"database is locked" on bulk edits.** A pooled transaction began as a reader and upgraded to a writer at
   its first write; if a heartbeat committed in between, SQLite refused the upgrade immediately (busy
   snapshot), whatever the busy timeout. Every pooled connection now begins transactions `IMMEDIATE` (all of
   them write), so they wait for the lock instead. `src/db/mod.rs` (pool `post_create`); test
   `tests/db_concurrency.rs::pooled_transactions_lock_out_writers_until_they_commit`.
8. **The WAL kept the size of its largest burst.** The first retention run of the soak (2.3 million deletes)
   grew the WAL to 646 MB next to a 664 MB database, and it stayed that size. Connections now set
   `journal_size_limit` to 64 MiB, so the WAL is cut back after the next checkpoint. Test
   `tests/db_concurrency.rs::the_wal_is_cut_back_after_a_burst_of_writes`.

Also added: the **`vigilant_check_delay_seconds`** histogram on `/metrics` (how long after its due time a
scheduled check started, including the wait for a probe slot), documented in [metrics.md](metrics.md) with a
"checks running late" alert. Heartbeat gaps only show lateness that changes from one check to the next; a
scheduler that is constantly 20 s behind has normal gaps.

Looked at and left alone: the hourly rollup re-aggregates yesterday and today in one statement (3.3 million
rows at 1000 monitors) but took 3.6–6.8 s and never blocked a heartbeat write; retention deletes in 5000-row
batches (2.3 million rows in 236 s during the soak) without check delays above 0.3 s; memory stayed flat
(RSS 80–110 MB, no growth over 2 hours); all hot heartbeat queries use indexes (query plans are in each
`summary.md`), so no new index was needed.

## Sizing notes

- **Disk.** Every check is a heartbeat row of about 170 bytes with its indexes. 1000 monitors at the mix above
  (~20 checks/s) write 1.9 million rows and **~320 MB a day**: about 10 GB at 30 days of retention and
  29 GB at the default 90. Disk use is roughly `checks per second × 15 MB × retention days`. Long-range
  uptime (status-page bars, 30/90/365-day uptime) comes from the daily rollups, so a short retention (Settings
  → General) costs only the per-check detail of older days. After lowering the retention the file does not
  shrink (freed pages are reused; the soak's database stayed at 664 MB while 2.3 million rows were deleted and
  new ones written); `VACUUM` with the server stopped gives the space back.
- **Memory.** 80–110 MB resident at 1000 monitors under load, flat over two hours; give the container the page
  cache for the hot part of the database on top (512 MB in all was plenty here).
- **CPU.** One core handled 1000 monitors, 50 open dashboards and 20 status-page requests/s at ~60 %. The
  heaviest parts per request are big pages: a dashboard or `/metrics` with 1000 monitors and a 300-monitor
  status page (2 MB of HTML; rendered once per 5 s and then served from memory). Dashboards and status pages
  get cheaper with filters and fewer monitors per page; `/metrics` is meant to be scraped every 15–60 s, not
  faster.
- **When the scheduler can't keep up**, `vigilant_check_delay_seconds` grows (alert on its p95, see
  [metrics.md](metrics.md)). Probes run at most `VV_MAX_CONCURRENT_CHECKS` (64) at a time.

## CI

`.github/workflows/loadtest.yml` runs `smoke` every Monday and on demand (`workflow_dispatch`, choose `smoke`
or `load`) on `ubuntu-24.04` **and** `ubuntu-24.04-arm`, so every week has a real arm64 run. It builds the
image from the checked-out ref (reusing the Docker image workflow's build cache), runs the harness, shows the
summary on the run page and uploads `samples.csv`, `summary.md`, `summary.json` and `server.log` as the
`loadtest-<mode>-<arch>` artifact. A threshold failure fails the job. The smoke limits
(`THRESHOLDS["smoke"]` in `tools/loadtest/report.py`) leave room for slower shared runners: status pages and
API p95 under 500 ms, check delay p95 under 2 s, heartbeat gaps p95 under 3 s, peak RSS under 200 MB, no
failed requests and no failed notifications.

Shared runners are noisy neighbours; a single slow run is a reason to look at the artifact, not proof of a
regression. Re-run the job before bisecting.
