# REST API

The full, always-current reference lives **in the app**:

- **`/api-docs`** — browsable reference with request/response examples (sign in first).
- **`/api/v1/openapi.json`** — the same as an OpenAPI 3.1 document, for code generators, Postman, Insomnia, etc.

Both are generated from one route table in the source, so they can't drift from the server.

## Authentication

Create a token under **Settings → API tokens**. Tokens look like `vv_` + 43 characters, are shown **once**,
and only their SHA-256 is stored. Pick an expiry (30, 90, 365 days or never) and a scope:

| Scope | Acts as |
|---|---|
| `read` | a **viewer**, whatever your role |
| `write` | **you**, with your role (viewer, editor or admin) |

Send it as a Bearer token:

```bash
curl -H "Authorization: Bearer vv_xxxxxxxxxxxxxxxx" https://status.example.com/api/v1/me
```

Roles: **viewer** can read everything; **editor** can also change monitors, tags, notification channels, status
pages, incidents and maintenance; **admin** can also change settings, users, agents, and import configuration.
API tokens only work under `/api/` and for `/metrics`; they can't open the web UI.

## Conventions

- JSON in, JSON out. Times are UTC strings `YYYY-MM-DD HH:MM:SS`.
- Lists: `{"data": [...], "next_cursor": "42" | null}`; pass `?limit=` (1–500, default 50) and `?cursor=`.
- `GET`s carry a weak `ETag` and honour `If-None-Match` (→ `304`).
- Errors: `{"error": {"code": "validation_failed", "message": "...", "fields": {"target": "..."}}}`
  with `400`/`401`/`403`/`404`/`409`/`422`.
- Secrets are never returned in full: they read as `"••••••"`, and sending `"••••••"` back keeps the stored value.
- Writes with a session cookie (from the browser) must be same-origin; token requests are exempt.

## Endpoints at a glance

| Area | Endpoints |
|---|---|
| Monitors | `GET/POST /api/v1/monitors`, `GET/PUT/PATCH/DELETE /api/v1/monitors/{id}`, `POST …/{id}/pause`, `…/resume`, `…/check`, `GET …/{id}/heartbeats`, `…/uptime`, `…/events`, `PUT …/{id}/tags` |
| Tags | `GET/POST /api/v1/tags`, `GET/PATCH/DELETE /api/v1/tags/{id}` |
| Notifications | `GET /api/v1/notification-providers`, `GET/POST /api/v1/notification-channels`, `GET/PATCH/DELETE …/{id}`, `POST …/{id}/test` |
| Status pages | `GET/POST /api/v1/status-pages`, `GET/PATCH/DELETE …/{id}`, `PUT …/{id}/sections`, `GET/POST …/{id}/incidents`, `GET/PATCH/DELETE …/{id}/incidents/{iid}` |
| Maintenance | `GET/POST /api/v1/maintenance`, `GET/PATCH/DELETE /api/v1/maintenance/{id}` |
| System | `GET /api/v1/me`, `GET /api/v1/health`, `GET /api/v1/agents`, `GET/PATCH /api/v1/settings`, `GET /api/v1/openapi.json` |
| Configuration | `GET/POST /api/v1/config` — see [config-as-code](config-as-code.md) |

Public and machine endpoints (no API token):

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET/POST /api/push/{token}?status=up\|down&msg=&ping=` | push token in the URL | Push monitors (max 1 request/second per monitor) |
| `GET /api/badge/{id}/{status\|uptime\|ping\|cert-exp}?duration=24h\|7d\|30d&style=flat\|flat-square&label=` | public* | SVG badges |
| `GET /api/status-page/{slug}` | public | A published status page as JSON (CORS `*`) |
| `GET /api/agent/v1/monitors`, `POST /api/agent/v1/results` | agent token | [Remote agents](remote-agents.md) |
| `GET /metrics` | API token | [Prometheus](metrics.md) |
| `GET /healthz` | public | Liveness probe (`ok`) |

\* Badges are public for monitors on a published status page, or for all monitors when *Public badges* is on.

## Examples

```bash
BASE=https://status.example.com
AUTH="Authorization: Bearer vv_xxxxxxxxxxxxxxxx"

# Monitors that are down
curl -s -H "$AUTH" "$BASE/api/v1/monitors?status=down" | jq '.data[] | {id, name, target}'

# Create an HTTP monitor with tags and a notification channel
curl -s -H "$AUTH" -H "Content-Type: application/json" -X POST "$BASE/api/v1/monitors" -d '{
  "name": "Website",
  "monitor_type": "http",
  "target": "https://example.com",
  "interval_seconds": 60,
  "retries": 1,
  "tags": ["production", {"name": "env", "value": "prod"}],
  "notifications": [1]
}'

# Pause during a deploy, resume after
curl -s -H "$AUTH" -X POST "$BASE/api/v1/monitors/7/pause"
curl -s -H "$AUTH" -X POST "$BASE/api/v1/monitors/7/resume"

# 30-day uptime with daily breakdown
curl -s -H "$AUTH" "$BASE/api/v1/monitors/7/uptime?days=30" | jq '.uptime_pct'

# Post an incident on a status page
curl -s -H "$AUTH" -H "Content-Type: application/json" -X POST "$BASE/api/v1/status-pages/1/incidents" \
  -d '{"title": "Elevated error rates", "body": "We are looking into it.", "severity": "warning"}'

# Push monitor heartbeat from a cron job
curl -fsS "$BASE/api/push/AbCdEf0123456789AbCdEf0123456789?status=up&msg=backup%20ok&ping=" > /dev/null
```
