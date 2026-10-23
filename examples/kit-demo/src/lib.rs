//! Kit Demo: the smallest product built on vole-kit. It has its own name,
//! cookies, token prefix, version, one migration and a few strings (its own,
//! and the texts the kit's pages take from a product), two token scopes of
//! its own and two fields on the kit's General page; everything else
//! (sign-in, users, tokens, Security, General, the API reference, the
//! layout and the design system) is the kit's, reached the way any product
//! reaches it: templates through `vole-kit-build`, pages through
//! `vole_kit::auth::pages`. The tests in `tests/` drive it.

use std::borrow::Cow;
use std::path::Path;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use askama::Template;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use sha2::Digest as _;
use vole_kit::auth::{CurrentUser, ExtraScope, Role, guard, pages};
use vole_kit::config::KitConfig;
use vole_kit::db::migrate::{self, MigrationError, MigrationSet};
use vole_kit::docs::{DocsSpec, r};
use vole_kit::http::assets::StaticFile;
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::Tr as _;
use vole_kit::mcp::{Handler, RpcResult};
use vole_kit::page::{NavItem, PageCtx, SettingsTab, Ui};
use vole_kit::settings::{FieldKind, GeneralSettings, SettingField, SettingsSection};
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

static DOCS: DocsSpec = DocsSpec {
    routes: &[
        r("GET", "/api/v1/notes", "viewer", "Notes", "List the notes.").res(r#"{"notes": []}"#),
        r("POST", "/api/v1/notes", "editor", "Notes", "Add a note: an editor's session or read-and-write token, or a token with the notes:add scope of an editor.")
            .req(r#"{"body": "Buy milk"}"#),
        r("POST", "/api/v1/inbox", "editor", "Notes", "Drop a note in the inbox: a token with the inbox scope (which can't read) of an editor.")
            .req(r#"{"body": "Buy milk"}"#),
    ],
    openapi_title: "Kit Demo API",
    openapi_description: "REST API for Kit Demo. Authenticate with `Authorization: Bearer kd_…`.",
    bearer_description: "API token (kd_…)",
    credentials: &[],
    error_example: ERROR_EXAMPLE,
    error_codes: &[],
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

/// Add notes (`POST /api/v1/notes`) with a token that otherwise only reads.
pub const SCOPE_ADD: &str = "notes:add";
/// Drop notes in the inbox (`POST /api/v1/inbox`) and nothing else: the
/// token can't even read.
pub const SCOPE_INBOX: &str = "inbox";

/// How often the General page saved (its `after_save` hook).
pub static GENERAL_SAVES: AtomicUsize = AtomicUsize::new(0);

fn general_saved(_: &Core) {
    GENERAL_SAVES.fetch_add(1, Ordering::SeqCst);
}

/// The kit's General page with a section of the demo's own.
static GENERAL: GeneralSettings = GeneralSettings {
    sections: &[SettingsSection {
        title: "demo.section_notes",
        fields: &[
            SettingField {
                key: "notes_per_page",
                label: "demo.per_page",
                help: "demo.per_page_help",
                kind: FieldKind::Number {
                    min: 5,
                    max: 100,
                    unit: "demo.notes_unit",
                },
                default: "20",
                error: "demo.err_per_page",
            },
            SettingField {
                key: "notes_public",
                label: "demo.public",
                help: "demo.public_help",
                kind: FieldKind::Switch,
                default: "0",
                error: "",
            },
        ],
    }],
    after_save: general_saved,
};

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
    csrf_exempt_prefixes: &["/api/v1", "/mcp"],
    csrf_exempt_path: no_exempt_path,
    extra_common_passwords: &["kitdemo"],
    migrations: &MIGRATIONS,
    embeddable_prefixes: &[],
    static_file,
    machine_path_prefixes: &["/static/", "/api/"],
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
            SettingsTab::GENERAL,
            SettingsTab::USERS,
            SettingsTab::SECURITY,
            SettingsTab::TOKENS,
        ],
        home_url: "https://example.com/kit-demo",
    },
    public_url_setting: None,
    token_scopes: &[
        ExtraScope {
            name: SCOPE_ADD,
            label: "demo.scope_add",
            help: "demo.scope_add_help",
            implies_read: true,
            paths: &[],
        },
        ExtraScope {
            name: SCOPE_INBOX,
            label: "demo.scope_inbox",
            help: "demo.scope_inbox_help",
            implies_read: false,
            paths: &["/api/v1/inbox"],
        },
    ],
    general: Some(&GENERAL),
};

/// Installs [`KIT`]; call before anything else (tests too).
pub fn install() {
    vole_kit::config::install(&KIT);
}

/// Opens (and migrates) the database in `data_dir` and returns the state.
pub async fn start(data_dir: &Path) -> Result<Core, migrate::UpgradeError> {
    install();
    std::fs::create_dir_all(data_dir).expect("data dir");
    let pool = vole_kit::db::build_pool(&data_dir.join("kit-demo.db"));
    migrate::migrate(&pool, &data_dir.join("backups")).await?;
    Ok(Core::new(
        pool,
        vole_kit::auth::load_or_create_key(data_dir),
        reqwest::Client::new(),
        &KIT,
    ))
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    ctx: PageCtx,
    notes: i64,
}

async fn home(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    let notes = core
        .with_conn(|c| c.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0)))
        .await?;
    let (ctx, jar) = PageCtx::build(&core, Some(&user), &jar, "Kit Demo", "home", "").await;
    Ok((jar, HtmlTemplate(HomeTemplate { ctx, notes })).into_response())
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

