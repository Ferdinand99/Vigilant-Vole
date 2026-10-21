# The kit (`src/kit/`)

`src/kit/` holds everything in Vigilant Vole that is not specific to one
product: authentication, the i18n engine, the database plumbing, HTTP
conventions, the API reference generator, the MCP protocol layer and the
notification providers. It is phase 2 of the Keen Kestrel plan: the code is
carved out *inside this crate* first, so that phase 3 can move the directory
into its own workspace crate, `vole-kit`, without editing it.

This was a pure refactoring: nothing a user, an API client or a database can
see changed. `tests/golden/` (see [golden-tests.md](golden-tests.md)) proves it.

## The one rule

> Nothing under `src/kit/` names anything outside `crate::kit`.

`tests/kit_boundary.rs` fails when a file there uses a `crate::` path that
doesn't start with `crate::kit` (or `crate::{…}`). The product (Vigilant Vole)
depends on the kit, never the other way round. What the kit needs to know
about its product comes in through `KitConfig`, the traits and the specs
listed below.

## Module map

| Module | What it is | Moved from |
| --- | --- | --- |
| `kit::config` | `KitConfig`, `register_config!`, `get()` | new |
| `kit::state` | `Core` (pool, session key, HTTP client, config) and `with_conn` | new |
| `kit::i18n` | languages, catalogs, plurals, number/date formats, `Tr`, `Accept-Language`, the request language middleware | `src/i18n/` |
| `kit::db` | pool, `interact`, `settings` (key/value + core keys), `users`, `role`, `api_tokens`, `migrate` (runner, `MigrationSet`) | `src/db/mod.rs`, `settings`, `users`, `api_tokens`, `Role` |
| `kit::auth` | sessions, CSRF policy, rate limiting, TOTP, tokens, password rules, `CurrentUser`; `guard` has `require_auth`, `csrf_guard`, token and session resolution | `src/auth/`, the middleware half of `src/api/auth.rs` |
| `kit::http` | `ApiError`, security headers/CSP, `HtmlTemplate`, static files, `healthz`; `rest` has the JSON API conventions (error envelope, extractors, cursor paging, ETags, session-write CSRF guard) | `src/api/error.rs`, `src/api/mod.rs`, `src/api/v1/mod.rs`, `src/api/v1/error.rs` |
| `kit::page` | `PageCtx`, `UserView`, flash messages | `src/api/page.rs` |
| `kit::docs` | `RouteDoc`, OpenAPI 3.1 document, Markdown reference | `src/api/v1/docs.rs`, `docs/markdown.rs` |
| `kit::mcp` | MCP protocol: origin check, Bearer auth, rate limit, JSON-RPC, `Handler` trait | `src/mcp/mod.rs` |
| `kit::notify` | the 40 providers, schemas, HTTP/SMTP transports, message voice, the alert model (`NotificationEvent`, `EventKind`, `NotificationChannel`) | `src/notification/`, `src/db/models.rs` |
| `kit::tls_roots` | system CA store with bundled fallback | `src/tls_roots.rs` |

The old paths still exist as re-exports (`crate::auth`, `crate::i18n`,
`crate::db`, `crate::notification`, `crate::api::{ApiError, PageCtx, …}`,
`crate::tls_roots`), so the product code, the tests and the golden tests did
not have to change.

## `Core` and `AppState`

```rust
// kit::state
pub struct Core {
    pub db: Pool,
    pub session_key: Key,
    pub http: reqwest::Client,
    pub cfg: &'static KitConfig,
}
```

`AppState` keeps its fields (`db`, `scheduler`, `session_key`, `update_tx`,
`http`, `config`); `impl FromRef<AppState> for Core` builds the kit's view of
it, and `AppState::core()` is the shortcut. Kit middleware and extractors
take `State<Core>`, so the product's routers stay `Router<AppState>`.
`PageCtx::build` is generic over any state that converts into `Core`.

