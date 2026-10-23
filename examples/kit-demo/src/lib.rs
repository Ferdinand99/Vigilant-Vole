//! Kit Demo: the smallest product built on vole-kit. It has its own name,
//! cookies, token prefix, version, one migration and a few strings (its own,
//! and the texts the kit's pages take from a product); everything
//! else (sign-in, users, tokens, Security, the API reference, the layout and
//! the design system) is the kit's, reached the way any product reaches it:
//! templates through `vole-kit-build`, pages through
//! `vole_kit::auth::pages`. The tests in `tests/` drive it.
//!
//! It also uses the kit's building blocks a product like Keen Kestrel
//! needs: a second database (`kit-demo-store.db`, its own migrations, no
//! backup, incremental auto-vacuum), an ingest endpoint for programs
//! (`POST /api/{project}/envelope/`: CSRF-exempt by a `*` pattern, machine
//! headers, a token bucket per address, a key in a header of its own), a
//! public script (`/js/d.js`), a live view over server-sent events, the
//! provider forms and "Send test", and the design system's bar list, line
//! chart and code lines on its home page.

use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use askama::Template;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::middleware::{from_fn, from_fn_with_state, map_response};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use deadpool_sqlite::Pool;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::Digest as _;
use tokio::sync::broadcast;
use vole_kit::auth::{CurrentUser, guard, pages};
use vole_kit::components::{BarRow, CodeLine, LineChart, share};
use vole_kit::config::KitConfig;
use vole_kit::db::migrate::{self, MigrateOptions, MigrationError, MigrationSet};
use vole_kit::db::{PoolOptions, build_pool_with, migrate_with};
use vole_kit::docs::{Credential, DocsSpec, HeaderScheme, RATE_LIMITED, r};
use vole_kit::http::assets::StaticFile;
use vole_kit::http::headers::{self, HeaderPolicy};
use vole_kit::http::live::{self, Changes, SharedRenders, StreamLimit};
use vole_kit::http::ratelimit::{Rate, TokenBuckets, too_many_requests};
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::{self, Lang, Tr as _};
use vole_kit::mcp::{Handler, RpcResult};
use vole_kit::notify::alert::Alert;
use vole_kit::notify::form::{FieldView, ProviderGroup, field_views, provider_groups};
use vole_kit::notify::schema::{self, ProviderSchema};
use vole_kit::notify::webhook::Wording;
use vole_kit::page::{NavItem, PageCtx, SettingsTab, Ui};
use vole_kit::state::Core;

/// The demo's version (its own, not the kit's).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn migrations() -> Vec<(String, String)> {
    vec![(
        "0001_notes.sql".into(),
        include_str!("../migrations/0001_notes.sql").into(),
    )]
}

fn no_fixups(_: &mut Connection) -> Result<(), MigrationError> {
    Ok(())
}

static MIGRATIONS: MigrationSet = MigrationSet {
    core: migrate::CORE,
    adopt_core: None,
    migrations,
    fk_off_marker: "-- kd:foreign_keys=off",
    baseline: None,
    after: no_fixups,
};

/// The store's file in the data directory: raw envelopes, disposable.
pub const STORE_FILE: &str = "kit-demo-store.db";

fn store_migrations() -> Vec<(String, String)> {
    vec![(
        "0001_envelopes.sql".into(),
        include_str!("../migrations/store/0001_envelopes.sql").into(),
    )]
}

/// The store's migrations: no core tables, nothing to adopt.
pub static STORE_MIGRATIONS: MigrationSet = MigrationSet {
    core: &[],
    adopt_core: None,
    migrations: store_migrations,
    fk_off_marker: "-- kd:foreign_keys=off",
    baseline: None,
    after: no_fixups,
};

/// The store gives pages back to the file system as it shrinks.
pub const STORE_POOL: PoolOptions = PoolOptions {
    max_size: Some(4),
    pragmas: "",
    new_database_pragmas: "PRAGMA auto_vacuum = INCREMENTAL;",
};

