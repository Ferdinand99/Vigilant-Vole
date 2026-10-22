# vole-kit

`vole-kit` is the shared product kit of Vigilant Vole and its sibling
products (first of them Keen Kestrel). It holds:

* authentication, with the pages every product needs: setup, sign-in with two
  factors, users, API tokens, Security and the API reference;
* the i18n engine and the core translations;
* the database plumbing, the migration runner and the core tables;
* HTTP conventions, the API reference generator and the MCP protocol layer;
* the 40 notification providers;
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
├── migrations/         # the core set: 0001_core.sql (users, api_tokens, settings)
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
* crypto: rand, hex, sha2, base64, argon2, totp-rs, qrcode, regex
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
| `state` | `Core` (pool, session key, HTTP client, config) and `with_conn` |
| `i18n` | languages, catalogs (core + product), plurals, number/date formats, `Tr` (template helpers, `kit_ui()`, `product_name()`), `Accept-Language`, the request language middleware |
| `db` | pool, `interact`, `settings` (key/value + core keys), `users`, `role`, `api_tokens`, `migrate` (runner, `MigrationSet`, `CORE`) |
| `auth` | sessions, CSRF policy, rate limiting, TOTP, API tokens, password rules, `CurrentUser`; `guard`: `require_auth`, `csrf_guard`, token and session resolution; `pages`: the shared pages and their routers |
| `http` | `ApiError`, security headers/CSP, `HtmlTemplate`, static files (`assets`), `healthz`, `toast_trigger`; `rest`: the JSON API conventions (error envelope, extractors, cursor paging, ETags, session-write CSRF guard) |
| `page` | `PageCtx`, `UserView`, flash messages, `Ui` (`NavItem`, `SettingsTab`, `TabAccess`) |
| `docs` | `RouteDoc`, `DocsSpec`, the OpenAPI 3.1 document, the Markdown reference; `page`: `/api-docs` and friends |
| `mcp` | MCP protocol: origin check, Bearer auth, rate limit, JSON-RPC, the `Handler` trait |
| `notify` | `Alert`, `AlertKind`, `Level`, the 40 providers, schemas, HTTP/SMTP transports, the message layout |
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
parse, and no key may be in both the kit's and the product's file. A problem
panics, so a broken build doesn't start.

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
| `version` | `info.version` in the OpenAPI document, MCP `serverInfo.version` | `CARGO_PKG_VERSION` of vigilant-vole |
| `user_agent` | outbound `User-Agent` (the product's clients) | `VigilantVole/<version>` |
| `backup_prefix` | name of the copy taken before a migration | `vigilant-vole-before-` |
| `env_prefix` | `<PREFIX>_TRUST_PROXY`, `<PREFIX>_SETUP_TOKEN`, `$<PREFIX>_URL` in examples, id prefixes sent to providers (`vv-monitor-7`) | `VV` |
| `csrf_exempt_prefixes`, `csrf_exempt_path` | token endpoints and the one-click unsubscribe link | `/api/push`, `/api/agent`, `/api/v1`, `/api/badge`, `/metrics`, `/mcp`; the unsubscribe path |
| `extra_common_passwords` | refused passwords besides the kit's list | `uptimekuma`, `vigilantvole` |
| `migrations` | `MigrationSet` | `crate::db::MIGRATIONS` |
| `embeddable_prefixes` | paths that may be framed | `/status/`, `/api/status-page/` |
| `static_file` | looks up one of the product's static files | `crate::api::static_file` |
| `machine_path_prefixes`, `machine_paths`, `browser_language_prefixes` | which paths stay English, which prefer `Accept-Language` | `/static/`, `/api/`; `/metrics`, `/healthz`; `/status/` |
| `docs` | `DocsSpec` (below) | `crate::api::v1::DOCS` |
| `locales` | `(code, text)` of the product's own strings per language | `locales/*.toml` |
| `ui` | `Ui` (below) | `src/product.rs` |

`DocsSpec` holds the routes, the OpenAPI texts, the error example, the config
file name and the `curl` placeholders. It also holds the product's Markdown
sections and, for the shared pages:

* `page_subtitle`, a translation key in the product's catalog;
* `openapi_path`, `api_base` and `example_path` (Vigilant Vole:
  `/api/v1/openapi.json`, `/api/v1`, `/api/v1/monitors`);
* `metrics_path` (`Some("/metrics")`; `None` drops the Prometheus hint on the
  Tokens page).

`Ui` holds what the layouts show:

* `logo`, `favicon`, `apple_touch_icon` and `manifest` (empty for none);
* `nav`, the main navigation (top bar and phone tab bar: id, link, label and
  short label keys, icon);
* `settings_tabs`, in order. The kit's own tabs are `SettingsTab::USERS`,
  `SECURITY` and `TOKENS`. Each tab has an access level (`Everyone`,
  `Editors`, `Admins`);
* `home_url`, the link in the public pages' "Powered by" footer.

`Core` is the kit's view of the product's state. A product implements
`FromRef<AppState> for Core` (and for the cookie `Key`). Kit handlers and
middleware take `State<Core>`, and the product's routers stay
`Router<AppState>`. A product can also use `Core` itself as its state, as
kit-demo does.

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
  `settings_users.html`, `settings_tokens.html`, `settings_security.html`,
  `api_docs.html` and their partials.

Vigilant Vole keeps its monitoring macros (status pill and dot, heartbeat
bar, incident timeline) in `templates/status_macros.html`, imported as `st`.

The layouts get the product's name, logo, icons, navigation and settings tabs
from `KitConfig` (`product_name()` and `kit_ui()`, helpers on `Tr` that every
template can call). Three kinds of names stay as they are, because they are
the contract between the layout and the design system's own scripts:

* the element ids `vv-i18n` and `vv-progress`;
* the theme radio and `localStorage` key `vv-theme`;
* the `vv:` events (`vv:toast`, `vv:sheet-open`, …).

`app.js` and `theme.js` look for exactly these names. Renaming them means
changing those files together with the layouts (see "Not done").

## Pages

`vole_kit::auth::pages` serves the pages every product needs, generic over
the product's state (anything `Core` and `Key` can be taken from):

* `public_router()`: `/setup`, `/login`, `/login/2fa`, `/logout`.
* `router()` must sit behind `require_auth`. It serves:
  * `/settings/users` (create, edit, password, 2FA reset, enable, disable,
    delete);
  * `/settings/tokens` (list, create, revoke);
  * `/settings/security` (profile and language, password, other sessions,
    2FA set-up/cancel/enable/disable/recovery codes);
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
snapshots) didn't change.

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
SMS sender …). Read fields through `Field::placeholder_text`,
`Field::default_value` and `Field::tr_placeholder`, which fill in the
configured names.

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