## `KitConfig`

The product describes itself once (`src/product.rs`) and registers it with
`register_config!(KIT)`. The macro defines a link-time symbol that
`kit::config::get()` calls, so the configuration exists in every binary that
contains the kit (the server, unit tests, integration tests) without an
`install()` call that someone could forget. The kit does not name the product;
the build fails to link if no product registers one.

| Field | Used for | Vigilant Vole |
| --- | --- | --- |
| `product` | texts, TOTP issuer fallback, default site name, MCP server title, auth realm, TLS warning | `Vigilant Vole` |
| `slug` | MCP server name, tags/sources sent to providers, timing-equaliser hash | `vigilant-vole` |
| `session_cookie`, `pending_2fa_cookie`, `flash_cookie` | cookie names | `vv_session`, `vv_2fa`, `vv_flash` |
| `token_prefix`, `other_token_prefixes` | API token format, MCP messages, docs checks | `vv_`, `["vva_"]` |
| `user_agent` | outbound `User-Agent` (the product's clients use it) | `VigilantVole/<version>` |
| `backup_prefix` | name of the copy taken before a migration | `vigilant-vole-before-` |
| `env_prefix` | `<PREFIX>_TRUST_PROXY`, `<PREFIX>_SETUP_TOKEN`, `$<PREFIX>_URL` in examples | `VV` |
| `csrf_exempt_prefixes`, `csrf_exempt_path` | token endpoints and the one-click unsubscribe link | `/api/push`, `/api/agent`, `/api/v1`, `/api/badge`, `/metrics`, `/mcp`; the unsubscribe path |
| `extra_common_passwords` | refused passwords besides the kit's list | `uptimekuma`, `vigilantvole` |
| `migrations` | `MigrationSet` (below) | `crate::db::MIGRATIONS` |
| `embeddable_prefixes` | paths that may be framed | `/status/`, `/api/status-page/` |
| `static_file` | looks up an embedded static file | `crate::api::static_file` |
| `machine_path_prefixes`, `machine_paths`, `browser_language_prefixes` | which paths stay English, which prefer `Accept-Language` | `/static/`, `/api/`; `/metrics`, `/healthz`; `/status/` |
| `docs` | `DocsSpec`: routes, OpenAPI texts, error example, config file name, `curl` placeholders, the product's Markdown sections | `crate::api::v1::DOCS` |
| `subject_type_label` | label of a monitor type in notifications | `MonitorType::label` |
| `locales` | `(code, locale file text)` for every language | `locales/*.toml` |

## What the product supplies besides `KitConfig`

* **Migrations.** `kit::db::migrate` owns the how (tracking table, safety
  copy, newer-schema refusal, foreign key handling). A `MigrationSet` says
  what: the migration files, the `-- vv:foreign_keys=off` marker, the legacy
  baseline (`0001_init.sql`, detected by the `users`/`monitors` tables) and
  the `after` hook for data fix-ups that SQL can't do (hashing old subscriber
  tokens). All of it is in `src/db/mod.rs`.
* **MCP tools.** `kit::mcp::Handler` is implemented by the product's `Ctx`
  (`src/mcp/mod.rs`); the tool table and the in-process REST calls stay in
  `src/mcp/tools.rs`.
* **API routes.** The route table, the docs pages and `DOCS` are in
  `src/api/v1/docs.rs`; the Markdown intro/middle/outro sections in
  `src/api/v1/docs/markdown.rs`.
* **Translations**, **static files** and the **monitor type labels**, through
  `KitConfig`.
* **Pages.** The login, setup and 2FA pages and the language/site-name
  settings UI stay with the product; they use the kit's guards and cookies.

## How the cycles were broken

* `auth ↔ api ↔ db`: the middleware moved with the primitives into
  `kit::auth::guard` and works on `Core` (not `AppState`); `ApiError` moved to
  `kit::http`; `Role`, `TokenScope`, users and API tokens moved to `kit::db`.
  `crate::api::auth` keeps thin wrappers that take `&AppState`.
