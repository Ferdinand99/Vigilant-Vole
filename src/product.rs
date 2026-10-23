//! What makes this build Vigilant Vole, as far as the shared kit
//! (`crate::kit`) is concerned.

use crate::kit::config::KitConfig;
use crate::kit::notify::webhook::Wording;
use crate::kit::page::{NavItem, SettingsTab, TabAccess, Ui};

/// The product's name.
pub const NAME: &str = "Vigilant Vole";
/// `User-Agent` of outgoing requests.
pub const USER_AGENT: &str = concat!("VigilantVole/", env!("CARGO_PKG_VERSION"));
/// Name of the signed session cookie.
pub const SESSION_COOKIE: &str = "vv_session";
/// Cookie between password and second factor.
pub const PENDING_2FA_COOKIE: &str = "vv_2fa";
/// Cookie carrying one-shot flash messages.
pub const FLASH_COOKIE: &str = "vv_flash";
/// Prefix of API tokens.
pub const TOKEN_PREFIX: &str = "vv_";

/// `/status/{slug}/subscribe/unsubscribe/{token}`: mail clients POST this
/// link from the `List-Unsubscribe` header (RFC 8058 one-click) without an
/// `Origin` or `Referer`. The secret token in the path is the proof, and the
/// session cookie plays no part.
fn csrf_exempt_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    matches!(
        parts.as_slice(),
        ["", "status", slug, "subscribe", "unsubscribe", token] if !slug.is_empty() && !token.is_empty()
    )
}

pub static KIT: KitConfig = KitConfig {
    product: NAME,
    slug: "vigilant-vole",
    short_name: "Vole",
    session_cookie: SESSION_COOKIE,
    pending_2fa_cookie: PENDING_2FA_COOKIE,
    token_prefix: TOKEN_PREFIX,
    other_token_prefixes: &["vva_"],
    version: env!("CARGO_PKG_VERSION"),
    releases_url: Some("https://github.com/Ferdinand99/Vigilant-Vole/releases/tag/v{version}"),
    user_agent: USER_AGENT,
    backup_prefix: "vigilant-vole-before-",
    flash_cookie: FLASH_COOKIE,
    env_prefix: "VV",
    csrf_exempt_prefixes: &[
        "/api/push",
        "/api/agent",
        "/api/v1",
        "/api/badge",
        "/metrics",
        "/mcp",
    ],
    csrf_exempt_path,
    extra_common_passwords: &["uptimekuma", "vigilantvole"],
    migrations: &crate::db::MIGRATIONS,
    embeddable_prefixes: &["/status/", "/api/status-page/"],
    static_file: crate::api::static_file,
    machine_path_prefixes: &["/static/", "/api/"],
    machine_paths: &["/metrics", "/healthz"],
    browser_language_prefixes: &["/status/"],
    docs: &crate::api::v1::DOCS,
    locales: &[
        ("en", include_str!("../locales/en.toml")),
        ("nb", include_str!("../locales/nb.toml")),
        ("ar", include_str!("../locales/ar.toml")),
        ("cs", include_str!("../locales/cs.toml")),
        ("da", include_str!("../locales/da.toml")),
        ("de", include_str!("../locales/de.toml")),
        ("es", include_str!("../locales/es.toml")),
        ("fa", include_str!("../locales/fa.toml")),
        ("fi", include_str!("../locales/fi.toml")),
        ("fr", include_str!("../locales/fr.toml")),
        ("he", include_str!("../locales/he.toml")),
        ("hu", include_str!("../locales/hu.toml")),
        ("it", include_str!("../locales/it.toml")),
        ("ja", include_str!("../locales/ja.toml")),
        ("ko", include_str!("../locales/ko.toml")),
        ("nl", include_str!("../locales/nl.toml")),
        ("pl", include_str!("../locales/pl.toml")),
        ("pt-BR", include_str!("../locales/pt-BR.toml")),
        ("pt-PT", include_str!("../locales/pt-PT.toml")),
        ("ro", include_str!("../locales/ro.toml")),
        ("ru", include_str!("../locales/ru.toml")),
        ("sv", include_str!("../locales/sv.toml")),
        ("tr", include_str!("../locales/tr.toml")),
        ("uk", include_str!("../locales/uk.toml")),
        ("zh-CN", include_str!("../locales/zh-CN.toml")),
        ("zh-TW", include_str!("../locales/zh-TW.toml")),
    ],
    ui: &Ui {
        logo: "/static/favicon.svg",
        favicon: "/static/favicon.svg",
        apple_touch_icon: "/static/apple-touch-icon.png",
        manifest: "/static/site.webmanifest",
        nav: &[
            NavItem {
                id: "dashboard",
                href: "/",
                label: "nav.dashboard",
                short_label: "nav.dashboard",
                icon: "pulse",
            },
            NavItem {
                id: "status-pages",
                href: "/status-pages",
                label: "nav.status_pages",
                short_label: "nav.status_short",
                icon: "globe",
            },
            NavItem {
                id: "maintenance",
                href: "/maintenance",
                label: "nav.maintenance",
                short_label: "nav.maintenance",
                icon: "wrench",
            },
            NavItem {
                id: "settings",
                href: "/settings",
                label: "nav.settings",
                short_label: "nav.settings",
                icon: "gear",
            },
        ],
        settings_tabs: &[
            // Served by our own page, under the kit's tab.
            SettingsTab::GENERAL,
            SettingsTab {
                id: "notifications",
                href: "/settings/notifications",
                label: "nav.notifications",
                icon: "bell",
                access: TabAccess::Editors,
            },
            SettingsTab::USERS,
            SettingsTab::SECURITY,
            SettingsTab::TOKENS,
            SettingsTab {
                id: "agents",
                href: "/settings/agents",
                label: "nav.agents",
                icon: "server",
                access: TabAccess::Admins,
            },
            SettingsTab {
                id: "integrations",
                href: "/settings/integrations",
                label: "nav.integrations",
                icon: "plug",
                access: TabAccess::Admins,
            },
            SettingsTab {
                id: "browsers",
                href: "/settings/browsers",
                label: "nav.browsers",
                icon: "browser",
                access: TabAccess::Admins,
            },
            SettingsTab {
                id: "backup",
                href: "/settings/backup",
                label: "nav.backup",
                icon: "archive",
                access: TabAccess::Admins,
            },
        ],
        home_url: "https://github.com/Ferdinand99/Vigilant-Vole",
    },
    public_url_setting: Some("Settings → General"),
    webhook: Wording {
        subject: "monitor",
        duration: "downtime",
        url_example: "https://example.com/hooks/vigilant…",
    },
    // No product token scopes: tokens are read-only or read-and-write.
    token_scopes: &[],
    // Our General page is our own (`api::settings`): it has fields the
    // kit's doesn't (retention, public badges, agent confirmation).
    general: None,
};

/// Installs [`KIT`] as the kit's configuration. Runs at start-up (`main`,
/// [`crate::server::prepare`]) and before every test binary's tests.
pub fn install() {
    crate::kit::config::install(&KIT);
}
