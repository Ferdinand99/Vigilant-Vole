# Live status boards

A live status board keeps **one message** (or a few, when the platform's limits require it) in a Discord or Fluxer
channel up to date. The message is posted through the channel's webhook and then **edited in place** whenever something
changes, so the channel does not fill up with status posts.

A board shows either

- a **status page**: its sections, order, names and visibility, and the overall status banner, or
- a **hand-picked list** of monitors and groups, in the order you give them.

Boards live under **Settings → Notifications → Live status boards** (editors and admins). A status page's editor has a
**Live status board** button that starts a board for that page.

## Set one up

1. Create a webhook in the channel: in Discord, *Channel settings → Integrations → Webhooks → New webhook → Copy
   webhook URL*. In Fluxer, the channel's webhook settings work the same way.
2. **Settings → Notifications → Live status boards → New board.** Pick the platform, paste the webhook address, choose
   what to show, and use **Test webhook** to post (and delete again) a short test message.
3. Save. The board posts its message within a few seconds. After that it is edited when something changes (within about
   10 seconds) and at least every *Refresh every* seconds (300 by default, 60 at the shortest).

The webhook address is a secret. It is stored encrypted (like the secrets of notification channels), shown nowhere after
you have saved it, and never appears in logs, the API, exports or MCP answers. When you edit a board, leave the field
empty to keep the saved address; a new address moves the board to that channel and removes its messages from the old one
(as far as that works).

## Options

| Option | What it does |
|---|---|
| Source | A status page (published, no password), or monitors and groups. A group shows its monitors under its name |
| Uptime, last 24 hours / 30 days | Adds the uptime figures to each line. A board that follows a status page shows them only when the page itself shows uptime |
| Response time | Adds the last response time of monitors that are up. For a status page, only when the page shows response times |
| Incidents and maintenance | Adds the open incidents and the maintenance in progress. A board of picked monitors shows the maintenance windows that cover them (incidents belong to status pages) |
| Title, description, color | The title defaults to the status page's title or the board's name. The color follows the status unless you pick one |
| Refresh every | 60 seconds to 24 hours |
| Language | The language of the message (one of the 26 interface languages) |
| Post a separate message when something goes down | Off by default. When a monitor on the board goes down, a new message is posted (it does not touch the board message). Optionally mentions one role, given by its numeric ID; nobody else is ever mentioned. At most 5 such messages per refresh; what is already down when the board is created is not announced |

## How it behaves

- **Edit in place.** The first refresh posts the message and remembers its ID. Later refreshes edit that message. A
  refresh caused by a change edits only what changed; the refresh on the interval also finds out when somebody deleted
  the message, and posts a new one.
- **Several messages when needed.** The platforms allow 10 embeds and 6000 characters per message, 4096 characters per
  embed description and 256 per title. A board that does not fit uses more messages, laid out the same way every time:
  header, incidents and maintenance, then one section after the other (a long section continues in the next embed). If a
  message has to be posted anew, the ones after it are posted anew too, so the channel keeps the board's order. A board
  uses at most 10 messages; what does not fit is cut with a note. Messages a smaller board no longer needs are deleted.
- **Rate limits.** The `429` answers and the `X-RateLimit-*` headers of the platform are honoured: a short wait is
  waited out, a long one makes the board try again later. A rate limit is not a failure.
- **Failures.** A board that fails five times in a row pauses, and so does a board whose webhook is refused for good
  (deleted, no access) or whose status page is a draft, protected by a password or deleted. The board list shows the
  error and a **Retry** action. Saving a board starts it again.
- **Deleting a board** deletes its messages in the channel, as far as that works.

## What a board shows

A board never shows more than its status page or its picked monitors:

- A status page board shows only what the page shows: its sections, names and order, and the figures the page itself
  shows. Monitors the page hides are not on the board. A draft, or a page behind a password, is not shown at all.
- A board of picked monitors shows only those monitors (the monitors of a picked group), by name.
- Neither shows a target, URL, host or check message, and nothing in the message can ping anybody (the one exception is
  the role you chose for the "went down" message). Names and titles are escaped so that they cannot format the
  message.

## Discord and Fluxer

**Discord.** Only `https://discord.com`, `discordapp.com`, `ptb.discord.com` and `canary.discord.com` webhook addresses
(`/api/webhooks/{id}/{token}`, optionally `/api/v10/…`) are accepted.

**Fluxer** can be hosted by anyone, so its webhook addresses come from any instance:
`https://<instance>/api/webhooks/{id}/{token}` (or `https://<api host>/webhooks/{id}/{token}`). Fluxer supports the
same calls as Discord (`?wait=true`, editing and deleting a webhook message) and the same embed limits, so a Fluxer
board is edited in place exactly like a Discord one. To stop the board from being pointed at something else, a Fluxer
address is used only when

- the host is a name (not an IP address, not `localhost`),
- the host answers `/.well-known/fluxer` with a Fluxer discovery document that lists the address's API base
  (`https://<instance>/api`) among its endpoints (checked when you save and again at least every hour),
- every address the name resolves to is a public one: loopback, private, link-local and metadata addresses are
  refused, which is checked again for every request, and the connection goes to exactly the addresses that were checked,
- no proxy is used and redirects are never followed.

## API, MCP, configuration

- **REST API:** `/api/v1/live-boards` (list, create, change, delete) and `…/{id}/test`, `…/{id}/refresh`,
  `…/{id}/retry`; see the [API reference](api.md). The webhook address can be sent on create and update and is never
  returned (`webhook_set` says whether there is one).
- **MCP:** `list_live_boards` (read) and `refresh_live_board` (write); see [mcp.md](mcp.md).
- **Config as code:** the `live_boards` section; see [config-as-code.md](config-as-code.md#live_boards). Exports never
  contain a webhook address.
- **Backup and restore.** A backup carries the boards (settings, picked monitors, the ids of the messages they keep), but the
  webhook addresses stay sealed with this instance's key (`session.key`), unlike the secrets of notification channels,
  which a download opens so that they survive a move. Restoring on the same instance changes nothing. Restoring on another
  instance, or with another `session.key`, pauses every board with the error "enter it again": edit each board and paste
  its webhook address once. The boards that were running keep the messages they have in their channels.
- **Applying the same address again** (an API update that repeats the webhook, or a config file that carries it and is merged on
  every start) changes nothing: the board keeps its messages and no message is posted again.
