//! Kit Demo: the smallest product built on vole-kit. It has its own name,
//! cookies, token prefix, version, one migration and one string; everything
//! else (sign-in, users, tokens, Security, the API reference, the layout and
//! the design system) is the kit's, reached the way any product reaches it:
//! templates through `vole-kit-build`, pages through
//! `vole_kit::auth::pages`. The tests in `tests/` drive it.

use std::borrow::Cow;
use std::path::Path;
use std::sync::LazyLock;

use askama::Template;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use sha2::Digest as _;
use vole_kit::auth::{CurrentUser, guard, pages};
use vole_kit::config::KitConfig;
use vole_kit::db::migrate::{self, MigrationError, MigrationSet};
use vole_kit::docs::{DocsSpec, r};
use vole_kit::http::assets::StaticFile;
use vole_kit::http::{ApiResult, HtmlTemplate};
use vole_kit::i18n::Tr as _;
use vole_kit::mcp::{Handler, RpcResult};
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
    ],
    openapi_title: "Kit Demo API",
    openapi_description: "REST API for Kit Demo. Authenticate with `Authorization: Bearer kd_…`.",
    bearer_description: "API token (kd_…)",
    agent_token_description: "",
    error_example: ERROR_EXAMPLE,
    config_file: "kit-demo.yml",
    agent_token_access: "",
    curl_path_param: no_param,
    markdown_intro: intro,
    markdown_middle: nothing,
    markdown_outro: nothing,
    page_subtitle: "demo.welcome",
    openapi_path: "/api/v1/openapi.json",
    api_base: "/api/v1",
    example_path: "/api/v1/notes",
    metrics_path: None,
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
    locales: &[("en", include_str!("../locales/en.toml"))],
    ui: &Ui {
        logo: "/static/logo.svg",
        favicon: "/static/logo.svg",
        apple_touch_icon: "",
        manifest: "",
        nav: &[
            NavItem {
                id: "home",
                href: "/",
                label: "nav.dashboard",
                short_label: "nav.dashboard",
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
    Ok(Core {
        db: pool,
        session_key: vole_kit::auth::load_or_create_key(data_dir),
        http: reqwest::Client::new(),
        cfg: &KIT,
    })
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
        .route("/api/v1/notes", get(notes))
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
