# vole-kit

`vole-kit` is the shared product kit of Vigilant Vole and its sibling
products (first of them Keen Kestrel): authentication, the i18n engine and
the core translations, the database plumbing and migration runner, HTTP
conventions, the API reference generator, the MCP protocol layer, the 40
notification providers and the design system's static files.

It lives in this repository as a Cargo workspace member, `kit/`. Vigilant
Vole depends on it by path; other products depend on it by git tag.

The kit was carved out without changing anything a user, an API client or a
database can see. `tests/golden/` (see [golden-tests.md](golden-tests.md))
proves it at every step.

## Layout

```
Cargo.toml          # [workspace] members "." and "kit"; [workspace.dependencies]; package vigilant-vole
kit/
├── Cargo.toml      # package vole-kit
├── locales/        # the kit's core strings, one file per language
└── src/            # the crate (lib.rs = the old src/kit/mod.rs)
src/                # Vigilant Vole
static/             # Vigilant Vole's files and the kit's design system (see "Static files")
locales/            # Vigilant Vole's own strings
migrations/         # Vigilant Vole's migrations (see "Migrations")
templates/          # Vigilant Vole's templates
```

`cargo test`, `cargo clippy`, `cargo build` without `-p` cover both crates
(`default-members`). CI runs `cargo clippy --workspace --all-targets` and
`cargo test --workspace --lib`; the Dockerfile copies `kit/`.

`vole-kit`'s dependencies are a subset of Vigilant Vole's, with versions and
features from `[workspace.dependencies]`: axum, axum-extra, askama (the
`Template` trait only), tokio, reqwest, rusqlite, deadpool-sqlite, rust-embed,
serde, serde_json, time, jiff, url, tracing, thiserror, rand, hex, sha2,
base64, argon2, totp-rs, qrcode, regex, lettre, rustls, rustls-native-certs,
webpki-root-certs, futures-util. Test only: tower, tokio-rustls, rcgen.

## The rule

> The kit never depends on the product.