const ERROR_EXAMPLE: &str = r#"{"error": {"code": "not_found", "message": "Not found."}}"#;

fn no_param(_: &vole_kit::docs::RouteDoc, _: &str) -> Option<String> {
    None
}

fn intro(out: &mut String, base: &str) {
    out.push_str(&format!(
        "# Kit Demo API\n\nBase URL: {base}/api/v1. Send `Authorization: Bearer kd_…`.\n\n"
    ));
}

fn nothing(_: &mut String) {}

/// The key ingest clients send in `X-Demo-Key`.
pub const INGEST_KEY: &str = "demo-ingest-key";

/// How many envelopes one address may send: five at once, five a minute.
pub const INGEST_RATE: Rate = Rate::per_minute(5);

static DOCS: DocsSpec = DocsSpec {
    routes: &[
        r("GET", "/api/v1/notes", "viewer", "Notes", "List the notes.").res(r#"{"notes": []}"#),
        r("POST", "/api/{project}/envelope/", "ingest key", "Ingest", "Stores one envelope (newline-delimited JSON) for a project, whose live view updates. 429 with `Retry-After` when an address sends more than five a minute.")
            .body_type("application/x-ndjson")
            .req("{\"event\": \"signup\"}\n{\"event\": \"login\"}")
            .res(r#"{"id": 1}"#),
        r("GET", "/js/d.js", "public", "Ingest", "The script a page embeds; any site may load it."),
    ],
    openapi_title: "Kit Demo API",
    openapi_description: "REST API for Kit Demo. Authenticate with `Authorization: Bearer kd_…`.",
    bearer_description: "API token (kd_…)",
    credentials: &[Credential {
        access: "ingest key",
        words: "the ingest key in `X-Demo-Key` (no API token)",
        bearer: None,
        header: Some(HeaderScheme {
            name: "ingestKey",
            header: "X-Demo-Key",
            description: "Ingest key",
            example: "$KD_INGEST_KEY",
        }),
    }],
    error_example: ERROR_EXAMPLE,
    error_codes: &[RATE_LIMITED],
    config_document: None,
    curl_path_param: no_param,
    markdown_intro: intro,
    markdown_middle: nothing,
    markdown_outro: nothing,
    page_subtitle: "demo.welcome",
    openapi_path: "/api/v1/openapi.json",
    api_base: "/api/v1",
    example_path: "/api/v1/notes",
    metrics_path: None,
    fallback_host: Some("127.0.0.1:3101"),
    // Projects are named by their slug.
    param_types: &[("project", "string")],
    strict_access: true,
};

const LOGO: &[u8] = include_bytes!("../static/logo.svg");
static LOGO_SHA: LazyLock<[u8; 32]> = LazyLock::new(|| sha2::Sha256::digest(LOGO).into());

/// The demo's one static file (the design system is the kit's).
fn static_file(path: &str) -> Option<StaticFile> {
    (path == "logo.svg").then(|| StaticFile {
        data: Cow::Borrowed(LOGO),
        sha256: *LOGO_SHA,
    })
}

fn no_exempt_path(_: &str) -> bool {
    false
}

pub static KIT: KitConfig = KitConfig {
    product: "Kit Demo",
    slug: "kit-demo",
    short_name: "Demo",
    session_cookie: "kd_session",
    pending_2fa_cookie: "kd_2fa",
    token_prefix: "kd_",
    other_token_prefixes: &[],
    version: VERSION,
    user_agent: concat!("KitDemo/", env!("CARGO_PKG_VERSION")),
    backup_prefix: "kit-demo-before-",
    flash_cookie: "kd_flash",
    env_prefix: "KD",
    // Ingest clients post from anywhere with a key, never with a cookie.
    csrf_exempt_prefixes: &["/api/v1", "/mcp", "/api/*/envelope"],
    csrf_exempt_path: no_exempt_path,
    extra_common_passwords: &["kitdemo"],
    migrations: &MIGRATIONS,
    embeddable_prefixes: &[],
    static_file,
    machine_path_prefixes: &["/static/", "/api/", "/js/"],
    machine_paths: &["/healthz"],
    browser_language_prefixes: &[],
    docs: &DOCS,
    locales: &[
        ("en", include_str!("../locales/en.toml")),
        ("nb", include_str!("../locales/nb.toml")),
    ],
    ui: &Ui {
        logo: "/static/logo.svg",
        favicon: "/static/logo.svg",
        apple_touch_icon: "",
        manifest: "",
        nav: &[
            NavItem {
                id: "home",
                href: "/",
                label: "demo.notes",
                short_label: "demo.notes",
                icon: "pulse",
            },
            NavItem {
                id: "settings",
                href: "/settings/security",
                label: "nav.settings",
                short_label: "nav.settings",
                icon: "gear",
            },
        ],
        settings_tabs: &[
            SettingsTab::USERS,
            SettingsTab::SECURITY,
            SettingsTab::TOKENS,
        ],
        home_url: "https://example.com/kit-demo",
    },
    public_url_setting: None,
    webhook: Wording {
        subject: "note",
        duration: "duration",
        url_example: "https://example.com/hooks/notes…",
    },
};

/// Installs [`KIT`]; call before anything else (tests too).
pub fn install() {
    vole_kit::config::install(&KIT);
}

/// Everything the demo runs on: the kit's state and the demo's own parts.
#[derive(Clone)]
pub struct Demo {
    pub core: Core,
    /// The second database.
    pub store: Pool,
    /// Which project got an envelope (the live views listen).
    pub changes: broadcast::Sender<String>,
    /// Envelopes per client address.
    pub limits: Arc<TokenBuckets<IpAddr>>,
}

/// Opens (and migrates) both databases in `data_dir` and returns the state.
pub async fn start(data_dir: &Path) -> Result<Demo, migrate::UpgradeError> {
    install();
    std::fs::create_dir_all(data_dir).expect("data dir");
    let pool = vole_kit::db::build_pool(&data_dir.join("kit-demo.db"));
    migrate::migrate(&pool, &data_dir.join("backups")).await?;
    let store = open_store(data_dir).await?;
    Ok(Demo {
        core: Core {
            db: pool,
            session_key: vole_kit::auth::load_or_create_key(data_dir),
            http: reqwest::Client::new(),
            cfg: &KIT,
        },
        store,
        changes: broadcast::channel(64).0,
        limits: Arc::new(TokenBuckets::new()),
    })
}

/// Opens and migrates the store: its own migration set, no backup (losing
/// it loses raw envelopes, nothing else), and its own backup-free newer
/// schema message.
pub async fn open_store(data_dir: &Path) -> Result<Pool, migrate::UpgradeError> {
    let store = build_pool_with(&data_dir.join(STORE_FILE), STORE_POOL);
    migrate_with(&store, &STORE_MIGRATIONS, MigrateOptions::default()).await?;
    Ok(store)
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    ctx: PageCtx,
    notes: i64,
    store_bytes: i64,
    share: f64,
    projects: Vec<BarRow>,
    chart: LineChart,
    code: Vec<CodeLine>,
}

async fn home(
    State(core): State<Core>,
    Extension(demo): Extension<Demo>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    let notes = core
        .with_conn(|c| c.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0)))
        .await?;
    let (store_bytes, counts) = vole_kit::db::interact(&demo.store, |c| {
        let bytes: i64 = c.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |r| r.get(0),
        )?;
        let counts: Vec<(String, i64)> = c
            .prepare("SELECT project, COUNT(*) FROM envelopes GROUP BY project ORDER BY 2 DESC, 1")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok((bytes, counts))
    })
    .await?;
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    let projects: Vec<BarRow> = counts
        .iter()
        .map(|(project, n)| {
            let part = share(*n as f64, total as f64);
            BarRow::new(project.clone(), i18n::fmt_int(*n), part)
                .href(format!("/projects/{project}"))
                .detail(i18n::fmt_pct0(part))
        })
        .collect();
    let chart = LineChart::new(
        i18n::fmt_last_days(7),
        vec![
            (
                i18n::t("demo.notes").to_string(),
                vec![1.0, 3.0, 2.0, 5.0, 4.0, 6.0, notes as f64],
                i18n::fmt_int(notes),
            ),
            (
                i18n::t("demo.envelopes").to_string(),
                vec![0.0, 1.0, 1.0, 2.0, 3.0, 2.0, total as f64],
                i18n::fmt_int(total),
            ),
        ],
        true,
    )
    .scale(i18n::fmt_int(6.max(notes).max(total)), i18n::fmt_int(0))
    .axis(i18n::fmt_last_days(7), i18n::t("common.today"));
    let code = CodeLine::numbered(
        "curl -X POST \"$KD_URL/api/notes/envelope/\" \\\n  -H \"X-Demo-Key: $KD_INGEST_KEY\" \\\n  --data-binary @envelope.ndjson",
        1,
        Some(2),
    );
    let (ctx, jar) = PageCtx::build(&core, Some(&user), &jar, "Kit Demo", "home", "").await;
    Ok((
        jar,
        HtmlTemplate(HomeTemplate {
            ctx,
            notes,
            store_bytes,
            share: share(notes as f64, (notes + total).max(1) as f64),
            projects,
            chart,
            code,
        }),
    )
        .into_response())
}

