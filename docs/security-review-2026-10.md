# Security review, October 2026

Date: 27 October 2026. Covers Vigilant Vole (the product) and vole-kit (the shared library in `kit/`, used by
Vigilant Vole, Keen Kestrel and Wise Wren), up to Vigilant Vole 0.14.0 and vole-kit 0.6.0.

## Method and limits

This was a read-only code review done by an AI reviewer, one pass for the kit and one for the product. It was not an
external audit and it was not done by a professional security firm. The reviewer read the code, traced each candidate
finding to the lines that cause it, tried to refute it, and rated it. Each fix was written with tests and merged after
review. Nothing was run against a live instance: there was no dynamic testing, fuzzing, or scanner run (no CodeQL or
Semgrep), and no proof-of-concept request was sent. Findings are therefore traced in code, not demonstrated; a few
depend on library behaviour that was recalled rather than checked (for example how reqwest strips headers on
redirects).

Treat this as one careful review, not as a guarantee.

Severity scale: High is unauthenticated or cross-site compromise of an account or host with realistic preconditions.
Medium needs a precondition (a role, a deployment mistake, a sibling site). Low is hardening or a narrow scenario.
Info needs no action.

## Scope

- **vole-kit**, reviewed at 0.5.0 (`main` at `8e85ba9`): sessions and cookies, CSRF, sign-in, throttling, passwords,
  TOTP and recovery codes, passkeys, API tokens, MCP, sealed secrets, notification providers, security headers,
  roles. How the three products use the kit was read to judge impact.
- **Vigilant Vole**, reviewed at `5ef03d4` (0.13.0 plus the first kit hardening): `src/`, `templates/`,
  `migrations/` and the deployment files. Kit findings are not repeated in the product table.

No finding was rated Critical. One was rated High, and only for Wise Wren as it shipped (VK-01).

## vole-kit findings

| ID | Severity | Title | Status |
|---|---|---|---|
| VK-01 | High for Wren, Medium for the kit | Cookie-authenticated writes to CSRF-exempt `/api/v1` relied on an opt-in guard | Fixed in #62 (vole-kit 0.5.1) |
| VK-02 | Medium | Notification providers: no address guard, redirects and proxies followed, response text reflected | Fixed in #62 for link-local, metadata, unspecified and multicast addresses, redirects, proxies and error text. Accepted for private and loopback addresses |
| VK-03 | Medium | Sign-in throttling can be turned into a lockout | Partly fixed in #62 (passkey sign-in has its own bucket; a warning when proxy headers arrive without `TRUST_PROXY`). Open: per-account throttle redesign |
| VK-04 | Medium | A deleted user's session cookie works for the next user with the same id | Fixed in #62 |
| VK-05 | Medium | First-run `/setup` open to the first visitor when no token is set | Fixed in #62 |
| VK-06 | Low-Medium | Stateless sessions: logout and some security changes do not revoke | Partly fixed in #62 (2FA and passkey changes sign other devices out). Open: server-side sessions |
| VK-07 | Low-Medium | HTTPS awareness hangs on `TRUST_PROXY`; thin cookie hardening | Partly fixed in #62 (`Secure` from the public URL, flash cookie, HSTS, startup warning). Open: `__Host-` cookie prefix |
| VK-08 | Low-Medium | SSE streams authorised once; lifetime and cap optional | Open in the kit (still optional). Vigilant Vole and Wise Wren bound their streams already; Keen Kestrel fixed its own in its PR #42 |
| VK-09 | Low-Medium | Re-authentication throttles are non-atomic | Fixed in #62 |
| VK-10 | Low | Persistence-grade actions need no re-authentication | Fixed in #65 (vole-kit 0.6.0) |
| VK-11 | Low | `session.key` as the single root of trust | Partly fixed in #62 (created 0600 with `create_new`, a damaged file stops the start). Open: row-bound AAD, key id and rotation |
| VK-12 | Low | TOTP secrets and pre-migration backups in plaintext | Fixed in #65 |
| VK-13 | Low | CSRF origin check ignores the scheme | Open |
| VK-14 | Low | Markdown and mention injection into notification messages | Partly fixed in #62 (Discord `allowed_mentions`). Open: Markdown escaping for the other providers |
| VK-15 | Low/Info | Miscellaneous | Partly fixed in #62 (segment-aware scope paths, the software authenticator is built for tests only). The rest is open or informational |

