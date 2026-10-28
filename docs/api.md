# REST API

The full, always-current reference lives **in the app**:

- **`/api-docs`** — browsable reference with request/response examples (sign in first).
- **`/api/v1/openapi.json`** — the same as an OpenAPI 3.1 document, for code generators, Postman, Insomnia, etc.
- **`/api-docs.md`** (also at **`/llms.txt`**) — the same as one Markdown document for AI assistants: this instance's
  base URL, authentication, conventions, error codes, every endpoint with a `curl` example (`$VV_URL`/`$VV_TOKEN`
  placeholders), config as code and common tasks. The **Copy for AI** button on `/api-docs` puts it on your clipboard.
  Like `/api-docs` it needs a signed-in session; it never contains tokens or secrets.

All of them are generated from one route table in the source, so they can't drift from the server.

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

- JSON in, JSON out. Times are UTC strings `YYYY-MM-DD HH:MM:SS` (except `raw_status_since`, which is RFC 3339).
- Lists: `{"data": [...], "next_cursor": "42" | null}`; pass `?limit=` (1–500, default 50) and `?cursor=`.
- `GET`s carry a weak `ETag` and honour `If-None-Match` (→ `304`).
- Errors: `{"error": {"code": "validation_failed", "message": "...", "fields": {"target": "..."}}}`
  with `400`/`401`/`403`/`404`/`409`/`422`.
- Secrets are never returned in full: they read as `"••••••"`, and sending `"••••••"` back keeps the stored value.
- Writes with a session cookie (from the browser) must be same-origin; token requests are exempt.

## Endpoints at a glance

| Area | Endpoints |
|---|---|
| Monitors | `GET/POST /api/v1/monitors`, `POST /api/v1/monitors/bulk`, `GET/PUT/PATCH/DELETE /api/v1/monitors/{id}`, `POST …/{id}/pause`, `…/resume`, `…/check`, `GET …/{id}/heartbeats`, `…/uptime`, `…/events`, `PUT …/{id}/tags` |
| Tags | `GET/POST /api/v1/tags`, `GET/PATCH/DELETE /api/v1/tags/{id}` |
| Remote browsers | `GET/POST /api/v1/remote-browsers`, `GET/PUT/PATCH/DELETE /api/v1/remote-browsers/{id}`, `POST …/{id}/test` (writes: admin; URLs come back with their token masked) |
| Notifications | `GET /api/v1/notification-providers`, `GET/POST /api/v1/notification-channels`, `GET/PATCH/DELETE …/{id}`, `POST …/{id}/test` |
| Status pages | `GET/POST /api/v1/status-pages`, `GET/PATCH/DELETE …/{id}`, `PUT …/{id}/sections`, `GET/POST …/{id}/incidents`, `GET/PATCH/DELETE …/{id}/incidents/{iid}`, `GET/POST …/{id}/incidents/{iid}/updates`, `GET/PUT …/{id}/subscription`, `GET …/{id}/subscribers`, `POST …/{id}/subscribers/{sid}/approve`, `DELETE …/{id}/subscribers/{sid}` |
| Maintenance | `GET/POST /api/v1/maintenance`, `GET/PATCH/DELETE /api/v1/maintenance/{id}` |
| System | `GET /api/v1/me`, `GET /api/v1/health`, `GET /api/v1/agents`, `GET/PATCH /api/v1/settings`, `GET /api/v1/openapi.json` |
| Push (Hedgerow app) | `POST /api/v1/push/devices`, `DELETE /api/v1/push/devices/{id}`, `GET /api/v1/push/events/{ref}` — for the Hedgerow app; any token, read-only ones included. Only one Hedgerow app channel can be active (409 `conflict` on the channel routes otherwise) |
| Configuration | `GET/POST /api/v1/config` — see [config-as-code](config-as-code.md) |

AI assistants can use the same tokens against the MCP endpoint `POST /mcp`: see [mcp.md](mcp.md).

### Status during maintenance

Monitors keep checking during a maintenance window. What they *record* is `maintenance`, and that is what status
pages, badges, uptime and alerting use; the real result is kept next to it for the API:

