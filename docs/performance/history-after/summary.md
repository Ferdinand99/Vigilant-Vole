# Vigilant Vole load run — after, 30 days of history

*2026-10-21 14:49 UTC* · **PASS**

## Setup

- 200 monitors, 15 min measured after 60 s warm-up; 50 SSE dashboards, 20 status-page req/s, 10 API req/s, 0.5 UI req/s, bulk edit every 60 s
- Instance: instance: docker, image: vv-load:final, image_id: sha256:010f5e3c6021, image_arch: amd64, limits: --cpus 1 --memory 512m, docker_host: Docker Desktop x86_64, 16 CPUs, 39.1 GiB
- Load generator host: Windows-10-10.0.26200-SP0 (AMD64), AMD64 Family 25 Model 33 Stepping 0, AuthenticAMD, 16 cores
- Startup 1.2 s; restart after backfill 0.4 s; monitor create p95 116 ms

## Resources

| | start | end | max |
|---|---|---|---|
| RSS (MB) | 59.7 | 67.4 | 78.3 |
| cgroup memory incl. page cache (MB) | | | 59.7 |
| CPU (% of one core) | avg 40.2 | p95 46.0 | |
| Open FDs / threads | | | 124 / 9 |
| DB file (MB) | 1283.63 | 1284.02 | growth 1.56 MB/h |
| WAL (MB) | | | 4.06 |
| Heartbeat rows | 9538560 | 9542315 | 15020 rows/h |

## Scheduler

- 3.9 checks/s recorded
- Start delay vs schedule (`vigilant_check_delay_seconds`, 3148 checks): p50 0.005 s, p95 0.023 s, p99 0.066 s, over 1 s: 0
- Heartbeat gap minus interval (1 s resolution): median p50 0.0 s, worst 10-s-sample p95 1.0 s, max 1.0 s, gaps over 5 s: 0

## Latency (ms, whole run)

| class | requests | p50 | p95 | p99 | max | errors |
|---|---|---|---|---|---|---|
| agent_poll | 30 | 8.6 | 351.4 | 564.9 | 564.9 | 0 |
| agent_results | 80 | 8.3 | 117.0 | 699.7 | 699.7 | 0 |
| api_heartbeats | 2755 | 3.8 | 66.7 | 222.5 | 757.0 | 0 |
| api_list | 909 | 27.2 | 116.9 | 285.0 | 525.0 | 0 |
| api_monitor | 3099 | 4.0 | 64.8 | 299.4 | 629.0 | 0 |
| api_uptime | 2238 | 6.0 | 65.2 | 292.6 | 656.0 | 0 |
| bulk_edit | 14 | 40.5 | 291.3 | 291.3 | 291.3 | 0 |
| metrics | 91 | 149.4 | 341.4 | 476.8 | 476.8 | 0 |
| push | 300 | 5.7 | 56.0 | 244.0 | 418.8 | 0 |
| status_fragment | 7331 | 10.0 | 125.9 | 395.7 | 1030.7 | 0 |
| status_json | 1756 | 11.4 | 130.4 | 357.9 | 939.3 | 0 |
| status_page | 8914 | 10.4 | 137.8 | 410.1 | 1187.6 | 0 |
| ui_dashboard | 215 | 30.4 | 299.5 | 469.3 | 545.3 | 0 |
| ui_monitor | 235 | 13.4 | 122.3 | 206.7 | 434.9 | 0 |

Requests 27967, errors 0 (0.000%), dropped by the in-flight cap 0.

## Live dashboards and notifications

- SSE: 33156 renders received (34.57/s, 7.68 MB/s), reconnects 0, refused 0
- Webhook deliveries by event: {'down': 61, 'up': 60, 'maintenance_start': 5, 'maintenance_end': 5}; app counters sent 135, failed 0
- Delivery latency (event time → sink): p50 0.002 s, p95 0.025 s
- Detection (target flips → webhook, 30 s interval, no retries): p50 5.4 s, p95 20.5 s, max 26.3 s

## Retention runs

- `2026-10-21T14:34:32.289116Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=396 heartbeats_deleted=0 events_deleted=0 retention_days=90 heartbeat_cutoff=2026-07-23 elapsed_ms=1806`

## Server warnings and errors

- none

## Thresholds

| check | value | limit | |
|---|---|---|---|
| status p95 (ms) | 137.773 | 1000 | ok |
| api p95 (ms) | 116.868 | 1000 | ok |
| ui p95 (ms) | 299.487 | 3000 | ok |
| check delay p95 (s) | 0.023 | 2 | ok |
| heartbeat gap p95, worst sample (s) | 1.000 | 3 | ok |
| peak RSS (MB) | 78.289 | 384 | ok |
| error rate | 0.000 | 0.0 | ok |
| notification failures | 0.000 | 0 | ok |

## Query plans

- rollup: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=?) / USE TEMP B-TREE FOR GROUP BY
- retention_delete: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_checked_at (checked_at<?)
- latest_local: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- agent_latest: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- chart: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=? AND checked_at>?)