async fn notes(State(core): State<Core>, _user: CurrentUser) -> ApiResult<Json<Value>> {
    let bodies: Vec<String> = core
        .with_conn(|c| {
            c.prepare("SELECT body FROM notes ORDER BY id")?
                .query_map([], |r| r.get(0))?
                .collect()
        })
        .await?;
    Ok(Json(json!({ "notes": bodies })))
}

/// `POST /api/{project}/envelope/`: stores the body in the store and tells
/// the project's live views. Programs call it with the ingest key, from
/// anywhere: no CSRF proof, no cookie, the machine header policy, and a
/// budget per client address.
async fn envelope(
    Extension(demo): Extension<Demo>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    UrlPath(project): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let key = headers.get("x-demo-key").and_then(|v| v.to_str().ok());
    if key != Some(INGEST_KEY) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "detail": "missing or unknown X-Demo-Key" })),
        )
            .into_response();
    }
    if let Err(wait) = demo.limits.take(&peer.ip(), INGEST_RATE, Instant::now()) {
        return too_many_requests(wait);
    }
    let slug = project.clone();
    let stored = vole_kit::db::interact(&demo.store, move |c| {
        c.execute(
            "INSERT INTO envelopes (project, body) VALUES (?1, ?2)",
            rusqlite::params![slug, body.to_vec()],
        )?;
        Ok(c.last_insert_rowid())
    })
    .await;
    match stored {
        Ok(id) => {
            let _ = demo.changes.send(project);
            (StatusCode::ACCEPTED, Json(json!({ "id": id }))).into_response()
        }
        Err(err) => ApiError::Internal(err.to_string()).into_response(),
    }
}

