# Prometheus metrics

Vigilant Vole exposes a Prometheus endpoint at **`GET /metrics`** (text exposition format 0.0.4).

Besides its own `vigilant_*` families it exports Uptime Kuma's metric names and labels
(`monitor_status`, `monitor_response_time`, `monitor_cert_days_remaining`, `monitor_cert_is_valid`),
so Grafana dashboards built for Kuma keep working after you switch.

## Turning it on

1. **Settings → Integrations → Prometheus metrics**: switch it on (admins only). While it's off,
   `/metrics` answers `404`. In config-as-code: `settings: { metrics_enabled: true }`.
2. **Settings → API tokens**: create a token for the scraper. A **read** token is enough.
3. Send the token either as a Bearer token or as the **basic-auth password** (any username).
   Without a valid token the endpoint answers `401` with `WWW-Authenticate: Basic`.

```bash
curl -H "Authorization: Bearer vv_xxxxxxxxxxxxxxxx" http://localhost:3001/metrics
```

## Prometheus scrape config

```yaml
scrape_configs:
  - job_name: vigilant-vole
    scrape_interval: 30s
    metrics_path: /metrics
    scheme: https            # or http
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/vigilant-vole.token   # contains vv_…
    static_configs:
      - targets: ["status.example.com"]
```

Basic auth works too (handy for tools that only support it, or for Kuma dashboards' default setup):

```yaml
    basic_auth:
      username: prometheus          # ignored
      password_file: /etc/prometheus/vigilant-vole.token
```

Values come from the database and the engine's in-memory counters at scrape time; scraping more often than
your shortest monitor interval adds nothing.

## Metric reference

Labels used below:

- **monitor labels**: `monitor_id`, `monitor_name`, `monitor_type` (`http`, `tcp`, …), `monitor_target`
  (credentials in URLs are stripped).
- **Kuma labels**: `monitor_id`, `monitor_name`, `monitor_type`, `monitor_url`, `monitor_hostname`, `monitor_port`
  (URL for the HTTP family and WebSocket; hostname/port for TCP, Steam, NTP; hostname for ping, DNS and domain
  expiry; all empty for Docker, push and group monitors).
- **agent labels**: `agent_id`, `agent_name`, `agent_location`.

Status values everywhere: `0` down, `1` up, `2` pending, `3` maintenance.

### Monitors

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `vigilant_build_info` | gauge | `version` | Always 1. |
| `vigilant_monitor_active` | gauge | monitor | 1 if active, 0 if paused. |
| `vigilant_monitor_status` | gauge | monitor | Latest local status (active monitors with data). |
| `vigilant_monitor_response_time_seconds` | gauge | monitor | Response time of the latest local check. |
| `vigilant_monitor_uptime_ratio` | gauge | monitor + `window` (`24h`, `30d`) | Share of up checks (0–1); pending and maintenance excluded. |
| `vigilant_monitor_cert_days_remaining` | gauge | monitor | Days until the TLS certificate expires (HTTPS monitors). |
| `vigilant_monitor_domain_days_remaining` | gauge | monitor | Days until the domain registration expires (domain-expiry monitors). |

### Uptime Kuma compatible

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `monitor_status` | gauge | Kuma | Same values as Kuma: 1 up, 0 down, 2 pending, 3 maintenance. |
| `monitor_response_time` | gauge | Kuma | Latest response time in **milliseconds** (Kuma's unit). |
| `monitor_cert_days_remaining` | gauge | Kuma | Days until the TLS certificate expires. |
| `monitor_cert_is_valid` | gauge | Kuma | 1 while the certificate hasn't expired. |

Kuma's labels gain a `monitor_id` label here; dashboards that select by `monitor_name` are unaffected.

### Engine

Counters reset when the process restarts (use `rate()`/`increase()`).

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `vigilant_checks_total` | counter | – | Heartbeats recorded since start. |
| `vigilant_checks_failed_total` | counter | – | Heartbeats that were down or pending. |
| `vigilant_running_tasks` | gauge | – | Monitors with a running check loop or push watchdog. |
| `vigilant_monitor_checks_total` | counter | `monitor_id` | Heartbeats per monitor. |
| `vigilant_monitor_checks_failed_total` | counter | `monitor_id` | Down or pending heartbeats per monitor. |
| `vigilant_check_duration_seconds` | histogram | `monitor_type` | Wall-clock duration of local probes. Buckets: 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60 s. |
| `vigilant_check_delay_seconds` | histogram | – | How long after its due time a scheduled check started its probe: timer lag plus the wait for a probe slot (`VV_MAX_CONCURRENT_CHECKS`). Near zero on a healthy server; growing values mean the scheduler can't keep up. Buckets: 0.01, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30 s. |
| `vigilant_notifications_sent_total` | counter | `kind` | Notifications delivered, by provider. |
| `vigilant_notifications_failed_total` | counter | `kind` | Failed deliveries, by provider. |

### Remote agents

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `vigilant_agent_up` | gauge | agent | 1 if the agent is enabled and checked in within 90 seconds. |
| `vigilant_agent_last_seen_timestamp_seconds` | gauge | agent | Unix time of the last check-in. |
| `vigilant_agent_monitor_status` | gauge | agent + `monitor_id` | Latest status the agent reported for a monitor (last hour). |
| `vigilant_agent_monitor_response_time_seconds` | gauge | agent + `monitor_id` | Latest response time the agent reported (last hour). |

### Docker discovery

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `vigilant_discovery_monitors` | gauge | – | Monitors managed by Docker label discovery. |
| `vigilant_discovery_last_sync_timestamp_seconds` | gauge | – | Unix time of the last sync attempt. |
| `vigilant_discovery_last_sync_success` | gauge | – | 1 if the last sync succeeded. |

## Useful queries and alerts

Vigilant Vole already notifies you when a monitor goes down; Prometheus alerts are most useful for the
things *around* it — the monitor itself, its agents and its notification pipeline.

```promql
# Monitors currently down
vigilant_monitor_status == 0

# 30-day availability per monitor, as a percentage
100 * vigilant_monitor_uptime_ratio{window="30d"}

# p95 probe duration by monitor type
histogram_quantile(0.95, sum by (le, monitor_type) (rate(vigilant_check_duration_seconds_bucket[15m])))

# Is the scheduler keeping up? p95 of how late checks start
histogram_quantile(0.95, sum by (le) (rate(vigilant_check_delay_seconds_bucket[15m])))

# Notification delivery failure ratio per provider
sum by (kind) (rate(vigilant_notifications_failed_total[1h]))
  / clamp_min(sum by (kind) (rate(vigilant_notifications_sent_total[1h]) + rate(vigilant_notifications_failed_total[1h])), 1e-9)
```

```yaml
groups:
  - name: vigilant-vole
    rules:
      - alert: VigilantVoleDown            # the monitor of your monitors
        expr: up{job="vigilant-vole"} == 0
        for: 5m
      - alert: VigilantAgentOffline
        expr: vigilant_agent_up == 0
        for: 10m
      - alert: VigilantNotificationsFailing
        expr: increase(vigilant_notifications_failed_total[1h]) > 3
      - alert: VigilantChecksRunningLate     # overloaded host: fewer monitors, longer intervals or more CPU
        expr: histogram_quantile(0.95, sum by (le) (rate(vigilant_check_delay_seconds_bucket[15m]))) > 5
        for: 15m
      - alert: CertificateExpiresSoon
        expr: vigilant_monitor_cert_days_remaining < 7
```

## Grafana

- **Existing Uptime Kuma dashboards** work as-is: point their Prometheus data source at a job scraping
  Vigilant Vole. They read `monitor_status`, `monitor_response_time` and `monitor_cert_days_remaining`.
  Note that `monitor_response_time` is in milliseconds, like Kuma's.
- For new dashboards prefer the `vigilant_*` families: seconds instead of milliseconds, a ready-made uptime
  ratio that survives heartbeat retention (it's computed from daily rollups), a probe-duration histogram
  and per-agent series.
