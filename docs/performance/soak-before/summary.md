# Vigilant Vole soak run — before (v0.4.0 + check delay metric)

*2026-10-21 14:18 UTC* · **FAIL**

## Setup

- 1000 monitors, 120 min measured after 60 s warm-up; 50 SSE dashboards, 20 status-page req/s, 10 API req/s, 0.5 UI req/s, bulk edit every 60 s
- Instance: instance: docker, image: vv-load:base, image_id: sha256:18c07ce50d9d, image_arch: amd64, limits: --cpus 1 --memory 512m, docker_host: Docker Desktop x86_64, 16 CPUs, 39.1 GiB
- Load generator host: Windows-10-10.0.26200-SP0 (AMD64), AMD64 Family 25 Model 33 Stepping 0, AuthenticAMD, 16 cores
- Retention 1 day(s); backfill: {'inserted': 4941094, 'seconds': 517.1, 'db_bytes': 695029760, 'wal_bytes': 0, 'shm_bytes': 0}
- Startup 0.8 s; restart after backfill 0.3 s; monitor create p95 661 ms

## Resources

| | start | end | max |
|---|---|---|---|
| RSS (MB) | 106.6 | 114.2 | 127.6 |
| cgroup memory incl. page cache (MB) | | | 124.6 |
| CPU (% of one core) | avg 99.7 | p95 100.1 | |
| Open FDs / threads | | | 668 / 19 |
| DB file (MB) | 663.23 | 663.28 | growth 0.03 MB/h |
| WAL (MB) | | | 22.33 |
| Heartbeat rows | 4943551 | 3881655 | -533125 rows/h |

## Scheduler

- 0.0 checks/s recorded
- Start delay vs schedule (`vigilant_check_delay_seconds`, – checks): p50 – s, p95 – s, p99 – s, over 1 s: –
- Heartbeat gap minus interval (1 s resolution): median p50 3.0 s, worst 10-s-sample p95 586.0 s, max 607.0 s, gaps over 5 s: 57809

## Latency (ms, whole run)

| class | requests | p50 | p95 | p99 | max | errors |
|---|---|---|---|---|---|---|
| agent_poll | 0 | – | – | – | – | 111 |
| api_heartbeats | 1171 | 59581.6 | 60657.8 | 60825.1 | 60974.7 | 5277 |
| api_list | 640 | 58694.2 | 60595.0 | 60805.4 | 60949.7 | 7037 |
| api_monitor | 1245 | 59564.4 | 60672.9 | 60857.6 | 60967.8 | 5183 |
| api_uptime | 996 | 59571.4 | 60685.6 | 60833.0 | 60961.2 | 4253 |
| bulk_edit | 0 | – | – | – | – | 53 |
| metrics | 0 | – | – | – | – | 118 |
| push | 0 | – | – | – | – | 3526 |
| status_fragment | 9424 | 32421.4 | 60443.1 | 60794.8 | 60979.1 | 6797 |
| status_json | 4110 | 98.9 | 38516.1 | 42637.8 | 53424.7 | 0 |
| status_page | 11967 | 32334.7 | 60452.2 | 60808.4 | 60979.3 | 8726 |
| ui_dashboard | 0 | – | – | – | – | 615 |
| ui_monitor | 0 | – | – | – | – | 623 |

Requests 52297, errors 42319 (80.921%), dropped by the in-flight cap 154236.

Error samples:

- agent_poll: TimeoutError: 
- api_heartbeats: TimeoutError: 
- api_list: TimeoutError: 
- api_monitor: TimeoutError: 
- api_uptime: TimeoutError: 
- bulk_edit: TimeoutError: 
- metrics: TimeoutError: 
- push: TimeoutError: 
- status_fragment: TimeoutError: 
- status_page: TimeoutError: 
- ui_dashboard: TimeoutError: 
- ui_monitor: TimeoutError: 

## Live dashboards and notifications

- SSE: 5316 renders received (0.74/s, 1.083 MB/s), reconnects 50, refused 0
- Webhook deliveries by event: {'down': 2402, 'up': 2370, 'maintenance_start': 48, 'maintenance_end': 48}; app counters sent 0, failed 0
- Delivery latency (event time → sink): p50 91.161 s, p95 101.752 s
- Detection (target flips → webhook, 30 s interval, no retries): p50 121.6 s, p95 152.4 s, max 165.6 s

## Retention runs

- none in this run

## Server warnings and errors

- none

## Thresholds

| check | value | limit | |
|---|---|---|---|
| status p95 (ms) | 60452.177 | 1000 | **FAIL** |
| api p95 (ms) | 60685.553 | 1000 | **FAIL** |
| ui p95 (ms) | – | 3000 | ok |
| check delay p95 (s) | – | 2 | ok |
| heartbeat gap p95, worst sample (s) | 586.000 | 3 | **FAIL** |
| peak RSS (MB) | 127.574 | 384 | ok |
| error rate | 0.809 | 0.0 | **FAIL** |
| notification failures | 0 | 0 | ok |

## Query plans

- rollup: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=?) / USE TEMP B-TREE FOR GROUP BY
- retention_delete: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_checked_at (checked_at<?)
- latest_local: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- agent_latest: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- chart: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=? AND checked_at>?)