/// The snippet other sites load.
async fn snippet() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        "/* Kit Demo */\n",
    )
        .into_response()
}

/// At most this many live views at once.
static STREAMS: StreamLimit = StreamLimit::new(32);

/// One render per (project, language) and change, however many views show it.
static RENDERS: LazyLock<SharedRenders<(String, Lang)>> = LazyLock::new(|| SharedRenders::new(32));

/// How long a burst of envelopes is merged into one update.
pub const LIVE_QUIET: Duration = Duration::from_millis(50);

async fn envelope_count(store: &Pool, project: &str) -> Result<String, ApiError> {
    let slug = project.to_string();
    let n: i64 = vole_kit::db::interact(store, move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM envelopes WHERE project = ?1",
            [slug],
            |r| r.get(0),
        )
    })
    .await?;
    Ok(format!(
        "<p id=\"envelopes\">{}</p>",
        i18n::tn("count.envelopes", n)
    ))
}

/// `/projects/{project}/live`: the project's envelope count, pushed after
/// every burst of envelopes.
async fn project_live(
    Extension(demo): Extension<Demo>,
    _user: CurrentUser,
    UrlPath(project): UrlPath<String>,
) -> Response {
    let Some(slot) = STREAMS.acquire() else {
        return live::busy();
    };
    let lang = i18n::current();
    let wanted = project.clone();
    let changes = Changes::new(demo.changes.subscribe(), move |p: &String| *p == wanted)
        .quiet(LIVE_QUIET)
        .lifetime(Duration::from_secs(60 * 60));
    let state = (changes, demo, project, slot);
    live::sse(futures_util::stream::unfold(
        state,
        move |(mut changes, demo, project, slot)| async move {
            let since = changes.next().await?;
            let render = RENDERS.get((project.clone(), lang), since, || {
                envelope_count(&demo.store, &project)
            });
            let html = i18n::scope(lang, render).await.ok()?;
            let event = live::html_event("envelopes-updated", &html);
            Some((event, (changes, demo, project, slot)))
        },
    ))
}

