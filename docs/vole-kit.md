# vole-kit

`vole-kit` is the shared product kit of Vigilant Vole and its sibling
products (first of them Keen Kestrel). It holds:

* authentication, with the pages every product needs: setup, sign-in with two
  factors, users, API tokens (with the product's own token scopes), Security,
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
│                       #   0002_token_scopes.sql (api_token_scopes)
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
| `state` | `Core` (pool, session key, HTTP client, config, sign-in throttles), `Core::new` and `with_conn` |
| `i18n` | languages, catalogs (core + product), plurals, number/date formats, `Tr` (template helpers, `kit_ui()`, `product_name()`), `Accept-Language`, the request language middleware |
| `db` | pool, `interact`, `settings` (key/value + core keys), `users`, `role`, `api_tokens` (with product scopes), `migrate` (runner, `MigrationSet`, `CORE`, `CORE_TABLES`, `TOKEN_SCOPES`) |
| `auth` | sessions, CSRF policy, rate limiting (`SignInLimits`), TOTP, API tokens, password rules, `CurrentUser` (`Scopes`, `has_scope`, `may_act_as`), `ExtraScope`; `guard`: `require_auth`, `csrf_guard`, token and session resolution; `pages`: the shared pages and their routers |
| `settings` | `SiteSettings` (site name, public URL, time zone, language), `set_site_language`, `normalize_base_url`, and the General page's description (`GeneralSettings`, `SettingsSection`, `SettingField`, `FieldKind`) |
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
| `locales` | `(code, text)` of the product's own strings per language, including the texts the kit's pages take from it (see "Translations") | `locales/*.toml` |
| `ui` | `Ui` (below) | `src/product.rs` |
| `public_url_setting` | where an admin sets the public URL, named by the MCP endpoint's refusal of a foreign browser origin; `None` says "the instance's public URL" | `Some("Settings → General")` |
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
  `settings_security.html`, `api_docs.html` and their partials.

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
  * `/settings/general` (admins), only with `KitConfig::general`;
  * `/settings/users` (create, edit, password, 2FA reset, enable, disable,
    delete);
  * `/settings/tokens` (list, create with the product's scopes, revoke);
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
set (`core: &[CORE_TABLES]`), so its schema, migration list and pages are
unchanged; the kit doesn't touch `api_token_scopes` when no scopes are
declared.

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
`style.css`, `app.js`, `theme.js`, `icons.svg`, `htmx.min.js`, `sse.js` and
`fonts/` (`assets::KIT_FILES`). Vigilant Vole embeds only its own files
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
  (`core: &[CORE_TABLES]`), so its schema and migration list are unchanged.
  Adding it later just runs it; leaving it out after it ran makes the
  database look newer (refused), as for any migration.

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

## Using vole-kit in another product (Keen Kestrel)

```toml
# Cargo.toml
[dependencies]
vole-kit = { git = "https://github.com/Ferdinand99/Vigilant-Vole", tag = "vole-kit-v0.1.1" }

[build-dependencies]
vole-kit-build = { git = "https://github.com/Ferdinand99/Vigilant-Vole", tag = "vole-kit-v0.1.1" }
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
* the OpenAPI and MCP versions are the demo's (0.3.7);
* a fresh database has the core tables (with `api_token_scopes`) plus
  `notes`, with `kit/0001_core.sql` and `kit/0002_token_scopes.sql`
  recorded first.

The demo depends on the kit by path; the template mechanism is the one a git
dependency uses.

### Upgrading from 0.1 to 0.2 (what Keen Kestrel changes)

0.2 is a breaking release (pre-1.0 minor):

1. **`KitConfig` has two new fields.** Add `token_scopes` and `general`
   (`&[]` and `None` keep 0.1's behaviour).
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
* **The webhook's template placeholders** (`{{monitor_name}}`,
  `{{monitor_id}}`, `{{monitor_type}}`, `{{downtime}}` …) are the kit's
  webhook provider's actual variable names, so its help text names them.
  Neutral names (`{{subject}}` …) with the old ones as aliases would be a
  feature of their own.
* **Path parameter types** in the API reference: `kind`, `token` and `slug`
  are strings, anything else an integer (`docs::path_param_type`).
* **Example placeholders** like `vigilant-alerts…` (ntfy topic) or
  `https://example.com/hooks/vigilant…` stay. They don't name the product.
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
