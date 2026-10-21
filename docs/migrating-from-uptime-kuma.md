# Migrating from Uptime Kuma

Vigilant Vole imports an Uptime Kuma setup directly — monitors, groups, tags, notifications, status pages and
maintenance windows — with a dry-run preview before anything is written. Kuma itself is never modified, so you
can run both side by side while you compare.

## 1. Get Kuma's data

Either of these works:

- **`kuma.db`** (recommended) — the SQLite database in Kuma's data folder (`/app/data/kuma.db` in the official
  container, e.g. `docker cp uptime-kuma:/app/data/kuma.db .`). Kuma **1.23** and **2.x** (SQLite) schemas are
  supported. **Stop Kuma first** (or copy the `-wal` file too), so the file is complete.
- **Kuma's JSON backup** (Settings → Backup → Export in older Kuma versions). It has no status pages or
  maintenance windows; use `kuma.db` to bring those too.

Kuma 2.x installs that use MariaDB/MySQL aren't supported directly — export a JSON backup instead.

## 2. Preview

**Settings → Backup → Move from Uptime Kuma** (admins): choose the file and click **Preview import**
(uploads up to 1 GB). You get the same plan as a config import:

- what will be **created**, **updated** or left **unchanged**;
- **skipped** items, each with a reason (e.g. unsupported monitor type);
- **warnings** for things that were imported but need a look (e.g. a Teams connector URL).

## 3. Apply

Click **Apply**. The import runs in *merge* mode: nothing existing is deleted. Importing the same file again
updates the same monitors instead of duplicating them, so you can re-run it after fixing things in Kuma.

Afterwards:

1. Open each notification channel and click **Send test**.
2. Point push-monitor clients at the new server: tokens are kept, only the host changes
   (`https://<new-host>/api/push/<same-token>?status=up&msg=OK&ping=`).
3. Check the warnings list from the preview.
4. When you're happy, turn off notifications in Kuma (or stop it) so you don't get double alerts.

## What's imported

