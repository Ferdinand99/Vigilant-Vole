# Vigilant Vole load run — before, 30 days of history

*2026-10-21 14:49 UTC* · **FAIL**

## Setup

- 200 monitors, 15 min measured after 60 s warm-up; 50 SSE dashboards, 20 status-page req/s, 10 API req/s, 0.5 UI req/s, bulk edit every 60 s
- Instance: instance: docker, image: vv-load:base, image_id: sha256:18c07ce50d9d, image_arch: amd64, limits: --cpus 1 --memory 512m, docker_host: Docker Desktop x86_64, 16 CPUs, 39.1 GiB
- Load generator host: Windows-10-10.0.26200-SP0 (AMD64), AMD64 Family 25 Model 33 Stepping 0, AuthenticAMD, 16 cores
- Startup 1.0 s; restart after backfill 0.4 s; monitor create p95 98 ms

## Resources

| | start | end | max |
|---|---|---|---|
| RSS (MB) | 91.5 | 107.7 | 113.7 |
| cgroup memory incl. page cache (MB) | | | 96.3 |
| CPU (% of one core) | avg 99.5 | p95 100.3 | |
| Open FDs / threads | | | 665 / 22 |
| DB file (MB) | 1284.22 | 1284.43 | growth 0.84 MB/h |
| WAL (MB) | | | 4.55 |
| Heartbeat rows | 9538347 | 9540547 | 8831 rows/h |

## Scheduler

- 0.0 checks/s recorded
- Start delay vs schedule (`vigilant_check_delay_seconds`, – checks): p50 – s, p95 – s, p99 – s, over 1 s: –
- Heartbeat gap minus interval (1 s resolution): median p50 11.0 s, worst 10-s-sample p95 415.0 s, max 518.0 s, gaps over 5 s: 1896

## Latency (ms, whole run)

| class | requests | p50 | p95 | p99 | max | errors |
|---|---|---|---|---|---|---|
| agent_poll | 0 | – | – | – | – | 14 |
| api_heartbeats | 166 | 52236.1 | 60674.5 | 60817.4 | 60834.1 | 763 |
| api_list | 50 | 49684.1 | 59859.9 | 60323.2 | 60323.2 | 295 |
| api_monitor | 191 | 51677.6 | 60647.1 | 60832.1 | 60895.6 | 944 |
| api_uptime | 110 | 50666.4 | 60700.8 | 60749.4 | 60773.1 | 673 |
| bulk_edit | 0 | – | – | – | – | 5 |
| metrics | 0 | – | – | – | – | 77 |
| push | 0 | – | – | – | – | 80 |
| sse_connect | 0 | – | – | – | – | 3 |
| status_fragment | 1535 | 25968.5 | 56750.3 | 60323.4 | 60878.5 | 1255 |
| status_json | 629 | 126.3 | 50826.1 | 57650.2 | 60636.8 | 12 |
| status_page | 1894 | 25982.2 | 55700.9 | 60147.6 | 60889.6 | 1533 |
| ui_dashboard | 0 | – | – | – | – | 70 |
| ui_monitor | 0 | – | – | – | – | 71 |

Requests 7053, errors 5795 (82.164%), dropped by the in-flight cap 18732.

Error samples:

- agent_poll: TimeoutError: 
- api_heartbeats: TimeoutError: 
- api_list: TimeoutError: 
- api_monitor: TimeoutError: 
- api_uptime: TimeoutError: 
- bulk_edit: TimeoutError: 
- metrics: TimeoutError: 
- push: TimeoutError: 
- sse_connect: SocketTimeoutError: Timeout on reading data from socket
- status_fragment: TimeoutError: 
- status_json: ClientConnectionError: Connector is closed.; TimeoutError: 
- status_page: TimeoutError: 
- ui_dashboard: TimeoutError: 
- ui_monitor: TimeoutError: 

## Live dashboards and notifications

- SSE: 613 renders received (0.68/s, 0.2 MB/s), reconnects 0, refused 0
- Webhook deliveries by event: {'up': 61, 'down': 71, 'maintenance_start': 5, 'maintenance_end': 5}; app counters sent 0, failed 0
- Delivery latency (event time → sink): p50 84.617 s, p95 121.081 s
- Detection (target flips → webhook, 30 s interval, no retries): p50 115.1 s, p95 181.3 s, max 181.3 s

## Retention runs

- `2026-10-21T14:37:12.112427Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=396 heartbeats_deleted=0 events_deleted=0 retention_days=90 heartbeat_cutoff=2026-07-23 elapsed_ms=181205`

## Server warnings and errors

- none

## Thresholds

| check | value | limit | |
|---|---|---|---|
| status p95 (ms) | 56750.314 | 1000 | **FAIL** |
| api p95 (ms) | 60700.821 | 1000 | **FAIL** |
| ui p95 (ms) | – | 3000 | ok |
| check delay p95 (s) | – | 2 | ok |
| heartbeat gap p95, worst sample (s) | 415.000 | 3 | **FAIL** |
| peak RSS (MB) | 113.680 | 384 | ok |
| error rate | 0.822 | 0.0 | **FAIL** |
| notification failures | 0 | 0 | ok |

## Query plans

- rollup: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=?) / USE TEMP B-TREE FOR GROUP BY
- retention_delete: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_checked_at (checked_at<?)
- latest_local: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- agent_latest: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- chart: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=? AND checked_at>?)
