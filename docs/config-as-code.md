# Configuration as code

Your whole setup — settings, tags, notification channels, monitors, status pages and maintenance windows — can be
exported as one readable YAML (or JSON) document, kept in git, reviewed in pull requests and applied back, with a
dry-run plan first. History, users and API tokens are not part of it (use a [database backup](#backups-vs-config) for those).

## Three ways to use it

| Where | Export | Import |
|---|---|---|
| **UI** — Settings → Backup | *Configuration as code → Export configuration* (YAML/JSON, optional secrets) | *Import configuration*: upload or paste, choose merge/replace, **Preview changes**, then **Apply** |
| **API** | `GET /api/v1/config` | `POST /api/v1/config` (`?dry_run=1`, `?mode=replace&confirm=1`) |
| **GitOps** — `VV_CONFIG_FILE` | – | The file is merged into the database on every start |

Export needs the **editor** role; secrets in the export and every import need **admin**.

### API

```bash
TOKEN=vv_xxxxxxxxxxxxxxxx   # Settings → API tokens (a write token of an admin for imports)

# Export (YAML by default; format=json for JSON; secrets=1 to include secrets, admins only)
curl -H "Authorization: Bearer $TOKEN" "https://status.example.com/api/v1/config" > vigilant.yml

# What would change? (nothing is written)
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/yaml" \
     --data-binary @vigilant.yml "https://status.example.com/api/v1/config?dry_run=1"

# Apply (merge)
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/yaml" \
     --data-binary @vigilant.yml "https://status.example.com/api/v1/config"

# Replace: also deletes objects the document doesn't list (in the sections it contains)
curl -X POST ... "https://status.example.com/api/v1/config?mode=replace&dry_run=1"
curl -X POST ... "https://status.example.com/api/v1/config?mode=replace&confirm=1"
```

The body is YAML or JSON (detected automatically; JSON if it starts with `{`), UTF-8, at most 2 MB. The response is
the plan:

```json
{
  "applied": false,
  "mode": "merge",
  "summary": {"create": 2, "update": 1, "delete": 0, "unchanged": 14, "skip": 0},
  "items": [
    {"kind": "monitor", "key": "Website", "action": "update", "changes": ["interval_seconds"]},
    {"kind": "tag", "key": "staging", "action": "create"}
  ],
  "errors": [],
  "warnings": []
}
```

If anything is invalid you get `422` with every error (`fields` keyed as `kind.key.field`) and **nothing is
written** — an import is one transaction, all or nothing.

### GitOps with `VV_CONFIG_FILE`

```yaml
services:
  vigilant-vole:
    volumes:
      - ./data:/data
      - ./vigilant.yml:/config/vigilant.yml:ro
    environment:
      - VV_CONFIG_FILE=/config/vigilant.yml
```

On every start the file is merged in the background (always **merge**, never replace). Results and validation
errors go to the log (`VV_CONFIG_FILE applied … created=… updated=… unchanged=…`); if any entry is invalid nothing
is applied and the server keeps running with its current configuration. Changes made in the UI in the meantime
stay until the next restart re-applies the file's values for the objects it lists.

## Merge vs replace

- **merge** (default) creates what's new and updates what changed. Nothing is deleted.
- **replace** additionally **deletes** objects of a section *present in the document* that the document doesn't
  list. Sections left out of the document are never touched, in either mode. Docker-discovered monitors are never
  deleted. The API requires `confirm=1` (or `dry_run=1`) for replace.

Within an object, fields you leave out take their **defaults** — the document describes the full desired state of
each object it lists. Two exceptions: a monitor's `tags`/`notifications` and a status page's `sections` keep their
current value when the key is missing (write `[]` to clear them).

## Keys and references

Objects are matched by stable, human-readable keys instead of database ids, so a document can be applied to a
fresh instance:

| Section | Key |
|---|---|
| `tags` | `name` |
| `notification_channels` | `name` |
| `remote_browsers` | `name` |
| `monitors` | `key` (defaults to `name`) |
| `status_pages` | `slug` |
| `maintenance` | `title` |

Keys are compared case-insensitively. Monitors created by an import get `source: config` with their key as
`external_id`, so renaming such a monitor in the document (same `key`) updates it instead of creating a new one —
give monitors an explicit `key` if you expect to rename them. Duplicate names on export are disambiguated as
`"Name #id"`.

References use these keys: a monitor's `parent` (a group's key), `tags`, `notifications`, `remote_browser`; a status page section's
`monitors`; a maintenance window's `monitors` and `status_pages` (slugs).

## Secrets

Exports mask secrets as `••••••` unless an admin asks for them (`secrets=1` / *Include secrets*): channel
credentials, monitors' `basic_auth_pass`, `oauth_client_secret` and `tls_client_key`, sensitive HTTP header values (names containing `authorization`, `cookie`,
`token`, `secret`, `key`, `password`, `auth`, `session`, `signature`), push tokens, and the token or password in a
remote browser's `url`. **Importing a masked value
keeps the value already stored**, so a masked export round-trips safely and can live in git. For a new object a
masked secret is an error — supply the real value (e.g. by templating the file in CI).

