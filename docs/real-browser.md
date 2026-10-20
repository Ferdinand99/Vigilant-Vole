# Real-browser monitors

A **real-browser** monitor loads a page the way a visitor's browser does: HTML, scripts, styles, images and the
requests the page makes on its own. Use it for single-page apps that render in JavaScript, for checkouts and
dashboards that can return `200 OK` while showing an error, and to catch JavaScript errors after a deploy.

Each check:

1. opens a fresh, isolated browser context (no cookies or storage carried over from the last check) with one page
   at the chosen viewport — desktop 1280×800 or mobile 390×844;
2. sends the monitor's extra headers and basic-auth credentials, if any;
3. navigates to the URL and waits for the `load` event, or until the network is idle (no requests for half a
   second);
4. checks the main document's status against the accepted codes (`200-399` by default);
5. optionally looks for text in the visible page (`document.body.innerText`, case-sensitive) or for a CSS
   selector (`document.querySelector`), or for their absence with *Invert*;
6. optionally fails on an uncaught JavaScript exception;
7. on failure, captures a JPEG of the viewport, shown on the monitor's page as **Last failure** (signed-in users
   only — never on public status pages or badges);
8. closes the page and disposes of the context, also when the check runs into its timeout.

The response time is the time from the start of the document request to the `load` event. Messages look like
`HTTP 200 · loaded in 1.84 s`, `Selector #app not found`, `JavaScript error: TypeError: …` or
`Remote browser unreachable: …`.

Give these monitors a timeout that fits a full page load (15–30 s is typical) and an interval of a minute or more.

## Why a separate browser

Vigilant Vole is one static binary in a ~20 MB image. Chromium is a few hundred megabytes, needs a long list of
system libraries and gets security fixes every few weeks. So the browser runs as **its own container** and
Vigilant Vole drives it remotely over the Chrome DevTools Protocol (CDP), the same protocol Puppeteer and
Playwright use. You update or restart the browser independently, and if you don't use real-browser monitors you
don't run it at all.

## Run a browser

Either image works. Put it on the same Docker network as Vigilant Vole and **don't publish its port**.

### chromedp/headless-shell (small, no token)

```yaml
services:
  vigilant-vole:
    image: vigilant-vole:latest
    # … as in docker-compose.yml
  chrome:
    image: chromedp/headless-shell:156.0.8078.36
    restart: unless-stopped
    shm_size: 1gb
    # no ports: only Vigilant Vole talks to it
```

Address: `http://chrome:9222`.

Chrome only serves DevTools to requests whose `Host` is an IP address or `localhost`. Vigilant Vole handles that
for you: when Chrome refuses the host name it retries with `Host: localhost` and points the WebSocket address
Chrome reports back at `chrome:9222`.

### browserless (token, session limits, more tooling)

```yaml
services:
  browserless:
    image: ghcr.io/browserless/chromium:v2.57.1
    restart: unless-stopped
    environment:
      - TOKEN=change-me-to-a-long-random-string
      - CONCURRENT=5
      - TIMEOUT=60000
    # no ports
```

Address: `ws://browserless:3000?token=change-me-to-a-long-random-string`.

Pin the image version (both tags above exist) and update it like any other container. Then, in Vigilant Vole:
**Settings → Remote browsers → Add remote browser**, enter a name and the address, press **Test connection**
(it shows the browser's product and protocol version) and save.

## Addresses

| Form | Example | Notes |
|---|---|---|
| Chrome's browser endpoint | `ws://chrome:9222/devtools/browser/0b5f…` | The id changes when Chrome restarts, so prefer the next form for plain Chrome. |
| DevTools HTTP endpoint | `http://chrome:9222` | Looked up through `/json/version` on every check. `https://` works too. |
| Service with a token | `wss://browser.example.com?token=…` | browserless and similar services. |

A token (any query parameter named like `token`, `key`, `secret`, `password`, `auth`) and a password in the
address are **secrets**: shown as `••••••` on the settings page, in the API and in config exports. When you edit
the address and keep the mask, the stored secret is kept only while the scheme, host and port stay the same;
point it somewhere else and you have to enter the token again.

The first remote browser is the default. Each monitor can pick another one; a monitor whose browser is deleted
falls back to the default. Each remote browser runs at most 3 checks at the same time, and more wait their turn
(within their timeout).

## Security

**A DevTools endpoint gives full control of that browser.** Anyone who can reach it can make it open any page
from inside your network (including internal admin interfaces and cloud metadata addresses), read what it shows
and run scripts in it. So:

- never publish the browser's port to the internet or to your LAN — keep it on a private Docker network that only
  Vigilant Vole joins;
- with browserless, set a long random `TOKEN`;
- only admins can add or change remote browsers; editors choose one per monitor;
- extra headers and basic auth are sent with **every request the page makes**, including to other sites the page
  loads scripts or images from — use a dedicated monitoring account.

## Limitations

- Remote agents don't run real-browser monitors: the browsers are configured on the server, so these monitors are
  never sent to agents. The monitor form says so when *Check from remote agents too* is on.
- No scripted multi-step journeys (log in, click, fill a form) — it's one page load with an optional text or
  selector check.
- No proxy or client-certificate settings for the page; configure those on the browser itself if you need them.
- Uptime Kuma's built-in Chromium has no equivalent here; imported Kuma real-browser monitors use your remote
  browser (see [migrating-from-uptime-kuma.md](migrating-from-uptime-kuma.md)).

## API and config as code

- `GET/POST /api/v1/remote-browsers`, `GET/PUT/PATCH/DELETE /api/v1/remote-browsers/{id}`,
  `POST /api/v1/remote-browsers/{id}/test` (writes need an admin) — see [api.md](api.md).
- In a config document, remote browsers are a `remote_browsers` section keyed by name, and monitors refer to one
  with `remote_browser: <name>` — see [config-as-code.md](config-as-code.md).

```yaml
remote_browsers:
  - name: Chrome
    url: http://chrome:9222
monitors:
  - name: Shop checkout
    monitor_type: real_browser
    target: https://shop.example.com/checkout
    timeout_seconds: 30
    remote_browser: Chrome
    browser_selector: "#checkout-button"
    browser_wait: network_idle
    browser_viewport: mobile
    browser_js_errors: true
```
