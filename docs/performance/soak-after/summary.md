# Vigilant Vole soak run — after: all fixes

*2026-10-21 16:24 UTC* · **FAIL**

## Setup

- 1000 monitors, 120 min measured after 60 s warm-up; 50 SSE dashboards, 20 status-page req/s, 10 API req/s, 0.5 UI req/s, bulk edit every 60 s
- Instance: instance: docker, image: vv-load:final, image_id: sha256:010f5e3c6021, image_arch: amd64, limits: --cpus 1 --memory 512m, docker_host: Docker Desktop x86_64, 16 CPUs, 39.1 GiB
- Load generator host: Windows-10-10.0.26200-SP0 (AMD64), AMD64 Family 25 Model 33 Stepping 0, AuthenticAMD, 16 cores
- Retention 1 day(s); backfill: {'inserted': 4941092, 'seconds': 480.9, 'db_bytes': 695779328, 'wal_bytes': 0, 'shm_bytes': 0}
- Startup 0.9 s; restart after backfill 0.6 s; monitor create p95 295 ms

## Resources

| | start | end | max |
|---|---|---|---|
| RSS (MB) | 72.9 | 92.8 | 114.4 |
| cgroup memory incl. page cache (MB) | | | 512.0 |
| CPU (% of one core) | avg 68.1 | p95 98.7 | |
| Open FDs / threads | | | 328 / 21 |
| DB file (MB) | 663.95 | 663.95 | growth 0.00 MB/h |
| WAL (MB) | | | 646.49 |
| Heartbeat rows | 4908717 | 2781597 | -1063471 rows/h |

## Scheduler

- 20.3 checks/s recorded
- Start delay vs schedule (`vigilant_check_delay_seconds`, 131905 checks): p50 0.006 s, p95 0.086 s, p99 0.233 s, over 1 s: 26
- Heartbeat gap minus interval (1 s resolution): median p50 0.0 s, worst 10-s-sample p95 5.0 s, max 7.0 s, gaps over 5 s: 14

## Latency (ms, whole run)

| class | requests | p50 | p95 | p99 | max | errors |
|---|---|---|---|---|---|---|
| agent_poll | 231 | 18.3 | 1589.7 | 3949.8 | 4453.4 | 0 |
| agent_results | 1321 | 22.7 | 1423.5 | 3164.8 | 8659.8 | 0 |
| api_heartbeats | 21363 | 7.8 | 1235.2 | 2763.9 | 8973.2 | 0 |
| api_list | 7181 | 144.2 | 1505.8 | 3053.4 | 9239.2 | 0 |
| api_monitor | 25418 | 8.0 | 1263.6 | 2727.4 | 9159.2 | 0 |
| api_uptime | 18039 | 10.8 | 1270.2 | 2818.7 | 9077.6 | 0 |
| bulk_edit | 119 | 189.6 | 2572.6 | 5521.1 | 6409.0 | 0 |
| metrics | 721 | 778.8 | 2991.8 | 4907.8 | 9805.8 | 0 |
| push | 11940 | 16.8 | 817.7 | 2120.8 | 4569.1 | 0 |
| status_fragment | 57786 | 17.0 | 918.7 | 2109.2 | 6673.5 | 0 |
| status_json | 14482 | 21.5 | 463.2 | 1179.5 | 5998.3 | 0 |
| status_page | 71733 | 17.3 | 907.2 | 2097.0 | 7109.0 | 0 |
| ui_dashboard | 1823 | 150.1 | 3316.5 | 7680.4 | 16688.0 | 0 |
| ui_monitor | 1778 | 19.3 | 2234.8 | 4922.7 | 11322.6 | 0 |

Requests 233935, errors 0 (0.000%), dropped by the in-flight cap 0.

## Live dashboards and notifications

- SSE: 135787 renders received (18.7/s, 14.62 MB/s), reconnects 50, refused 0
- Webhook deliveries by event: {'down': 2400, 'up': 2400, 'maintenance_start': 60, 'maintenance_end': 60}; app counters sent 4891, failed 0
- Delivery latency (event time → sink): p50 0.003 s, p95 0.070 s
- Detection (target flips → webhook, 30 s interval, no retries): p50 19.0 s, p95 29.1 s, max 43.5 s

## Retention runs

- `2026-10-21T14:28:27.537820Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=1980 heartbeats_deleted=2316636 events_deleted=0 retention_days=1 heartbeat_cutoff=2026-10-20 elapsed_ms=235613`
- `2026-10-21T15:24:37.716137Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=1980 heartbeats_deleted=0 events_deleted=0 retention_days=1 heartbeat_cutoff=2026-10-20 elapsed_ms=6089`
- `2026-10-21T16:24:38.040630Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=1980 heartbeats_deleted=0 events_deleted=0 retention_days=1 heartbeat_cutoff=2026-10-20 elapsed_ms=6616`

## Server warnings and errors

- none

## Thresholds

| check | value | limit | |
|---|---|---|---|
| status p95 (ms) | 918.749 | 1000 | ok |
| api p95 (ms) | 1505.758 | 1000 | **FAIL** |
| ui p95 (ms) | 3316.451 | 3000 | **FAIL** |
| check delay p95 (s) | 0.086 | 2 | ok |
| heartbeat gap p95, worst sample (s) | 5.000 | 3 | **FAIL** |
| peak RSS (MB) | 114.383 | 384 | ok |
| error rate | 0.000 | 0.0 | ok |
| notification failures | 0.000 | 0 | ok |

## Query plans

- rollup: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=?) / USE TEMP B-TREE FOR GROUP BY
- retention_delete: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_checked_at (checked_at<?)
- latest_local: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- agent_latest: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- chart: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=? AND checked_at>?)
