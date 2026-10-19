# Docker label auto-discovery

Label a container and it becomes a monitor — no clicking. Vigilant Vole watches the Docker host, creates a
monitor for every container labelled `vigilant.enable=true`, keeps it in sync with the labels, and pauses it
when the container is removed.

## Turn it on

1. Give Vigilant Vole access to Docker (see [Permissions](#permissions)).
2. **Settings → Integrations → Docker discovery** (admins): switch on **Discover containers**. Optionally set a
   **Docker host**; empty means the local socket (or `DOCKER_HOST` if set in the container's environment).
3. Click **Sync now**, or just wait: a sync runs every 60 seconds and immediately when containers start, stop
   or change.

Config-as-code equivalent:

```yaml
settings:
  discovery_enabled: true
  discovery_docker_host: ""          # or tcp://docker-proxy:2375
```

Supported Docker host formats: empty (local defaults), `unix:///var/run/docker.sock`, `/var/run/docker.sock`,
`tcp://host:2375`, `http://host:2375`, and `npipe://` on Windows. TLS Docker endpoints (`https://`) are not
supported — use a socket proxy on a private network instead.

## Labels

| Label | Example | Meaning |
|---|---|---|
| `vigilant.enable` | `true` | **Required.** Monitor this container (`true`, `1`, `yes`, `on`). |
| `vigilant.name` | `Payments API` | Display name. Defaults to the container name. |
| `vigilant.type` | `http` | Monitor type: `docker` (default), `http`, `keyword`, `json_query`, `tcp`, `ping`, `dns`, `websocket`, `ntp`, `steam`, `domain_expiry`. Aliases: `https` → `http`, `port` → `tcp`, `container` → `docker`, and `-` may replace `_` (`json-query`). |
| `vigilant.target` | `http://payments:8080/health` | URL or `host:port` to check. Required for every type except `docker`, where it defaults to the container's name. |
| `vigilant.interval` | `60` | Seconds between checks (20–86400). Default 60. |
| `vigilant.retries` | `2` | Failed checks before the monitor goes down (0–10). Default 0. |
| `vigilant.timeout` | `10` | Seconds before a check gives up (1–300). |
| `vigilant.keyword` | `OK` | Text the response must contain (`keyword` type). |
| `vigilant.group` | `Production` | Put the monitor in this group; the group monitor is created if needed. |
| `vigilant.tags` | `prod,team:payments` | Comma-separated tags; `name:value` sets a tag value. New tags get a colour automatically. |
| `vigilant.notify` | `Ops Discord,Pager` | Comma-separated notification channel **names** to link. |
| `vigilant.description` | `Customer checkout` | Description on the monitor page. |

The `docker` type checks that the container is running, and healthy if it has a `HEALTHCHECK`
(`starting` counts as pending, not down).

### Example

```yaml
services:
  payments:
    image: ghcr.io/acme/payments:latest
    labels:
      vigilant.enable: "true"
      vigilant.name: "Payments API"
      vigilant.type: "http"
      vigilant.target: "http://payments:8080/health"
      vigilant.interval: "30"
      vigilant.group: "Production"
      vigilant.tags: "prod,team:payments"
      vigilant.notify: "Ops Discord"
  redis:
    image: redis:7
    labels:
      vigilant.enable: "true"   # type defaults to docker: running + healthy
```

For `http`/`tcp` targets, use an address Vigilant Vole can reach — e.g. the service name when both containers
share a Docker network, or the host's address and published port otherwise.

## What a sync does

| Situation | Result |
|---|---|
| New labelled container | Monitor created with `source = docker` and the container name as its key, plus its group, tags and channel links. |
| Labels changed | The label-managed fields (name, description, type, target, interval, retries, timeout, keyword, and the Docker host for `docker` monitors) are updated — UI edits to these fields are overwritten on the next sync. Group membership, tags and channel links from labels are added if missing. |
| Container stopped | Nothing changes in discovery; a `docker` monitor reports it as down. |
| Container removed (or its `vigilant.enable` label removed) | Monitor **paused**, history kept. |
| Container back | Monitor resumed — but only if discovery paused it. Monitors you paused by hand stay paused. |
| Invalid label (bad type, missing target, out-of-range number) | That container is skipped; the error is shown under Settings → Integrations. |

Monitors you create by hand (`source = manual`) or through config-as-code (`source = config`) are never
touched, even if their name matches a container. Discovered monitors are excluded from config exports and are
never deleted by a config `replace` import.

Things labels don't manage — e.g. accepted status codes, escalation, maintenance windows, status-page
placement — can be edited on the discovered monitor in the UI and are kept across syncs.

Status: **Settings → Integrations** shows the last sync time, its summary and any error. Prometheus exports
`vigilant_discovery_monitors`, `vigilant_discovery_last_sync_timestamp_seconds` and
`vigilant_discovery_last_sync_success` ([metrics](metrics.md)).

## Permissions

The image runs as the unprivileged user `vole` (uid/gid 1000). Docker monitors and discovery only read from the
Docker API (container list/inspect and the event stream). Pick one:

**Option A — mount the socket read-only and add its group.**

```yaml
services:
  vigilant-vole:
    volumes:
      - ./data:/data
      - /var/run/docker.sock:/var/run/docker.sock:ro
    group_add:
      - "999"   # stat -c %g /var/run/docker.sock
```

The GID is the socket's group on the **host** (often the `docker` group, e.g. 999 or 998; on Docker Desktop the socket is `root:root`, so use `"0"`). Check from inside the container with
`docker exec vigilant-vole stat -c %g /var/run/docker.sock`.
`:ro` only stops the container from replacing the socket file; anyone who can talk to the socket can still
control Docker. Only do this on hosts where that's acceptable.

**Option B — a socket proxy (recommended).** Run a proxy that only allows read endpoints, and point
Vigilant Vole at it:

```yaml
services:
  docker-proxy:
    image: tecnativa/docker-socket-proxy
    environment:
      - CONTAINERS=1
      - EVENTS=1
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
  vigilant-vole:
    # ...
    environment:
      - DOCKER_HOST=tcp://docker-proxy:2375
```

Then leave the Docker host empty (it follows `DOCKER_HOST`) or enter `tcp://docker-proxy:2375` in
Settings → Integrations and on individual Docker monitors.

**Option C — run as root** (`user: "0:0"` in Compose). Simplest, least isolated.