* `i18n → api, db::settings`: the middleware uses `Core::with_conn` and the
  kit's settings accessors; the catalogs come from `KitConfig::locales`
  (`LangInfo::source` derefs to the registered text).
* `db → notification`: `NotificationEvent`, `EventKind` and
  `NotificationChannel` moved to `kit::notify::alert`; `crate::db::models`
  re-exports them.
* `notification → db::models::MonitorType`: replaced by
  `KitConfig::subject_type_label`.
* `mcp → api, auth, monitor`: the protocol part needs only `Core`; the tools
  stayed.

## Status

Done (all verified with the golden tests, the 1218 lib tests, `tests/upgrade.rs`,
`backup_restore`, `db_concurrency`, `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings`):

* kit skeleton, `KitConfig`, `Core`, boundary test
* `kit::i18n`, `kit::db`, `kit::http` (+ `rest`), `kit::auth`, `kit::page`,
  `kit::docs`, `kit::mcp`, `kit::notify`, `kit::tls_roots`

Left for phase 3 (not done now on purpose):

1. **Workspace crate.** Move `src/kit/` to `crates/vole-kit/` and make
   Vigilant Vole depend on it. `register_config!` already uses `$crate`, so
   it keeps working; its path becomes `vole_kit::register_config!`.
2. **Embedded resources.** Templates (`layout.html`, `macros.html`, the auth
   pages, `api_docs.html`), `static/` (CSS, fonts, `theme.js`, `app.js`,
   icons), `locales/` (the shared keys such as `error.*`, `users.err_*`,
   `msg.*`, `notif.*`) and `migrations/` stay where they are. The kit refers
   to translation keys that live in the product's catalogs (`error.not_found`,
   `msg.down`, …). Phase 3 moves the shared ones into the crate and adds the
   template/locale synchronisation.
3. **Core migration set.** `users`, `api_tokens` and `settings` are created by
   the product's `0001`–`00NN` migrations. The kit should own a core set and
   the product append its own.
4. **Kit tests.** Many tests moved with their code and rely on the registered
   product configuration (cookie names, `vv_` tokens, English `msg.*` texts)
   and on Vigilant Vole's migrations and locale files (`kit::db::test_conn`,
   the i18n catalog tests in `src/i18n/tests.rs`, the notification fixtures in
   `kit::notify::tests`). The kit crate needs its own test product config.
5. **Texts fixed at compile time.** The example placeholders and two defaults
   in the provider forms say "Vigilant Vole" (`bark` group, `zulip` topic,
   bot-name examples). They are `const` data, which a link-time config can't
   fill in; the golden tests read them raw. A build-time constant or locale
   overrides (`notif.<kind>.<field>.placeholder`) would remove them.
6. **Remaining product names outside the kit's reach:** the `localStorage`
   key in `static/theme.js`, the `vv-i18n` element id in the layout, `vv:`
   in the foreign key marker (that one *is* configurable).
7. **`src/mcp_bridge.rs`** (the stdio bridge, `vigilant-vole mcp --url`) is
   generic and could join `kit::mcp`.

Next step: phase 3, in the order above.

## Risks where behaviour could differ without a test noticing

* `register_config!` is a link-time symbol (`#[unsafe(no_mangle)]` and one
  `unsafe` call in `kit::config::get`). A second product linked into the same
  binary would clash; none is.
* Kit code calls `config::get()` per use instead of caching; it is a plain
  function returning a static reference.
* Environment variable names are built at run time from `env_prefix`; the
  values (`VV_TRUST_PROXY`, `VV_SETUP_TOKEN`) are unchanged.
* Message texts the kit builds from catalog keys (`users.err_*`, `error.*`,
  `msg.*`) are only as complete as the product's locale files; the i18n
  coverage test still checks every key used in Rust and templates.