// -- Notification channels: the kit's provider picker and forms -------------

#[derive(Template)]
#[template(path = "kit/partials/notification_picker.html")]
struct PickerPartial {
    query: String,
    groups: Vec<ProviderGroup>,
}

#[derive(Template)]
#[template(path = "kit/partials/notification_provider_grid.html")]
struct GridPartial {
    query: String,
    groups: Vec<ProviderGroup>,
}

/// What the kit's name and fields partials need.
pub struct ChannelForm {
    pub kind: &'static str,
    pub label: &'static str,
    pub name: String,
    pub name_error: String,
    pub fields: Vec<FieldView>,
}

#[derive(Template)]
#[template(path = "channel_form.html")]
struct FormPartial {
    form: ChannelForm,
}

#[derive(Deserialize)]
struct ProvidersQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    grid: bool,
}

async fn providers(user: CurrentUser, Query(q): Query<ProvidersQuery>) -> ApiResult<Response> {
    user.require_editor()?;
    let groups = provider_groups(&q.q);
    Ok(if q.grid {
        HtmlTemplate(GridPartial { query: q.q, groups }).into_response()
    } else {
        HtmlTemplate(PickerPartial { query: q.q, groups }).into_response()
    })
}

#[derive(Deserialize)]
struct KindQuery {
    #[serde(default)]
    kind: String,
}

fn provider(kind: &str) -> ApiResult<&'static ProviderSchema> {
    vole_kit::notify::provider(kind)
        .ok_or_else(|| ApiError::bad_request(i18n::t("notif.err_service")))
}

async fn channel_form(user: CurrentUser, Query(q): Query<KindQuery>) -> ApiResult<Response> {
    user.require_editor()?;
    let schema = provider(&q.kind)?;
    Ok(HtmlTemplate(FormPartial {
        form: ChannelForm {
            kind: schema.kind,
            label: schema.tr_label(),
            name: String::new(),
            name_error: String::new(),
            fields: field_views(schema, &Value::Null, &Value::Null, &[], true),
        },
    })
    .into_response())
}