The compiler enforces it now: `vole-kit` can't name `vigilant_vole` without a
dependency on it. `tests/kit_boundary.rs` keeps that door shut (no
`vigilant-vole` in `kit/Cargo.toml`, no `vigilant_vole` or leftover
`crate::kit` in the kit's sources). The kit forbids `unsafe` code
(`#![forbid(unsafe_code)]`).

Vigilant Vole re-exports the crate as `crate::kit` (`pub use vole_kit as
kit`), and keeps every older path (`crate::auth`, `crate::i18n`, `crate::db`,
`crate::notification`, `crate::api::{ApiError, PageCtx, …}`,
`crate::tls_roots`) as a re-export or a thin wrapper, so no test had to
change.

## Module map

| Module | What it is |
| --- | --- |
| `config` | `KitConfig`, `install`, `get` |
| `state` | `Core` (pool, session key, HTTP client, config) and `with_conn` |
| `i18n` | languages, catalogs (core + product), plurals, number/date formats, `Tr`, `Accept-Language`, the request language middleware |
| `db` | pool, `interact`, `settings` (key/value + core keys), `users`, `role`, `api_tokens`, `migrate` (runner, `MigrationSet`) |
| `auth` | sessions, CSRF policy, rate limiting, TOTP, API tokens, password rules, `CurrentUser`; `guard`: `require_auth`, `csrf_guard`, token and session resolution |
| `http` | `ApiError`, security headers/CSP, `HtmlTemplate`, static files (`assets`), `healthz`; `rest`: the JSON API conventions (error envelope, extractors, cursor paging, ETags, session-write CSRF guard) |
| `page` | `PageCtx`, `UserView`, flash messages |
| `docs` | `RouteDoc`, `DocsSpec`, the OpenAPI 3.1 document and the Markdown reference |
| `mcp` | MCP protocol: origin check, Bearer auth, rate limit, JSON-RPC, the `Handler` trait |
| `notify` | `Alert`, `AlertKind`, `Level`, the 40 providers, schemas, HTTP/SMTP transports, the message layout |
| `tls_roots` | system CA store with the bundled fallback |

## Configuration

A product describes itself once in a static `KitConfig` and installs it when
it starts, before any kit code runs:

```rust
static KIT: KitConfig = KitConfig { product: "Keen Kestrel", slug: "kestrel", /* … */ };

fn main() {
    vole_kit::config::install(&KIT);
    // …
}
```

`install` is a `OnceLock`: installing the same configuration again does
nothing, a different one panics. The first installation also checks the
translations (every catalog parses, no key is in both the kit's and the
product's file) and panics on a problem, so a broken build doesn't start.
`get()` without an installed configuration panics with a message that says
what to do; the kit's own tests fall back to their test configuration
(`kit/src/testing.rs`: Vigilant Vole's names, translations and migrations).

Vigilant Vole installs in `main` and in `server::prepare`. Test binaries have
no `main`, so its unit tests (`src/lib.rs`) and every integration test
(`tests/common/mod.rs` and the three tests that don't use it) install from a
constructor that runs before the tests (`ctor`, a dev-dependency), so no test
can forget it.

| Field | Used for | Vigilant Vole |
| --- | --- | --- |
| `product` | texts, TOTP issuer fallback, default site name, MCP server title, auth realm, provider names, TLS warning | `Vigilant Vole` |
| `slug` | MCP server name, tags/sources sent to providers, timing-equaliser hash | `vigilant-vole` |
| `session_cookie`, `pending_2fa_cookie`, `flash_cookie` | cookie names | `vv_session`, `vv_2fa`, `vv_flash` |
| `token_prefix`, `other_token_prefixes` | API token format, MCP messages, docs checks | `vv_`, `["vva_"]` |
| `user_agent` | outbound `User-Agent` (the product's clients) | `VigilantVole/<version>` |
| `backup_prefix` | name of the copy taken before a migration | `vigilant-vole-before-` |
| `env_prefix` | `<PREFIX>_TRUST_PROXY`, `<PREFIX>_SETUP_TOKEN`, `$<PREFIX>_URL` in examples, id prefixes sent to providers (`vv-monitor-7`) | `VV` |
| `csrf_exempt_prefixes`, `csrf_exempt_path` | token endpoints and the one-click unsubscribe link | `/api/push`, `/api/agent`, `/api/v1`, `/api/badge`, `/metrics`, `/mcp`; the unsubscribe path |
| `extra_common_passwords` | refused passwords besides the kit's list | `uptimekuma`, `vigilantvole` |
| `migrations` | `MigrationSet` | `crate::db::MIGRATIONS` |
| `embeddable_prefixes` | paths that may be framed | `/status/`, `/api/status-page/` |
| `static_file` | looks up one of the product's static files | `crate::api::static_file` |
| `machine_path_prefixes`, `machine_paths`, `browser_language_prefixes` | which paths stay English, which prefer `Accept-Language` | `/static/`, `/api/`; `/metrics`, `/healthz`; `/status/` |
| `docs` | `DocsSpec`: routes, OpenAPI texts, error example, config file name, `curl` placeholders, the product's Markdown sections | `crate::api::v1::DOCS` |
| `locales` | `(code, text)` of the product's own strings per language | `locales/*.toml` |

`Core` is the kit's view of the product's state: Vigilant Vole implements
`FromRef<AppState> for Core` (`AppState::core()`), kit middleware takes
`State<Core>`, and the product's routers stay `Router<AppState>`.

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
out (summary, title, plain-text, Markdown and HTML bodies). Vigilant Vole keeps
its `NotificationEvent` and `EventKind`; `notification::message::alert` turns
one into an `Alert` (headline, detail and facts in Vigilant Vole's wording), and
`crate::notification` keeps `PROVIDERS`, `Transport`, `send`, `send_config`,
`send_all` and `email::{build_message, send}` with their old signatures as
wrappers. Every provider's request and the email MIME are byte-identical (the
golden notification snapshots).

## Resources

### Translations

A language's catalog is two files: the kit's core strings
(`kit/locales/<code>.toml`: the sections `common`, `nav`, `js`, `fmt`,
`count`, `login`, `setup`, `sec`, `users`, `tokens`, `auth`, `error`, `role`,
`notif` and the key `msg.sent_by`), compiled into the kit, and the product's
(`locales/<code>.toml`, handed over in `KitConfig::locales`). `LangInfo::source`
is the two joined. A key in both is an error at start-up (with tests). All 26
languages are split this way; keys and texts are unchanged.

### Static files

The kit embeds the design system: `style.css`, `app.js`, `theme.js`,
`icons.svg`, `htmx.min.js`, `sse.js` and `fonts/` (`assets::KIT_FILES`), and
serves it first at `/static/…`; Vigilant Vole embeds only its own files (icons,
favicon, manifest). The files stay in the repository's `static/` folder: the
golden test `static_assets` enumerates that folder, so moving them under
`kit/` would change a golden test. The kit embeds them from `../static/`,
which a git dependency checks out along with the kit. A test checks that every
file in `static/` belongs to exactly one of the two. URLs, contents, content
types, ETags and caching are unchanged.

## Migrations

Not done in phase 3, deliberately. `users`, `api_tokens` and `settings` are
still created by Vigilant Vole's migrations, and the kit's runner applies the
product's `MigrationSet`.

A separate core set would add rows to `schema_migrations` (or another
tracking table) on every database, and the golden schema test freezes both
`sqlite_master` and the list of applied migrations for a fresh database and
for every release fixture. Any core set therefore changes a golden snapshot,
which the refactoring must not do. The plan, for when the owner accepts that
snapshot change:

1. `kit/migrations/` with the final schema of `users`, `api_tokens` and
   `settings` (`CREATE TABLE IF NOT EXISTS`), tracked as `kit/0001_core.sql`
   etc. in `schema_migrations`.
2. `MigrationSet` grows a `core` list; the runner applies core first, then the
   product's set.
3. Vigilant Vole adds `01NN_adopt_kit_core.sql`, which only records the core
   migrations as applied. Databases that already have the tables run it; fresh
   databases run the core set and then Vigilant Vole's migrations, whose
   `CREATE TABLE` statements for those tables are guarded by the adoption
   (or rewritten to `IF NOT EXISTS`).
4. `tests/upgrade.rs` must still give an identical schema and identical data
   for every fixture; only the migration list in the golden schema snapshot
   changes.

## Using vole-kit in another product (Keen Kestrel)

```toml
# Cargo.toml
[dependencies]
vole-kit = { git = "https://github.com/Ferdinand99/Vigilant-Vole", tag = "vole-kit-v0.1.0" }
```

```toml
# .cargo/config.toml (not committed): work against a local checkout
[patch."https://github.com/Ferdinand99/Vigilant-Vole"]
vole-kit = { path = "../Vigilant-Vole/kit" }
```

Then:

1. Write a `static KitConfig` (`kestrel`, `kk_session`, `kk_`,
   `KeenKestrel/<version>`, …), its `MigrationSet`, `DocsSpec` and product
   locales, and call `vole_kit::config::install(&KIT)` first thing in `main`
   (and in test set-up).
2. Implement `FromRef<AppState> for vole_kit::state::Core`; use
   `require_auth`, `csrf_guard`, `security_headers`, `static_asset`,
   `i18n::layer` and `ApiError` like Vigilant Vole's `api::build_router`.
3. Send alerts by building `Alert`s and calling `vole_kit::notify::send`.
4. Serve the design system with `vole_kit::http::assets::static_asset`.

Until there is a tag, pin a commit (`rev = "…"`).

## Not done (proposals)

* **Templates.** `layout.html`, `macros.html`, the sign-in pages and
  `api_docs.html` stay in `templates/`. askama compiles templates into the crate
  that uses them, so sharing means the product reading the kit's template
  directory at build time: vole-kit declares `links = "vole-kit"` and a
  `build.rs` that exports its `templates/` path (`DEP_VOLE_KIT_TEMPLATES`), and
  each product's `askama.toml` lists that directory after its own. A test in
  each product would render the shared pages. Not attempted here.
* **Releases.** release-please with two components: `.` (`vigilant-vole`,
  tags `vX.Y.Z` as now) and `kit` (`vole-kit`, tags `vole-kit-vX.Y.Z`), each with
  its own changelog and version (`release-please-config.json` packages, plus
  `include-component-in-tag` for the kit). No version was bumped and nothing
  was tagged.
* **Product names outside the kit's reach:** the provider form placeholders
  and two defaults that say "Vigilant Vole" (compile-time schema data the
  golden tests read raw), the `localStorage` key in `static/theme.js`, the
  `vv-i18n` element id in the layout, the `vv:` foreign key marker (that one is
  configurable).
* **`src/mcp_bridge.rs`** (the stdio bridge) is generic and could join
  `vole_kit::mcp`.

## Status

Phase 3 done: the workspace and crate; configuration without `unsafe`; the
generic `Alert`; the kit's own translations and design system files. Left: the
core migration set (plan above), template sharing and release-please (proposals
above), then tagging `vole-kit`.

## Risks where behaviour could differ without a test noticing

* The design system is embedded by name; a new design-system file added to
  `static/` without adding it to `KIT_FILES` and Vigilant Vole's exclusions is
  served by Vigilant Vole instead (still served; the split test notices).
* Release builds embed files at compile time, debug builds read them from
  disk; both read `static/` through the kit's path `../static/`.
* `install` validates the catalogs; a duplicate key now stops the start where
  before a broken file only logged an error and fell back to English.
* Test binaries install the configuration from a `ctor` constructor (life
  before `main`, test builds only).
* Id prefixes sent to providers (`vv-monitor-7`, the Google Chat card id, the
  Matrix transaction id) come from `env_prefix` lower-cased.
