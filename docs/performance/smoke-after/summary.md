# Vigilant Vole smoke run — after: all fixes

*2026-10-21 16:38 UTC* · **PASS**

## Setup

- 200 monitors, 5 min measured after 60 s warm-up; 50 SSE dashboards, 20 status-page req/s, 10 API req/s, 0.5 UI req/s, bulk edit every 60 s
- Instance: instance: docker, image: vv-load:final3, image_id: sha256:31c24e0936a3, image_arch: amd64, limits: --cpus 1 --memory 512m, docker_host: Docker Desktop x86_64, 16 CPUs, 39.1 GiB
- Load generator host: Windows-10-10.0.26200-SP0 (AMD64), AMD Ryzen 7 5800X 8-Core Processor, 16 cores
- Startup 0.8 s; restart after backfill 0.3 s; monitor create p95 90 ms

## Resources

| | start | end | max |
|---|---|---|---|
| RSS (MB) | 54.8 | 67.2 | 75.5 |
| cgroup memory incl. page cache (MB) | | | 57.2 |
| CPU (% of one core) | avg 29.4 | p95 31.1 | |
| Open FDs / threads | | | 103 / 9 |
| DB file (MB) | 42.57 | 42.70 | growth 1.56 MB/h |
| WAL (MB) | | | 3.98 |
| Heartbeat rows | 317933 | 319174 | 14892 rows/h |

## Scheduler

- 3.9 checks/s recorded (– = `/metrics` never answered in time)
- Start delay vs schedule (`vigilant_check_delay_seconds`, 1056 checks): p50 0.005 s, p95 0.010 s, p99 0.034 s, over 1 s: 0
- Heartbeat gap minus interval (1 s resolution): median p50 0.0 s, p95 of the 10-s samples' p95 1.0 s, worst sample 1.0 s, max 1.0 s, gaps over 5 s: 0

## Latency (ms, whole run)

| class | requests | p50 | p95 | p99 | max | errors |
|---|---|---|---|---|---|---|
| agent_poll | 10 | 11.9 | 173.5 | 173.5 | 173.5 | 0 |
| agent_results | 17 | 6.8 | 86.0 | 86.0 | 86.0 | 0 |
| api_heartbeats | 869 | 3.1 | 23.7 | 109.9 | 281.0 | 0 |
| api_list | 283 | 19.0 | 47.4 | 125.4 | 175.4 | 0 |
| api_monitor | 1076 | 2.8 | 23.1 | 99.2 | 228.3 | 0 |
| api_uptime | 773 | 4.7 | 20.4 | 117.9 | 284.3 | 0 |
| bulk_edit | 4 | 179.8 | 222.2 | 222.2 | 222.2 | 0 |
| metrics | 31 | 57.0 | 205.6 | 209.7 | 209.7 | 0 |
| push | 100 | 4.4 | 16.4 | 120.4 | 120.4 | 0 |
| status_fragment | 2418 | 5.9 | 58.5 | 223.8 | 475.9 | 0 |
| status_json | 624 | 8.0 | 52.1 | 245.3 | 407.4 | 0 |
| status_page | 2959 | 6.2 | 72.1 | 263.6 | 470.8 | 0 |
| ui_dashboard | 81 | 19.3 | 38.7 | 377.1 | 377.1 | 0 |
| ui_monitor | 69 | 9.4 | 58.1 | 86.3 | 86.3 | 0 |

Requests 9314, errors 0 (0.000%), dropped by the in-flight cap 0.

## Live dashboards and notifications

- SSE: 13359 renders received (37.22/s, 8.072 MB/s), reconnects 0, refused 0
- Webhook deliveries by event: {'down': 22, 'up': 20}; app counters sent 59, failed 0
- Delivery latency (event time → sink): p50 0.002 s, p95 0.004 s
- Detection (target flips → webhook, 30 s interval, no retries): p50 10.5 s, p95 25.7 s, max 25.7 s

## Retention runs

- `2026-10-21T16:33:35.987133Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=396 heartbeats_deleted=0 events_deleted=0 retention_days=90 heartbeat_cutoff=2026-07-23 elapsed_ms=402`

## Server warnings and errors

- none

## Thresholds

| check | value | limit | |
|---|---|---|---|
| status p95 (ms) | 72.064 | 500 | ok |
| api p95 (ms) | 47.427 | 500 | ok |
| ui p95 (ms) | 58.107 | 2000 | ok |
| check delay p95 (s) | 0.010 | 2 | ok |
| heartbeat gap p95, 95th-percentile sample (s) | 1.000 | 3 | ok |
| peak RSS (MB) | 75.453 | 200 | ok |
| error rate | 0.000 | 0.0 | ok |
| notification failures | 0.000 | 0 | ok |

## Query plans

- rollup: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=?) / USE TEMP B-TREE FOR GROUP BY
- retention_delete: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_checked_at (checked_at<?)
- latest_local: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- agent_latest: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- chart: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=? AND checked_at>?)