/// "Send test" from the add form: the kit's test message in the language
/// the form asks for (`language`, else the request's).
async fn send_test(
    State(core): State<Core>,
    user: CurrentUser,
    body: Bytes,
) -> ApiResult<Response> {
    user.require_editor()?;
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let pair = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let schema = provider(pair("kind").unwrap_or_default())?;
    let parsed = schema::parse_form(schema, &pairs);
    let toast = |message: &str, kind: &str| {
        (
            HeaderName::from_static("hx-trigger"),
            vole_kit::http::toast_trigger(message, kind, false),
        )
    };
    if let Err(errors) = schema::validate(schema, &parsed.config) {
        return Ok(([toast(&errors[0].1, "error")], StatusCode::OK).into_response());
    }
    let lang = pair("language")
        .and_then(Lang::from_exact_code)
        .unwrap_or_else(i18n::current);
    let sent = vole_kit::notify::send_in(&core.http, schema.kind, &parsed.config, lang, || {
        Alert::test(
            i18n::tf("demo.test_title", &[("product", KIT.product)]),
            i18n::t("demo.test_body"),
            i18n::t("demo.test_event"),
            i18n::t("demo.view_notes"),
            None,
        )
    })
    .await;
    Ok(match sent {
        Ok(()) => (
            [toast(
                &i18n::tf("notif.test_sent", &[("label", schema.tr_label())]),
                "success",
            )],
            StatusCode::OK,
        )
            .into_response(),
        Err(err) => {
            let message = i18n::tf("notif.test_failed", &[("err", &err)]);
            ([toast(&message, "error")], message).into_response()
        }
    })
}

/// MCP with no tools: enough to show the kit's protocol layer announces
/// the demo, not Vigilant Vole.
struct Mcp {
    can_write: bool,
}

impl Handler for Mcp {
    fn can_write(&self) -> bool {
        self.can_write
    }

    fn instructions(&self) -> String {
        "Kit Demo keeps notes. It has no tools yet.".into()
    }

    fn tools(&self) -> Vec<Value> {
        Vec::new()
    }

    async fn call_tool(&self, name: &str, _args: Map<String, Value>) -> RpcResult {
        Err((-32602, format!("Unknown tool {name}.")))
    }

    async fn list_resources(&self) -> RpcResult {
        Ok(json!({ "resources": [] }))
    }

    fn resource_templates(&self) -> Value {
        json!([])
    }

    async fn read_resource(&self, uri: &str) -> RpcResult {
        Err((-32002, format!("Unknown resource {uri}.")))
    }
}

async fn mcp(State(core): State<Core>, headers: HeaderMap, body: Bytes) -> Response {
    let caller = match vole_kit::mcp::authenticate(&core, &headers, 0).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    vole_kit::mcp::serve(
        &Mcp {
            can_write: caller.can_write,
        },
        &body,
    )
    .await
}

/// The whole app: the demo's routes, the kit's pages, static files and
/// MCP, behind the kit's middleware.
pub fn router(demo: &Demo) -> Router {
    let core = demo.core.clone();
    let protected = Router::new()
        .route("/", get(home))
        .route("/api/v1/notes", get(notes))
        .route("/projects/{project}/live", get(project_live))
        .route("/settings/notifications/providers", get(providers))
        .route("/settings/notifications/form", get(channel_form))
        .route("/settings/notifications/test", post(send_test))
        .merge(pages::router())
        .route_layer(from_fn_with_state(core.clone(), guard::require_auth));
    // Programs, not browsers: the machine headers on every answer.
    let ingest = Router::new()
        .route("/api/{project}/envelope/", post(envelope))
        .route_layer(map_response(headers::machine));
    // Loaded by other sites.
    let assets = Router::new()
        .route("/js/d.js", get(snippet))
        .route_layer(map_response(headers::public_asset));
    let public = Router::new()
        .route("/healthz", get(vole_kit::http::healthz))
        .route("/static/{*path}", get(vole_kit::http::assets::static_asset))
        .route(
            "/mcp",
            post(mcp).layer(map_response(|res: Response| async move {
                (HeaderPolicy::Machine, res).into_response()
            })),
        )
        .merge(pages::public_router())
        .merge(ingest)
        .merge(assets);
    protected
        .merge(public)
        .fallback(vole_kit::http::error::not_found)
        .layer(Extension(demo.clone()))
        .layer(from_fn_with_state(core.clone(), guard::csrf_guard))
        .layer(from_fn_with_state(core.clone(), vole_kit::i18n::layer))
        .layer(from_fn(vole_kit::http::error::negotiate))
        .layer(from_fn(vole_kit::http::headers::security_headers))
        .with_state(core)
}
