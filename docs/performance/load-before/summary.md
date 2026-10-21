# Vigilant Vole load run — before: v0.4.0 + check delay metric

*2026-10-21 14:13 UTC* · **FAIL**

## Setup

- 1000 monitors, 30 min measured after 60 s warm-up; 50 SSE dashboards, 20 status-page req/s, 10 API req/s, 0.5 UI req/s, bulk edit every 60 s
- Instance: instance: docker, image: vv-load:base, image_id: sha256:18c07ce50d9d, image_arch: amd64, limits: --cpus 1 --memory 512m, docker_host: Docker Desktop x86_64, 16 CPUs, 39.1 GiB
- Load generator host: Windows-10-10.0.26200-SP0 (AMD64), AMD64 Family 25 Model 33 Stepping 0, AuthenticAMD, 16 cores
- Startup 1.3 s; restart after backfill 1.6 s; monitor create p95 278 ms

## Resources

| | start | end | max |
|---|---|---|---|
| RSS (MB) | 104.8 | 112.7 | 123.0 |
| cgroup memory incl. page cache (MB) | | | 164.2 |
| CPU (% of one core) | avg 99.5 | p95 100.2 | |
| Open FDs / threads | | | 637 / 15 |
| DB file (MB) | 441.99 | 445.27 | growth 6.57 MB/h |
| WAL (MB) | | | 5.71 |
| Heartbeat rows | 3294940 | 3317389 | 44993 rows/h |

## Scheduler

- 0.0 checks/s recorded
- Start delay vs schedule (`vigilant_check_delay_seconds`, – checks): p50 – s, p95 – s, p99 – s, over 1 s: –
- Heartbeat gap minus interval (1 s resolution): median p50 2.0 s, worst 10-s-sample p95 527.0 s, max 530.0 s, gaps over 5 s: 15356

## Latency (ms, whole run)

| class | requests | p50 | p95 | p99 | max | errors |
|---|---|---|---|---|---|---|
| agent_poll | 0 | – | – | – | – | 27 |
| api_heartbeats | 408 | 59303.8 | 60620.0 | 60816.8 | 60881.8 | 1038 |
| api_list | 77 | 58213.6 | 60375.3 | 60894.7 | 60894.7 | 398 |
| api_monitor | 549 | 59296.8 | 60684.7 | 60881.0 | 60913.1 | 1181 |
| api_uptime | 344 | 59253.1 | 60719.6 | 60809.0 | 60898.0 | 871 |
| bulk_edit | 0 | – | – | – | – | 10 |
| metrics | 0 | – | – | – | – | 164 |
| push | 0 | – | – | – | – | 785 |
| status_fragment | 3557 | 33469.0 | 60376.9 | 60749.0 | 60962.2 | 1798 |
| status_json | 1292 | 66.7 | 38557.5 | 42768.3 | 46441.5 | 9 |
| status_page | 4430 | 32971.6 | 60336.6 | 60711.5 | 60944.5 | 2178 |
| ui_dashboard | 0 | – | – | – | – | 158 |
| ui_monitor | 0 | – | – | – | – | 169 |

Requests 14080, errors 8786 (62.401%), dropped by the in-flight cap 38255.

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
- status_json: ClientConnectionError: Connector is closed.
- status_page: TimeoutError: 
- ui_dashboard: TimeoutError: 
- ui_monitor: TimeoutError: 

## Live dashboards and notifications

- SSE: 1300 renders received (0.72/s, 1.058 MB/s), reconnects 0, refused 0
- Webhook deliveries by event: {'down': 619, 'up': 590, 'maintenance_start': 15, 'maintenance_end': 10}; app counters sent 0, failed 0
- Delivery latency (event time → sink): p50 90.194 s, p95 100.018 s
- Detection (target flips → webhook, 30 s interval, no retries): p50 118.8 s, p95 149.1 s, max 160.2 s

## Retention runs

- `2026-10-21T13:45:45.164882Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=1980 heartbeats_deleted=0 events_deleted=0 retention_days=90 heartbeat_cutoff=2026-07-23 elapsed_ms=163941`

## Server warnings and errors

- none

## Thresholds

| check | value | limit | |
|---|---|---|---|
| status p95 (ms) | 60376.949 | 1000 | **FAIL** |
| api p95 (ms) | 60719.640 | 1000 | **FAIL** |
| ui p95 (ms) | – | 3000 | ok |
| check delay p95 (s) | – | 2 | ok |
| heartbeat gap p95, worst sample (s) | 527.000 | 3 | **FAIL** |
| peak RSS (MB) | 123.035 | 384 | ok |
| error rate | 0.624 | 0.0 | **FAIL** |
| notification failures | 0 | 0 | ok |

## Query plans

- rollup: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=?) / USE TEMP B-TREE FOR GROUP BY
- retention_delete: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_checked_at (checked_at<?)
- latest_local: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- agent_latest: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- chart: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=? AND checked_at>?)
