# Vigilant Vole load run — after: all fixes + phase-spread checks

*2026-10-21 15:38 UTC* · **FAIL**

## Setup

- 1000 monitors, 30 min measured after 60 s warm-up; 50 SSE dashboards, 20 status-page req/s, 10 API req/s, 0.5 UI req/s, bulk edit every 60 s
- Instance: instance: docker, image: vv-load:final2, image_id: sha256:62fb988fd92d, image_arch: amd64, limits: --cpus 1 --memory 512m, docker_host: Docker Desktop x86_64, 16 CPUs, 39.1 GiB
- Load generator host: Windows-10-10.0.26200-SP0 (AMD64), AMD Ryzen 7 5800X 8-Core Processor, 16 cores
- Startup 1.0 s; restart after backfill 0.3 s; monitor create p95 285 ms

## Resources

| | start | end | max |
|---|---|---|---|
| RSS (MB) | 78.9 | 84.9 | 106.6 |
| cgroup memory incl. page cache (MB) | | | 87.0 |
| CPU (% of one core) | avg 61.3 | p95 72.1 | |
| Open FDs / threads | | | 145 / 13 |
| DB file (MB) | 442.09 | 448.84 | growth 13.50 MB/h |
| WAL (MB) | | | 4.71 |
| Heartbeat rows | 3296111 | 3334916 | 77610 rows/h |

## Scheduler

- 20.5 checks/s recorded (– = `/metrics` never answered in time)
- Start delay vs schedule (`vigilant_check_delay_seconds`, 33155 checks): p50 0.006 s, p95 0.049 s, p99 0.148 s, over 1 s: 0
- Heartbeat gap minus interval (1 s resolution): median p50 0.0 s, worst 10-s-sample p95 30.0 s, max 120.0 s, gaps over 5 s: 89

## Latency (ms, whole run)

| class | requests | p50 | p95 | p99 | max | errors |
|---|---|---|---|---|---|---|
| agent_poll | 59 | 11.5 | 385.0 | 1418.9 | 1418.9 | 0 |
| agent_results | 329 | 15.1 | 562.0 | 862.8 | 1146.5 | 0 |
| api_heartbeats | 5376 | 4.8 | 310.5 | 629.5 | 5664.0 | 0 |
| api_list | 1777 | 107.6 | 484.7 | 865.1 | 1117.4 | 0 |
| api_monitor | 6284 | 4.8 | 328.5 | 669.4 | 1386.9 | 0 |
| api_uptime | 4565 | 6.3 | 334.8 | 622.7 | 1101.7 | 0 |
| bulk_edit | 29 | 180.3 | 814.1 | 1443.0 | 1443.0 | 0 |
| metrics | 181 | 606.4 | 1132.4 | 1663.0 | 2040.6 | 0 |
| push | 2994 | 6.8 | 206.1 | 436.1 | 1051.0 | 0 |
| status_fragment | 14537 | 15.0 | 342.1 | 756.9 | 1506.8 | 0 |
| status_json | 3554 | 12.3 | 201.6 | 734.3 | 1289.0 | 0 |
| status_page | 17910 | 15.1 | 338.7 | 728.7 | 1358.9 | 0 |
| ui_dashboard | 451 | 127.9 | 1016.2 | 1454.0 | 7033.9 | 0 |
| ui_monitor | 451 | 18.5 | 706.7 | 1307.3 | 1809.1 | 0 |

Requests 58497, errors 0 (0.000%), dropped by the in-flight cap 0.

## Live dashboards and notifications

- SSE: 38010 renders received (20.46/s, 15.186 MB/s), reconnects 0, refused 0
- Webhook deliveries by event: {'up': 600, 'down': 600, 'maintenance_start': 15, 'maintenance_end': 15}; app counters sent 1240, failed 0
- Delivery latency (event time → sink): p50 0.002 s, p95 0.037 s
- Detection (target flips → webhook, 30 s interval, no retries): p50 14.8 s, p95 29.8 s, max 30.3 s

## Retention runs

- `2026-10-21T15:08:32.192373Z  INFO vigilant_vole::monitor::retention: retention run finished rollup_rows=1980 heartbeats_deleted=0 events_deleted=0 retention_days=90 heartbeat_cutoff=2026-07-23 elapsed_ms=6809`

## Server warnings and errors

- none

## Thresholds

| check | value | limit | |
|---|---|---|---|
| status p95 (ms) | 342.124 | 1000 | ok |
| api p95 (ms) | 484.724 | 1000 | ok |
| ui p95 (ms) | 1016.153 | 3000 | ok |
| check delay p95 (s) | 0.049 | 2 | ok |
| heartbeat gap p95, worst sample (s) | 30.000 | 3 | **FAIL** |
| peak RSS (MB) | 106.648 | 384 | ok |
| error rate | 0.000 | 0.0 | ok |
| notification failures | 0.000 | 0 | ok |

## Query plans

- rollup: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=?) / USE TEMP B-TREE FOR GROUP BY
- retention_delete: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_checked_at (checked_at<?)
- latest_local: SEARCH heartbeats USING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- agent_latest: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=?)
- chart: SEARCH heartbeats USING COVERING INDEX idx_heartbeats_agent (agent_id=? AND monitor_id=? AND checked_at>?)
