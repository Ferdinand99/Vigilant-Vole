# Golden tests

`tests/golden/` freezes Vigilant Vole's externally visible behaviour in
snapshot files (`tests/golden/snapshots/*.snap`, [insta](https://insta.rs)).
They exist so that refactors which should change nothing, such as moving
auth, i18n, the database layer, settings, security headers, the docs
generator, MCP, notifications or the templates into a shared crate, provably
change nothing. A failing golden test means some output changed.

```sh
cargo test --test golden
```

## What is frozen

| Area | File | Snapshots |
| --- | --- | --- |
| Auth names and formats | `auth.rs` | cookie names, attributes and value encodings; `vv_`/`vva_` token shapes; how tokens are read; the CSRF policy (table and over HTTP); the login, 2FA and logout flow; API rejections; tokens from every release fixture still authenticate |
| Security headers | `security_headers.rs` | every response header (exact CSP, framing, referrer, sniffing, caching, compression) for app pages, public status pages, a custom domain, the API, static files, errors and MCP |
| Rendered HTML | `pages.rs` | setup, login, dashboard, general settings, users, tokens, API docs, public status page, 404, each in English and Arabic (right-to-left) |
| i18n | `i18n.rs` | the language registry, `Accept-Language` negotiation, the English `js_bundle`, the English key list and a per-language summary (key count, keys missing from English, hashes of keys and values) |
| Static files | `static_assets.rs` | every file under `static/` with content type, size and hash; the paths templates and the web manifest reference |
| Database | `schema.rs` | the schema after all migrations (`sqlite_master`, columns, indexes), the migration list, and for each release fixture: what the upgrade applied, the backup file name and that the result equals a fresh database |
| API docs and MCP | `docs_mcp.rs` | `/api/v1/openapi.json`, `/api-docs.md` (= `/llms.txt`), MCP `initialize`, `tools/list` (write and read tokens) and `resources/list` |
| Notifications | `notifications.rs` | for every provider, the requests `build` makes (method, URL, headers, body) for a fixed down and up event, from a minimal and a full form config; the email MIME message |
| Product strings | `product.rs` | user agent, product and default site names, data directory files, download file names, auth challenges, TOTP URL, and a webhook as the receiving server sees it |

## What is normalised

Snapshots must only change when behaviour does, so `tests/golden/norm.rs`
replaces what varies between runs or machines: the package version
(`<VERSION>`), CRLF line endings (a Windows checkout embeds templates,
static files and migrations with CRLF), signed cookie values and expiry
dates, ETags and `Content-Length`, dates and times derived from "now"
(`<DAY-n>` = n days ago in the page's own format, `<DATETIME>`, `<DATE>`,
`<TIME>`), backup and download time stamps, the order of `Set-Cookie`
headers (the cookie jar emits them in no fixed order), the time zone
picker's list (it is the machine's tz database), the MIME boundary and
`Date` of emails, the DingTalk/Feishu/Nextcloud Talk signatures (checked
against the provider's scheme first, then replaced) and the Matrix
transaction counter. Generated tokens are only recorded by shape
(`vv_<43 base62>`); the seeded tokens are fixed strings. The scheduler is
stopped, so no checks run while pages render. `Cache-Control` of static
files differs between debug and release builds and is checked per profile.

## When a golden test fails

1. Read the diff. If the change was **not** intended, fix the code.
2. If it was intended (a new feature, a wording change, a new migration),
   accept it deliberately and commit the updated `.snap` files with the
   change, so the review shows exactly what changed for users:

   ```sh
   cargo install cargo-insta          # once
   cargo insta test --test golden --review
   # or, without cargo-insta: write the new snapshots, then read the git diff
   INSTA_UPDATE=always cargo test --test golden
   git diff tests/golden/snapshots
   ```

New `.snap.new` files (written on a failed run outside CI) must never be
committed. In CI (`CI=true`) insta never writes snapshots; a mismatch fails.

Never accept a snapshot that contains something run-specific (a random
value, a port, a time stamp): add a normalisation in `norm.rs` instead.