- Monitors (`GET /api/v1/monitors`, `GET /api/v1/monitors/{id}`) carry `status` (the recorded status, `maintenance`
  during a window), `maintenance` (`true` while a window covers the monitor), `raw_status` (`up`, `down`, `pending`
  or `null`: the latest real result, also during a window), `raw_status_since` (RFC 3339, when that real status
  began; looked up over the last 100 checks) and `raw_message`. The `raw_*` fields are `null` for paused monitors.
- Heartbeats (`GET …/{id}/heartbeats`, `POST …/{id}/check`) carry `raw_status` next to `status`. Outside
  maintenance they are equal; during maintenance `status` is `maintenance` and `raw_status` is the real result.
  Heartbeats recorded during maintenance before 0.9.0 have `raw_status: null`.
- `raw_status` applies upside-down mode and agent confirmation, but not retries: one failed check during a window
  shows as `down`.
- Group monitors aggregate their children's real results into their own `raw_status` while they are in
  maintenance.

```json
{"id": 7, "name": "Website", "status": "maintenance", "maintenance": true,
 "raw_status": "up", "raw_status_since": "2026-10-19T14:01:00Z", "raw_message": "200 OK", "…": "…"}
```

### Agent-checked monitors

`check_from_agent_id` (integer or `null`, default `null`) names the [remote agent](remote-agents.md#checks-only-an-agent-can-reach)
(`GET /api/v1/agents` lists them) that checks the monitor instead of this server. `null` means this server checks
it. The agent's results are then the monitor's status, history and uptime, and the monitor goes `down` when the agent
stops reporting or is disabled. Writes are rejected with `422` on `check_from_agent_id` for an unknown agent, for
`push`, `group` and `real_browser` monitors, and together with `run_on_agents` or `confirm_with_agents` set to
`true` (send those as `false`). `POST …/{id}/check` answers `409` for such a monitor: only the agent runs it. Deleting
the agent (Settings → Remote agents) pauses its monitors and sets the field to `null`, so a monitor resumed later is
checked by this server. `GET /api/v1/agents` reports `checked_monitors` per agent (active monitors only it checks).

```bash
curl -X PATCH -H "$AUTH" -H 'Content-Type: application/json' $BASE/api/v1/monitors/12 -d '{"check_from_agent_id": 3}'
```

Public and machine endpoints (no API token):

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET/POST /api/push/{token}?status=up\|down&msg=&ping=` | push token in the URL | Push monitors (max 1 request/second per monitor) |
| `GET /api/badge/{id}/{status\|uptime\|ping\|cert-exp}?duration=24h\|7d\|30d&style=flat\|flat-square&label=` | public* | SVG badges |
| `GET /api/status-page/{slug}` | public | A published status page as JSON (CORS `*`); `401 {"error": "password_required"}` for a password-protected page without a session |
| `GET /status/{slug}/logo`, `/status/{slug}/favicon` | public | Uploaded logo and browser icon (ETag; `?v=<hash>` URLs are cached for a year) |
| `GET /api/agent/v1/monitors`, `POST /api/agent/v1/results` | agent token | [Remote agents](remote-agents.md) |
| `GET /metrics` | API token | [Prometheus](metrics.md) |
| `GET /healthz` | public | Liveness probe (`ok`) |

\* Badges are public for monitors on a published status page without a password, or for all monitors when *Public badges* is on.

Status pages in `/api/v1/status-pages` also carry `password_protected` (bool), `analytics` (as in [config-as-code](config-as-code.md#status_pages)), `logo_url` and `favicon_url`. Writes accept `password` (write-only; `""` removes), `analytics`, and `logo`/`favicon` as `data:image/…;base64,…` URLs (`""` removes):

```bash
curl -X PATCH -H "$AUTH" -H 'Content-Type: application/json' $BASE/api/v1/status-pages/1   -d '{"password": "correct horse", "analytics": {"provider": "plausible", "site_id": "status.example.com", "script_url": "https://plausible.io/js/script.js"}}'
```

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

# Move every HTTP monitor tagged production to 2-minute checks from the agents too
curl -s -H "$AUTH" -H "Content-Type: application/json" -X POST "$BASE/api/v1/monitors/bulk" -d '{
  "filter": {"type": "http", "tag": "production"},
  "set": {"interval_seconds": 120, "run_on_agents": true, "confirm_with_agents": true}
}'

# 30-day uptime with daily breakdown
curl -s -H "$AUTH" "$BASE/api/v1/monitors/7/uptime?days=30" | jq '.uptime_pct'

# Post an incident on a status page
curl -s -H "$AUTH" -H "Content-Type: application/json" -X POST "$BASE/api/v1/status-pages/1/incidents" \
  -d '{"title": "Elevated error rates", "body": "We are looking into it.", "severity": "warning"}'

# Push monitor heartbeat from a cron job
curl -fsS "$BASE/api/push/AbCdEf0123456789AbCdEf0123456789?status=up&msg=backup%20ok&ping=" > /dev/null
```

## Bulk edit

`POST /api/v1/monitors/bulk` (editor) changes a few settings on many monitors at once — the API side of
*Select → Edit…* on the dashboard. Pick the monitors with **either** `ids` **or** `filter` (the list
endpoint's filters: `status`, `type`, `tag` by name, `q`; `{}` selects every monitor), and say what to
change in `set`. Every key of `set` is optional; leave one out (or send `null`) to keep each monitor's own value.

| `set` key | Value |
|---|---|
| `run_on_agents`, `confirm_with_agents` | `true` / `false` |
| `interval_seconds`, `retry_interval_seconds`, `timeout_seconds`, `retries` | whole numbers, same limits as the monitor form (the timeout can't exceed the interval) |
| `channels` | `{"add": [...], "remove": [...]}` or `{"replace": [...]}`; entries are a channel id, a name, or `{"id"\|"name", "escalation": bool}` |
| `tags` | `{"add": [...], "remove": [...], "clear": bool}`; `add` takes names, ids or `{"name", "color", "value"}` (unknown names are created with that palette colour), `remove` takes names or ids; order: clear, remove, add |

```bash
curl -s -H "$AUTH" -H "Content-Type: application/json" -X POST "$BASE/api/v1/monitors/bulk" -d '{
  "ids": [3, 7, 12],
  "set": {
    "retries": 2,
    "channels": {"add": [{"name": "Ops Discord"}, {"id": 4, "escalation": true}], "remove": ["Old pager"]},
    "tags": {"add": [{"name": "eu", "color": "teal", "value": "west"}], "remove": ["legacy"]}
  }
}'
# → {"updated": 2, "skipped": [{"id": 12, "reason": "escalation_needs_delay"}]}
```

Everything is validated first (422 with `fields` like `set.interval_seconds`) and applied in one
transaction. `updated` counts monitors that actually changed. A setting that doesn't apply to a
monitor is left alone and listed in `skipped` (one entry per monitor and reason):

| Reason | When |
|---|---|
| `agents_unsupported` | agent settings on push, group or real-browser monitors |
| `timing_unsupported` | any timing on a group; timeout or retry interval on a push monitor |
| `timeout_exceeds_interval` | the monitor's timeout would end up longer than its interval |
| `escalation_needs_delay` | an escalation channel on a monitor without an *Escalate after* delay |
| `agent_checked` | agent settings on a monitor that one agent checks instead of this server (`check_from_agent_id`) |

## Service monitors

`monitor_type` also accepts `tls`, `smtp`, `redis`, `postgres`, `mysql`, `mqtt`, `kafka`, `rabbitmq`, `grpc`,
`radius`, `snmp`, `sqlserver` and `mongodb`. Their `target` is `host[:port]` (`tls`, `smtp`) or
`scheme://host[:port][/path]` where the path is the database (`redis`, `postgres`, `mysql`, `sqlserver`; for
`mongodb` the authentication database), topic (`mqtt`, `kafka`), virtual host (`rabbitmq`), gRPC service or OID
(`snmp`). MongoDB targets take several hosts (`mongodb://a,b:27018/?replicaSet=rs0`) or `mongodb+srv://`.
Credentials never go into the target:

| Field | Used by |
|---|---|
| `basic_auth_user`, `basic_auth_pass` | `redis` (password alone is fine), `postgres` (user required), `mysql`, `mqtt`, `rabbitmq`, `radius`, `sqlserver` (SQL Server login), `mongodb` (SCRAM), `snmp` v3 (user and authentication password) |
| `secret` | `radius` (shared secret, required), `snmp` (v1/v2c: community, default `public`; v3: privacy password), `game_server` (Satisfactory API token, optional) |
| `keyword`, `keyword_invert` | `mqtt` (message payload, needs a topic), `snmp` (value text), `game_server` (server name or MOTD) |
| `ignore_tls` | every type that can use TLS |

`secret` is masked like the other secrets: responses carry `"••••••"`, sending that value back keeps the stored one
while the host and port of `target` (every MongoDB host) are unchanged and its protection isn't lowered
(`rediss://` → `redis://`, a lower PostgreSQL/MySQL `sslmode`, a lower SQL Server `encrypt`, MongoDB TLS turned off
or its certificate no longer checked): then the secret has to be sent again.

The MQTT topic is the rest of the target after the host and port, as written: `mqtt://broker/home/#` subscribes
to `home/#` (escape a literal `%` as `%25`). PostgreSQL and MySQL take `?sslmode=` with libpq's meaning:
`disable`, `prefer` (default: TLS when offered), `require`, `verify-ca` or `verify-full`; only the `verify-*`
modes check the certificate (unless `ignore_tls`).

SQL Server takes `?encrypt=`: absent (default: the whole session is encrypted when the server can), `false`
(only the login is encrypted), `disable` (no TLS), `true` (encrypt or fail) or `strict` (TDS 8.0, TLS first); the
certificate is checked only with `encrypt=true|strict&trustServerCertificate=false`. Without `basic_auth_user`
only the pre-login handshake runs. MongoDB uses TLS with `?tls=true` (and for `mongodb+srv://`) and checks the
certificate unless `tlsAllowInvalidCertificates=true` or `tlsInsecure=true`; `replicaSet=` makes a set without a
primary *down*, and `authSource=` (default: the path, else `admin`) and `authMechanism=SCRAM-SHA-256|SCRAM-SHA-1`
are honoured.

### SNMP versions

`snmp` takes `?version=1|2c|3` (default `2c`). Version 3 uses the User-based Security Model: `basic_auth_user` is
the user (required), and which passwords are set picks the security level — none (noAuthNoPriv), `basic_auth_pass`
(authNoPriv) or both it and `secret` (authPriv; a privacy password alone is refused). `?auth=` names the
authentication protocol, `md5`, `sha` (SHA-1, the default), `sha224`, `sha256`, `sha384` or `sha512`; `?priv=` the
privacy protocol, `des`, `aes` (AES-128, the default), `aes192` or `aes256` (AES-192/256 extend the key the way
net-snmp does; Cisco's "-C" variant isn't supported). `?context=` sets the context name. Passwords have at least 8
characters. Example: `snmp://core.local/1.3.6.1.2.1.1.5.0?version=3&auth=sha256&priv=aes`.

Changing the version from 3 to 1 or 2c counts as lowering the protection (the community travels in the clear), so
a masked `secret` has to be sent again. Agents older than 0.3.0 don't get SNMPv3 monitors.

### Tailscale ping monitors

`tailscale_ping` asks the `tailscaled` of the machine running the check to ping a peer. Its `target` is the peer's
machine name, MagicDNS name or Tailscale IP, optionally with `?type=tsmp` or `?type=icmp` (default: a disco ping,
like `tailscale ping`). It needs the daemon's socket: see [tailscale-ping.md](tailscale-ping.md).

### Game server monitors

`game_server` queries a game server in its own protocol. The `target` scheme names the game:
`minecraft://host[:port]` (Java Edition Server List Ping, port 25565; without a port the `_minecraft._tcp.host` SRV
record is tried first), `bedrock://` (Minecraft Bedrock, RakNet ping, 19132), `satisfactory://` (Lightweight Query
API over UDP, 7777), `fivem://` (FiveM or RedM, `/info.json` and `/dynamic.json` over HTTP, 30120) or `mumble://`
(UDP ping, 64738); no path or options. Valve/Source games (Counter-Strike, Rust, ARK, Valheim, …) use `steam`
instead. `secret` is only used by Satisfactory: an API token makes the check also read the player count from the
server's HTTPS API (its certificate isn't checked; it is self-signed unless the owner installed one).

### gRPC keyword monitors

`grpc_keyword` calls one unary method. Its `target` is `grpc[s]://host[:port]/package.Service/Method` (port 50051,
443 for `grpcs://`); a short service name (`Greeter`) works when it is unique, and the method may be written in
lowerCamel (`sayHello`) like Uptime Kuma does. The other fields:

| Field | Meaning |
|---|---|
| `grpc_proto` | `.proto` source that defines the service (one file; `google/protobuf/*.proto` imports work). `null` = ask the server through server reflection (`grpc.reflection.v1`, falling back to `v1alpha`) |
| `http_body` | The request message as a JSON object (unknown fields are ignored); empty = all defaults |
| `http_headers` | Metadata sent with the call, a JSON object like `{"authorization": "Bearer …"}` (`content-type`, `te`, `grpc-*` are refused) |
| `keyword`, `keyword_invert` | Text that must (or, inverted, must not) appear in the answer rendered as JSON: camelCase field names, enum names, 64-bit integers as strings, default values left out. Empty = any OK answer is up |
| `ignore_tls` | Don't check the `grpcs://` certificate |

A non-OK gRPC status is *down* (`gRPC status NOT_FOUND: …`), including `CANCELLED`. Saving checks the `.proto`
(compiler errors come back on `grpc_proto` with the line), that the method exists and is unary, and that the body
fits the request message.

## HTTP monitor authentication

HTTP, keyword and JSON query monitors take an `auth_method`: `none` (default), `basic`, `oauth2` or `mtls`. Only
the chosen method's fields are kept (the others are cleared, so `{"auth_method": "none"}` switches basic auth
off). A request that sets `basic_auth_user` without `auth_method` (or with an empty one) means `basic`, as before.

| Method | Fields |
|---|---|
| `basic` | `basic_auth_user` (required), `basic_auth_pass` |
| `oauth2` | OAuth2 client credentials grant: `oauth_token_url`, `oauth_client_id`, `oauth_client_secret` (required), `oauth_scope` (space-separated, optional), `oauth_audience` (optional), `oauth_client_auth` (`client_secret_basic` = Authorization header, default, or `client_secret_post` = request body). Each check sends `Authorization: Bearer <token>`; the token is cached per monitor until shortly before it expires. A failing token endpoint makes the check *down* with the endpoint's error, never the secret. |
| `mtls` | `tls_client_cert` (PEM, chain allowed), `tls_client_key` (unencrypted PEM), `tls_ca_cert` (optional extra PEM CA). `target` must be `https://`. |

`basic_auth_pass`, `oauth_client_secret` and `tls_client_key` are never returned: responses carry `"••••••"` instead.
Send that value back (or leave the field out on `PATCH`) to keep the stored secret; it is only kept while the
destination is unchanged (`target` and `proxy_url` for the password and the key, `oauth_token_url` and `proxy_url`
for the client secret). Otherwise the request is refused with a field error asking for the secret again.

```bash
curl -s -H "$AUTH" -H "Content-Type: application/json" -X POST "$BASE/api/v1/monitors" -d '{
  "name": "Partner API",
  "monitor_type": "http",
  "target": "https://api.example.com/health",
  "auth_method": "oauth2",
  "oauth_token_url": "https://login.example.com/oauth/token",
  "oauth_client_id": "vigilant-vole",
  "oauth_client_secret": "…",
  "oauth_scope": "health.read"
}'
```
