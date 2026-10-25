# Remote agents (multi-location checks)

A remote agent is the same `vigilant-vole` binary (and the same Docker image) started in **agent mode**.
It runs on another host, network or region, fetches the monitors you've marked for agents, runs them with
exactly the same checkers the server uses, and posts the raw results back.

Use agents to:

- see a service from more than one place (each agent's latest result shows on the monitor page and in
  [`/metrics`](metrics.md));
- **confirm outages** before alerting, so a problem with the server's own network doesn't page you;
- **check what only the agent can reach**: let one agent check a monitor *instead of* the server, e.g. LAN
  addresses and Docker containers at home while the server runs on a public VPS
  (see [Checks only an agent can reach](#checks-only-an-agent-can-reach)).

Agents only need outbound HTTPS to the server. They have no web UI, no database and no inbound port.
Run one with Docker, as a systemd service from the Linux installer, in a Proxmox container, or as a plain
binary (see [3. Start the agent](#3-start-the-agent)).

## 1. Create the agent (server)

**Settings → Remote agents → New agent** (admins). Give it a name and an optional location
(e.g. `eu-central`). The token (`vva_…`) is shown **once**, together with ready-to-paste `docker run`,
Compose and binary commands. Only a SHA-256 of the token is stored.

From the same page you can **rotate** the token (the old one stops working immediately), **disable**
the agent (it gets `403` and its results stop counting) or delete it. Deleting an agent pauses the monitors it
checks for the server (see [Checks only an agent can reach](#checks-only-an-agent-can-reach)); the confirmation
says how many.

## 2. Pick the monitors

On a monitor's form, under the alerting options:

| Switch | Field | Effect |
|---|---|---|
| **Check from remote agents too** | `run_on_agents` | Every enabled agent runs this monitor on the monitor's own interval, in addition to the local check. |
| **Confirm outages with agents** | `confirm_with_agents` | When the local check fails (after retries), only mark the monitor down and notify if no agent has a fresh result, or a strict majority of the fresh agent results are down. Otherwise the monitor stays *pending* with "Local check failed; N/M remote agents report up". |

"Fresh" means the agent's latest result for that monitor is at most 2× the monitor's interval old, and only
enabled agents count. With these two switches the local check is always authoritative for the monitor's own
status, history, uptime and badges; agent results confirm it, they never drive it on their own. To let one agent's
results *be* the monitor's status, use **Check from** instead (next section).

To pre-tick the second switch for every new monitor, set `agent_confirmation_default: true` under
`settings:` in a [config-as-code](config-as-code.md) document (there's no UI toggle for it yet).

Agents can run every active check type (HTTP, keyword, JSON query, TCP, ping, DNS, WebSocket, Docker, NTP,
Steam, game servers, domain expiry, and the service types below) except **real browser**: those checks drive the server's
remote browsers (Settings → Remote browsers), which agents don't have, so real-browser monitors are never sent
to agents (the form says so when you tick *Check from remote agents too*). Push and group monitors have
nothing to run.

## Checks only an agent can reach

Some things can only be checked from one place: a NAS at `10.0.3.7`, a router's admin page, the Docker containers
on the home server. Run Vigilant Vole on a public VPS, run an agent (the same image, `agent` mode) on the home
server, and pick that agent under **Check from** on the monitor's form (below *Remote agents*; the default is
*This server*):

- **The server doesn't run the check.** The chosen agent runs it on the monitor's own interval, whether or not
  *Check from remote agents too* is on. Agents of any version do this: the server simply lists the monitor for that
  agent (and only that agent).
- **The agent's results are the monitor's status.** Each one goes through the same pipeline as a local check:
  history, status changes, retries and *pending*, maintenance windows, notifications, uptime, badges, status pages
  and the live dashboard. The monitor page and the monitor list say "Checked by agent *name*".
- **Silence is an outage.** When the agent sends no result for the monitor for more than 2× its interval plus
  15 seconds, the monitor records `down` with "Agent *name* has not reported for *n* s" (then again every retry
  interval), and recovers with the agent's next result. So when the whole home goes offline, power, internet or the
  home server itself, its monitors go down and the VPS alerts you. After a server restart the agent gets one extra
  poll interval (30 s) to report. Like a missed push, a silent agent records a `down` result every retry interval
  until it reports again; [retention](config-as-code.md#settings) (Settings → General) cleans that history up as
  usual.
- **Disabled agent:** its monitors go down with "Agent *name* is disabled" until you enable it again.
- **Deleted agent:** its monitors are **paused** in the same step (the server logs which ones, and the message
  after deleting says how many), so targets only that agent could reach don't all go down from the server at once.
  They no longer name an agent: resume one and this server checks it, or pick another agent under *Check from*
  first.

*Check from remote agents too* and *Confirm outages with agents* don't apply to such a monitor (the form hides them,
the API and config files reject the combination), and *Check now* is not offered: only the agent runs it. Push,
group and real-browser monitors are always checked by the server. Docker container monitors use the agent host's
Docker socket, so mount it into the agent container
(see [Docker discovery](docker-discovery.md#permissions)).

In the [API](api.md#agent-checked-monitors) the field is `check_from_agent_id`; in
[config-as-code](config-as-code.md) it is `check_from: <agent name>`.

## 3. Start the agent

### Docker

```bash
docker run -d --name vigilant-vole-agent --restart unless-stopped \
  -e VV_AGENT_TOKEN=vva_xxxxxxxxxxxxxxxx \
  vigilant-vole:latest agent \
  --server https://status.example.com \
  --name "Frankfurt"
```

### Docker Compose

```yaml
services:
  vigilant-vole-agent:
    image: vigilant-vole:latest
    container_name: vigilant-vole-agent
    restart: unless-stopped
    command: agent
    environment:
      - VV_AGENT_SERVER=https://status.example.com
      - VV_AGENT_TOKEN=vva_xxxxxxxxxxxxxxxx
      - VV_AGENT_NAME=Frankfurt
```

Ping checks work as the image's non-root user through unprivileged ICMP sockets (Docker enables them by
default). Add `cap_add: [NET_RAW]` only if your runtime disables them. Docker-container monitors run on an agent
check the agent host's Docker, so mount its socket the same way as on the server
(see [Docker discovery](docker-discovery.md#permissions)). Tailscale ping monitors use the agent host's
`tailscaled` the same way: mount its socket ([Tailscale ping](tailscale-ping.md#giving-the-container-the-socket)).

### Linux service (installer)

On any 64-bit Linux with systemd (Debian, Ubuntu, a VM, a Raspberry Pi), as root, run the
[installer](install-linux.md) and pick **2) Remote agent**; it asks for the server URL, the token (hidden)
and a name:

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)"
```

Non-interactive (for automation; the environment variable keeps the token out of `ps`):

```bash
export VV_AGENT_TOKEN=vva_xxxxxxxxxxxxxxxx
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- \
  --agent --server https://status.example.com --name Frankfurt
```

(`VV_INSTALL_MODE=agent`, `VV_AGENT_SERVER` and `VV_AGENT_NAME` work as well.) The installer checks that the
server answers and accepts the token, writes `/etc/vigilant-vole/agent.env` (mode 0600) and starts
`vigilant-vole-agent.service`. `-- update` updates it, `-- uninstall --agent` removes it. Details:
[install-linux.md → Remote agent](install-linux.md#remote-agent).

### Proxmox VE container

The [Proxmox LXC script](install-linux.md#proxmox-ve-lxc) asks whether to install the server or a remote
agent; an agent container gets 1 CPU, 256 MB RAM and a 1 GB disk by default. In the **Proxmox VE shell**:

```bash
export COMMUNITY_SCRIPTS_URL="https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/proxmox"
bash -c "$(curl -fsSL "$COMMUNITY_SCRIPTS_URL/ct/vigilant-vole.sh")"     # choose "agent"
```

Unattended, with the answers given up front:

```bash
export COMMUNITY_SCRIPTS_URL="https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/proxmox"
export var_mode=agent var_agent_server=https://status.example.com var_agent_name=Proxmox
read -rsp "Agent token: " var_agent_token && export var_agent_token && echo
mode=default bash -c "$(curl -fsSL "$COMMUNITY_SCRIPTS_URL/ct/vigilant-vole.sh")"
unset var_agent_token
```

Run `update` in the container console to update the agent.

### Plain binary

```bash
VV_AGENT_TOKEN=vva_xxxxxxxxxxxxxxxx vigilant-vole agent --server https://status.example.com
```

### CLI reference

```
vigilant-vole agent --server <URL> --token <TOKEN> [--name <NAME>] [--once] [--concurrency <N>]
```

| Flag | Env var | Default | Meaning |
|---|---|---|---|
| `--server` | `VV_AGENT_SERVER` | – (required) | Base URL of the server, e.g. `https://status.example.com`. |
| `--token` | `VV_AGENT_TOKEN` | – (required) | The `vva_…` token from Settings → Remote agents. Prefer the env var so it doesn't show up in `ps`. |
| `--name` | `VV_AGENT_NAME` | hostname | Display name reported with each request. |
| `--once` | – | off | Fetch the monitors, run every check once, post the results, print a summary and exit. Non-zero exit if the server rejected any result. Good for testing a new agent or for cron. |
| `--concurrency` | `VV_AGENT_CONCURRENCY` | `16` | Checks allowed to run at the same time. |

Logging follows `RUST_LOG` (default `info`).

## How it works

- Every 30 seconds the agent polls `GET /api/agent/v1/monitors` with `Authorization: Bearer vva_…` and
  `If-None-Match`, so an unchanged list costs a `304`.
- Each monitor runs on its own interval with a jittered start, under the concurrency limit.
- Results are buffered and posted to `POST /api/agent/v1/results` every 5 seconds (at most 500 per request).
- If the server is unreachable the agent retries with exponential backoff (up to 60 s) and keeps up to
  5,000 results, dropping the oldest first. The server drops results that are older than 5 minutes
  (or 2× the monitor's interval, if longer) as stale.
- Ctrl-C / SIGTERM stops the checks and flushes what's buffered.

- An agent only receives the monitors its version can run: TLS, SMTP and the service types (Redis,
  PostgreSQL, MySQL, MQTT, Kafka, RabbitMQ, gRPC, RADIUS, SNMP) and HTTP monitors with OAuth2 or mTLS need
  agent 0.2.0 or newer, SQL Server, MongoDB, gRPC keyword, game server, Tailscale ping and SNMPv3 (`?version=3`) monitors agent 0.3.0 or newer; older agents keep running the
  rest, and the server logs which agent to upgrade.
  An agent that gets a monitor it doesn't understand skips it with a warning and runs the others.

**Trust model.** An agent token can read the full configuration of the monitors marked *Check from remote
agents too* and of the monitors it checks for the server (*Check from*) — including their own headers and credentials (basic-auth and service passwords, RADIUS/SNMP
secrets, the OAuth2 client secret and the mTLS private key), because the agent needs them to run the
check (push tokens are never sent). It can report results for those monitors only, and can't read or change
anything else. Treat agent hosts accordingly, and rotate the token if one is compromised.

## Seeing the results

- **Monitor page**: the latest result from each agent next to the local status.
- **Settings → Remote agents**: online/offline (checked in within 90 s), version, last seen, and how many monitors
  each agent reported on in the last 15 minutes, with how many of them only that agent checks ("12 monitors (4
  checked only by this agent)"). *Monitors run on agents* counts both kinds; *Checked only by one agent* is the
  agent-checked part.
- **Prometheus**: `vigilant_agent_up`, `vigilant_agent_last_seen_timestamp_seconds`,
  `vigilant_agent_monitor_status`, `vigilant_agent_monitor_response_time_seconds`.
- **API**: `GET /api/v1/agents` (admin); `checked_monitors` is the number of active monitors only that agent checks.
