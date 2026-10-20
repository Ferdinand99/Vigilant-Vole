# MCP: let your AI assistant use Vigilant Vole

Vigilant Vole speaks the [Model Context Protocol](https://modelcontextprotocol.io). That lets **your own** AI
assistant (Claude Code, Claude Desktop, or any MCP client) look at your monitors, explain an outage, and, if you
allow it, pause a monitor, post an incident or start maintenance.

Vigilant Vole **never calls an AI itself** and does not relay AI usage: your assistant connects *to* Vigilant Vole,
using your own subscription. If you don't connect one, nothing happens.

## Set up

1. Under **Settings → API tokens**, create a token. **Start with a read token**: it can only look.
2. Connect the assistant. **Settings → Integrations → MCP and AI assistants** shows these with your address filled in.

**Claude Code** (talks to the server directly):

```sh
claude mcp add --transport http vigilant-vole https://status.example.com/mcp \
  --header "Authorization: Bearer vv_YOUR_API_TOKEN"
```

**Claude Desktop and other apps that only speak stdio** use the `vigilant-vole` program as a bridge. Put this in the
app's MCP settings (the token goes in the environment, never on the command line):

```json
{
  "mcpServers": {
    "vigilant-vole": {
      "command": "vigilant-vole",
      "args": ["mcp", "--url", "https://status.example.com"],
      "env": { "VV_MCP_TOKEN": "vv_YOUR_API_TOKEN" }
    }
  }
}
```

The bridge reads `VV_MCP_TOKEN` (or `VV_TOKEN`) and the address from `--url` / `VV_MCP_URL`. It forwards each
message to `POST <url>/mcp` and never follows redirects. Run `vigilant-vole` from the release binary or the
container (`docker run -i -e VV_MCP_TOKEN=… ghcr.io/ferdinand99/vigilant-vole mcp --url …`).

## What the assistant can do

Read tools (any token):

| Tool | What it gives |
|---|---|
| `instance_summary` | Version, counts per status, the monitors that are down or pending right now, status pages, agents |
| `list_monitors` | Monitors, filtered by status, type, tag or search text; paginated |
| `get_monitor` | One monitor with all settings and tags |
| `monitor_history` | Uptime over N days with per-day entries, plus recent checks |
| `list_events` | A monitor's outages, maintenance, pauses |
| `why_is_it_down` | One call for diagnosis: status, ongoing outage and its start, last events, last checks with their error messages, certificate and domain expiry |
| `list_status_pages`, `list_incidents` | Status pages and their incidents (optionally only open ones) |
| `list_maintenance` | Maintenance windows, schedule and state |

Editing tools (write token held by an editor or admin):

| Tool | What it does |
|---|---|
| `create_monitor`, `update_monitor` | Same fields and validation as the REST API |
| `pause_monitor`, `resume_monitor`, `check_now` | |
| `bulk_edit` | `POST /api/v1/monitors/bulk` semantics: `ids` or a `filter`, and `set` |
| `create_incident`, `update_incident` | Post an incident; change it; add a timeline update (`investigating`, `identified`, `monitoring`, `resolved`) |
| `start_maintenance`, `end_maintenance` | A manual window over some monitors, on until ended |

Resources: `vigilant-vole://api-reference` (this server's REST API reference as Markdown, the same text as
**Copy for AI** on the API page) and `vigilant-vole://monitor/{id}`.

## Security

- **Tokens only.** `/mcp` accepts `Authorization: Bearer vv_…` and nothing else: no session cookies, no Basic
  auth. A **read** token sees only the read tools; a **write** token also gets the editing tools, and the owner's
  role still applies (a viewer's write token can't edit).
- **No destructive tools.** Nothing deletes monitors, pages, incidents or history, and nothing touches users,
  settings, API tokens, notification channels or backups. To undo what an assistant did, use the web UI.
- **Secrets are masked.** Passwords, tokens, API keys, SNMP communities and credentials inside URLs come back as
  `••••••`, and header values are hidden (names stay). Content from monitor names, messages and incident texts is
  data the assistant reads: treat prompts hidden in a monitor name as untrusted, as with any tool output.
- **Rate limit** of 120 requests a minute per token (HTTP 429 with `Retry-After`).
- **Origin check.** A browser request whose `Origin` isn't this server's gets 403 (DNS-rebinding protection);
  assistants and the bridge send no `Origin`. Use HTTPS in front of the server when it is reachable from outside.
- Every call is made as the token's user inside the server, through the same REST code, so audit and validation are
  identical to the REST API. Revoke the token under **Settings → API tokens** to cut access immediately.

## Protocol details

`POST /mcp`, JSON-RPC 2.0, MCP versions 2025-06-18, 2025-03-26 and 2024-11-05. The server is stateless (no session id),
answers every request with plain JSON (batches are accepted), `202` for notifications, and `405` for `GET`/`DELETE`
because it has no server-initiated stream. Methods: `initialize`, `ping`, `tools/list`, `tools/call`,
`resources/list`, `resources/templates/list`, `resources/read`. Tool failures are returned as results with
`isError: true` and a message the assistant can act on.