What the VK-02 fix does: HTTP notifications use a kit-owned client. Link-local (including cloud metadata
addresses), unspecified and multicast destinations are refused at connect time, so redirects and changed DNS
answers are covered; environment proxies are ignored; only same-host redirects are followed (at most 5); failure
text shows the status and at most 80 characters of the answer. The SMTP host follows the same rule.

## Vigilant Vole findings

| ID | Severity | Title | Status |
|---|---|---|---|
| VV-01 | Medium | An editor could run JavaScript on the app origin through a status page's analytics script URL | Fixed in #68 |
| VV-02 | Medium | Monitors are an SSRF read primitive for editors; response text reflected | Fixed in #68 for HTTP, keyword, JSON, OAuth token and FiveM monitors. Open: TCP and websocket monitors, and checks that go through a proxy |
| VV-03 | Low-Medium | Credentials embedded in a monitor URL or proxy URL shown to viewers | Fixed in #68 |
| VV-04 | Low | Redirects forwarded custom header secrets to other hosts | Fixed in #68 |
| VV-05 | Low-Medium | A compromised agent token could flood results and flip monitors | Fixed in #68 |
| VV-06 | Low | `run_on_agents` hands a monitor's credentials to every agent, and editors could switch it on | Fixed in #68 (admin only) |
| VV-07 | Low-Medium | Built-in analytics let an anonymous visitor grow the database without bound | Fixed in #68 (caps and shorter referrer retention). Analytics stays on by default |
| VV-08 | Low | Badges ignored the page's uptime and response-time switches | Fixed in #68 |
| VV-09 | Low | MCP: untrusted text, weakly annotated write tools | Fixed in #68 |
| VV-10 | Low | Subscription form: pending room could be kept full; "Full" answered like success | Fixed in #68 |
| VV-11 | Low | Backup snapshot and database written with default permissions | Fixed in #68 |
| VV-12 | Info | Smaller notes | Partly done in #68 (`Vary: Host` on the feed, the `:ro` Docker socket wording). Open: see below |

## Accepted and open items

- **Private and loopback notification destinations (VK-02).** Accepted. Self-hosted ntfy, Gotify and Home Assistant
  on a LAN are normal use, so only link-local, metadata, unspecified and multicast ranges are blocked. Vigilant Vole
  editors can still manage notification channels, so an editor can aim a webhook at a LAN address. The editor role
  is documented as trusted with the network.
- **Monitors reach the LAN (VV-02).** Accepted: monitoring internal services is the purpose of the product. Response
  text is no longer quoted in messages, and the boundary is written down in the README.
- **TCP and websocket monitors, and proxied checks (VV-02).** Open. These are not restricted against metadata
  addresses; a check through a proxy is only checked for literal addresses and redirects, because the proxy
  resolves the name.
- **Account throttle redesign (VK-03).** Open. A patient attacker who knows a username can keep that account in
  backoff for the password path. Passkeys and API tokens are not affected. A trusted-proxy list is also not built.
- **Server-side sessions (VK-06).** Open. Logout only removes the browser's cookie, and a stolen cookie lives up to 30
  days unless the version is bumped (password change, "sign out other sessions", 2FA or passkey changes).
- **`__Host-` cookies (VK-07).** Open. It renames the cookie and forces one re-login.
- **AAD and key rotation for sealed secrets (VK-11).** Open. Sealed values are not bound to their row, and there is
  no key id or rotation path. Exploiting the first needs write access to the database.
- **Scheme check in the CSRF origin check (VK-13).** Open. Residual risk is small because browsers keep `SameSite`
  cookies off cross-scheme posts.
- **VK-08, kit streams.** Open in the kit; each product bounds its own streams.
- **VK-14, Markdown in non-Discord providers; rest of VK-15.** Open, low.
- **VV-12 remainder.** Open: framable unlock and subscribe pages, the admin UI answering on every custom-domain
  hostname (a front proxy that allows only `/`, `/status/*` and `/static/*` is the suggested workaround), and size
  caps on the Kuma import.