| Kuma | Vigilant Vole |
|---|---|
| HTTP(s) | `http` |
| HTTP(s) – Browser Engine (real-browser) | `real_browser`, linked to the imported remote browser. Monitors that used Kuma's built-in Chromium (no remote browser) are imported too and the plan warns you to set one: they use the first remote browser here, so add one under Settings → Remote browsers ([guide](real-browser.md)). Kuma accepts 200-399 and checks nothing else on the page, which are the defaults here; ignore TLS errors carries over |
| Remote browsers | Remote browsers (name and address). Kuma connects with Playwright; a browserless `/playwright` address becomes its DevTools (CDP) endpoint on the same host with the same token, and the plan reminds you to test each one |
| HTTP(s) – Keyword | `keyword` (incl. inverted keyword) |
| HTTP(s) – Json Query | `json_query` |
| TCP Port | `tcp` |
| Ping | `ping` |
| DNS | `dns` |
| Docker Container | `docker` |
| Push | `push` (token kept) |
| Steam Game Server | `steam` |
| GameDig | GameDig 4.x ids (Kuma 1.x) and 5.x ids (Kuma 2.x) are both understood. The 137 games GameDig queries with Valve's protocol (Counter-Strike, Team Fortress 2, Rust, ARK, Valheim, 7 Days to Die, …) become `steam` monitors: with Kuma's default *given port only* the port as entered, otherwise the game's query port (game port + offset, or its fixed query port), which the plan points out since the Steam type checks that one port only. Minecraft, Minecraft Bedrock, Satisfactory (token → API token), FiveM/RedM and Mumble become `game_server` monitors (`minecraft://host` keeps Minecraft's SRV lookup when the port is the default 25565). Every other game is skipped with its name and query protocol |
| PostgreSQL, MySQL/MariaDB, Redis | `postgres`, `mysql`, `redis` (the login moves out of the connection string into the user name and password; a custom query isn't run) |
| Microsoft SQL Server | `sqlserver` (`Server=host,port;Database=…;User Id=…;Password=…;Encrypt=…;TrustServerCertificate=…` becomes `sqlserver://host:port/db?encrypt=…&trustServerCertificate=…` plus the login; Windows authentication is skipped; a named instance needs its port; a custom query isn't run) |
| MongoDB | `mongodb` (the `mongodb://` / `mongodb+srv://` address without its login, which moves into the user name and password; a custom command isn't run) |
| MQTT | `mqtt` (topic in the address, success message as the keyword; WebSocket brokers aren't supported) |
| Kafka Producer | `kafka` (first broker; checks the topic exists, no message is produced; SASL isn't supported) |
| RabbitMQ | `http` against the management API's alarms endpoint, like Kuma does |
| RADIUS | `radius` |
| SNMP | `snmp` (v1/v2c: community string and expected value kept; v3: `?version=3` with Kuma's SNMPv3 user name and no passwords, which is noAuthNoPriv as in Kuma — add passwords afterwards for authNoPriv/authPriv) |
| SMTP | `smtp` |
| gRPC(s) – Keyword | `grpc_keyword`: the URL, TLS switch, service and method become one target (`grpcs://host:port/pkg.Service/method`); the `.proto`, JSON body, keyword and inverted keyword carry over. Kuma never sent its metadata; here it becomes the call's metadata and is sent, which the plan points out. Kuma counted a `CANCELLED` answer as up; here every non-OK status is down (the plan warns). Without a `.proto` the server's reflection service is used |
| Tailscale ping | `tailscale_ping` with the same host name; the container needs the host's `tailscaled` socket ([guide](tailscale-ping.md)) |
| Group | `group` (hierarchy kept) |
| WebSocket upgrade | `websocket` |
| Interval, retry interval, timeout, retries, resend interval, upside-down, paused state | Same fields |
| Method, headers, body, body encoding, accepted status codes, max redirects, ignore TLS, proxy | Same fields |
| Authentication method `basic` (or a user name without a method, as in older Kuma) | `auth_method: basic` with the user name and password |
| Authentication method `oauth2-cc` (token URL, client ID and secret, scopes, audience, client authentication method) | `auth_method: oauth2` with `oauth_token_url`, `oauth_client_id`, `oauth_client_secret`, `oauth_scope`, `oauth_audience` and `oauth_client_auth` (`client_secret_basic` / `client_secret_post`) |
| Authentication method `mtls` (CA, client certificate and key) | `auth_method: mtls` with `tls_ca_cert`, `tls_client_cert` and `tls_client_key` (the key must not be passphrase-protected, and the URL must be `https://`) |
| Authentication method `ntlm` | Not supported: the monitor is imported without authentication and the import lists a warning; it will report the server’s 401 until you change what it checks or put an authenticating proxy in front |
| Tags (with values and colours) | Tags (colours mapped to the nearest palette colour) |
| Notifications and per-monitor links | Notification channels and links |
| Status pages (sections, monitors, description, footer, custom CSS, domain) | Status pages (pages with a password are imported as drafts: set a password, then publish) |
| Status page icon | Logo and browser icon when Kuma has it as a data URL; an uploaded file (`/upload/…`) lives in Kuma's data folder, so the plan asks you to upload it again |
| Google Analytics tag (1.x), analytics type/ID/script URL (2.x) | `analytics` with the same provider (Google, Plausible, Umami, Matomo; Rybbit isn't supported) |
| Maintenance (manual, single window, recurring interval, weekday, day of month, cron) | Maintenance windows (`manual`, `once`, `daily`, `weekly`, `interval`, `monthly`, `cron`). “Last day” entries (`lastDay1`…`lastDay4`) become `-1`…`-4`. |

### Notification providers mapped

Discord, Slack, Telegram, Webhook, SMTP (→ Email), ntfy, Gotify, Pushover, Microsoft Teams, Matrix,
Mattermost, Rocket.Chat, Google Chat, PagerDuty, Opsgenie, Pushbullet, Signal, Apprise, Home Assistant,
Bark, Feishu, DingTalk (`dingding`), Twilio, Threema, Nextcloud Talk, WhatsApp (WAHA and Evolution API), SIGNL4,
Squadcast, Grafana OnCall, GoAlert, Splunk On-Call (its REST endpoint is split into API key and routing key), Keep,
SendGrid, Brevo (CC and BCC are not imported), ClickSend, 46elks and Gorush.

Any other Kuma provider is skipped with the hint to use **Apprise** (90+ services through an Apprise API server)
or a **Webhook** instead. Notes:

- **Webhook** custom bodies use Kuma's template syntax; Vigilant Vole uses `{{placeholder}}` names
  (see the webhook form) — check them.
- **Teams** needs a Workflows webhook URL; old Office 365 connector URLs stop working.
- **Apprise** here talks to an [Apprise API](https://github.com/caronc/apprise-api) server; add its URL to the
  channel.

### Not imported

| Kuma feature | Why / what to do |
|---|---|
| SQL Server with Windows authentication | Not supported yet. Skipped and listed. |
| GameDig games outside the Valve protocol, Minecraft, Satisfactory, FiveM/RedM and Mumble (Battlefield, Quake 3, TeamSpeak 3, Terraria, …) | Not supported. Skipped and listed with the game's name. |
| Manual monitors | Use a push monitor. |
| Maintenance with a cron expression the parser doesn't accept (six fields with seconds, `L`/`W`/`#`), or “every N days” without a start date | Skipped and listed with the reason. |
| Heartbeat history, uptime history | Starts fresh. Keep Kuma around read-only if you need the old history. |
| Users, 2FA, API keys | Create users and tokens in Settings. |
| Kuma's own settings (time zone, base URL, …) | Set them in Settings → General. |

## Grafana / Prometheus

If you scraped Kuma's `/metrics`, the Kuma-compatible series (`monitor_status`, `monitor_response_time`,
`monitor_cert_days_remaining`, `monitor_cert_is_valid`) are exported with the same labels, so dashboards keep
working. Kuma used basic auth with an API key; here you use a Vigilant Vole API token as Bearer token or
basic-auth password. See [metrics](metrics.md).