### Static files

The kit embeds the design system and serves it first at `/static/…`:
`style.css`, `app.js`, `theme.js`, `icons.svg`, `htmx.min.js`, `sse.js` and
`fonts/` (`assets::KIT_FILES`). Vigilant Vole embeds only its own files
(icons, favicon, manifest).

The files stay in the repository's `static/` folder, because the golden test
`static_assets` enumerates that folder. The kit embeds them from
`../static/`, which a git dependency checks out along with the kit. A test
checks that every file in `static/` belongs to exactly one of the two.

## Migrations

The kit has its own migration set, `vole_kit::db::migrate::CORE`. Today that
is `kit/migrations/0001_core.sql`, which creates `users`, `api_tokens` (with
its index) and `settings`. The statements are copied exactly as they stand in
Vigilant Vole's `sqlite_master`. A Vigilant Vole test checks that the core
set builds the same objects, columns and foreign keys as Vigilant Vole's
migrations.

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
  included) to Vigilant Vole's. Commits that touch only `kit/` or
  `examples/` don't count towards Vigilant Vole (`exclude-paths`).
* **The cargo-workspace plugin isn't used.** It would bump vigilant-vole for
  every kit release. Vigilant Vole depends on the kit by path without a
  version, so a kit release needs no Vigilant Vole release.
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
* a later kit-only fix proposes only vole-kit 0.1.1;
* a Vigilant Vole fix proposes only 0.7.1;
* a kit-demo change proposes nothing.

## Using vole-kit in another product (Keen Kestrel)

```toml
# Cargo.toml
[dependencies]
vole-kit = { git = "https://github.com/Ferdinand99/Vigilant-Vole", tag = "vole-kit-v0.1.0" }

[build-dependencies]
vole-kit-build = { git = "https://github.com/Ferdinand99/Vigilant-Vole", tag = "vole-kit-v0.1.0" }
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
   * a `DocsSpec`, the product's locales and a `Ui`.

   Call `vole_kit::config::install(&KIT)` first thing in `main` (and in test
   set-up).
2. Build `Core` (or implement `FromRef<AppState> for Core` and `Key`), run
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
* `/static/style.css` is served;
* the OpenAPI and MCP versions are the demo's (0.3.7);
* a fresh database has the core tables plus `notes`, with
  `kit/0001_core.sql` recorded first.

The demo depends on the kit by path; the template mechanism is the one a git
dependency uses.

## Not done (proposals)

* **The design system's DOM names.** `vv-i18n`, `vv-progress`, `vv-theme`
  and the `vv:` events are shared by the layouts and `app.js`/`theme.js`. A
  neutral prefix (`vk-`) means changing those scripts, the layouts and
  Vigilant Vole's templates that listen for `vv:sheet-open` and
  `vv:reorder`. That changes the golden static asset hashes.
* **Unused or product-flavoured strings in the kit's catalog.**
  * `setup.title` ("Set up Vigilant Vole") is used nowhere. Removing it
    changes the i18n key snapshot.
  * Some kit keys keep their old section names (`detail.expires`,
    `monitor_form.edit_heading` …). Renaming them is the same kind of
    snapshot change.
* **Example placeholders** like `vigilant-alerts…` (ntfy topic) or
  `https://example.com/hooks/vigilant…` stay. They don't name the product.
* **`src/mcp_bridge.rs`** (the stdio bridge) is generic and could join
  `vole_kit::mcp`.
* **Product settings pages.** `/settings` (the redirect) and General stay
  Vigilant Vole's; a product adds its own tabs to `Ui::settings_tabs`.

## Status

Phase 3b is done: shared templates and pages, the core migration set, two
release components, kit-demo, and product-neutral notification forms. Next:
merge, then let release-please tag `vole-kit-v0.1.0` for Keen Kestrel.

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
  before a broken file only logged an error and fell back to English.
* Test binaries install the configuration from a `ctor` constructor (life
  before `main`, test builds only).
* Id prefixes sent to providers (`vv-monitor-7`, the Google Chat card id, the
  Matrix transaction id) come from `env_prefix`, lower-cased.
* The release `Cargo.lock` updates use a JSONPath filter on release-please's
  TOML parse (`@.name.value`). A future release-please that parses TOML
  differently would leave the lock unchanged, and the release PR's CI
  (`--locked`) would fail. `tools/release-dry-run` shows it.