fn note_body(body: &Value) -> ApiResult<String> {
    body.get("body")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ApiError::bad_request("body is required"))
}

async fn insert_note(core: &Core, user: &CurrentUser, body: String) -> ApiResult<StatusCode> {
    let author = user.id;
    core.with_conn(move |c| {
        c.execute(
            "INSERT INTO notes (author_id, body) VALUES (?1, ?2)",
            rusqlite::params![author, body],
        )
    })
    .await?;
    Ok(StatusCode::CREATED)
}

/// What Keen Kestrel's upload check looks like on the kit: the scope (a
/// session and a read-and-write token have every scope) and the owner's
/// own role, whatever the token caps.
async fn add_note(
    State(core): State<Core>,
    user: CurrentUser,
    Json(body): Json<Value>,
) -> ApiResult<StatusCode> {
    if !(user.has_scope(SCOPE_ADD) && user.may_act_as(Role::Editor)) {
        return Err(ApiError::Forbidden);
    }
    insert_note(&core, &user, note_body(&body)?).await
}

/// Reached by an `inbox` token although it can't read: the path is the
/// scope's.
async fn inbox(
    State(core): State<Core>,
    user: CurrentUser,
    Json(body): Json<Value>,
) -> ApiResult<StatusCode> {
    if !(user.has_scope(SCOPE_INBOX) && user.may_act_as(Role::Editor)) {
        return Err(ApiError::Forbidden);
    }
    insert_note(&core, &user, note_body(&body)?).await
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

/// The whole app: the demo's two routes, the kit's pages, static files and
/// MCP, behind the kit's middleware.
pub fn router(core: Core) -> Router {
    let protected = Router::new()
        .route("/", get(home))
        .route("/api/v1/notes", get(notes).post(add_note))
        .route("/api/v1/inbox", post(inbox))
        .merge(pages::router())
        .route_layer(from_fn_with_state(core.clone(), guard::require_auth));
    let public = Router::new()
        .route("/healthz", get(vole_kit::http::healthz))
        .route("/static/{*path}", get(vole_kit::http::assets::static_asset))
        .route("/mcp", post(mcp))
        .merge(pages::public_router());
    protected
        .merge(public)
        .fallback(vole_kit::http::error::not_found)
        .layer(from_fn_with_state(core.clone(), guard::csrf_guard))
        .layer(from_fn_with_state(core.clone(), vole_kit::i18n::layer))
        .layer(from_fn(vole_kit::http::error::negotiate))
        .layer(from_fn(vole_kit::http::headers::security_headers))
        .with_state(core)
}