## Document reference

```yaml
version: 1                    # format version (optional; currently 1)
settings: { … }               # every section is optional
tags: [ … ]
notification_channels: [ … ]
remote_browsers: [ … ]
monitors: [ … ]
status_pages: [ … ]
maintenance: [ … ]
```

Unknown keys are rejected (except inside a channel's `config`, where unknown keys are dropped).

### `settings`

Missing keys keep their current value.

| Key | Type | Notes |
|---|---|---|
| `site_name` | string | Shown in the header and page titles. |
| `base_url` | string | Public URL, e.g. `https://status.example.com`; used in notification links, push URLs and agent snippets. `""` clears it. |
| `timezone` | string | IANA name, e.g. `Europe/Oslo`. |
| `language` | string | `en` or `nb`. |
| `retention_days` | int | 1–3650. Raw heartbeats older than this are deleted; daily rollups are kept. |
| `badges_public` | bool | Badges for every monitor without auth (otherwise only monitors on a published status page). |
| `metrics_enabled` | bool | Enables `/metrics`. |
| `discovery_enabled` | bool | Docker label discovery. |
| `discovery_docker_host` | string | Empty = local socket. `""` clears it. |
| `agent_confirmation_default` | bool | New monitors start with *Confirm outages with agents* on. |

### `tags`

| Key | Type | Default | Notes |
|---|---|---|---|
| `name` | string | – | Required, unique. |
| `color` | string | `blue` | Palette name: `red`, `orange`, `yellow`, `green`, `mint`, `teal`, `cyan`, `blue`, `indigo`, `purple`, `pink`, `brown`, `gray`. Hex values map to the nearest. |

### `notification_channels`

| Key | Type | Default | Notes |
|---|---|---|---|
| `name` | string | – | Required, unique, < 100 characters. |
| `kind` | string | – | Provider, see below. |
| `active` | bool | `true` | |
| `default` | bool | `false` | Attach automatically to new monitors. |
| `config` | object | `{}` | Provider settings (required keys in **bold**, secrets marked 🔒). |

| `kind` | `config` keys |
|---|---|
| `discord` | **`url`** 🔒, `username`, `avatar_url`, `thread_id` |
| `slack` | **`url`** 🔒, `channel`, `username`, `icon_emoji` |
| `teams` | **`url`** 🔒 (Workflows webhook) |
| `googlechat` | **`url`** 🔒 |
| `mattermost` | **`url`** 🔒, `channel`, `username`, `icon_url` |
| `rocketchat` | **`url`** 🔒, `channel`, `username`, `emoji` |
| `matrix` | **`homeserver`**, **`room_id`**, **`access_token`** 🔒 |
| `webex` | **`token`** 🔒, `room_id`, `person_email` |
| `feishu` | **`url`** 🔒, `secret` 🔒 |
| `dingtalk` | **`url`** 🔒, `secret` 🔒 |
| `telegram` | **`bot_token`** 🔒, **`chat_id`**, `message_thread_id`, `silent`, `server_url` |
| `signal` | **`url`** (signal-cli REST API), **`number`**, **`recipients`** |
| `twilio` | **`account_sid`**, **`auth_token`** 🔒, **`from`**, **`to`** |
| `ntfy` | `server`, **`topic`**, `tags`, `access_token` 🔒, `username`, `password` 🔒 |
| `pushover` | **`user_key`** 🔒, **`app_token`** 🔒, `device`, `down_priority`, `priority`, `sound`, `retry`, `expire` |
| `gotify` | **`server`**, **`app_token`** 🔒, `priority` |
| `pushbullet` | **`access_token`** 🔒, `device_iden`, `channel_tag` |
| `bark` | `server`, **`device_key`** 🔒, `group`, `sound`, `critical` |
| `email` | **`host`**, `port`, `security` (`starttls` default, `tls`, `none`), `username`, `password` 🔒, **`from`**, **`to`**, `cc`, `bcc`, `subject_prefix`, `ignore_tls` |
| `pagerduty` | **`routing_key`** 🔒, `region` |
| `opsgenie` | **`api_key`** 🔒, `region`, `tags` |
| `webhook` | **`url`**, `method`, `content_type`, `headers` 🔒, `body_template` |
| `apprise` | **`url`** (Apprise API server), `config_key`, `urls` 🔒, `tag` |
| `homeassistant` | **`url`**, **`token`** 🔒, `service` |

`GET /api/v1/notification-providers` returns the same list with field types, labels and select options. The
webhook `body_template` supports `{{monitor_name}}`, `{{monitor_id}}`, `{{monitor_type}}`, `{{target}}`,
`{{status}}`, `{{event}}`, `{{message}}`, `{{summary}}`, `{{headline}}`, `{{url}}`, `{{downtime}}`,
`{{downtime_seconds}}`, `{{expires_in_days}}`, `{{occurred_at}}`.

### `remote_browsers`

The headless Chromium endpoints that `real_browser` monitors drive (Settings → Remote browsers; see
[real-browser.md](real-browser.md)). The first one is the default for monitors that don't name one.

| Key | Type | Notes |
|---|---|---|
| `name` | string | Required, unique, up to 80 characters. |
| `url` | string | `ws(s)://host:port/devtools/browser/<id>`, `http(s)://host:port` (discovered through `/json/version`) or a service address like `wss://host?token=…`. A token or password in it is exported masked; a masked URL keeps the stored secret while scheme, host and port stay the same. |

```yaml
remote_browsers:
  - name: Chrome
    url: http://chrome:9222
```

In `replace` mode, remote browsers the document doesn't list are deleted; their monitors fall back to the
default one.

### `monitors`

| Key | Type | Default | Notes |
|---|---|---|---|
| `key` | string | `name` | Stable identity, see above. |
| `name` | string | – | Required. |
| `monitor_type` | string | – | `http`, `keyword`, `json_query`, `tcp`, `ping`, `tailscale_ping`, `dns`, `push`, `docker`, `websocket`, `domain_expiry`, `ntp`, `steam`, `game_server`, `tls`, `smtp`, `redis`, `postgres`, `mysql`, `mqtt`, `kafka`, `rabbitmq`, `grpc`, `grpc_keyword`, `radius`, `snmp`, `sqlserver`, `mongodb`, `real_browser`, `group`. |
| `target` | string | `""` | URL (`http`, `keyword`, `json_query`, `real_browser`; `ws://`/`wss://` for `websocket`), `host:port` (`tcp`, `steam`; `ntp` port optional), `minecraft://`, `bedrock://`, `satisfactory://`, `fivem://` or `mumble://` + `host[:port]` (`game_server`; the scheme picks the game and its default port), host (`ping`, `dns`), Tailscale peer name or IP with optional `?type=tsmp|icmp` (`tailscale_ping`), domain (`domain_expiry`), container name or id (`docker`); `host[:port]` for `tls` and `smtp`; `scheme://host[:port][/path]` for the service types (`redis://`, `rediss://`, `postgres://`, `mysql://`, `mqtt://`, `mqtts://`, `kafka://`, `kafkas://`, `amqp://`, `amqps://`, `grpc://`, `grpcs://`, `radius://`, `snmp://`, `sqlserver://`, `mongodb://`, `mongodb+srv://`), where the path is the database, topic, virtual host, gRPC service or OID (MongoDB targets may list several hosts: `mongodb://a,b:27018/?replicaSet=rs0`); `grpc_keyword` takes `grpc[s]://host[:port]/package.Service/Method`; `snmp://` takes `?version=1|2c|3` and, for v3, `&auth=md5|sha|sha224|sha256|sha384|sha512&priv=des|aes|aes192|aes256&context=…`. Credentials never go into the target: use `basic_auth_user` / `basic_auth_pass` (SNMPv3: user and authentication password), and `secret` for the RADIUS shared secret, the SNMP community (SNMPv3: the privacy password) and the Satisfactory API token (`game_server`). Empty for `push` and `group`. |
| `parent` | string | – | Key of a `group` monitor. |
| `tags` | list | keep | `["production", {name: env, value: prod}]`. Unknown tag names are created. |
| `notifications` | list | keep (new: default channels) | Channel names: `["Ops Discord", {name: PagerDuty, escalation: true}]`. Escalation channels are only used after `escalation_minutes`. |
| `push_token` | string | generated | Push monitors only; missing or masked keeps the current token. |
| `description` | string | `""` | Up to 2,000 characters. |
| `active` | bool | `true` | `false` = paused. |
| `interval_seconds` | int | `60` | 10–86400. |
| `retry_interval_seconds` | int\|null | `null` | Interval while failing; `null` = same as `interval_seconds`. 10–86400. |
| `timeout_seconds` | int | `10` | 1–300, and not more than the interval. |
| `retries` | int | `0` | 0–20 failed checks (recorded as *pending*) before *down*. |
| `upside_down` | bool | `false` | Invert up/down. |
| `http_method` | string | `GET` | HTTP family. |
| `http_headers` | object\|string | – | `{Authorization: "Bearer …"}` or JSON text. For `grpc_keyword`: the call's metadata. |
| `http_body` | string | – | Up to 64 KB. For `grpc_keyword`: the request message as a JSON object. |
| `http_body_encoding` | string | `json` | `json`, `form`, `raw`. |
| `accepted_status_codes` | string | `200-399` | e.g. `200-299,301,2xx`. |
| `max_redirects` | int | `10` | 0–30. |
| `ignore_tls` | bool | `false` | Don't verify certificates. |
| `auth_method` | string | `none` | HTTP family: `none`, `basic`, `oauth2` or `mtls`. A document that sets `basic_auth_user` without `auth_method` (or with an empty one) means `basic`; an explicit `none` is kept. Only the chosen method's fields below are kept; the others are cleared. |
| `basic_auth_user` / `basic_auth_pass` | string | – | `basic`. The password is a secret. |
| `oauth_token_url` | string | – | `oauth2` (client credentials grant): the token endpoint, `http://` or `https://`. |
| `oauth_client_id` / `oauth_client_secret` | string | – | `oauth2`. The client secret is a secret; a masked value is only kept while `oauth_token_url` and `proxy_url` are unchanged. |
| `oauth_scope` / `oauth_audience` | string | – | `oauth2`, optional. Scopes are separated by spaces. |
| `oauth_client_auth` | string | `client_secret_basic` | `oauth2`: how the client authenticates, `client_secret_basic` (Authorization header) or `client_secret_post` (request body). |
| `tls_client_cert` / `tls_client_key` | string | – | `mtls`: PEM client certificate (chain) and its unencrypted PEM private key (a secret; a masked key is only kept while `target` and `proxy_url` are unchanged). The target must be `https://`. Use a YAML block scalar (`\|`) for the PEM text. |
| `tls_ca_cert` | string | – | `mtls`, optional: extra PEM CA certificate(s) to trust for the monitored server. |
| `proxy_url` | string | – | `http://`, `https://`, `socks5://` or `socks5h://` proxy. |
| `keyword` | string | – | `keyword` type: text to find (case-sensitive). `grpc_keyword`: text to find in the answer rendered as JSON (camelCase field names, enum names, 64-bit numbers as strings); optional. `game_server`: text the server name or MOTD must contain; optional. |
| `keyword_invert` | bool | `false` | Down when the keyword *is* found. |
| `json_path` | string | – | `json_query`: RFC 9535 JSONPath, e.g. `$.status`. |
| `json_operator` | string | `==` | `==`, `!=`, `contains`, `<`, `<=`, `>`, `>=`. |
| `json_expected` | string | – | Value to compare with. |
| `dns_record_type` | string | `A` | `A`, `AAAA`, `CNAME`, `MX`, `NS`, `TXT`, `SOA`, `SRV`, `CAA`. |
| `dns_resolver` | string | system | e.g. `1.1.1.1` or `1.1.1.1:53`. |
| `dns_expected` | string | – | Optional value the answer must contain. |
| `docker_host` | string | local | `unix:///var/run/docker.sock`, `tcp://host:2375`, … |
| `expiry_notify` | bool | `true` | Warn before TLS certificates (HTTPS) and domains (`domain_expiry`) expire. |
| `expiry_days` | int | `14` | 1–365; further warnings at 7, 3, 1 and 0 days. |
| `resend_interval` | int | `0` | Reminder every N down checks; 0 = off. |
| `escalation_minutes` | int | `0` | Notify escalation channels after N minutes down; 0 = off. |
| `flap_detection` | bool | `true` | Mute alerts while a monitor flaps (≥ 4 changes in 10 checks). |
| `run_on_agents` | bool | `false` | Remote agents run this check too. |
| `confirm_with_agents` | bool | `false` | Only alert when agents confirm the outage. |
| `check_from` | string | this server | The remote agent's `name` (case-insensitive) that checks this monitor instead of this server; its results are the monitor's status, and the monitor goes down when the agent stops reporting ([details](remote-agents.md#checks-only-an-agent-can-reach)). Agents aren't part of the document: create them in Settings → Remote agents first. An unknown or ambiguous name is a validation error, and so are `push`, `group` and `real_browser` monitors and `run_on_agents` / `confirm_with_agents` together with it. Leaving it out means this server checks the monitor. |
| `sort_order` | int | `0` | Dashboard order. |
| `remote_browser` | string | first one | `real_browser`: the remote browser's `name`. Without one, the first remote browser is used (the plan warns when there is none yet). |
| `browser_selector` | string | – | `real_browser`: CSS selector that must match; checked instead of `keyword` (which is the visible text to find). `keyword_invert` inverts either. |
| `browser_wait` | string | `load` | `real_browser`: `load` or `network_idle`. |
| `browser_viewport` | string | `desktop` | `real_browser`: `desktop` (1280×800) or `mobile` (390×844). |
| `browser_js_errors` | bool | `false` | `real_browser`: an uncaught JavaScript exception fails the check. |
| `grpc_proto` | string | – | `grpc_keyword`: the `.proto` source defining the called service (one file; `google/protobuf/*.proto` imports work), checked on import. Leave it out to ask the server through server reflection. Use a YAML block scalar (`\|`). |

### `status_pages`

| Key | Type | Default | Notes |
|---|---|---|---|
| `slug` | string | – | Required; public at `/status/<slug>`. |
| `title` | string | – | Required. |
| `description` | string | `""` | Shown under the title. |
| `custom_domain` | string | – | Serve the page at `/` on this host (point the DNS at Vigilant Vole / your proxy). |
| `published` | bool | `true` | `false` = draft (not public). |
| `theme` | string | `auto` | `auto`, `light`, `dark`. |
| `show_tags` | bool | `false` | |
| `show_uptime` | bool | `true` | 90-day uptime bars and percentages. |
| `show_response` | bool | `true` | Response times. |
| `show_powered_by` | bool | `true` | "Powered by Vigilant Vole" footer. |
| `footer_text` | string | – | Markdown. |
| `custom_css` | string | – | Added to the page (`</style`, `<script` and HTML comments are rejected). |
| `refresh_seconds` | int | `60` | Auto-refresh interval. |
| `sections` | list | keep (new: one empty "Services") | `[{name: API, monitors: [Website, "API health"]}]`, in display order. |
| `password` | string | keep | Write-only page password (8+ characters): a new value sets it, `""` removes it. Exports without secrets show `"••••••"` for a protected page, which keeps the stored password (and is an error for a page that has none). Re-applying the same plain password changes nothing. |
| `password_hash` | string | – | The Argon2 hash of the page password, in full exports (`?secrets=1`) instead of `password`, so a restore keeps the page protected. Use one of the two. |
| `analytics` | object | keep | `{enabled: true, unique_visitors: true, provider: none, site_id: "", script_url: ""}`. `enabled`/`unique_visitors`: the built-in, cookie-free counters. `provider`: `none`, `plausible` (`site_id` = domain(s), `script_url` = script), `umami` (`site_id` = website UUID, `script_url` = script), `matomo` (`site_id` = numeric ID, `script_url` = Matomo base URL) or `google` (`site_id` = `G-…`). URLs must be `https://`; IDs are checked strictly. Exported only when not the defaults. |
| `logo`, `favicon` | string | keep | `data:image/png;base64,…` (PNG, JPEG, WebP or ICO, ≤ 1 MB; checked by content, SVG refused); `""` removes. Included only in full exports (`?secrets=1`) because they can be large. |

Incidents are not part of the document (post them in the UI or with `POST /api/v1/status-pages/{id}/incidents`).

### `maintenance`

| Key | Type | Default | Notes |
|---|---|---|---|
| `title` | string | – | Required. |
| `description` | string | `""` | Markdown, shown on status pages. |
| `strategy` | string | – | `manual`, `once`, `daily`, `weekly`, `interval` (every N days), `monthly` (days of the month) or `cron`. |
| `active` | bool | `true` | Manual: in maintenance right now. Scheduled: enabled. |
| `start_at` / `end_at` | string | – | `once`: UTC `YYYY-MM-DD HH:MM:SS`. For the repeating strategies they optionally bound the schedule; `interval` counts its days from the date of `start_at` (required). |
| `weekdays` | string | – | `weekly`: ISO weekdays, `"1,3,5"` (Monday = 1). |
| `time_from` / `time_to` | string | – | `daily`/`weekly`/`interval`/`monthly`: local `HH:MM` in `timezone` (may cross midnight). |
| `interval_days` | int | – | `interval`: repeat every this many days (1–366). |
| `month_days` | string | – | `monthly`: days of the month, `"1,15,-1"`. Negative numbers count from the end (`-1` is the last day). A day a month doesn't have (the 31st in April) is skipped. |
| `cron` | string | – | `cron`: five fields (minute hour day-of-month month day-of-week), evaluated in `timezone`; lists, ranges, steps, month and weekday names and `@daily`-style macros work. When both day fields are set, either matches (as in Vixie cron). |
| `duration_minutes` | int | – | `cron`: how long each run lasts (1–44640). |
| `timezone` | string | `UTC` | IANA name. |
| `monitors` | list | `[]` | Monitor keys. A group covers its children. |
| `status_pages` | list | `[]` | Slugs of status pages that show the window. |

During maintenance, monitors keep checking but record *maintenance* instead of up/down, and send no notifications.
Status pages, badges and uptime show maintenance; the real result is still available through the API
(`raw_status`, see [api.md](api.md#status-during-maintenance)). A monitor that is still down when the window ends
starts a fresh outage then: retries, the outage's start time and the Down notification count from the end of the
window.

## A complete example

```yaml
version: 1

settings:
  site_name: Acme Status
  base_url: https://status.acme.example
  timezone: Europe/Oslo
  retention_days: 180
  metrics_enabled: true

tags:
  - name: production
    color: green
  - name: team
    color: purple

notification_channels:
  - name: Ops Discord
    kind: discord
    default: true
    config:
      url: "••••••"                 # keeps the stored webhook (set it once in the UI)
  - name: PagerDuty
    kind: pagerduty
    config:
      routing_key: "••••••"

monitors:
  - name: Production
    monitor_type: group

  - key: website
    name: Website
    monitor_type: http
    target: https://acme.example
    parent: Production
    interval_seconds: 30
    retries: 2
    accepted_status_codes: "200-299"
    escalation_minutes: 15
    tags: [production, {name: team, value: web}]
    notifications: [Ops Discord, {name: PagerDuty, escalation: true}]

  - name: API health
    monitor_type: json_query
    target: https://api.acme.example/health
    parent: Production
    json_path: $.status
    json_operator: "=="
    json_expected: ok
    http_headers:
      Authorization: "••••••"
    run_on_agents: true
    confirm_with_agents: true

  - name: Postgres port
    monitor_type: tcp
    target: db.internal:5432

  - name: Home NAS
    monitor_type: tcp
    target: 10.0.3.7:445
    check_from: Home                 # the agent named "Home" checks it, not this server

  - name: Nightly backup
    monitor_type: push
    interval_seconds: 86400          # down if no push for a day (+10% grace)

  - name: acme.example registration
    monitor_type: domain_expiry
    target: acme.example
    interval_seconds: 86400
    expiry_days: 30

status_pages:
  - slug: acme
    title: Acme status
    custom_domain: status.acme.example
    footer_text: "Questions? [support@acme.example](mailto:support@acme.example)"
    sections:
      - name: Services
        monitors: [website, API health]

maintenance:
  - title: Weekly database upgrade window
    strategy: weekly
    weekdays: "2"
    time_from: "22:00"
    time_to: "23:30"
    timezone: Europe/Oslo
    monitors: [Postgres port]
    status_pages: [acme]
```

## Backups vs config

| | Database backup | Config document |
|---|---|---|
| Contains | Everything: history, users, 2FA, API tokens, agents, secrets | Settings, tags, channels, monitors, status pages, maintenance |
| Format | SQLite file (`VACUUM INTO` snapshot) | YAML / JSON |
| Restore | Settings → Backup → *Restore on next start* (or swap the file while stopped) | Import (merge/replace), API, or `VV_CONFIG_FILE` |
| Use for | Disaster recovery, moving servers | Git, review, templating, cloning a setup |