- **Analytics default (VV-07).** Built-in page analytics remains on by default; the review suggested making it
  opt-in.

## Not covered

- Kit: the full provider set one by one (payload quirks), `settings`, `components`, `tz` and `i18n` internals, the
  cookie crate's exact MAC input, ciborium and lettre behaviour on hostile input, and dependency audits
  (`cargo audit` and `cargo deny` were not run).
- Product: the hand-written protocol clients and parsers (SQL Server, MongoDB, MySQL, Postgres, SNMPv3, Kafka,
  RabbitMQ, RADIUS, game servers, NTP, gRPC reflection, WebSocket, RDAP, Tailscale LocalAPI, the browser CDP client),
  the alert state machine and maintenance scheduling, the install scripts, Dockerfile hardening, CI and release
  pipelines, the marketing site and load-test tools.
- Locale files (content is escaped on output, translations were not checked for markup).
- Deployments: reverse-proxy configuration, whether `TRUST_PROXY` is set on a given instance, and data-directory
  permissions on a given host.
- Dynamic behaviour of any kind.

## Notes for operators

Release notes: Vigilant Vole [0.13.1](https://github.com/Ferdinand99/Vigilant-Vole/releases/tag/v0.13.1) (kit
hardening, vole-kit 0.5.1) and [0.14.0](https://github.com/Ferdinand99/Vigilant-Vole/releases/tag/v0.14.0)
(re-authentication, sealed TOTP, product hardening, vole-kit 0.6.0); vole-kit
[0.5.1](https://github.com/Ferdinand99/Vigilant-Vole/releases/tag/vole-kit-v0.5.1) and
[0.6.0](https://github.com/Ferdinand99/Vigilant-Vole/releases/tag/vole-kit-v0.6.0). See also
[upgrading](upgrading.md).

- **First run needs a setup token.** A fresh instance with no users logs a random token once at start
  (`/setup?token=...`) unless `VV_SETUP_TOKEN` is set. `VV_SETUP_OPEN=1` keeps the old open page.
- **Two-factor secrets are sealed in 0.14.0.** The upgrade takes a pre-migration copy first and an older release
  refuses the migrated database, so going back needs that copy. The copy is deleted automatically by a later start
  that finds everything sealed, or after 7 days. A replaced `session.key` makes two-factor secrets unreadable; those
  users can only sign in with a recovery code.
- **Password confirmation.** Creating a write or never-expiring API token, adding or promoting an admin, resetting
  another user's password or two-factor sign-in, and deleting a passkey ask for the current password. New tokens
  default to 90 days. Scripts that post these forms must send `current_password`.
- **Other devices are signed out** when a user turns 2FA on or off or adds or deletes a passkey; the current device
  stays signed in.
- **Cookies** are `Secure` when the public URL is https, and HSTS is sent for requests known to be https. Over plain
  http to a LAN address a browser will not keep a `Secure` cookie, so use the public URL. A damaged `session.key`
  now stops the start instead of being replaced.
- **Notifications.** Webhooks that relied on a redirect to another host now fail. Channels pointing at link-local,
  metadata or multicast addresses fail, and proxy variables from the environment are ignored for them.
- **Cookie-authenticated writes** to `/api/v1` from a foreign origin are refused; browser pages are unaffected and
  API tokens are not affected.
- **Monitors.** Messages no longer quote the first characters of a response: keyword failures read `Keyword "x" not
  found`, and JSON checks describe text by length. Redirects are capped at 5 whatever is stored. A monitor with a
  literal link-local or metadata address cannot be saved, and existing ones go down with "Refused: ...". A monitor
  with custom headers does not follow a redirect to another host or from https to http.
- **Editors** can no longer choose a status page's analytics script URL or turn on *Check from remote agents too*,
  and no longer see `user:pass@` in monitor URLs through the API (they still see it in the edit form). The
  third-party analytics script is never served to signed-in users.
- **Agents.** A result for a monitor is accepted no sooner than half its interval (at least 5 seconds), and uploads
  over 512 KB are refused with 413. Pending status-page subscriptions now expire after 24 hours instead of 3 days.
- **Files.** `data/tmp` and `data/backups` are 0700, the database and backups are 0600.
