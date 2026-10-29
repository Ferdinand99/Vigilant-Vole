# vole-kit

`vole-kit` is the shared product kit of Vigilant Vole and its sibling
products (first of them Keen Kestrel). It holds:

* authentication, with the pages every product needs: setup, sign-in with two
  factors or a passkey, users, API tokens (with the product's own token scopes), Security,
  the API reference and, for products that want it, General settings;
* the i18n engine and the core translations;
* the database plumbing, the migration runner and the core tables;
* HTTP conventions, the API reference generator and the MCP protocol layer;
* the 41 notification providers;
* the design system: static files, layouts and component macros.

It lives in this repository as a Cargo workspace member, `kit/`. Vigilant
Vole depends on it by path; other products depend on it by git tag
(`vole-kit-vX.Y.Z`).

The kit was carved out without changing anything a user, an API client or a
database can see. `tests/golden/` (see [golden-tests.md](golden-tests.md))
proves it at every step. The only exception is the list of applied
migrations, which grew by two rows (see "Migrations").

## Layout

```
Cargo.toml              # [workspace] ".", "kit", "kit/kit-build", "examples/kit-demo"; package vigilant-vole
build.rs                # vole_kit_build::templates()
kit/
├── Cargo.toml          # package vole-kit (links = "vole-kit")
├── build.rs            # exports the template folder to products
├── CHANGELOG.md        # vole-kit's own (release-please)
├── kit-build/          # package vole-kit-build: the build-script helper
├── locales/            # the kit's strings, one file per language
├── migrations/         # the core set: 0001_core.sql (users, api_tokens, settings),
│                       #   0002_token_scopes.sql (api_token_scopes),
│                       #   0003_passkeys.sql (passkeys, passkey_challenges),
│                       #   0004_sealed_secrets.sql, 0005_sealed_totp.sql (records only, no table),
│                       #   0006_push.sql (push_devices, push_events)
├── templates/          # layouts, macros, the shared pages
└── src/
examples/kit-demo/      # a second product on the kit (tests only, not released)
src/                    # Vigilant Vole
static/                 # Vigilant Vole's files and the kit's design system (see "Static files")
locales/                # Vigilant Vole's own strings
migrations/             # Vigilant Vole's migrations
templates/              # Vigilant Vole's templates (+ templates/kit/, a build-time copy)
```

Cargo commands without `-p` cover every member (`default-members`). CI runs
`cargo clippy --workspace --all-targets` and `cargo test --workspace --lib`,
then the golden, `kit_boundary` and kit-demo tests. The Docker image copies
`kit/` and `examples/` (Cargo has to load the whole workspace) and builds only
`-p vigilant-vole`.

`vole-kit`'s dependencies are a subset of Vigilant Vole's, with versions and
features from `[workspace.dependencies]`:

* web: axum, axum-extra, askama, tokio, reqwest
* database: rusqlite, deadpool-sqlite, rust-embed
* data and errors: serde, serde_json, time, jiff, url, tracing, thiserror
* crypto: rand, hex, sha2, base64, argon2, totp-rs, qrcode, regex, aws-lc-rs
  and ciborium (passkeys; aws-lc-rs is also rustls' crypto)
* mail and TLS: lettre, rustls, rustls-native-certs, webpki-root-certs, futures-util

Test only: tower, tokio-rustls, rcgen. `vole-kit-build` has no dependencies.

## The rule

> The kit never depends on the product.

The compiler enforces it: `vole-kit` can't name `vigilant_vole` without a
dependency on it. `tests/kit_boundary.rs` keeps that door shut:

* there is no `vigilant-vole` in `kit/Cargo.toml`;
* the kit's sources contain no `vigilant_vole` and no leftover `crate::kit`;
* kit code outside tests uses no `CARGO_PKG_*`. That is the kit's own package
  data; product-facing versions and names come from `config::get()`.

Further kit tests check two things:

* every string the kit's templates and code use is in the kit's own catalog;
* no notification field names a product.

kit-demo checks that the kit's pages show its own name and never Vigilant
Vole's. The kit forbids `unsafe` code (`#![forbid(unsafe_code)]`).

Vigilant Vole re-exports the crate as `crate::kit` (`pub use vole_kit as
kit`). It also keeps its older paths (`crate::auth`, `crate::i18n`,
`crate::db`, `crate::notification`, `crate::api::{ApiError, PageCtx, …}`,
`crate::tls_roots`) as re-exports or thin wrappers.

## Module map

| Module | What it is |
| --- | --- |
| `config` | `KitConfig`, `install`, `get` |
| `crypto` | sealing small secrets at rest under the instance key: `seal`, `open`, `is_sealed`, `Sealer`, `OpenError`, `PREFIX` (see "Sealed secrets") |
| `state` | `Core` (pool, session key, HTTP client, config, sign-in throttles), `Core::new` and `with_conn` |
| `i18n` | languages, catalogs (core + product), plurals, number/date formats, `Tr` (template helpers, `kit_ui()`, `product_name()`), `Accept-Language`, the request language middleware |
| `db` | pools (`build_pool`, `build_pool_with` + `PoolOptions`), `interact`, `settings` (key/value + core keys), `users`, `role`, `api_tokens` (with product scopes), `passkeys` (credentials and challenges), `migrate` (runner, `MigrationSet`, `CORE`, `CORE_TABLES`, `TOKEN_SCOPES`, `PASSKEYS`, `SEALED_SECRETS`, `SEALED_TOTP`, `PUSH`, `INVITES`, `SIGN_IN_CODES`, `migrate_with` + `MigrateOptions`) |
| `auth` | sessions, CSRF policy, rate limiting (`SignInLimits`), TOTP, API tokens, password rules, `CurrentUser` (`Scopes`, `has_scope`, `may_act_as`), `ExtraScope`; `webauthn`: passkey ceremonies (`RelyingParty`, `verify_registration`, `verify_assertion`, the test authenticator `soft`); `guard`: `require_auth`, `csrf_guard`, token and session resolution; `reauth`: the fresh-confirmation grant of external credentials; `pages`: the shared pages and their routers |
| `settings` | `SiteSettings` (site name, public URL, time zone, language), `set_site_language`, `normalize_base_url`, and the General page's description (`GeneralSettings`, `SettingsSection`, `SettingField`, `FieldKind`) |
| `http` | `ApiError`, security headers/CSP and `HeaderPolicy`, `HtmlTemplate`, static files (`assets`), `healthz`, `toast_trigger`; `rest`: the JSON API conventions (error envelope, extractors, cursor paging, ETags, `session_write_allowed`/`session_write_guard`); `ratelimit`: token buckets, `429` with `Retry-After`; `live`: live updates over server-sent events |
| `page` | `PageCtx`, `UserView`, flash messages, `Ui` (`NavItem`, `SettingsTab`, `TabAccess`) |
| `push` | push from the servers through the Hedgerow push relay: `install`, the device register (`devices`), the stored push texts (`events`), the relay client (`relay`) and the three routes (`api::router`, `api::ROUTE_DOCS`); see "Push (0.7)" |
| `docs` | `RouteDoc`, `DocsSpec`, the OpenAPI 3.1 document, the Markdown reference; `page`: `/api-docs` and friends |
| `mcp` | MCP protocol: origin check, Bearer auth, rate limit, JSON-RPC, the `Handler` trait |
| `notify` | `Alert` (with `Alert::test`), `AlertKind`, `Level`, the 41 providers, schemas, HTTP/SMTP transports, the message layout, `send_in`/`send_config_in` (a channel's language); `form`: the provider form's view-model; `sealed`: channel secrets sealed at rest; `webhook::Wording` |
| `components` | view-models of the design system's data components: `BarRow`, `LineChart`, `CodeLine` |
| `tls_roots` | system CA store with the bundled fallback |
| `tz` | time zones for pickers and validation |

## Configuration

A product describes itself once in a static `KitConfig` and installs it when
it starts, before any kit code runs:

```rust
static KIT: KitConfig = KitConfig { product: "Keen Kestrel", slug: "keen-kestrel", /* … */ };

fn main() {
    vole_kit::config::install(&KIT);
    // …
}
```

`install` is backed by a `OnceLock`. Installing the same configuration again
does nothing; installing a different one panics.

The first installation also checks the translations: every catalog must
parse, no key may be in both the kit's and the product's file, and the
product's English file must define the texts the kit's pages take from it
(see "Texts a product must supply"). A problem panics, so a broken build
doesn't start.

`get()` without an installed configuration panics with a message that says
what to do. The kit's own tests fall back to their test configuration
(`kit/src/testing.rs`: Vigilant Vole's names, translations and migrations).

Vigilant Vole installs the configuration in `main` and in `server::prepare`.
Test binaries have no `main`, so they install it from a constructor that runs
before the tests (`ctor`, a dev-dependency):

* its unit tests in `src/lib.rs`;
* every integration test, through `tests/common/mod.rs` and the three tests
  that don't use it.

No test can forget it.

| Field | Used for | Vigilant Vole |
| --- | --- | --- |
| `product` | texts, page titles, TOTP issuer fallback, default site name, MCP server title, auth realm, provider names, `{product}` in provider forms | `Vigilant Vole` |
| `slug` | MCP server name, tags/sources sent to providers, timing-equaliser hash, API reference download names | `vigilant-vole` |
| `short_name` | `{short_name}` in provider forms (SMS sender names) | `Vole` |
| `session_cookie`, `pending_2fa_cookie`, `flash_cookie` | cookie names | `vv_session`, `vv_2fa`, `vv_flash` |
| `token_prefix`, `other_token_prefixes` | API token format, MCP messages, docs checks, `curl` examples | `vv_`, `["vva_"]` |
| `version` | `info.version` in the OpenAPI document, MCP `serverInfo.version`, the signed-in pages' footer | `CARGO_PKG_VERSION` of vigilant-vole |
| `releases_url` | where the footer's version links: the release notes, with `{version}` filled in; `None` shows the version as plain text | `Some("https://github.com/Ferdinand99/Vigilant-Vole/releases/tag/v{version}")` |
| `user_agent` | outbound `User-Agent` (the product's clients) | `VigilantVole/<version>` |
| `backup_prefix` | name of the copy taken before a migration | `vigilant-vole-before-` |
| `env_prefix` | `<PREFIX>_TRUST_PROXY`, `<PREFIX>_SETUP_TOKEN`, `<PREFIX>_SETUP_OPEN`, `$<PREFIX>_URL` in examples, id prefixes sent to providers (`vv-monitor-7`) | `VV` |
| `csrf_exempt_prefixes`, `csrf_exempt_path` | token endpoints and the one-click unsubscribe link. Prefixes match whole segments; a `*` segment matches any one segment (see "CSRF exemptions") | `/api/push`, `/api/agent`, `/api/v1`, `/api/badge`, `/metrics`, `/mcp`; the unsubscribe path |
| `extra_common_passwords` | refused passwords besides the kit's list | `uptimekuma`, `vigilantvole` |
| `migrations` | `MigrationSet` | `crate::db::MIGRATIONS` |
| `embeddable_prefixes` | paths that may be framed | `/status/`, `/api/status-page/` |
| `static_file` | looks up one of the product's static files | `crate::api::static_file` |
| `machine_path_prefixes`, `machine_paths`, `browser_language_prefixes` | which paths stay English, which prefer `Accept-Language` | `/static/`, `/api/`; `/metrics`, `/healthz`; `/status/` |
| `docs` | `DocsSpec` (below) | `crate::api::v1::DOCS` |
| `locales` | `(code, text)` of the product's own strings per language, including the texts the kit's pages take from it (see "Translations") | `locales/*.toml` |
| `ui` | `Ui` (below) | `src/product.rs` |
| `public_url_setting` | where an admin sets the public URL, named by the MCP endpoint's refusal of a foreign browser origin; `None` says "the instance's public URL" | `Some("Settings → General")` |
| `webhook` | how the webhook provider names what alerts are about: the default body's fields, the body template's placeholders, the URL field's example (`notify::webhook::Wording`; `Wording::NEUTRAL` says `subject` and `duration`) | `subject: "monitor"`, `duration: "downtime"`, `url_example: "https://example.com/hooks/vigilant…"` |
| `token_scopes` | the product's own API token scopes (see "API token scopes") | `&[]` |
| `general` | the kit's General page with the product's fields (see "General settings"); `None` for none or the product's own | `None` (its own page) |
| `features` | which optional parts the product has (`KitFeatures`): `api_tokens` and `api_docs` (both on by default; off, their routes are 404, the menus do not link them and `require_auth` answers 401 to any `Authorization` token; `docs` is still required), `credentials` (`CredentialMode::Plain`, or `External` for a product that signs people in on its own pages: see "External credentials"), `invites` (invitation links: see "Invitation links"), `sign_in_codes` (one-time sign-in codes and the instance's mail settings: see "One-time sign-in codes"). `&KitFeatures::DEFAULT` is the kit as it was | `&KitFeatures::DEFAULT` |

`DocsSpec` holds the routes, the OpenAPI texts, the error example and the
`curl` placeholders. It also holds the product's Markdown sections and, for
the shared pages:

* `page_subtitle`, a translation key in the product's catalog;
* `openapi_path`, `api_base` and `example_path` (Vigilant Vole:
  `/api/v1/openapi.json`, `/api/v1`, `/api/v1/monitors`);
* `metrics_path` (`Some("/metrics")`; `None` drops the Prometheus hint on the
  Tokens page).

The parts only some products have are optional:

* `credentials`: machine credentials besides API tokens and sessions, each a
  `Credential` named by the `access` of its routes, with the Markdown's words
  for it and, if it is sent as a bearer token, a `BearerScheme` (OpenAPI
  scheme name and description, and the `$<PREFIX>_…` variable of the `curl`
  examples). The OpenAPI document declares `bearerAuth`, `sessionCookie` and
  only the schemes listed here. Vigilant Vole lists `push token` (in the URL,
  no scheme) and `agent token` (`agentToken`, `$VV_AGENT_TOKEN`); a product
  without either leaves it empty (`&[]`).
* `config_document`: `Some(ConfigDocument { path, file, body })` for an
  endpoint that takes and returns a whole configuration document as YAML
  (`RouteDoc::is_yaml` is true for that path only). `file` is what the `curl`
  examples save and send (`POST` examples add `?dry_run=1`), `body` how the
  Markdown describes the request. Vigilant Vole: `/api/v1/config`,
  `vigilant.yml`. `None` for none.
* `error_codes`: the product's rows of the error code table. The kit has the
  generic ones (`docs::GENERIC_ERROR_CODES`: `bad_request`, `unauthorized`,
  `forbidden`, `not_found`, `conflict`, `unsupported_media_type`,
  `validation_failed`, `internal`). A product row with one of those codes
  replaces it in place; any other goes after the last row with the same or a
  lower HTTP status, and `internal` stays last (`docs::error_codes()`).
  `on_page` puts a code in the short list on `/api-docs` too. Vigilant Vole
  adds `too_large`, `invalid_document` and `delivery_failed` and words
  `conflict` its own way, so its reference is unchanged. `&[]` for none.
* `fallback_host`: the host the reference names when a request has no `Host`
  header and no public URL is set (Vigilant Vole: `Some("localhost:3001")`;
  `None` says `localhost`).
* `param_types`: JSON Schema types of path parameters by name, for those
  that aren't integers (`&[("event_id", "string"), ("org", "string")]`).
  `kind`, `token` and `slug` are strings anyway; a route can declare its
  own with `RouteDoc::params`. Vigilant Vole: `&[]`.
* `strict_access`: `true` makes `install` refuse to start when a route's
  `access` is neither a role, `public`, `metrics token` nor one of
  `credentials` (`docs::undeclared_access` lists them). With `false` such a
  word is documented as "public (no token)", the old behaviour. Vigilant
  Vole keeps `false`: its `POST /mcp` says `api token`, and its published
  reference shows that as public. New products set `true`.

A `Credential` is sent as a bearer token (`bearer`), in a header of its own
(`header: Some(HeaderScheme { name, header, description, example })`: an
`apiKey` scheme in the OpenAPI document, `-H "<header>: <example>"` in the
`curl` examples), or in the URL (neither). A route whose body isn't JSON
says so with `RouteDoc::body_type("application/x-sentry-envelope")`: the
OpenAPI document gives the example as a string of that media type, the
Markdown names the type, and `curl` sends the example as it is
(`--data-binary`, newlines included).

`Ui` holds what the layouts show:

* `logo`, `favicon`, `apple_touch_icon` and `manifest` (empty for none);
* `nav`, the main navigation (top bar and phone tab bar: id, link, label and
  short label keys, icon);
* `settings_tabs`, in order. The kit's own tabs are `SettingsTab::GENERAL`
  (with `KitConfig::general`), `USERS`, `SECURITY` and `TOKENS`. Each tab has
  an access level (`Everyone`, `Editors`, `Admins`);
* `home_url`, the link in the public pages' "Powered by" footer.

`Core` is the kit's view of the product's state. A product implements
`FromRef<AppState> for Core` (and for the cookie `Key`). Kit handlers and
middleware take `State<Core>`, and the product's routers stay
`Router<AppState>`. A product can also use `Core` itself as its state, as
kit-demo does.

Build a `Core` with `Core::new(db, session_key, http, cfg)` once per running
instance and clone it. It holds the instance's sign-in throttles
(`limits: SignInLimits`, see "Sign-in throttling"), which clones share. A
product that builds a `Core` on demand from its own state (Vigilant Vole's
`FromRef<AppState>`) keeps one `SignInLimits` in that state
(`AppState::sign_in_limits`) and clones it in; a fresh one per request would
throttle nothing.

## Templates

askama compiles templates into the crate that uses them. A product whose
page extends the kit's layout therefore compiles the layout itself, so the
kit's template files must be inside the product's package at build time. The
mechanism works the same for a path and a git dependency, with no manual
steps:

1. vole-kit declares `links = "vole-kit"`. Its `build.rs` exports
   `cargo::metadata=templates=<kit/templates>`, which the build script of
   every package depending on vole-kit sees as `DEP_VOLE_KIT_TEMPLATES`.
2. The product's `build.rs` calls `vole_kit_build::templates()` (the
   `vole-kit-build` build-dependency). It copies that folder into the
   product's `templates/kit/`, which carries its own `.gitignore` (`*`).
   * Files are written only when their content changed, with the source's
     modification time, and copies the kit no longer has are deleted.
   * It tells Cargo to run again when the kit's folder or the copy changes.
     Editing a kit template rebuilds the product once; deleting the copy
     brings it back; a no-op build stays a no-op.
3. The product's templates say `{% extends "kit/layout.html" %}` and
   `{% import "kit/macros.html" as m %}`. Inside the kit's templates, names
   are relative (`"layout.html"`, `"partials/user_row.html"`): askama looks
   next to the including file first, so they resolve both in the kit crate
   and in the copy.

Edit the kit's templates in `kit/templates/`, never in `templates/kit/`.

What the kit shares:

* `layout.html`, `auth_layout.html`, `settings_layout.html` and
  `public_layout.html`;
* `macros.html`, the design system's components (icon, page and section
  heads, cards, fields, buttons, callouts, copy field, tag, stat, skeletons,
  time, uptime, ms, live indicator);
* the pages: `setup.html`, `login.html`, `login_2fa.html`,
  `settings_general.html`, `settings_users.html`, `settings_tokens.html`,
  `settings_security.html`, `api_docs.html` and their partials;
* the provider form's partials, `partials/notification_picker.html`,
  `notification_provider_grid.html`, `notification_name.html` and
  `notification_fields.html` (see "Notification channels").

A kit partial a product renders on its own (an htmx fragment) imports the
macros as `"../macros.html"` and includes its neighbours by their bare file
name: both resolve next to the file, in the kit and in a product's
`templates/kit/` copy.

Vigilant Vole keeps its monitoring macros (status pill and dot, heartbeat
bar, incident timeline) in `templates/status_macros.html`, imported as `st`.

`layout.html` ends signed-in pages with a quiet footer: the product's name
and version ("Vigilant Vole 0.10.0"), the version linking to
`KitConfig::release_notes_url()` when the product sets `releases_url`. It
has no words to translate. It is left out when the page has no user, and
`auth_layout.html` (sign-in, setup) and `public_layout.html` (status pages)
have none: the version is for signed-in users only, since it tells an
attacker what to aim at.

The layouts get the product's name, logo, icons, navigation and settings tabs
from `KitConfig` (`product_name()` and `kit_ui()`, helpers on `Tr` that every
template can call). Three kinds of names stay as they are, because they are
the contract between the layout and the design system's own scripts:

* the element ids `vv-i18n` and `vv-progress`;
* the theme radio and `localStorage` key `vv-theme`;
* the `vv:` events (`vv:toast`, `vv:sheet-open`, …).

`app.js` and `theme.js` look for exactly these names. Renaming them means
changing those files together with the layouts (see "Not done").

## Security hardening (0.6)

The October 2026 review (findings VK-01 to VK-15) changed these defaults.
All are additive for a product's code; the behaviour changes are listed.

* **Session writes to exempt paths** (VK-01). `require_auth` refuses a
  request that authenticated with the **session cookie**, uses an unsafe
  method (anything but GET, HEAD, OPTIONS) and goes to a CSRF-exempt path
  (`csrf_exempt_prefixes`, `csrf_exempt_path`), unless it proves it is ours
  as `http::rest::session_write_allowed` says: PUT, PATCH, DELETE or a
  content type a cross-site form can't send (they need a preflight nobody
  approves), or `HX-Request`, or an `Origin`/`Referer` of this site
  (`Host`, the trusted `X-Forwarded-Host`, or the public URL). Tokens are
  unaffected, and a cookie sent along with an `Authorization` header is
  ignored. The refusal is `403` (the v1 error envelope under `/api/`).
  `rest::session_write_guard` stays and is redundant now.
* **First-run setup token** (VK-05). With no `<PREFIX>_SETUP_TOKEN` and no
  user yet, the kit makes up a random token when it starts (`migrate()`
  does it), logs it once at WARN with the whole `/setup?token=…` path and
  requires it. The locked page has a field for it, so it can be typed
  instead of put in the query; the form carries it in a hidden field.
  Wrong tokens count against the client address in `SignInLimits::ip`
  (key `setup:<ip>`, the login backoff). `<PREFIX>_SETUP_OPEN=1` keeps the
  old open page, for scripted installs. Tests: the `test-support` feature
  opens setup by default, and `auth::session::set_setup_open_for_tests`
  switches it per thread.
* **Random session version** (VK-04). New users (`insert_user_full`,
  `insert_first_admin`) start with a random `session_version` from 1 to
  2^32 instead of 1. SQLite reuses the id of a deleted user, and a cookie
  carries id and version only: with every user at 1, the deleted user's
  cookie would sign in as the next one. Existing users keep their values.
* **Other devices are signed out on security changes** (VK-06, part).
  Turning two-factor on or off, adding a passkey and deleting a passkey
  bump `session_version` and re-issue the cookie of the device that did it
  (`pages::security::sign_out_others_keep_this`). The user's other
  browsers have to sign in again. Server-side sessions with a list and
  logout everywhere are not done.
* **Secure cookies and HSTS** (VK-07, part). The session and pending-2FA
  cookies are `Secure` when the request is HTTPS (trusted proxy) **or**
  the configured public URL is `https://`; the flash cookie follows the
  public URL. The kit reads the public URL every 30 seconds at most (in
  `csrf_guard`) and whenever `Core::base_url` is called, and logs one WARN
  when it is https while `<PREFIX>_TRUST_PROXY` is off. `security_headers`
  adds `Strict-Transport-Security: max-age=31536000` to requests known to
  be HTTPS (the proxy header, trusted). No `__Host-` rename.
* **`session.key`** (VK-11, part). `auth::try_load_or_create_key` creates
  the file with `create_new` (and mode 0600 from the start on Unix) and
  writes it once. A file that exists but is shorter than 64 bytes or can't
  be read stops the start with a message (`load_or_create_key` panics with
  it) instead of silently making a new key, which would sign everyone out
  and make every sealed channel secret unreadable.
* **Notification destinations** (VK-02). Every HTTP notification goes
  through the kit's own client (the `http` argument of `notify::send*` is
  kept but no longer used): connections to link-local addresses
  (169.254.0.0/16, fe80::/10), the cloud metadata addresses (also
  `fd00:ec2::254`, `100.100.100.200`, `168.63.129.16`), unspecified and
  multicast addresses are refused when they are made (`net::GuardedResolver`),
  so a redirect or a DNS answer that changes can't reach them, and an IP
  written in the URL is refused before connecting. The same rule is
  applied to the SMTP host, which is also connected by the address that was
  checked. Private and loopback addresses stay allowed (a homelab's ntfy
  or Gotify). Proxies from the environment are ignored. Redirects are
  followed only within the same host, five at most; a redirect to another
  host is an error. A non-2xx answer shows its status and at most the first
  80 characters of the body. `net::is_public` (the stricter rule for
  strangers' webhooks) moved here from Vigilant Vole's `subscribers`.
* **Re-authentication** (VK-10). The pages ask the signed-in user for the
  current password again (`sec.reauth_needed`; the same throttled check as
  for adding a passkey or changing the password: counted before it is
  verified, key `reauth:<id>`, cleared by a right one) before:
  * creating an API token with **write** access or **no expiry**
    (`current_password` on the new token sheet; a read-only token that
    expires needs none; product scopes follow the same rule);
  * adding a user with the **admin** role, or making a user an admin
    (`current_password` on the add and edit sheets);
  * an admin's **password reset** or **two-factor reset** of a user
    (`current_password`; the 2FA reset is a sheet now, not a confirm
    dialog);
  * **deleting a passkey** (`current_password`; the delete button opens a
    sheet). Renaming stays as it was.

  New tokens default to 90 days (the sheet's selected option; "never" is
  still offered, behind the password). Existing tokens are untouched.
  There is no passkey or TOTP alternative: every account has a password,
  and the one confirmation mechanism the kit had was the password.
  Changing your own password and an admin's password reset offer **"Also
  revoke API tokens"** (`revoke_tokens`, a checkbox that is on in the
  forms; a post that doesn't send it keeps the tokens). It deletes all
  tokens of that user (`db::api_tokens::delete_all_for_user`), including
  a product's scoped ones, such as a CI token.
* **TOTP secrets sealed** (VK-12). `users.totp_secret` (the secret of a
  finished enrolment and the pending one) holds `sealed:v1:…` under the
  purpose `vole-kit/totp` instead of base32. See "Sealed secrets". The
  pre-migration copies that still hold plaintext secrets are deleted by
  a later start, or after 7 days (see "Pre-migration copies").
* **Discord** (VK-14, part) sends `allowed_mentions: {"parse": []}`, so a
  name like `@everyone` pings nobody.
* **Small ones** (VK-15, part). `CurrentUser::scope_allows_path` matches
  whole path segments like the CSRF exemptions. The software passkey
  authenticator `auth::webauthn::soft` is only built for the kit's tests
  and with the `test-support` feature (enable it under
  `[dev-dependencies]` only).

### Upgrading from 0.5 to 0.6

No code change is needed. What users and operators notice:

* A fresh instance without a setup token shows a locked `/setup` and logs
  the token (`<PREFIX>_SETUP_OPEN=1` for the old behaviour). Instances
  that have users, or a configured token, are unchanged. Scripted setups
  and end-to-end tests set `<PREFIX>_SETUP_OPEN=1`.
* A product's own tests that post to `/setup` add
  `features = ["test-support"]` to `vole-kit` under `[dev-dependencies]`
  (also what the software authenticator needs).
* Notification webhooks that relied on a redirect to another host stop
  working: use the final URL. Redirects within the host (http to https,
  a trailing slash) still work. Channels pointing at link-local, metadata
  or multicast addresses fail.
* Cookies become `Secure` when the public URL is https. Over plain http to
  an https public URL (a LAN address) a browser won't keep them: use the
  public URL.
* After turning two-factor on or off, or adding or deleting a passkey, the
  user's other devices have to sign in again.
* A damaged `session.key` stops the start (restore it from a backup).
* Creating a write or never-expiring API token, adding or promoting an
  admin, an admin's password or two-factor reset and deleting a passkey
  ask for the user's password. A script that posts these forms must send
  `current_password`.
  The new-token sheet selects 90 days; tokens that exist keep their
  expiry. The password form and an admin's reset have a checkbox "Also
  revoke API tokens", on by default.
* Two-factor secrets are sealed in the database (see "Sealed secrets"). A
  product on `migrate::CORE` records `kit/0005_sealed_totp.sql` at the
  next start and takes the pre-migration copy; `Core::new` then seals the
  secrets in the background, with no code change. **Going back to 0.5
  needs that copy** (it holds the secrets in plaintext): 0.5 refuses the
  database, as for any newer migration, and could not check a code in a
  sealed secret.
* A product that lists its core set by hand adds `migrate::SEALED_TOTP`
  (a product that builds `Core` without `Core::new` also calls
  `auth::totp::seal_stored` after migrating, as Vigilant Vole does).
* Everybody who signed up before keeps their session version, so no one is
  signed out by the upgrade.

## Pages

`vole_kit::auth::pages` serves the pages every product needs, generic over
the product's state (anything `Core` and `Key` can be taken from):

* `public_router()`: `/setup`, `/login`, `/login/2fa`, `/login/passkey`
  (and `/options`), `/logout`, with `KitFeatures::invites`
  `/invite/{token}` and with `KitFeatures::sign_in_codes` `/sign-in-code`.
* `router()` must sit behind `require_auth`. It serves:
  * `/settings/general` (admins), only with `KitConfig::general` (with
    `KitFeatures::sign_in_codes` also `/settings/general/smtp-test`);
  * `/settings/users` (create, edit, password, 2FA reset, passkey reset,
    enable, disable, delete; with `KitFeatures::invites` also `/settings/invites`
    and `/settings/invites/{id}/revoke`, with `KitFeatures::sign_in_codes`
    also `/settings/users/{id}/sign-in-code` and `.../sign-in-code/revoke`);
  * `/settings/tokens` (list, create with the product's scopes, revoke);
  * `/settings/security` (profile and language, password, other sessions,
    2FA set-up/cancel/enable/disable/recovery codes, passkeys:
    add/rename/delete);
  * `/api-docs`, `/api-docs.md`, `/llms.txt` and `DocsSpec::openapi_path`.

Vigilant Vole merges both into its routers (`api::auth::router()` and
`public_router()`). Its rendered pages, redirects, cookies and flash messages
are byte-identical to before. A comparison of about 130 raw pages besides the
golden snapshots showed this: every settings tab as admin, editor and viewer,
2FA enrolment, recovery codes, a created token, failed setup and sign-in,
every provider form, in English, Arabic and Norwegian.

The pages' strings are in the kit's catalog. They include 33 keys that used
to be Vigilant Vole's: the API reference texts, `detail.expires`,
`detail.request`, `dashboard.response`, `monitor_form.edit_heading`,
`general.language` and `general.err_site_name`. They moved unchanged, in all
26 languages, keeping their names, so the joined catalogs (and the i18n
snapshots) didn't change. In 0.2 the provider form's key–value editor brought
five more the same way: `form.value`, `form.header_name`,
`form.header_value`, `form.remove_header` and `form.add_header`.

## API token scopes

An API token is "read only" or "read and write" (`api_tokens.scope`,
`TokenScope`). A product can add its own permissions, such as Keen Kestrel's
`difs:write` (upload debug files):

```rust
static KIT: KitConfig = KitConfig {
    // …
    token_scopes: &[ExtraScope {
        name: "difs:write",              // stored and checked
        label: "difs.scope",             // product catalog: the choice's title and the Access badge
        help: "difs.scope_help",         // product catalog: what such a token may do
        implies_read: true,              // it also reads, like a read-only token
        paths: &[],                      // only for scopes that don't imply read
    }],
    // …
};
```

* **Tokens page.** Each scope is one more choice after "Read only" and "Read
  and write" in the new token sheet (radio `scope=<name>`). Such a token is
  stored as a read-only token plus a row in `api_token_scopes`; its Access
  badge shows the scope's label. Without declared scopes the page is
  byte-identical to 0.1's.
* **Checking.** `CurrentUser::has_scope(name)` is true for a session and a
  read-and-write token (they have every scope; the role still applies) and
  for a read-only token that was given the scope. It is false for a name the
  product doesn't declare (debug builds panic, to catch typos).
  `CurrentUser::scopes` is `Scopes::All` or `Scopes::Only(names)`.
* **The owner's real role.** `CurrentUser::role` is still capped by the
  token's access (read → viewer). `CurrentUser::account_role` is the owner's
  own role, and `may_act_as(role)` compares it. Together:
  `user.has_scope("difs:write") && user.may_act_as(Role::Editor)` lets a
  build machine's token upload when its owner may edit, while it reads
  everything else as a viewer and changes nothing. Products no longer need to
  read the user again.
* **Scopes that don't imply read** (`implies_read: false`): the token can only
  do that one thing. `require_auth` answers 403 for such a token except under
  the scope's `paths`; `authenticate_token`/`authenticate_headers` return
  `None` for it, and MCP refuses it. `CurrentUser::can_read()` says whether a
  request may read.
* **Existing tokens keep working exactly as before**: the core migration
  `kit/0002_token_scopes.sql` only adds `api_token_scopes`
  `(token_id, scope)`, `ON DELETE CASCADE`, and a token without rows is what
  it was. A token whose stored scopes the product no longer declares (all of
  them) stops working instead of turning into a read-only token; scopes it
  still declares count.
* **Data access**: `db::api_tokens::{add_scope, scopes_of, fill_scopes,
  with_scope}`; `ApiToken::scopes` (filled by `fill_scopes`).
* `install` refuses invalid names (1–40 of `a-z 0-9 : . _ -`, not `read` or
  `write`), duplicates, labels or help texts without English text, and
  declared scopes without `TOKEN_SCOPES` in `MigrationSet::core`.

Vigilant Vole declares no scopes and leaves `TOKEN_SCOPES` out of its core
set (`core: &[CORE_TABLES, PASSKEYS]`), so it has no `api_token_scopes`;
the kit doesn't touch that table when no scopes are declared.

## Sign-in throttling

The sign-in throttles (per account and per client IP, see
`auth::rate_limit::{ACCOUNT_POLICY, IP_POLICY}`) are `Core::limits`, a
`SignInLimits` per running instance, instead of process-wide statics. The
policies and keys are the same as before, so a production server (one
instance per process) behaves exactly as it did. Two test servers in one
process no longer share counters, so tests that sign in in parallel don't
get each other's 429s. `SignInLimits::with_policies` builds throttles with
other policies (tests of the throttling itself).

`SignInLimits::passkey` (policy `PASSKEY_POLICY`: 100 free failures, then
1 s doubling to 5 minutes) counts failed passkey sign-ins per address, apart
from the password step's `ip` bucket. A failed passkey attempt guesses
nothing, so the bucket only bounds load. The `ip` limiter also holds
`setup:<address>` keys for wrong setup tokens.

The re-authentication checks inside a session (`reauth:<id>` for every
password confirmation, `2fa-setup:<id>`, `2fa:<id>` on the Security page,
`passkey-setup:<id>`) count the attempt before the slow check with
`reserve`, like the sign-in does, and `clear` it on success. A burst of
parallel guesses therefore can't all pass the throttle before the first
failure is recorded.

When a request carries `X-Forwarded-For` or `X-Forwarded-Proto` while
`<PREFIX>_TRUST_PROXY` is off, `csrf_guard` logs one WARN (the instance is
probably behind a proxy, so every client shares the proxy's address and
throttle). The throttle redesign (per account and client) is still open, see
"Not done".

## General settings

### The page

A product turns on the kit's General page (`/settings/general`, admins
only) with `KitConfig::general` and adds `SettingsTab::GENERAL` to its tabs:

```rust
static GENERAL: GeneralSettings = GeneralSettings {
    sections: &[SettingsSection {
        title: "retention.title",                 // product catalog
        fields: &[SettingField {
            key: "event_retention_days",          // settings key and form name
            label: "retention.days",
            help: "retention.days_help",          // "" for none
            kind: FieldKind::Number { min: 1, max: 3650, unit: "retention.unit" },
            default: "90",
            error: "retention.err_days",          // shown for a refused value
        }],
    }],
    after_save: forget_cached_settings,           // fn(&Core), runs after saving
};
```

It shows the core settings in two sections, "Site" (site name, public URL)
and "Region" (time zone, language), then the product's sections. Field kinds
are `Text { max_chars, placeholder }`, `Number { min, max, unit }`, `Switch`
(stored `1`/`0`) and `Select { options }`. Values live in `settings` under
the field's key (`SettingField::load`/`store`); a refused value re-renders
the page with 422 and the field's `error` text. Saving stores the site name
(empty: back to the product's name), the public URL (normalised by
`settings::normalize_base_url`), the time zone and the site language, then
the product's fields, all in one transaction, refreshes the cached site
language, runs `after_save` and confirms in the language the next page uses.

The product's English catalog defines `general.sub` (the subtitle) and
`general.base_url_help` (what the public URL is used for)
(`i18n::catalog::GENERAL_PRODUCT_KEYS`); `install` checks them, the field
texts, keys (`a-z 0-9 _`, unique, none of `site_name`, `base_url`,
`timezone`, `language`) and defaults.

### Without the page

`vole_kit::settings::SiteSettings::load(conn)` reads the four core settings
(`site_name`, `base_url`, `timezone`, `language`; each `None` until set).
`vole_kit::settings::set_site_language(core, Some(lang))` sets the site
language (and the cached default) for a product that sets it from its own
configuration; `vole_kit::i18n::site_language(core)` reads it. Messages sent
outside a request (notifications) use it.

### Vigilant Vole keeps its own page

Vigilant Vole's General page has fields the kit's doesn't (retention with the
database size, public badges, agent confirmation), its own placeholders and a
cache to drop. It keeps its page and sets `general: None`, so its HTML and
behaviour are byte-identical (the golden `page_settings_general_*`
snapshots). It takes what the two share from the kit: the tab
(`SettingsTab::GENERAL`, identical values), `normalize_base_url` (moved
unchanged) and the strings both pages use. Those moved to the kit's catalog
unchanged, in all 26 languages, keeping their names: `general.site`,
`general.site_name_help`, `general.base_url`, `general.region`,
`general.language_help`, `general.save`, `general.saved`,
`general.err_timezone`, `general.err_url`, `general.err_url_credentials`,
`general.err_url_query` and `mw.timezone`. The joined catalogs (and the i18n
snapshots) are unchanged; the kit adds no new strings.

## Password rules

The same rules apply wherever a password is chosen (setup, a new user, an
admin's reset, the Security page), in `auth::password_problem`:

* at least 10 characters (`MIN_PASSWORD_LEN`, counted as characters, not
  bytes) and at most 1024 bytes (`MAX_PASSWORD_LEN`);
* it can't contain the username (any case);
* it needs at least 4 different characters, and it can't be one of the
  kit's common passwords or the product's `extra_common_passwords` (any
  case).

The setup page states the first two under the password field
(`setup.password_help`: "At least 10 characters, and it can’t contain your
username. A few unrelated words work well."); the strength meter
(`static/app.js`, advisory only) replaces that line while you type, and a
refused password names the rule it broke (`users.err_password_short`,
`err_password_long`, `err_password_username`, `err_password_weak`). Usernames
are 1–64 characters of letters, digits, `.`, `_`, `-` and `@`.

## External credentials

Some products never let the server see a password: they sign people in on
their own pages (a zero-knowledge password manager that proves the password
in the browser, say) and only need the kit for the session, the account
pages and the admin panel. They set

```rust
features: &KitFeatures {
    credentials: CredentialMode::External(&ExternalCredentials {
        login_path: "/vault/login",
        setup_path: "/vault/setup",
        password_path: "/vault/password",
        reauth_path: "/vault/reauth",
    }),
    ..KitFeatures::DEFAULT
},
```

The four paths are routes of the product (absolute, no query; `config::check`
refuses anything else at start-up). `CredentialMode::Plain`, the default, is
the kit as described above and nothing in it changes.

### What the kit's pages do

| Page | In `External` mode |
| --- | --- |
| `GET /login` | `303` to `login_path`, with `?next=` when the kit would have followed the `next` it got (a plain local path; anything else is dropped) |
| `POST /login` | `404` |
| `/setup` (GET and POST) | `303` to `setup_path`; a query string (the setup token the start-up message links with) goes along |
| Security page, Password | a link to `password_path` in place of the form; `POST /settings/security/password` is `404` |
| Users page | "New user" (a temporary password) and "Reset password" are hidden; `POST /settings/users` and `POST /settings/users/{id}/password` are `404`. A product replaces them with invitations and sign-in codes |
| `/login/2fa`, `/logout`, `/login/passkey` and `/login/passkey/options` | as before |

Signing in with a passkey stays: it needs no password and gives a session
(the same as the kit's other products). A product whose secrets are held
only by the client has to know that a passkey session does not unlock them.

### Confirming it's you again

Anything that asked for the current password (adding or removing a
passkey, switching two-factor authentication off, new recovery codes, a
write or never-expiring token, making someone an administrator, resetting
someone's two-factor authentication) asks for a **grant**
instead. Without one, the sheet shows "Confirm it’s you", a link to
`reauth_path?next=<the page>`, and the change is refused with the same
message when it is posted anyway; a password sent along counts for nothing.

The product's page at `reauth_path` confirms the signed-in user in its own
way and finishes with

```rust
let jar = vole_kit::auth::reauth::grant(jar, &core.session_key, secure_cookies(&headers));
(jar, Redirect::to(&session::sanitize_next(next)))
```

`grant` binds the grant to the session cookie in `jar` (also one that
`login_jar` just added in the same response), so call it for the session the
user is in; with no session in the jar it gives nothing, and the pages keep
asking. The grant is the cookie `<prefix>_reauth` (the session cookie's name
with `_session` replaced: `vv_session` gives `vv_reauth`). It is signed with
the session key like the other cookies, and its value is also sealed for this
one purpose (`crypto::Sealer`, purpose `reauth-grant`, a key derived from the
session key as for the kit's secrets at rest): inside is
`r2.<user id>.<session version>.<session issued at>.<granted at>`.

* **Purpose.** A cookie's signature does not cover its name, so any other
  value the session key has signed could be put under the grant's name. A
  product that signs text a visitor chose (a remembered user name, an
  invitation code) would otherwise hand out grants. Only a value sealed for
  `reauth-grant` counts. A product should still never sign text a visitor
  chose and send it back as a cookie value.
* **Session.** It counts only for the user, the session version and the
  session it was given in (the session cookie's `issued at`, which a sign-in
  sets and nothing moves). A grant from another device's sign-in is no good
  here, signing out everywhere (which moves the version) ends it, and so do
  signing out and `login_jar`.
* **Time.** It lasts five minutes (`reauth::TTL_SECS`), is `HttpOnly`,
  `SameSite=Strict`, `Path=/`, and `Secure` over https.
* **Used up.** Three changes use the grant up: adding a passkey, making
  someone an administrator, creating a write token. When one succeeds the kit
  clears the cookie, and the next such change needs a new confirmation (a
  refused change does not use it). The other changes (new recovery codes,
  two-factor off, removing a passkey, never-expiring read tokens, resetting
  someone's two-factor authentication) keep to the five minutes, so a few in a
  row take one confirmation. Clearing removes the cookie from the browser; a
  copy somebody kept stays valid until it runs out.
* **Several cookies.** If a browser sends more than one `<prefix>_reauth`
  (another Path, or one tossed from a sibling subdomain), the kit keeps the
  first that verifies and really is a grant, and drops the others from the
  request, so a stale one cannot shadow the real one.

`reauth::is_fresh(jar, &core.session_key)` tells whether one is held for the
session in `jar`, for the product's own sensitive routes; `reauth::clear(jar)`
uses it up. `reauth::grant_at` and `is_fresh_at` (any time, any session) exist
only for tests, with the `test-support` feature.

### What a product uses from the kit

* `auth::guard::login_jar(jar, user_id, session_version, secure)`: the
  session cookie, after the product's own sign-in (and its second factor,
  `auth::pages::verify_second_factor`).
* `Core::limits` (`SignInLimits`: `account`, `ip`, `passkey`): the same
  throttles as the kit's own sign-in.
* `auth::pages::check_setup_token(core, headers, peer, given)`: the setup
  token check of the kit's own setup page (a wrong token counts against the
  address, or against one shared key when there is none: `peer = None` is
  throttled all the same; `SetupTokenError` says why it was refused). Call it first on the
  product's setup page.
* `auth::pages::insert_first_admin(core, username, password_hash, site_name,
  timezone)`: creates the first admin, the site name and the time zone in
  one transaction; `None` if someone else was first. `password_hash` is
  opaque to the kit. A product that keeps the credential elsewhere stores a
  marker such as `"!external"`: it is not a hash, so `verify_password` never
  matches it.
* `auth::reauth::grant`, `is_fresh` and `clear`, above.

## Invitation links

A product that wants people to create their own accounts, instead of an
admin handing out temporary passwords, turns on invitation links:

```rust
features: &KitFeatures { invites: true, ..KitFeatures::DEFAULT },
migrations: &MigrationSet {
    core: &[CORE_TABLES, PASSKEYS, /* ... */ INVITES],
    ..
},
```

They are off by default and change nothing for a product that leaves them
off: the routes are `404`, the Users page shows nothing, and the table does
not exist. The `INVITES` migration (`kit/migrations/0007_invites.sql`, the
table `invites`) is **not** part of `CORE`, so a product that uses
`CORE` as it is (Keen Kestrel, Wise Wren) keeps its schema. `config::check`
refuses to start a product with `invites: true` whose `MigrationSet::core`
lacks `INVITES` (the same rule as `TOKEN_SCOPES` for token scopes).

### What an admin does

The Users page (admins only) gets an "Invite" button. The sheet asks for the
role, an optional display name and the admin's own password (with
`CredentialMode::External` the usual "Confirm it’s you" link, so a stolen
session can't hand out accounts, whatever the role). It then shows the link
once, in a copy field:

```
{public URL}/invite/{token}
```

The base is the site's public URL when one is set, otherwise the address the
admin is using. The token is 32 random bytes written in base62 (43 characters,
256 bits); only its SHA-256 is stored (`invites.token_hash`, 32 bytes), so a
database copy holds no usable link. The page is sent with `Cache-Control:
no-store`.

A link works once and for seven days (`auth::invites::LIFETIME_DAYS`). The
page lists the pending invitations (display name, role, expiry, who made
them), each with Revoke. At most 50 can be pending
(`auth::invites::MAX_PENDING`). Used, revoked and long-expired rows are
removed 30 days after they finished.

### What the invitee does

`GET /invite/{token}` shows a form with user name, display name (prefilled
with the admin's suggestion), password and confirmation, with the usual
password rules ("Password rules"). `POST` creates the user with the
invitation's role and marks the invitation used in **one transaction**, then
signs the new user in (`login_jar`) and goes to `/`. A taken user name or a
refused password shows the form again and keeps the link alive. The POST is
throttled per address with the sign-in IP limiter (key `invite:<ip>`): a
token that doesn't work counts as a failure, a valid token with a form to
correct does not.

An expired, used, revoked and unknown token (and one of the wrong shape) all
get the same page: `404`, "This invitation is no longer valid.", with the
same headers. The token is looked up by its hash, so no comparison of secrets
takes place, and nothing tells which of the four it was. Of two people racing
for one link, exactly one gets an account: the claim is a single `UPDATE`
inside an immediate transaction.

### With external credentials

With `CredentialMode::External` the kit never sees a password, so
`GET /invite/{token}` checks the token and answers `303` to
`{setup_path}?invite={token}` (an invalid token gets the generic page), and
`POST /invite/{token}` is `404`. The product's setup page does the rest with
`vole_kit::auth::invites`:

* `lookup(core, token) -> ApiResult<Option<Invite>>`: the pending invitation
  (`role`, `display_name`, `expires_at` ...), `None` for any token that is
  not pending.
* `accept(core, token, username, display_name, password_hash) ->
  ApiResult<Result<i64, AcceptError>>`: creates the user with the
  invitation's role and uses the invitation up in one transaction, and
  returns the new user's id. `AcceptError::Invalid` (not pending any more) or
  `UsernameTaken` (the invitation stays pending). `password_hash` is opaque
  to the kit, like for `insert_first_admin` (`"!external"`). Check the user
  name with `auth::username_problem` first. Sign the user in with
  `guard::login_jar`.
* `create_user(core, username, display_name, role, password_hash) ->
  ApiResult<Option<i64>>` and `consume(core, token, user_id) ->
  ApiResult<Result<(), NotPending>>`: the two halves, for a product that
  creates the account its own way. `consume` is one statement that fails when
  the invitation was used, revoked or has expired in the meantime; the
  product then removes the account it just made (`accept` has no such
  window). `create_user` is `None` when the user name is taken
  (case-insensitively).
* `generate()`, `hash(token)`, `is_well_formed(token)`, `TOKEN_LEN`.

The same functions exist on a bare connection in `db::invites` (`insert`,
`find_pending`, `consume`, `accept`, `revoke`, `list_pending`), for a product
that works inside its own transaction.

### Tests

The unit tests are in `db::invites` and `auth::invites`. The features can't
share a test binary with the default configuration (the kit's configuration
is installed once per process), so the end-to-end tests are the kit-demo
binaries `tests/invites.rs` (the kit's own sign-in), `tests/invites_external.rs`
(`External`) and, for the feature off, `tests/features_off.rs`.

## One-time sign-in codes

A person who can't sign in (a forgotten password, a lost device) gets a
one-time code from an admin and chooses a new password with it. A product
turns the feature on, and adds the migration to its core set:

```rust
features: &KitFeatures { sign_in_codes: true, ..KitFeatures::DEFAULT },
migrations: &MigrationSet {
    core: &[CORE_TABLES, PASSKEYS, /* ... */ SIGN_IN_CODES],
    ..
},
```

Like invitation links it is off by default and changes nothing then: the
routes are `404`, no page shows anything of it and the tables do not exist.
The `SIGN_IN_CODES` migration (`kit/migrations/0008_sign_in_codes.sql`) is
**not** part of `CORE`, so a product on `CORE` as it is keeps its schema, and
`config::check` refuses `sign_in_codes: true` without it. The tables are
`sign_in_codes` (`user_id`, `code_hash`, `created_by`, `created_at`,
`expires_at`, `used_at`, `revoked_at`) and `user_emails` (`user_id`, `email`,
`verified_at`; optional, one address per user). `users` is not changed.

### What an admin does

On the Users page (admins only) every other user has "Reset sign-in" (never
oneself). The sheet asks for the admin's own password, or with
`CredentialMode::External` the usual "Confirm it’s you" link, which is used up
by creating the code: a code is a way into the account. The page then shows
the code once, in a copy field, with `Cache-Control: no-store`:

```
7GQ2-M4XB-0C5T-WZ9K-HE1N-Q8DY-A3RF-V6PS
```

It is 20 random bytes (160 bits) in Crockford base32, written as eight groups
of four characters. Only the SHA-256 of the 32 characters is stored
(`sign_in_codes.code_hash`), so a database copy holds no usable code. A code
works **once**, for **24 hours**, and a newer code for the same user revokes
the earlier unused ones. A disabled user's code does not work (and the sheet
refuses to make one).

A pending code ends early, and the Users page shows it while it lasts: the
row has a "Code pending" label (with the expiry in its tooltip) and a "Revoke
code" action. Revoking asks for the admin's password, or a fresh grant, like
creating a code. A pending code is also revoked by:

* a new password: the user's own, an admin's temporary one, or one set with
  `db::users::set_password`;
* a two-factor reset (`db::users::disable_totp`);
* disabling and enabling the account (`db::users::set_active`; a code made
  for a disabled user is revoked when the account is enabled);
* a successful sign-in of the user, with a password (and the second factor)
  or a passkey. A product with `External` credentials that signs people in
  itself calls `db::sign_in_codes::revoke_for_user` there;
* deleting the user (the rows cascade).

The functions of `db::users` revoke through `db::sign_in_codes::revoke_if_enabled`,
which does nothing when the `sign_in_codes` table does not exist, so a product
without the feature is unchanged.

The edit sheet of a user has an optional email address (`user_emails`; empty
clears it; a form without the field leaves it alone). **Changing, setting or
clearing an address asks for the admin's password** (or a fresh grant, used up
by it): the address is where codes can be sent. An address is taken as typed
and is **not verified**: `verified_at` stays empty, nothing is sent to the new
address to confirm it, and a mistyped address sends the code to a stranger.
The check is that it looks like an address. That is why mailing is a choice.

The create sheet has an "Also send by email to ..." box. It is offered, and
ticked, only when the instance has mail settings (below) and the user has an
address, and it shows that address. A request without `send_email=true` sends
no mail. When it is ticked the code is emailed in the user's language as a
plain-text message, through `notify::email::deliver`; the page says so, or that
the mail failed. The message is made from the translated templates with fixed
fields: the subject has only the site name; the body has the person's name,
user name, the site and the link; the code is put into the body **last**, after
every other value, so no name can bring it into the subject. Braces and control
characters (line breaks) are dropped from the names that go in, and a line
break can't start another header (`mail::build` drops control characters from
the subject and refuses them in the sender and recipient). The code is never
logged: the log has the user and the admin, and for a mail that failed the
error, not the message.

### Instance mail settings

With the feature on, the General page (`KitConfig::general`) has an "Email
(SMTP)" section: server, port, security (STARTTLS, TLS or none), user name,
password and sender. The settings are in the `settings` table (`smtp_host`,
`smtp_port`, `smtp_security`, `smtp_username`, `smtp_password`, `smtp_from`;
a product's own General fields can't use these keys). The password is sealed
under the instance key ("Sealed secrets", purpose
`vole-kit/instance-smtp`) and never shown again: leave the field empty to keep
it, tick the box to remove it. Fields that are not in the submitted form keep
their saved value (a stale page or a script sending part of the form does not
switch mail off); a field that is there and empty clears it, and a form with
every field empty turns mail off. The `smtp_*` keys (and `current_password`)
are reserved for the kit only when `sign_in_codes` is on; with the feature off
a product's own General field may use them.

Changing the mail settings or the public URL (`base_url`, where the link in the
mail points) asks for the admin's password (a password field at the bottom of
the page; a fresh grant with `External` credentials, used up by the change);
saving the same values again, or changing something else, asks for nothing.
Without that, a stolen session could point the mail at a server it controls and
wait for the next reset.

A connection without encryption (`none`) with a user name or password is
refused unless the server is on this machine (`localhost` or a loopback
address), when the settings are saved and again when a message is sent.
"Send a test email" (`POST /settings/general/smtp-test`) sends through the
saved settings, to any valid address, at most five per ten minutes per admin
(then the page says how long to wait). The same address rule as for alert
channels applies (no link-local or cloud-metadata addresses). `vole_kit::mail`
has the pieces (`Smtp`, `load`, `build`, `send`, `parse`) for a product without
the General page. The alert channels keep their own SMTP settings; this is
separate.

### What the user does

`GET /sign-in-code` (linked from the sign-in page) is a form with user name,
code, new password and confirmation, with the usual password rules ("Password
rules"). The code may be typed with any case, spaces or dashes, and with
Crockford's look-alikes (`O` for `0`, `I` and `L` for `1`). On `POST` the kit
throttles the attempt per account (key `code:<name>`) and per address (key
`code:<ip>`, an IPv6 address by its /64) with limiters of their own
(`SignInLimits::code_account`, `code_ip`), apart from the sign-in ones, so that
guessing codes can't use up an account's sign-in budget nor the other way round.
They are a backoff, as at sign-in, never a hard lock: ten free failures per
account, then 2 s doubling up to five minutes (the code has 160 bits, so a
guess is hopeless and the throttle only bounds the load). A user name longer
than a user name can be is replaced by its digest in the key, so the key stays
small. Then, in one immediate transaction, the attempt claims the code and sets
the password (hashed whether or not the code turns out to be good). The claim is a single `UPDATE` that matches the code's hash
*and* the user it was made for, so a good code of somebody else is not used up
and answers like any wrong one, and of two people racing for one code exactly
one wins. Every failure (expired, used, revoked, unknown, someone else's,
disabled or unknown user) is the same `401` page; only a throttled attempt is
told apart (`429`, with the wait), as at sign-in. A new password the rules
refuse is a `422` on the form and counts as no attempt, and the code stays.

On success the user's `session_version` moves on (every session is signed
out), the password is the new one, **all the user's API tokens are deleted**
(in the same transaction as the claim: a code is for an account whose
credentials may have leaked, and a token is the longest-lived one; with
`External` credentials the admin has no password reset to do it with), and the
user is signed in. Two-factor and passkeys are not touched: with two-factor on
the user goes on to `/login/2fa` first, because a code replaces the password,
not the second factor. The sheet that creates a code says that using it deletes
the tokens.

The log has what happened, not what was typed: a failed or throttled attempt
logs `user_ref`, eight hex digits of the digest of the typed name (a name could
hold a line break, or a code pasted into the wrong field), and the address; the
code is never logged.

### With external credentials

With `CredentialMode::External` the kit never sees a password, so
`GET /sign-in-code` answers `303` to `{password_path}?code=1` and `POST` is
`404`. The product's page asks for the user name, the code and its own new
credential, and uses `vole_kit::auth::sign_in_codes`:

* `redeem_from(core, username, code, ip: Option<IpAddr>) -> ApiResult<Result<RedeemToken, Generic>>`:
  uses the code up (once, atomically), bumps the user's `session_version` and
  deletes the user's API tokens. Pass the visitor's address
  (`session::client_ip`): it is throttled per account and per address with the
  code limiters; only without any peer address is `None` right. Every failure,
  a throttled attempt included, is the one `Generic`. (There is no `redeem`
  without the address.)
* `RedeemToken` is the proof of the redeem: `user_id()` and
  `session_version()` (for `guard::login_jar`). It is not `Clone`, is used up
  by `set_password_hash`, lives in memory, and is good for ten minutes
  (`REDEEM_TOKEN_TTL`).
* `set_password_hash(core, token, hash) -> ApiResult<bool>`: stores the
  product's credential (opaque to the kit, like `"!external"`) for the user the
  token names; `false` and nothing stored when the token is too old. It signs
  nobody out, the redeem did. A product cannot set a credential through this
  module without a redeem. Then `guard::login_jar` (after
  `verify_second_factor` for a user with two-factor).
* `generate()`, `normalize(text)`, `hash(code)`, `log_ref(name)`, `CODE_LEN`.

The same functions exist on a bare connection in `db::sign_in_codes`
(`insert`, `redeem`, `revoke_for_user`, `revoke_if_enabled`, `count_pending`,
`pending_users`, `email`, `emails`, `set_email`).

A product that signs in itself should also call
`db::sign_in_codes::revoke_for_user` after a successful sign-in, as the kit's
own sign-in does.

### Tests

Unit tests are in `db::sign_in_codes`, `auth::sign_in_codes` and `mail`. The
end-to-end tests are the kit-demo binaries `tests/sign_in_codes.rs` (the
kit's own sign-in, with a fake SMTP server and a check of what is logged),
`tests/sign_in_codes_external.rs` (`External`, including a race for one code)
and, for the feature off, `tests/features_off.rs`.

## Passkeys

0.4 adds passkeys (WebAuthn) to every product: discoverable credentials
with user verification, so a passkey sign-in counts as both factors and
skips the TOTP step. Password plus TOTP stays exactly as it was, as the
fallback.

* **Sign-in.** `login.html` gets a "Log in with a passkey" button and
  `autocomplete="username webauthn"` on the username field, so the browser
  offers saved passkeys (conditional UI). `POST /login/passkey/options`
  issues a challenge, `POST /login/passkey` verifies the assertion, sets
  the session (with `next`) and answers `{"redirect": …}`.
* **Security page.** A Passkeys section lists them (created, last used),
  with rename and delete. Adding one asks for the current password first
  (`confirm_password`, the re-authentication the other sensitive changes
  there use), then runs `navigator.credentials.create`. The default name
  is the passkey provider's when its AAGUID is a well-known one
  ("iCloud Keychain", "Google Password Manager", "Windows Hello" …),
  otherwise "Passkey N".
* **Users page.** Admins can remove all of a user's passkeys (shown only
  when the user has some). Like the 2FA reset, it signs the user out
  everywhere and is logged (`tracing`, with the admin's name).
* **Relying party.** The RP ID is the host of the public URL (`base_url`),
  and the only accepted origin is that URL's origin
  (`webauthn::RelyingParty::from_public_url`). It must be HTTPS with a
  domain name, or `http://localhost`. Without such a URL the pages offer no
  passkeys, and Security says "Passkeys need this site’s public URL over
  HTTPS."; on another origin (`http://10.0.3.26:3001`) `static/passkeys.js`
  keeps the buttons hidden and Security says "Passkeys work at <public URL>
  over HTTPS.". The check is the browser's own origin, so reverse proxies
  need nothing; the sign-in throttles use the client address as
  `<PREFIX>_TRUST_PROXY` says. Changing the public URL's host makes
  existing passkeys useless (they are bound to the old RP ID).
* **Server side** (`auth::webauthn`): WebAuthn Level 3 §7.1 and §7.2
  step by step: type, challenge, origin, no cross-origin, RP ID hash, the
  UP and UV flags, consistent backup flags, the signature over
  `authData || SHA-256(clientDataJSON)`, the user handle, and the signature
  counter (it must grow unless both sides are 0, as with most synced
  passkeys). Attestation "none": the statement isn't verified, any format
  is accepted and only the credential's public key is taken. ES256, EdDSA
  (Ed25519) and RS256, verified with aws-lc-rs; CBOR is read with
  ciborium. No OpenSSL: `webauthn-rs` (0.5.5) needs it, so the kit has
  its own small verifier.
* **Challenges** are 32 random bytes in `passkey_challenges`, single use
  (taken with `DELETE … RETURNING` before anything else is checked),
  valid for five minutes, bound to their purpose and, for a registration,
  to the user. Expired rows go when the next one is stored, and the table
  keeps at most 10 000.
* **Throttling.** Starting a sign-in is a request budget per client
  address (30 a minute: the page asks for a challenge on every visit for
  the autofill) and refused while the address is in sign-in backoff; a
  failed sign-in counts against the address (`SignInLimits::passkey`, its
  own and far more generous bucket, so password failures from the same
  address, or from everybody behind one proxy, don't keep passkeys out and
  the other way round), a success undoes it. Failed registrations back off
  per user (`passkey-setup:<id>`).
* **Front end.** `static/passkeys.js` (a kit file, no inline script),
  loaded only by the two pages when the public URL allows passkeys. Its
  messages come from `data-msg-*` attributes: cancelled or timed out,
  unsupported browser, already registered, failed; the login page shows
  them in a callout, Security as a toast.
* **Tests.** `webauthn::soft::SoftAuthenticator` makes real ES256, EdDSA
  and RS256 credentials and assertions. The kit's unit tests check every
  refusal (wrong origin, challenge, RP ID, type, missing UP or UV, a bad
  signature, another user handle, a counter that didn't grow, changed
  backup eligibility, malformed CBOR); kit-demo runs both ceremonies over
  HTTP, including a reused challenge and the admin's reset. Products can
  use the soft authenticator for their own tests.

The core migration `kit/0003_passkeys.sql` (`migrate::PASSKEYS`, part of
`CORE`) creates `passkeys` (credential id, COSE key, algorithm, user handle,
name, sign count, transports, backup flags, created and last used) and
`passkey_challenges`, both `ON DELETE CASCADE` with their user. `install`
refuses a configuration whose `MigrationSet::core` lacks it.

## Sealed secrets

Notification channels keep their provider settings as JSON in the
product's `notification_channels` table (`id`, `kind`, `config`). Since
0.5 the secret fields in it are sealed. These are the fields the schema
marks `secret`, the same ones `schema::redact` masks: tokens, passwords,
webhook URLs with a key in them, header maps. Every other field stays
readable JSON.

* **`crypto`**: AES-256-GCM (aws-lc-rs) with a fresh random 96-bit nonce
  per value. The key comes from HKDF-SHA256 over the instance key
  (`session.key`, the 64 bytes `auth::load_or_create_key` loads), with the
  kit's salt and a *purpose* string as info. Each purpose has its own key,
  and none of them is the cookie key (neither half of the master key). The
  purpose is also the AEAD's associated data. A sealed value is
  `sealed:v1:` + unpadded URL-safe base64 of `nonce ‖ ciphertext ‖ tag`.
  The prefix carries the version and tells sealed values from plaintext
  (`is_sealed` also checks the body decodes and is long enough).
  `open` fails with `OpenError::Unreadable` for another key, another
  purpose or a changed byte.
* **`notify::sealed`** (purpose `vole-kit/notification-channels`):
  * `seal_config(key, kind, &config)` seals the secret values before a
    write. Values that are sealed already stay as they are, and key–value
    secrets are sealed value by value.
  * `open_config(key, kind, &config) -> Opened` opens them for sending,
    testing and editing. A secret that doesn't open is left out of
    `Opened::config` and named in `Opened::unreadable`. `Opened::errors()`
    gives "enter it again" field errors for the edit form, and
    `Opened::reentry_errors(&submitted, &clear)` gives errors for a save
    that doesn't replace such a secret.
  * `open_channel(key, &channel)` opens a stored channel before
    `notify::send`. It fails with `notif.err_secret_unreadable` ("This
    channel's secret can't be read (the instance key changed); enter it
    again") if a secret doesn't open.
  * `seal_stored(conn, key)` seals what an older version stored in
    plaintext, in place and in one transaction, and logs how many channels
    it sealed. Run it at startup after the migrations. A second run changes
    nothing (sealed values are recognised by the prefix). It skips unknown
    kinds, configs that aren't JSON and databases without the table.
  * `open_stored(conn, key)` does the opposite, for a **copy** that has to
    work without this key (a backup download). Secrets that don't open
    stay sealed.
* **Sending refuses sealed text.** `send`, `send_config`, `send_in` and
  `send_config_in` return the same "enter it again" error, and send
  nothing, when a secret value is still sealed. A product that forgets to
  open a channel, or can't, never sends ciphertext to a provider.
* **Masking is unchanged.** `redact` and `redacted_config` mask sealed
  values like plaintext ones, so API, UI and MCP output are the same as
  before.

**Key loss.** A sealed secret only opens with the instance key that sealed
it. If `session.key` is lost or replaced (a new data directory, a
database restored without its key), the channel's edit form shows the
field as not saved, with "enter it again". A save that leaves it out is
refused, and sending fails with that message, logged by the product.

### Two-factor secrets

The base32 secret of a user's TOTP (`users.totp_secret`, the pending one
while enrolling too) is sealed like a channel secret, under the purpose
`vole-kit/totp` (`auth::totp::PURPOSE`). Recovery codes were always Argon2
hashes and are unchanged.

* The pages seal when they store (`totp::seal_secret(&core.session_key, ..)`)
  and open when they use it (`totp::open_secret`): showing the QR code
  while enrolling, confirming the first code, every login and `2fa/disable`.
  `db::users` still stores and returns the text as it is, so
  `set_pending_totp_secret` and `get_totp` keep their signatures.
* `open_secret` accepts **plaintext** too, so a login keeps working
  between the migration and the sealing, and for a product's tests that
  store a plaintext secret directly. A sealed secret that doesn't open (a
  replaced `session.key`) opens as `None`: that user's codes never match,
  logged at WARN, and their recovery codes still work. Turning two-factor
  off and on again, with a recovery code to get in, makes a new one.
* `totp::seal_stored(conn, key)` seals every plaintext secret in place and
  returns how many it sealed; a second run changes nothing. It swaps each
  row only while it still holds the text it read, so an enrolment started
  meanwhile isn't overwritten. `Core::new` runs it in the background when
  called inside a Tokio runtime (Keen Kestrel and Wise Wren build their
  `Core` that way, so they need no change). A product that builds `Core`
  by hand calls it at startup after migrating, next to
  `notify::sealed::seal_stored` (Vigilant Vole does, in `server::prepare`).
* `totp::open_stored(conn, key)` is the opposite, for a **copy** that has
  to work on another instance (Vigilant Vole's backup download runs it
  with the channel secrets' `open_stored`); the instance that restores it
  seals again at its start.
* `totp::has_plaintext_stored(conn)` says whether any plaintext is left.

The migration `kit/0005_sealed_totp.sql` (`migrate::SEALED_TOTP`, in
`CORE`) changes no table. It is there so that an upgrade takes its
pre-migration copy first and an older kit refuses the database, like
`SEALED_SECRETS`. **Rolling back to a kit without it needs that copy**:
the copy has the secrets in plaintext, and the older kit would otherwise
read `sealed:v1:…` as a base32 secret and reject every code.

### Pre-migration copies

The runner's copy before an upgrade (`VACUUM INTO`, in the product's
backups folder) holds whatever was in the database, so the copies taken
before the sealing migrations (`SEALED_SECRETS`, `SEALED_TOTP`) hold the
channel and two-factor secrets in plaintext. `migrate()` now deletes them
(`migrate::prune_plaintext_backups`, called at the end of every start of
the product's main database):

* **when** a later start finds no plaintext secret left in the database
  (`totp::has_plaintext_stored`, `notify::sealed::has_plaintext_stored`):
  the earlier start sealed them. The copy this start took itself is kept
  for the next one.
* **or after 7 days** (`PLAINTEXT_BACKUP_MAX_AGE`, by the file's
  modification time), whatever the database holds.

Only these files are ever touched, all regular files (never a link) in
the backups folder:

* `{backup_prefix}kit-0004_sealed_secrets-{unix time}[-n].db` and
  `{backup_prefix}kit-0005_sealed_totp-{unix time}[-n].db`, the names of
  copies taken when one of those two was the first pending migration;
* any `….db` with a `….db.plaintext-secrets` file next to it. The runner
  writes that marker whenever the pending migrations include one of the
  two, so copies named after another migration (a product migration that
  ran first) are found too. The marker goes with the copy; a marker
  whose copy is gone is removed.

A copy from before this release that carries another migration's name has
no marker and stays: delete it yourself once the instance has run on
sealed data. Everything else in the folder (manual copies, the copies a
restore moves aside, notes) is left alone. A failure to delete is logged
and never stops a start. There is no setting to keep them longer: copy one
elsewhere if a rollback must stay possible after a week.
Nothing crashes. The other fields and the other channels keep working.

**Security note.** Sealing protects the secrets in a copy of the database
on its own: a leaked file, a database-only backup, a support upload, or
SQL-level read access. It does not protect against someone who has both
the database and `session.key`, or who controls the running process. That
key also signs and encrypts the session cookies, so it was already the
data directory's most sensitive file. Keep the data directory private
(the kit writes `session.key` with mode 0600). Config exports with secrets
and Vigilant Vole's database backup download hold the secrets opened, so
they need the same care as before.

## Alerts

Every provider builds from a `vole_kit::notify::alert::Alert`:

* `kind: AlertKind`: `Problem`, `Recovered`, `Reminder`, `Escalation`,
  `Expiring`, `Unstable`, `MaintenanceStarted`, `MaintenanceEnded`, `Test`.
  Providers use it to open, resolve or skip incidents and to pick priorities
  and icons.
* `level: Level` (`Critical`, `Warning`, `Ok`, `Info`), `colour`, `emoji`;
  `AlertKind::level()`, `AlertKind::emoji()` and `Level::colour()` give the
  family's defaults.
* the finished texts: `title`, `detail`, `facts` (label/value pairs),
  `link_label`, `event` (machine name, e.g. `down`), `event_label`, `status`;
* the subject: `subject_id`, `subject`, `subject_type`, `target`, plus
  `message`, `duration`, `expires_in_days`, `url`, `occurred_at`.

The product writes the texts in the reader's language; the kit only lays them
out (summary, title, plain-text, Markdown and HTML bodies).

Vigilant Vole keeps its `NotificationEvent` and `EventKind`.
`notification::message::alert` turns one into an `Alert`, with headline,
detail and facts in Vigilant Vole's wording. `crate::notification` keeps
`PROVIDERS`, `Transport`, `send`, `send_config`, `send_all` and
`email::{build_message, send}` with their old signatures, as wrappers. Every
provider's request and the email MIME are byte-identical (the golden
notification snapshots).

Provider forms don't name a product. A placeholder or default that shows one
says `{product}` or `{short_name}` (the Slack bot name, the Zulip topic, the
SMS sender …), or `{webhook_url}` for the webhook's URL example. Read fields
through `Field::placeholder_text`, `Field::default_value` and
`Field::tr_placeholder`, which fill in the configured names, and help texts
through `Field::tr_help`, which puts the product's webhook placeholders in
(see "Notification channels").

## Push (0.7)

Alerts as push notifications in the Hedgerow app, through the Hedgerow push
relay ("Postal Pigeon", `pigeon.hedgerow.tools`). Push is a notification
channel of kind `hedgerow` ("Hedgerow app"), not a system of its own: an
administrator adds it once under Notifications and attaches it like any other
channel (monitors in Vigilant Vole, alert rules in Keen Kestrel, every channel
in Wise Wren). Without such a channel push is off. The channel has no
settings: it reaches every app install that registered with the server.

How it works:

1. The app makes one *handle* (`hrh_…`) at the relay per connection and
   gives it to the server: `POST /api/v1/push/devices`.
2. When the channel fires, the server stores the alert's text under a random
   `ref` (`push_events`, 24 hours) and sends `POST /v1/push` to the relay
   with the handles, the `ref`, `kind: alert` and `ttl` 3600. Neither the
   relay nor Apple sees the text.
3. The app's Notification Service Extension fetches the text with
   `GET /api/v1/push/events/{ref}` from the server it belongs to, with the
   token from the shared keychain, and rewrites the notification.

### What the kit provides

* `notify::hedgerow`: the provider (`kind` `hedgerow`, `Transport::Relay`,
  category Push, no fields).
* `push::install(&core)`: registers the delivery context (database, instance
  key, product) for the process. Call it once at start-up, after
  `config::install`. `Core::new` does not do it, because products often build
  their `Core` per request. Without it a `hedgerow` channel refuses to send
  ("Push isn't available on this server"). It panics for a product the
  relay doesn't know (`push::relay::relay_product`: `vigilant-vole`,
  `keen-kestrel` and `wise-wren`, by `KitConfig::slug`) and for an invalid
  `KIT_PUSH_RELAY_URL`: both are mistakes of the deployment, not something to
  find out when the first alert fires.
* `push::api::router::<S>()`: the three routes (mount it behind
  `guard::require_auth`; the paths are under `/api/v1`, which products
  exempt from the CSRF guard) and `push::api::ROUTE_DOCS`, three `RouteDoc`s
  (group `Push`, access `viewer`) for the product's API reference.
* `migrate::PUSH` (`kit/0006_push.sql`).

### The routes

| Route | Answer |
|---|---|
| `POST /api/v1/push/devices` `{"handle": "hrh_…"}` | `201 {"id": 3}` with `Location`; the same handle again `200` with the same id; `409 push_disabled` while no active `hedgerow` channel exists; `409 too_many_devices` at 16 devices per user (256 per server); `409 too_many_pending` while 4 of the user's devices, or 20 on the whole server, have not received a push yet; `429 rate_limited` after 4 new handles in an hour (deleting a device does not give the registration back); `422 validation_failed` for something that isn't a handle |
| `DELETE /api/v1/push/devices/{id}` | `204`; `404` for an unknown id or someone else's device |
| `GET /api/v1/push/events/{ref}` | `{"title", "body", "kind", "level", "occurred_at"}` (plain text; no link: the alert's deep link is not stored, the monitored target stays as a fact line) (`kind` is `problem`, `recovered`, `reminder`, `escalation`, `expiring`, `unstable`, `maintenance_started`, `maintenance_ended` or `test`; `level` `critical`, `warning`, `ok` or `info`; `occurred_at` RFC 3339 UTC), `Cache-Control: no-store`; `404` when unknown or older than 24 hours |

Errors use the REST envelope (`{"error": {"code", "message"}}`).

**Read-only tokens may call exactly these three routes.** The app's tokens are
often read-only, and registering only changes the caller's *own* list of
devices. Nothing is added to the guard for it: the handlers check no role,
take the user from the session or token (never from the request), and a
device registered with a token is removed when the token (or the user) is.
Every other route keeps its own role check, so a read-only token still
reaches nothing else that writes. The routes also take sessions and
read-and-write tokens.

### Registration with the relay

The first time a `hedgerow` channel fires with devices registered, the
server registers itself: `GET /v1/challenge`, a proof of work (about a
second at 20 bits, on a blocking thread; abandoned after four minutes), then
`POST /v1/instances` with the product and version. The credential (`hrk_…`)
is kept in the `settings` table sealed under the purpose `push-relay`
(`push_relay_credential`), next to the relay it belongs to
(`push_relay_credential_url`): a credential is only used with the relay that
issued it. Registration runs in a task of its own, one at a time, and after
a failure waits a minute before trying again; an alert that gives up waiting
leaves it to finish for the next one. A relay that answers `401
unknown_instance` (it lost its database) gets one new registration per alert.
`403 suspended` is logged as an error and nothing is sent for an hour.

The relay's address is `push_relay_url` in `settings`, else the environment
variable `KIT_PUSH_RELAY_URL`, else `https://pigeon.hedgerow.tools`. It must
be `https`, or `http` on the local network (a relay you run yourself).

### Sending

* One `POST /v1/push` per batch of 25 handles, with a new `event_id` (UUID)
  per call and the same `ref`. The text is stored first.
* **Handles nobody has confirmed go last, in small requests.** A handle that
  doesn't exist counts against the whole server at the relay (20 unknown
  handles an hour; more in one request and the relay refuses the whole
  request with `429`). So handles a push has already reached ("proven",
  `last_used_at` set) are sent first, 25 to a request, and the others
  ("unproven": a fresh install, or junk a token registered) after them, 5 to
  a request. While proven handles exist, nothing that goes wrong with the
  unproven ones (a `429`, a failed request) fails the delivery or touches the
  proven requests, which were sent first; the first failed unproven request
  ends the attempt for the rest. With no proven handle at all, the unproven
  ones count like any other. The counts of both are logged. To keep junk
  out, a user can have 4 devices no push has reached (20 on the whole
  server) and register 4 new handles an hour (`push_registrations` keeps
  the count).
* **Two `hedgerow` channels fire an alert once.** The kit can't reject a
  second channel when it is saved (a channel is a row in the product's table
  and the provider's check sees one config), so `hedgerow::send` shares one
  delivery per alert (kind, event, subject, time) between its callers and
  remembers a success for two minutes. A failure is shared by the callers
  that were waiting, and is not remembered. Whatever its state, an entry is
  dropped after five minutes (one whose callers were all cancelled never
  finishes).
* `gone` deletes the handle (`push_devices` rows with that hash); `accepted`
  sets `last_used_at`; `duplicate` and `unsupported_kind` need nothing;
  `limited`, `unavailable` and anything unknown are logged and given up.
* **A deadline.** The whole delivery, registration included, takes at most
  30 seconds, every request at most 10, and all requests go through the
  kit's guarded client (no proxy, forbidden addresses refused, replies read
  up to 64 KiB). A relay that is down, slow or suspended never holds up the
  alert's other channels, which `notify::send_all` runs side by side.
* **Errors show short codes.** What a person sees ("Send test") is `Push
  relay problem: {detail}` (`notif.hedgerow.err_relay`, 26 languages) with
  a stable code as the detail (`rate_limited`, `timeout`, `connect_failed`,
  `unknown_instance`, `http_400` …). A code from the relay is shown only if
  it is at most 64 characters of `a-z` and `_`. A `401 invalid_credentials`
  is logged as an error for the operator and not recovered by registering
  again.
* **The outcome** ([`push::relay::deliver`]): `Ok` when at least one handle
  was accepted, or when nothing needed delivering (no devices, or every
  handle `gone`, `duplicate` or unsupported); `Err` when nothing got through
  (relay unreachable, `429`, `503`, everything `limited` or `unavailable`,
  suspended). A product that retries on `Err` (Keen Kestrel's alert queue)
  therefore never repeats a push that reached someone; Vigilant Vole and Wise
  Wren don't retry.
* A "Send test" with no device registered says so instead of reporting
  success.

### Secrets

Handles are sealed (`crypto::seal`, purpose `push-handle`); `handle_hash`
(SHA-256) is what lookups use, and the same user registering the same handle
again gets the same device. The relay credential is sealed as described
above. The strings of the channel are in the catalog under
`notif.hedgerow.*` (26 languages).

### Upgrading from 0.6 to 0.7

A product that does nothing still builds, apart from one thing: `Transport`
has a new variant, `Relay`, and `PROVIDERS` a new entry. A product that
matches on `Transport` or lists the providers by hand (Vigilant Vole's
`notification::PROVIDERS` macro) handles it. To offer push a product:

1. runs `migrate::PUSH` (part of `migrate::CORE`; add it to a hand-written
   core set);
2. calls `push::install(&core)` at start-up;
3. mounts `push::api::router::<S>()` behind `require_auth` and adds
   `push::api::ROUTE_DOCS` to its API reference;
4. shows the `hedgerow` provider (it comes with `PROVIDERS`).

## Resources

### Translations

A language's catalog is two files:

* the kit's strings, compiled into the kit (`kit/locales/<code>.toml`): the
  sections `common`, `nav`, `js`, `fmt`, `count`, `login`, `setup`, `sec`,
  `users`, `tokens`, `auth`, `error`, `role` and `notif`, plus the keys the
  shared pages use (above) and `msg.sent_by`;
* the product's (`locales/<code>.toml`), handed over in `KitConfig::locales`.

`LangInfo::source` is the two joined. A key in both is an error at start-up
(with tests). A product may ship fewer languages than the kit; a language it
doesn't translate shows the kit's strings, and its own keys fall back to
English.

The kit's strings name no product and none of its concepts (a kit test
checks the English ones for monitors, dashboards, status pages, heartbeats
and the product's names; providers' own fields may use their service's
words).

#### Texts a product must supply

A few texts on the kit's pages depend on what the product is about. Their
keys are in `vole_kit::i18n::catalog::PRODUCT_KEYS`, and the product's
catalog defines them. `install` checks that its English catalog has every
one and refuses to start otherwise, as it does for a key in both catalogs.
Other languages fall back to English like any product key.

| Key | Where | Vigilant Vole (English) |
| --- | --- | --- |
| `login.subtitle` | sign-in page, under the title | Welcome back. Log in to see your monitors. |
| `setup.site_name_placeholder` | setup page, site name placeholder | Acme status… |
| `setup.tz_after` | setup page, after "Times are shown in *zone*" (with the full stop) | . You can change this in Settings. |
| `users.roles_help` | Users page, what each role may do | Viewers can look but not change anything. Editors manage monitors, … |
| `tokens.sub` | Tokens page subtitle | Let scripts, dashboards and Prometheus read or change your monitors. |
| `tokens.scope_read_sub` | Tokens page, what a read-only token reads | Monitors, status, metrics |
| `auth.all_set` | flash message after setup | You're all set. Add your first monitor to get started. |
| `auth.setup_token_required` | locked setup page: how to open it with `<PREFIX>_SETUP_TOKEN` | This instance needs a setup token. Open /setup?token=… with the value of VV_SETUP_TOKEN … |
| `error.back` | error pages, the link to `/` | Back to dashboard |

Vigilant Vole keeps these, unchanged in all 26 languages, at the end of its
locale files, together with its own words that the kit's catalog used to
hold but the kit never uses: `setup.title`, `common.recent_checks`,
`nav.dashboard`, `nav.status_pages`, `nav.status_short`, `nav.maintenance`,
`nav.agents`, `nav.browsers`, the `count.` plurals `monitors`, `checks`,
`status_pages`, `incidents`, `outages` and `agents`, and the notification
page's `notif.` texts about monitors (`sub`, `delete_msg`, `used_by`,
`add_new`, `add_new_help`, `add_existing`, `add_existing_help`,
`add_existing_help_new`, `empty_text`, `default_tip`, `foot`,
`no_monitors`). The keys kept their names, so the joined catalogs and the
i18n snapshots are the same as before.

#### Coverage

`vole_kit::i18n::catalog::coverage(lang)` measures one language against
English, over the joined catalog: `total` strings it needs (plural keys once
per CLDR form), `done`, and `problems`, one line each (missing keys, keys
English lacks, plural forms the language doesn't use, empty values,
placeholders other than English's). `assert_full_coverage()` panics with
every gap unless each language the product ships (`KitConfig::locales`) is
complete. A product's test suite calls it after installing its
configuration:

```rust
#[test]
fn every_language_is_complete() {
    my_product::install();
    vole_kit::i18n::catalog::assert_full_coverage();
}
```

Vigilant Vole's `coverage_report` and kit-demo's
`every_language_the_demo_ships_is_complete` use it.

### Static files

The kit embeds the design system and serves it first at `/static/…`:
`style.css`, `app.js`, `theme.js`, `icons.svg`, `htmx.min.js`, `sse.js`,
`passkeys.js` and `fonts/` (`assets::KIT_FILES`). Vigilant Vole embeds only its own files
(icons, favicon, manifest).

The files stay in the repository's `static/` folder, because the golden test
`static_assets` enumerates that folder. The kit embeds them from
`../static/`, which a git dependency checks out along with the kit. A test
checks that every file in `static/` belongs to exactly one of the two.

## Migrations

The kit has its own migration set, `vole_kit::db::migrate::CORE`:

* `CORE_TABLES`, `kit/migrations/0001_core.sql`, creates `users`,
  `api_tokens` (with its index) and `settings`. The statements are copied
  exactly as they stand in Vigilant Vole's `sqlite_master`. A Vigilant Vole
  test checks that its core set builds the same objects, columns and foreign
  keys as Vigilant Vole's migrations.
* `TOKEN_SCOPES`, `kit/migrations/0002_token_scopes.sql` (0.2), creates
  `api_token_scopes` (see "API token scopes"). A product that declares no
  token scopes may leave it out; Vigilant Vole does
  (`core: &[CORE_TABLES, PASSKEYS]`).
  Adding it later just runs it; leaving it out after it ran makes the
  database look newer (refused), as for any migration.
* `PASSKEYS`, `kit/migrations/0003_passkeys.sql` (0.4), creates `passkeys`
  and `passkey_challenges` (see "Passkeys"). Every product runs it;
  Vigilant Vole's set is `&[CORE_TABLES, PASSKEYS]`, and since its adoption
  migration (0104) records only `kit/0001_core.sql`, this one runs after it
  on databases that are still adopting and first on the others.
* `SEALED_SECRETS`, `kit/migrations/0004_sealed_secrets.sql` (0.5), changes
  no table. It records that notification channel secrets are sealed (see
  "Sealed secrets"): an upgrade takes its pre-migration copy (secrets still
  in plaintext) before the product seals them, and older versions, which
  would send the sealed text to the provider, refuse the database. Vigilant
  Vole's set is `&[CORE_TABLES, PASSKEYS, SEALED_SECRETS]`.
* `SEALED_TOTP`, `kit/migrations/0005_sealed_totp.sql`, changes no table.
  It records that two-factor secrets are sealed (see "Two-factor secrets"),
  with the same effect as the one above: the upgrade's copy holds them in
  plaintext, and older kits refuse the database. Every product on
  `migrate::CORE` has it; Vigilant Vole's set is `&[CORE_TABLES, PASSKEYS,
  SEALED_SECRETS, SEALED_TOTP]`.
* `PUSH`, `kit/migrations/0006_push.sql` (0.7), creates `push_devices` and
  `push_events` (see "Push (0.7)"). Every product on `migrate::CORE` has it;
  a product that lists its core set by hand adds it to offer the `hedgerow`
  channel. Leaving it out after it ran makes the database look newer
  (refused), as for any migration.
* `INVITES`, `kit/migrations/0007_invites.sql`, creates `invites` (see
  "Invitation links"). Not part of `migrate::CORE`: a product that turns
  `KitFeatures::invites` on adds it to its core set (`config::check` demands
  it); the others keep their schema.

* `SIGN_IN_CODES`, `kit/migrations/0008_sign_in_codes.sql`, creates
  `sign_in_codes` and `user_emails` (see "One-time sign-in codes"). Not part
  of `migrate::CORE`: a product that turns `KitFeatures::sign_in_codes` on
  adds it to its core set (`config::check` demands it); the others keep
  their schema. The `users` table is not changed.

Core migrations are recorded in `schema_migrations` under `kit/…` names, so
they can't clash with a product's file names.

`MigrationSet` has two new fields:

* `core: &'static [(name, sql)]`, normally `CORE`;
* `adopt_core: Option<&'static str>`, for a product whose own, older
  migrations already created the core tables.

The runner's order:

* **A product without `adopt_core`** (Keen Kestrel, kit-demo): pending core
  migrations first, then the product's own.
* **A product with `adopt_core`** (Vigilant Vole, `0104_adopt_kit_core.sql`):
  while that migration is pending, the product's migrations run first. The
  adoption migration records the core migrations as applied
  (`INSERT OR IGNORE INTO schema_migrations …`), and any it didn't record run
  after it. Once adopted, new core migrations run first, as for any product.

This gives an identical schema for a fresh database and for every upgrade
fixture (`schema_fresh` and the per-fixture hashes are unchanged). Only the
migration lists changed: `schema_migrations_fresh` gained the two rows, and
`schema_upgrade_matrix` shows `0104_adopt_kit_core.sql` applied. That makes
upgrades from 0.5.x and 0.6.x take a pre-migration copy where they took none
before. No shipped migration was edited (the runner keeps names, not
checksums, but nothing needed editing).

The newer-schema guard counts names it doesn't know; how names sort doesn't
matter. `kit/0001_core.sql` sorts after `0103_…` and is known, so it can't
make a database look newer. A newer core or product migration is still
refused, and the message names the migration recorded last. The backup
restore check uses the same list (`all_migration_names()`). Pre-migration
copies of a core migration get a flat file name (`…-before-kit-0002_x-….db`).

A database migrated by this release can't be opened by Vigilant Vole 0.6.x or
earlier: they don't know the two new rows and refuse to start, as designed.
See [upgrading.md](upgrading.md#downgrades).

## Releases

release-please manages two components with separate release PRs
(`release-please-config.json`, `.release-please-manifest.json`):

| Path | Package | Tags | Changelog | Bumps |
| --- | --- | --- | --- | --- |
| `.` | vigilant-vole | `vX.Y.Z` (as before) | `CHANGELOG.md` | `Cargo.toml`, its `Cargo.lock` entry |
| `kit` | vole-kit | `vole-kit-vX.Y.Z` | `kit/CHANGELOG.md` | `kit/Cargo.toml`, `kit/kit-build/Cargo.toml`, both `Cargo.lock` entries |

Each release PR updates the root `Cargo.lock`, so `--locked` builds keep
working.

* **Vigilant Vole is a `simple` package with TOML extra files.** The `rust`
  strategy on a workspace root would set every member's version (vole-kit
  included) to Vigilant Vole's. Commits that touch only `examples/` don't
  count towards Vigilant Vole (`exclude-paths`).
* **Kit commits count towards both.** Vigilant Vole embeds the kit, so a fix
  in `kit/` (say in auth) must reach Vigilant Vole's users too: a kit-only
  `fix` opens two release PRs, one per component, each bumping only its own
  files.
* **The cargo-workspace plugin isn't used.** It would bump vigilant-vole for
  every kit release, including ones that change nothing Vigilant Vole
  ships. Vigilant Vole depends on the kit by path without a version.
* **The kit starts at 0.0.0** in the manifest and `Cargo.toml`, so its first
  release is 0.1.0. That needs at least one user-facing commit (`feat`,
  `fix`) touching `kit/`.

A kit release must never be the repository's "Latest" release. These read
`/releases/latest`:

* `deploy/install.sh`
* `docs/install-linux.md`
* the site
* the Proxmox scripts (`fetch_and_deploy_gh_release … "latest"`)
* the release notes written by `deploy/package-release.sh`

release-please can't set `make_latest`, so:

1. The kit's release is created as a draft (`"draft": true`). Drafts are never
   Latest. `force-tag-creation` pushes the tag at once.
2. The `vole-kit` job in `.github/workflows/release-please.yml` publishes it
   with `make_latest=false`. It then checks that Latest is still a `vX.Y.Z`
   release, and fails loudly otherwise.

Docker images and binaries are built only for Vigilant Vole's release: the
`docker` and `binaries` jobs depend on the root package's outputs
(`release_created`, `tag_name`; the kit's are `kit--…`). The tag triggers of
`docker-publish.yml` and `release-binaries.yml` match only `vX.Y.Z` (a
`v*.*.*` glob matched `vole-kit-v0.1.0`).

`tools/release-dry-run/` runs release-please's own code on a local checkout
and prints the release PRs it would open. It was used to check this setup:

* merging this branch gives "release 0.7.0" (Vigilant Vole) and "release
  vole-kit 0.1.0", each bumping only its own files;
* a later kit-only fix proposes vole-kit 0.1.1 and Vigilant Vole 0.7.1;
* a Vigilant Vole fix proposes only 0.7.1;
* a kit-demo change proposes nothing.

## Building blocks for products (0.2)

What Keen Kestrel had to build itself on 0.1, now in the kit. Vigilant Vole
uses what fits without changing anything it sends; kit-demo uses and tests
each one (`examples/kit-demo/tests/demo.rs`).

### A second database

A product may keep more than one SQLite file (Keen Kestrel's
`store.sqlite`: raw payloads, disposable). The kit's pool and runner work
for any of them:

```rust
pub static STORE: MigrationSet = MigrationSet {
    core: &[],              // the kit's core tables belong in the main database
    adopt_core: None,
    migrations: store_migrations,
    fk_off_marker: "-- kk:foreign_keys=off",
    baseline: None,
    after: no_fixups,
};

let pool = db::build_pool_with(&dir.join("store.sqlite"), PoolOptions {
    max_size: Some(4),
    pragmas: "",                                              // every connection
    new_database_pragmas: "PRAGMA auto_vacuum = INCREMENTAL;", // before the first table
});
db::migrate_with(&pool, &STORE, MigrateOptions::default()).await?;
```

* `PoolOptions::new_database_pragmas` runs on every new connection while the
  file has no schema, so whichever connection creates the first table has
  it. `db::apply_new_database_pragmas(conn, sql)` does the same on a
  connection of your own.
* `MigrateOptions { backup: Some(dir), backup_prefix: Some("kestrel-store-before-") }`
  takes a copy before migrating an existing file, as `migrate` does for the
  main database; `backup: None` takes none.
* A file from a newer release is refused as always. Without a backup
  directory the message just says to upgrade again; it doesn't point at a
  backup folder (`MigrationError::NewerSchema::backups` is an `Option`).
* `migrate(pool, dir)` is `migrate_with(pool, <the product's set>,
  MigrateOptions { backup: Some(dir), .. })`, unchanged.

### CSRF exemptions

`KitConfig::csrf_exempt_prefixes` match whole path segments, and a `*`
segment matches any one non-empty segment (`csrf::prefix_matches`):
`/api/*/envelope` exempts `/api/7/envelope`, `/api/7/envelope/` and
`/api/7/envelope/x`, but not `/api//envelope`, `/api/7/8/envelope` or
`/api/7/envelopes`. A prefix ending in `/` needs the slash. Prefixes
without `*` behave as before. `csrf_exempt_path` stays for rules a pattern
can't say (a numeric id only, a closed list of endings).

### Request budgets

`http::ratelimit::TokenBuckets<K>` keeps a token bucket per key (an IP
address, a key id) in memory: a bucket holds `Rate::burst` requests and
refills continuously over `Rate::per`. The rate is passed with every call,
so it can differ per key and change at once. Full buckets are dropped when
there are more than `DEFAULT_MAX_KEYS` keys (`with_max_keys` for another
limit).

```rust
static BY_IP: LazyLock<TokenBuckets<IpAddr>> = LazyLock::new(TokenBuckets::new);

if let Err(wait) = BY_IP.take(&ip, Rate::per_minute(600), Instant::now()) {
    return ratelimit::too_many_requests(wait);   // 429, Retry-After, REST envelope
}
```

`take_n` takes several tokens at once (a batch). `retry_after_secs(wait)`
rounds up to whole seconds (at least 1), `retry_after(wait)` is the header
for a response of your own (Keen Kestrel's Sentry-style ingest errors).
`too_many_requests` answers with the REST error code `rate_limited`; a
product that documents it adds `docs::RATE_LIMITED` to its error codes. The
sign-in backoff (`auth::rate_limit`) is unchanged.

### Header policies

`http::headers::security_headers` gives app pages the app CSP and
`X-Frame-Options: DENY`. A response marked with a `HeaderPolicy` gets
headers that fit it instead:

| Policy | CSP | `X-Frame-Options` | Also |
| --- | --- | --- | --- |
| `App` (default) | `APP_CSP` | `DENY` | |
| `Embeddable` (as before: `embeddable_prefixes`, the `Embeddable` extension) | `PUBLIC_CSP` | none | |
| `PublicAsset` (a script or image other sites load) | none | none | `Cross-Origin-Resource-Policy: cross-origin` |
| `Machine` (ingest, webhooks, JSON for programs) | `MACHINE_CSP` (`default-src 'none'; frame-ancestors 'none'`) | none | |

Every policy keeps `X-Content-Type-Options: nosniff` and
`Referrer-Policy: same-origin`, and a CSP or CORP header the handler set is
kept. Mark a whole router or one response:

```rust
Router::new()
    .route("/ingest/v1/event", post(event))
    .route_layer(axum::middleware::map_response(headers::machine));   // also public_asset, embeddable

(HeaderPolicy::PublicAsset, [(header::CONTENT_TYPE, "text/javascript")], SNIPPET)
```

Vigilant Vole marks nothing, so its headers are the same.

### Live updates

`http::live` has the parts of a live view over server-sent events (htmx's
`sse-connect`):

* `Changes<T>`: a stream's view of a `broadcast` channel. `next()` waits for
  a change `wanted` accepts (a lagged receiver counts as one), then for the
  quiet period, drains what arrived meanwhile and returns when the first
  change came. `Changes::all(rx)` takes every change; `.quiet(d)` sets the
  quiet period (500 ms by default), `.set_quiet(d)` changes it between
  updates, `.lifetime(d)` ends the stream so the browser reconnects and
  authenticates again.
* `SharedRenders<K>`: streams that wake for the same change and show the
  same view (`K`: query, rights, language …) share one render; a render that
  started before the change is never reused.
* `StreamLimit` (in a `static`): a cap on open streams; `acquire()` gives a
  `StreamSlot` to keep in the stream's state, `busy()` the `503` with
  `Retry-After: 30`.
* `sse(stream)`: the response, with a keep-alive comment every 15 s;
  `html_event(name, html)` an event for `sse-swap`.

Vigilant Vole's `/events` (dashboard) is built from them with the same
coalescing, adaptive quiet period, shared renders, stream cap, lifetime and
keep-alive as before. For Keen Kestrel's issue list:

```rust
let changes = Changes::new(ingest.writer().subscribe(), move |p: &i64| *p == project_id)
    .quiet(Duration::from_secs(1));
let lang = i18n::current();      // the stream runs outside the request's scope
live::sse(stream::unfold((changes, live), move |(mut changes, mut live)| async move {
    loop {
        changes.next().await?;
        if let Ok(html) = i18n::scope(lang, render(&live)).await {
            return Some((live::html_event("issues-updated", &html), (changes, live)));
        }
    }
}))
```

### Formats

* `fmt_bytes(n)` / `fmt_bytes_in(lang, n)`: sizes in steps of 1024,
  "512 B", "840 KB", "1.6 MB", "12.0 GB"; bytes and kilobytes whole,
  bigger units with one decimal. The units are the language's
  (`fmt.bytes` … `fmt.terabytes`: "Ko"/"Mo" in French, "КБ"/"МБ" in Russian
  and Ukrainian, Arabic and Persian abbreviations).
* `fmt_pct0(p)`: a whole-number percentage in the language's style
  ("42%", "42 %", "%42", "۴۲٪"). It never reads a misleading 0 or 100: above
  0 shows at least 1, below 100 at most 99.
* `fmt_last_days(n)`: "Last {n} days" in the language's plural form for `n`
  (`count.last_days`): "Last 1 day", "Последний 21 день",
  "Ultimele 30 de zile", "آخر 7 أيام".
* Templates: `{{ filesize(bytes) }}`, `{{ pct0(value) }}`,
  `{{ last_days(7) }}` (`Tr` helpers).

All 26 languages have the new strings, with tests per language
(`i18n::format` tests).

### Design system components

The bar list, line charts with several lines, and code with highlighted
lines are part of the design system (`static/style.css`), with macros and
view-models (`components::BarRow`, `LineChart`, `CodeLine`). See
[design-system.md](design-system.md#charts-svg) for the markup.

### Notification channels

The provider form is the kit's, so every product renders it the same way:

* `notify::form`: `field_views(schema, values, stored, errors, fresh)`
  (saved secrets render empty with the mask as placeholder),
  `provider_groups(query)`, `FieldView`, `OptionView`, `ProviderGroup`,
  `ProviderCard`.
* Templates: `kit/partials/notification_picker.html` (needs `query`,
  `groups`; includes the grid), `notification_provider_grid.html`,
  `notification_name.html` (needs `form.name`, `form.name_error`,
  `form.label`), `notification_fields.html` (needs `form.fields`). They load
  `/settings/notifications/providers` (`?grid=true&q=`) and
  `/settings/notifications/form?kind=` into `#channel-sheet-body`; the
  product serves those routes, its channel list and its own toggles.
* Vigilant Vole renders its picker, grid, name and fields from these; its
  notification pages are byte-identical to before (compared page by page
  for every provider, the list, edit pages and error states, in four
  languages).

The webhook provider's words are the product's (`KitConfig::webhook`): its
default body sends the subject as `"<subject>"`, `"<subject>_id"`,
`"<subject>_type"` and `"<duration>_seconds"`; its body template knows
`{{<subject>_name}}`, `{{<subject>_id}}`, `{{<subject>_type}}`,
`{{<duration>}}` and `{{<duration>_seconds}}` (`webhook::placeholders()`);
its help text says the same; its URL field's example is `url_example`
(marked `{webhook_url}` in the schema, like `{product}`). Vigilant Vole's
wording (`monitor`, `downtime`) gives exactly its old requests and form.

"Send test" without wrapping `send_config`:

```rust
notify::send_in(&core.http, kind, &config, lang, || {
    Alert::test(title, body, event_label, link_label, url)
})
.await
```

`send_in` builds the alert inside `lang`'s scope, so its texts and the
provider's own words (email footer, field labels) are in the channel's
language. `send_config_in(http, kind, config, alert, lang)` sends an alert
that is already built; `send_config` uses the request's or the site's
language as before. `NotificationChannel::is_default` stays (Vigilant
Vole's "attach to new monitors"); the kit never reads it, a product without
such a flag leaves it `false`, and JSON without it reads as `false`.

### API reference

* Path parameter types: `DocsSpec::param_types` and `RouteDoc::params`
  (see "Configuration").
* Credentials in a header of their own: `Credential::header`.
* Request bodies that aren't JSON: `RouteDoc::body_type`.
* Access words: `DocsSpec::strict_access`, `docs::undeclared_access`.

Vigilant Vole's reference (the page, `/api-docs.md`, the OpenAPI document)
is unchanged.

## Using vole-kit in another product (Keen Kestrel)

```toml
# Cargo.toml
[dependencies]
vole-kit = { git = "https://github.com/Ferdinand99/Vigilant-Vole", tag = "vole-kit-v0.2.0" }

[build-dependencies]
vole-kit-build = { git = "https://github.com/Ferdinand99/Vigilant-Vole", tag = "vole-kit-v0.2.0" }
```

```toml
# .cargo/config.toml (not committed): work against a local checkout
[patch."https://github.com/Ferdinand99/Vigilant-Vole"]
vole-kit = { path = "../Vigilant-Vole/kit" }
vole-kit-build = { path = "../Vigilant-Vole/kit/kit-build" }
```

```rust
// build.rs
fn main() {
    vole_kit_build::templates();
}
```

Then:

1. Write a `static KitConfig`:
   * names: `Keen Kestrel`, `keen-kestrel`, `kk_session`, `kk_`,
     `KeenKestrel/<version>`, `version: env!("CARGO_PKG_VERSION")`, …;
   * a `MigrationSet` with `core: migrate::CORE` and `adopt_core: None`;
   * a `DocsSpec` (with only the optional parts the product has), the
     product's locales and a `Ui`;
   * `public_url_setting`, if the product has a place to set its public URL;
   * `token_scopes` and `general`, if it has its own token scopes or wants
     the kit's General page.

   The English locale file defines the texts in "Texts a product must
   supply". Call `vole_kit::config::install(&KIT)` first thing in `main` (and
   in test set-up), and call `vole_kit::i18n::catalog::assert_full_coverage()`
   from a test.
2. Build `Core` with `Core::new` (or implement `FromRef<AppState> for Core`
   and `Key`, keeping one `SignInLimits` in the state), run
   `vole_kit::db::migrate`, and merge `auth::pages::public_router()` and,
   behind `guard::require_auth`, `auth::pages::router()`.
3. Add `csrf_guard`, `i18n::layer`, `http::error::negotiate` and
   `http::headers::security_headers` as layers, serve `/static/{*path}` with
   `http::assets::static_asset`, and use `http::error::not_found` as the
   fallback.
4. Extend `kit/layout.html` for the product's own pages. Pass a `PageCtx`
   whose `nav` matches one of `Ui::nav`.
5. Send alerts by building `Alert`s and calling `vole_kit::notify::send`;
   answer MCP with a `vole_kit::mcp::Handler`.

`examples/kit-demo` does all of this in about 300 lines. Its tests are the
proof:

* setup creates the first admin;
* sign-in works with a password and then a TOTP code;
* every shared page says "Kit Demo" and never "Vigilant Vole" or `vv_`;
* no shared page, error page, API reference (`/api-docs`, `/api-docs.md`,
  `/llms.txt`, the OpenAPI document) or MCP refusal says "monitor",
  "dashboard", "Vigilant", "Vole" or other Vigilant Vole words: the texts a
  product supplies are the demo's own (in English and Norwegian), its error
  table has only the kit's generic codes, and its OpenAPI document declares
  no `agentToken`;
* both of its languages are complete (`assert_full_coverage`);
* its two token scopes: `notes:add` (implies read) is offered and shown on
  the Tokens page, and `has_scope` plus `may_act_as(Role::Editor)` decide who
  may add notes, for every kind of token; `inbox` (no read) works under
  `/api/v1/inbox` and is refused everywhere else, MCP included; a token whose
  scope the demo no longer declares stops working;
* the kit's General page with the demo's section (a number and a switch):
  shown, refused with 422 and the field errors, saved with the core settings,
  the hook run, the next page in the new site language, admins only;
  `set_site_language` without the page;
* sign-in throttling is per instance: one test server throttling doesn't
  hold up another;
* `/static/style.css` is served;
* a second database (`kit-demo-store.db`) has only its own migrations, no
  backup and incremental auto-vacuum, and a newer one is refused without a
  backup hint;
* its ingest endpoint (`POST /api/{project}/envelope/`) needs no CSRF proof
  (the `/api/*/envelope` pattern), answers with the machine headers, takes
  its key in `X-Demo-Key` and allows five envelopes a minute per address
  (`429`, `Retry-After`, `rate_limited`); `/js/d.js` is a public asset;
* a project's live view merges a burst of envelopes into one update;
* the provider picker and forms are the kit's partials, its webhook
  speaks of a `note` and "Send test" arrives in the language asked for;
* its API reference types `{project}` as a string, documents the
  `X-Demo-Key` header scheme and the NDJSON body, and has no undeclared
  access word (`strict_access`);
* its home page draws a bar list, a two-line chart and highlighted code;
* the OpenAPI and MCP versions are the demo's (0.3.7);
* a fresh database has the core tables (with `api_token_scopes`,
  `passkeys` and `passkey_challenges`) plus `notes`, with
  `kit/0001_core.sql`, `kit/0002_token_scopes.sql`,
  `kit/0003_passkeys.sql`, `kit/0004_sealed_secrets.sql` and
  `kit/0005_sealed_totp.sql` recorded first;
* passkeys: without a public URL only the note; with one, adding a passkey
  needs the password, the software authenticator's credential is stored
  (transports filtered, the user handle kept for the next one), a replayed
  registration is refused, a sign-in from another origin is refused and
  uses up its challenge, the real sign-in skips password and TOTP and
  lands on `next`, a replayed one is refused, rename and delete work, a
  deleted passkey signs nobody in, and an admin's reset removes a user's
  passkeys and signs them out.

The demo depends on the kit by path; the template mechanism is the one a git
dependency uses.

### Upgrading from 0.1 to 0.2 (what Keen Kestrel changes)

0.2 is a breaking release (pre-1.0 minor):

1. **`KitConfig` has three new fields.** Add `token_scopes` and `general`
   (`&[]` and `None` keep 0.1's behaviour), and `webhook`, how the webhook
   provider names what alerts are about: `webhook: Wording { subject: "issue", duration: "duration",
   url_example: "https://example.com/hooks/kestrel…" }` (or the words it
   prefers). Its alert texts are unchanged; webhook receivers now get
   `"issue"`, `"issue_id"`, `"issue_type"` and `"duration_seconds"`.
2. **`Core` has a new field, `limits`.** Build it with
   `Core::new(pool, key, http, &KIT)` instead of a struct literal (Kestrel's
   `start_with`).
3. **`CurrentUser` has two new fields**, `account_role` and `scopes`. Code
   that builds one by hand (tests) sets them (`account_role: role`,
   `scopes: Scopes::All` for a session).
4. **Token scopes** replace Kestrel's `token_grants`:
   * declare `ExtraScope { name: "difs:write", label, help, implies_read:
     true, paths: &[] }` (two new product strings in 26 languages);
   * keep `core: migrate::CORE` (it now includes `kit/0002_token_scopes.sql`,
     which runs before Kestrel's own pending migrations) and add a migration
     that moves the grants and drops the table:
     ```sql
     INSERT OR IGNORE INTO api_token_scopes (token_id, scope)
         SELECT token_id, scope FROM token_grants;
     DROP TABLE token_grants;
     ```
   * `difs::upload::may_upload` becomes, for tokens,
     `user.has_scope("difs:write") && user.may_act_as(Role::Editor)` (a
     read-and-write token of an editor still passes: it has every scope). The
     re-read of the owner (`users::get_user`) and `db::difs::{token_has,
     grant}` go away; the Debug files page makes its tokens with
     `api_tokens::insert` + `add_scope` and lists them with `with_scope`
     (or `fill_scopes`). The kit's API tokens page shows them with the
     scope's label instead of "Read only", and offers the scope too.
5. **General page** (optional): set `general: Some(&GENERAL)` with Kestrel's
   fields (or `&GeneralSettings::CORE_ONLY`), add `SettingsTab::GENERAL`,
   define `general.sub` and `general.base_url_help` in 26 languages and set
   `public_url_setting: Some("Settings → General")`. `setup.tz_after` can then
   say ". You can change this in Settings." `KK_PUBLIC_URL` stops being
   mirrored into `base_url` on every start (which also cleared it when
   unset); if it stays, store it only when set, so the page's value isn't
   wiped. Without the page, `settings::set_site_language` sets the language
   notification channels use when theirs is "" (`i18n::site_language`).
6. **Keys now in the kit's catalog** (see "General settings"): the product's
   catalog must not define `general.site`, `general.site_name_help`,
   `general.base_url`, `general.region`, `general.language_help`,
   `general.save`, `general.saved`, `general.err_timezone`,
   `general.err_url*` or `mw.timezone` (Kestrel defines none of them).
7. **Tests**: the sign-in throttle is per `Core`, so separate test servers no
   longer need to wait for each other.

From the building blocks (see "Building blocks for products"), each change
removes a workaround:

8. `DocsSpec`: add `param_types: &[("event_id", "string"), ("org", "string"),
   ("version", "string")]` and `strict_access: true`. `Credential` gets
   `header`: `dsn key` becomes `header: Some(HeaderScheme { name: "sentryAuth",
   header: "X-Sentry-Auth", description: "…", example: "Sentry sentry_key=$KK_DSN_KEY, sentry_version=7" })`,
   `api token` `header: None`. Mark the envelope route
   `.body_type("application/x-sentry-envelope")` and the k.js event route
   `.body_type("text/plain")` with their examples.
9. `store.rs`: replace `prepare` and the hand-run `apply_migrations_with`
   with `build_pool_with(…, PoolOptions { new_database_pragmas:
   "PRAGMA auto_vacuum = INCREMENTAL;", .. })` and `migrate_with(&pool,
   &MIGRATIONS, MigrateOptions::default())`.
10. `product.rs`: `csrf_exempt_prefixes: &["/api/v1", "/api/0", "/mcp",
    "/api/*/envelope", "/api/*/store", "/ingest/v1/event",
    "/ingest/v1/signals"]` and `csrf_exempt_path: no_exempt_path`, or keep
    `ingest::is_ingest_path` for its digits-only rule.
11. `ingest/limit.rs`: delete it; use `TokenBuckets<i64>` and
    `TokenBuckets<IpAddr>` with `Rate::per_minute(key.rate_limit_per_minute)`
    and `ratelimit::retry_after_secs`. The ingest errors keep their own body
    and `X-Sentry-Rate-Limits`.
12. Mark `/js/k.js` with `headers::public_asset` and the ingest routes with
    `headers::machine` (`map_response`).
13. `issues/live.rs`: use `Changes::new(…).quiet(Duration::from_secs(1))`
    and `live::sse`; `next_change` goes away.
14. `alerts/channels.rs`: delete `FieldView`, `OptionView`, `ProviderCard`,
    `ProviderGroup`, `matches_query`, `provider_groups`, `input_type`,
    `display_value`, `blank` and `field_views`; import them from
    `vole_kit::notify::form` (`OptionView` has `String` fields, as
    Kestrel's). Delete `templates/partials/notification_fields.html`,
    `notification_name.html`, `notification_picker.html` and
    `notification_provider_grid.html` and include the kit's
    (`kit/partials/…`); render the picker and grid fragments from
    `kit/partials/…` paths. Its `channels.kv_*` strings go: the field
    partial uses the kit's `form.value`, `form.header_name`,
    `form.header_value`, `form.remove_header` and `form.add_header`.
15. `Alerts::send_test`: `notify::send_in(&http, &kind, &config, lang, ||
    message::test(base.as_deref()))` instead of the scope around
    `send_config`; `message::test` can use `Alert::test`.
16. `static/analytics.css`: delete the bar list rules (`.bars-head`, `.bars`,
    `.bar-row`, `.bar-row-2`, `.bar-label`, `.bar-num`); the markup stays.
    `m::bar_list` and `components::BarRow` can replace the hand-written
    lists, `m::line_chart` and `components::LineChart` its chart macro, the
    `series-N` classes its inline `--chart` colours, and `m::code_lines` the
    `<strong>` current line in stack frames.
17. Formats: `i18n::fmt_pct0` instead of `fmt_pct2(…).replace(".00", "")`,
    `fmt_bytes` instead of `difs::pages::size_label`, `fmt_last_days(n)`
    instead of `web.period_last_days` (delete that key in all 26 files).
18. Kestrel's locale files must not define the kit's new keys
    (`fmt.bytes` … `fmt.terabytes`, `count.last_days.*`, `form.value`,
    `form.header_name`, `form.header_value`, `form.remove_header`,
    `form.add_header`); `install` refuses a key in both catalogs.

### Upgrading from 0.2 to 0.3

0.3 adds the version footer on signed-in pages. One change:

1. **`KitConfig` has a new field, `releases_url`.** `None` shows the
   version as plain text; `Some("…/releases/tag/v{version}")` links it to
   the release notes (the product decides the tag pattern; `{version}` is
   `KitConfig::version`). Keen Kestrel:
   `Some("https://github.com/Ferdinand99/Keen-Kestrel/releases/tag/v{version}")`;
   Wise Wren:
   `Some("https://github.com/Ferdinand99/Wise-Wren/releases/tag/v{version}")`.

Nothing else: no new strings, and the footer's style is in the kit's
`style.css`. Snapshot tests of signed-in pages gain the footer's lines.

### Upgrading from 0.3 to 0.4

0.4 adds passkeys. A product whose `MigrationSet::core` is
`migrate::CORE` (Keen Kestrel, Wise Wren) changes nothing: the new core
migration `kit/0003_passkeys.sql` runs at the next start, before the
product's own pending migrations, and the pages, routes, strings (26
languages) and `static/passkeys.js` come with the kit. A product that lists
its core set by hand adds `migrate::PASSKEYS` (`install` refuses to start
otherwise); Vigilant Vole: `core: &[CORE_TABLES, PASSKEYS]`.

What else a product may notice:

* Snapshot tests change where they cover the Security page (the Passkeys
  section and its note) and the Users page when a user has passkeys; the
  sign-in page only changes when a public URL is set (HTTPS, or
  `http://localhost`).
* Schema snapshots and migration lists gain `passkeys`,
  `passkey_challenges`, `idx_passkeys_user` and `kit/0003_passkeys.sql`.
* A static-file list gains `/static/passkeys.js`.
* The product's catalog must not define the kit's new keys
  (`login.passkey`, `login.passkey_failed`, `sec.pk_*`, `users.reset_pk`,
  `users.reset_pk_q`, `users.reset_pk_msg`, `users.pk_reset`).
* Passkeys need the public URL. A product that sets it from its
  environment (`KK_PUBLIC_URL`) should make sure it is the URL people
  really open, since the RP ID and origin come from it.

### Upgrading from 0.4 to 0.5

0.5 seals notification channel secrets at rest (see "Sealed secrets").

A product that does nothing still builds and works: its channels stay in
plaintext, and nothing it stores looks sealed. A product whose
`MigrationSet::core` is `migrate::CORE` records `kit/0004_sealed_secrets.sql`
at the next start (no table changes). To get the sealing, a product with
channels changes four things:

1. **Startup**, after the migrations:
   `db::interact(&pool, move |c| notify::sealed::seal_stored(c, &key))`
   (with the key from `auth::load_or_create_key`). A product that lists its
   core set by hand adds `migrate::SEALED_SECRETS`, so older versions
   refuse the sealed database and an upgrade takes its plaintext copy
   first.
2. **Writes**: store `sealed::seal_config(&core.session_key, kind, &config)`
   instead of `config`.
3. **Reads to use**:
   * before `notify::send`, `sealed::open_channel(&key, &channel)?`;
   * before `send_in`/`send_config` with a stored config,
     `sealed::open_config(..).config`;
   * if `!opened.is_readable()`, report `sealed::unreadable_message()`
     and don't send.
4. **The edit form**:
   * open the stored config first, and pass `opened.config` as `stored` to
     `merge_secrets_checked` and `field_views`;
   * pass `opened.errors()` as the field errors of the edit page;
   * add `opened.reentry_errors(&parsed.config, &parsed.clear)` to the
     errors of a save or a test.

What else a product may notice:

* Snapshot tests that dump `notification_channels.config` see
  `sealed:v1:…` values that differ on every run (random nonces). Compare
  opened configs instead.
* Migration lists gain `kit/0004_sealed_secrets.sql`. Schemas don't change.
* The catalog gains `notif.err_secret_unreadable` (26 languages). The
  product's catalog must not define it.
* A database copied without its `session.key` loses the channel secrets.
  A backup a product writes should hold `session.key` too, or the secrets
  opened (`open_stored` on the copy), and say so.

Keen Kestrel and Wise Wren: both keep channels in `notification_channels`
and send through `notify::send`/`send_in`, so the steps above are all.
Wise Wren seals its own registry passwords and its Vigilant Vole token
(`registries::seal_for`/`open_for`: AES-256-GCM, key SHA-256 of a product
label and the master key, raw `nonce ‖ ciphertext ‖ tag` bytes). It can
move to `crypto::seal`/`open` later, with its own purpose strings. Because
the key derivation and the format differ, that move has to open each
value with the old code and seal it again (a one-time startup step like
`seal_stored`). The old values can't simply be read with the new code.

## Not done (proposals)

* **The design system's DOM names.** `vv-i18n`, `vv-progress`, `vv-theme`
  and the `vv:` events are shared by the layouts and `app.js`/`theme.js`. A
  neutral prefix (`vk-`) means changing those scripts, the layouts and
  Vigilant Vole's templates that listen for `vv:sheet-open` and
  `vv:reorder`. That changes the golden static asset hashes.
* **Old key names in the kit's catalog.** Some kit keys keep their old
  section names (`detail.expires`, `dashboard.response`,
  `monitor_form.edit_heading` …), though their texts are neutral. Renaming
  them changes the i18n key snapshot. `setup.title` ("Set up Vigilant Vole")
  is now Vigilant Vole's and still used nowhere; removing it is the same kind
  of snapshot change.
* **Example placeholders** like `vigilant-alerts…` (ntfy topic) or
  `vigilant…` (Apprise, WhatsApp) stay. They don't name the product.
* **Vigilant Vole's `api token` access word** (`POST /mcp`) is documented as
  "public (no token)". Declaring it as a `Credential` (and
  `strict_access: true`) would fix the reference, and change it.
* **A kit notification router.** Products serve the provider form's routes
  themselves; the channel list, toggles and storage differ between them.
* **`src/mcp_bridge.rs`** (the stdio bridge) is generic and could join
  `vole_kit::mcp`.
* **Server-side sessions** (VK-06). Sessions are signed cookies; logout
  only deletes the browser's copy, and there is no list of devices.
  Bumping the version signs out all devices, which the security changes
  above now do.
* **Re-authentication by passkey or TOTP** (VK-10, rest). The confirmation
  is the password only; every account has one. Renaming a passkey and the
  admin's reset of a user's passkeys ask for nothing.
* **Sealed secrets** (VK-11, rest): binding the AAD to the row and field,
  a key id and rotation. The TOTP secrets are sealed (VK-12), and the
  pre-migration copies are deleted as described under "Pre-migration
  copies"; the copies of earlier releases whose name carries another
  migration are not.
* **CSRF origin scheme** (VK-13), **SSE re-authorisation** (VK-08) and the
  **per-account sign-in throttle** (VK-03: a stranger's failures can still
  put a known account in backoff).
* **Product settings pages.** `/settings` (the redirect) stays each
  product's; a product adds its own tabs to `Ui::settings_tabs`. The kit's
  General page has no free-form product markup (only the declared field
  kinds), so Vigilant Vole keeps its own.
* **A public URL set by the environment.** The General page always lets an
  admin edit the public URL; a product that sets it from its environment
  (Keen Kestrel's `KK_PUBLIC_URL`) can't show it as locked.
* **Token scopes in the API reference.** The OpenAPI document and the
  Markdown reference don't list the product's scopes; the route's `access`
  text has to say which scope it needs.
* **One product scope per token from the page.** The Tokens page offers each
  scope as its own choice; a token with several scopes is possible
  (`add_scope`), but only from the product's code.
* **The password rule while typing.** The strength meter replaces the setup
  page's rule line while a password is typed; changing that is a change to
  `app.js` (a golden static asset).

## Status

Phase 3b is done: shared templates and pages, the core migration set, two
release components, kit-demo, and product-neutral notification forms.
vole-kit 0.1.0 is tagged. 0.1.1 makes the shared texts and the API reference
product-neutral (the texts a product supplies, the optional `DocsSpec`
parts, product error codes, the coverage helper), as Keen Kestrel needed.
0.2 closes the gaps Keen Kestrel worked around: product token scopes, the
owner's real role on token requests, sign-in throttling per instance, the
General settings page and site settings for products without it.
It also adds the building blocks Keen Kestrel built itself (see
"Building blocks for products").

## Risks where behaviour could differ without a test noticing

* A template edited in `templates/kit/` instead of `kit/templates/` is
  overwritten by the next build.
* The design system is embedded by name. A new design-system file added to
  `static/` without adding it to `KIT_FILES` and Vigilant Vole's exclusions
  is served by Vigilant Vole instead. It is still served, and the split test
  notices.
* Release builds embed files at compile time; debug builds read them from
  disk. Both read `static/` through the kit's path `../static/`.
* `install` validates the catalogs. A duplicate key stops the start, where
  before a broken file only logged an error and fell back to English. It also
  validates the token scopes and the General page's fields.
* Passkeys are bound to the public URL's host (the RP ID). Changing it, or
  the public URL being wrong, leaves them useless until users add new ones;
  the password keeps working.
* Channel and two-factor secrets are sealed under `session.key`. Replacing
  that file (or restoring a database without it) makes every sealed secret
  unreadable. The channels then ask for them again and their alerts fail,
  logged; the users with two-factor on can only sign in with a recovery
  code (then turn it off and on again). A
  plaintext secret that looks exactly like `sealed:v1:<base64 of 28+
  bytes>` would be taken for a sealed one and never sealed.
* `seal_stored` (channels and two-factor secrets) runs at startup, not as a
  migration. A copy of the
  database taken between the migrations and that step still holds
  plaintext. The pre-migration copy does on purpose, for a rollback.
* Vigilant Vole's core set leaves out `kit/0002_token_scopes.sql`. If it ever
  declares a token scope, `install` refuses to start until the migration is
  in its set, which changes its schema snapshots.
* Test binaries install the configuration from a `ctor` constructor (life
  before `main`, test builds only).
* Id prefixes sent to providers (`vv-monitor-7`, the Google Chat card id, the
  Matrix transaction id) come from `env_prefix`, lower-cased.
* The release `Cargo.lock` updates use a JSONPath filter on release-please's
  TOML parse (`@.name.value`). A future release-please that parses TOML
  differently would leave the lock unchanged, and the release PR's CI
  (`--locked`) would fail. `tools/release-dry-run` shows it.
