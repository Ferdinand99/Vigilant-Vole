# Vigilant Vole smoke run — before

*2026-10-21 16:45 UTC* · **FAIL**

## Setup

- 200 monitors, 5 min measured after 60 s warm-up; 50 SSE dashboards, 20 status-page req/s, 10 API req/s, 0.5 UI req/s, bulk edit every 60 s
- Instance: instance: docker, image: vv-load:base, image_id: sha256:18c07ce50d9d, image_arch: amd64, limits: --cpus 1 --memory 512m, docker_host: Docker Desktop x86_64, 16 CPUs, 39.1 GiB
- Load generator host: Windows-10-10.0.26200-SP0 (AMD64), AMD Ryzen 7 5800X 8-Core Processor, 16 cores
- Startup 0.8 s; restart after backfill 0.3 s; monitor create p95 84 ms

## Resources

| | start | end | max |
|---|---|---|---|
| RSS (MB) | 91.8 | 96.8 | 99.6 |
| cgroup memory incl. page cache (MB) | | | 85.6 |
| CPU (% of one core) | avg 99.6 | p95 99.9 | |
| Open FDs / threads | | | 718 / 6 |
| DB file (MB) | 42.56 | 42.67 | growth 1.33 MB/h |
| WAL (MB) | | | 4.09 |
| Heartbeat rows | 317843 | 318870 | 12448 rows/h |

## Scheduler

- – checks/s recorded (– = `/metrics` never answered in time)
- Start delay vs schedule (`vigilant_check_delay_seconds`, – checks): p50 – s, p95 – s, p99 – s, over 1 s: –
- Heartbeat gap minus interval (1 s resolution): median p50 4.0 s, p95 of the 10-s samples' p95 52.0 s, worst sample 52.0 s, max 54.0 s, gaps over 5 s: 647

## Latency (ms, whole run)

| class | requests | p50 | p95 | p99 | max | errors |
|---|---|---|---|---|---|---|
| agent_poll | 3 | 44983.5 | 57042.4 | 57042.4 | 57042.4 | 0 |
| agent_results | 0 | – | – | – | – | 2 |
| api_heartbeats | 605 | 30566.6 | 53373.2 | 57768.8 | 59494.2 | 53 |
| api_list | 197 | 30875.6 | 53085.3 | 57459.4 | 57868.7 | 10 |
| api_monitor | 712 | 29600.3 | 52764.5 | 57298.4 | 59583.1 | 67 |
| api_uptime | 506 | 29808.1 | 52282.0 | 57575.2 | 58756.2 | 50 |
| bulk_edit | 1 | 43997.8 | 43997.8 | 43997.8 | 43997.8 | 0 |
| metrics | 0 | – | – | – | – | 28 |
| push | 10 | 50871.7 | 59876.6 | 59876.6 | 59876.6 | 16 |
| status_fragment | 1492 | 18986.3 | 35796.5 | 39528.6 | 40137.0 | 99 |
| status_json | 367 | 53.9 | 22461.1 | 27135.4 | 27728.2 | 9 |
| status_page | 1837 | 18316.2 | 35708.5 | 39806.2 | 40571.4 | 104 |
| ui_dashboard | 5 | 55046.7 | 58143.2 | 58143.2 | 58143.2 | 66 |
| ui_monitor | 17 | 53593.8 | 58663.1 | 58663.1 | 58663.1 | 36 |

Requests 5868, errors 540 (9.202%), dropped by the in-flight cap 3254.

Error samples:

- agent_results: TimeoutError: 
- api_heartbeats: ClientConnectionError: Connector is closed.
- api_list: ClientConnectionError: Connector is closed.
- api_monitor: ClientConnectionError: Connector is closed.
- api_uptime: ClientConnectionError: Connector is closed.
- metrics: TimeoutError: 
- push: TimeoutError: 
- status_fragment: ClientConnectionError: Connector is closed.
- status_json: ClientConnectionError: Connector is closed.
- status_page: ClientConnectionError: Connector is closed.
- ui_dashboard: TimeoutError: 
- ui_monitor: TimeoutError: 

## Live dashboards and notifications

- SSE: 646 renders received (1.35/s, 0.4 MB/s), reconnects 0, refused 0
- Webhook deliveries by event: {'down': 32, 'maintenance_start': 5, 'up': 18, 'maintenance_end': 5}; app counters sent 0, failed 0
- Delivery latency (event time → sink): p50 40.030 s, p95 58.300 s
- Detection (target flips → webhook, 30 s interval, no retries): p50 63.2 s, p95 114.8 s, max 114.8 s

## Retention runs

- `2026-10-21T16:41:18.389100Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=396 heartbeats_deleted=0 events_deleted=0 retention_days=90 heartbeat_cutoff=2026-07-23 elapsed_ms=68482`

## Server warnings and errors

- none

## Thresholds

| check | value | limit | |
|---|---|---|---|
| status p95 (ms) | 35796.480 | 500 | **FAIL** |
| api p95 (ms) | 53373.151 | 500 | **FAIL** |
| ui p95 (ms) | 58663.097 | 2000 | **FAIL** |
| check delay p95 (s) | – | 2 | ok |
| heartbeat gap p95, 95th-percentile sample (s) | 52.000 | 3 | **FAIL** |
| peak RSS (MB) | 99.613 | 200 | ok |
| error rate | 0.092 | 0.0 | **FAIL** |
| notification failures | 0 | 0 | ok |

## Query plans

- rollup: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=?) / USE TEMP B-TREE FOR GROUP BY
- retention_delete: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_checked_at (checked_at<?)
- latest_local: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- agent_latest: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- chart: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=? AND checked_at>?)
