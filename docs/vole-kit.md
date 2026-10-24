# vole-kit

`vole-kit` is the shared product kit of Vigilant Vole and its sibling
products (first of them Keen Kestrel). It holds:

* authentication, with the pages every product needs: setup, sign-in with two
  factors or a passkey, users, API tokens (with the product's own token scopes), Security,
  the API reference and, for products that want it, General settings;
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
├── migrations/         # the core set: 0001_core.sql (users, api_tokens, settings),
│                       #   0002_token_scopes.sql (api_token_scopes),
│                       #   0003_passkeys.sql (passkeys, passkey_challenges),
│                       #   0004_sealed_secrets.sql (a record only, no table)
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
| `db` | pools (`build_pool`, `build_pool_with` + `PoolOptions`), `interact`, `settings` (key/value + core keys), `users`, `role`, `api_tokens` (with product scopes), `passkeys` (credentials and challenges), `migrate` (runner, `MigrationSet`, `CORE`, `CORE_TABLES`, `TOKEN_SCOPES`, `PASSKEYS`, `SEALED_SECRETS`, `migrate_with` + `MigrateOptions`) |
| `auth` | sessions, CSRF policy, rate limiting (`SignInLimits`), TOTP, API tokens, password rules, `CurrentUser` (`Scopes`, `has_scope`, `may_act_as`), `ExtraScope`; `webauthn`: passkey ceremonies (`RelyingParty`, `verify_registration`, `verify_assertion`, the test authenticator `soft`); `guard`: `require_auth`, `csrf_guard`, token and session resolution; `pages`: the shared pages and their routers |
| `settings` | `SiteSettings` (site name, public URL, time zone, language), `set_site_language`, `normalize_base_url`, and the General page's description (`GeneralSettings`, `SettingsSection`, `SettingField`, `FieldKind`) |
| `http` | `ApiError`, security headers/CSP and `HeaderPolicy`, `HtmlTemplate`, static files (`assets`), `healthz`, `toast_trigger`; `rest`: the JSON API conventions (error envelope, extractors, cursor paging, ETags, session-write CSRF guard); `ratelimit`: token buckets, `429` with `Retry-After`; `live`: live updates over server-sent events |
| `page` | `PageCtx`, `UserView`, flash messages, `Ui` (`NavItem`, `SettingsTab`, `TabAccess`) |
| `docs` | `RouteDoc`, `DocsSpec`, the OpenAPI 3.1 document, the Markdown reference; `page`: `/api-docs` and friends |
| `mcp` | MCP protocol: origin check, Bearer auth, rate limit, JSON-RPC, the `Handler` trait |
| `notify` | `Alert` (with `Alert::test`), `AlertKind`, `Level`, the 40 providers, schemas, HTTP/SMTP transports, the message layout, `send_in`/`send_config_in` (a channel's language); `form`: the provider form's view-model; `sealed`: channel secrets sealed at rest; `webhook::Wording` |
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
| `env_prefix` | `<PREFIX>_TRUST_PROXY`, `<PREFIX>_SETUP_TOKEN`, `$<PREFIX>_URL` in examples, id prefixes sent to providers (`vv-monitor-7`) | `VV` |
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

## Pages

`vole_kit::auth::pages` serves the pages every product needs, generic over
the product's state (anything `Core` and `Key` can be taken from):

* `public_router()`: `/setup`, `/login`, `/login/2fa`, `/login/passkey`
  (and `/options`), `/logout`.
* `router()` must sit behind `require_auth`. It serves:
  * `/settings/general` (admins), only with `KitConfig::general`;
  * `/settings/users` (create, edit, password, 2FA reset, passkey reset,
    enable, disable, delete);
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
  failed sign-in counts against the address like a failed password
  (`SignInLimits::ip`), a success undoes it. Failed registrations back off
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
  `kit/0003_passkeys.sql` and `kit/0004_sealed_secrets.sql` recorded first;
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
* Channel secrets are sealed under `session.key`. Replacing that file (or
  restoring a database without it) makes every sealed secret unreadable.
  The channels then ask for them again and their alerts fail, logged. A
  plaintext secret that looks exactly like `sealed:v1:<base64 of 28+
  bytes>` would be taken for a sealed one and never sealed.
* `seal_stored` runs at startup, not as a migration. A copy of the
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
