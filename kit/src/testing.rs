//! The product configuration the kit's own tests run with: Vigilant Vole's
//! names, translations and migrations, read from the repository (the kit is
//! a member of the Vigilant Vole workspace). Tests only.

use rusqlite::Connection;
use rust_embed::Embed;

use crate::config::KitConfig;
use crate::db::migrate::{Baseline, MigrationError, MigrationSet};
use crate::docs::{DocsSpec, RouteDoc};
use crate::page::{NavItem, SettingsTab, TabAccess, Ui};

#[derive(Embed)]
#[folder = "../migrations/"]
struct Migrations;

fn migration_files() -> Vec<(String, String)> {
    let mut names: Vec<_> = Migrations::iter().filter(|n| n.ends_with(".sql")).collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let file = Migrations::get(&name).expect("embedded migration missing");
            let sql = String::from_utf8(file.data.into_owned()).expect("migration is not utf8");
            (name.into_owned(), sql)
        })
        .collect()
}

fn no_fixups(_: &mut Connection) -> Result<(), MigrationError> {
    Ok(())
}

static MIGRATIONS: MigrationSet = MigrationSet {
    core: crate::db::migrate::CORE,
    adopt_core: Some("0104_adopt_kit_core.sql"),
    migrations: migration_files,
    fk_off_marker: "-- vv:foreign_keys=off",
    baseline: Some(Baseline {
        name: "0001_init.sql",
        tables: &["users", "monitors"],
    }),
    after: no_fixups,
};

fn no_markdown(_: &mut String) {}
fn no_intro(_: &mut String, _: &str) {}
fn no_param(_: &RouteDoc, _: &str) -> Option<String> {
    None
}

static DOCS: DocsSpec = DocsSpec {
    routes: &[],
    openapi_title: "Test API",
    openapi_description: "",
    bearer_description: "API token (vv_…)",
    agent_token_description: "Remote agent token (vva_…)",
    error_example: "{}",
    config_file: "test.yml",
    agent_token_access: "agent token",
    curl_path_param: no_param,
    markdown_intro: no_intro,
    markdown_middle: no_markdown,
    markdown_outro: no_markdown,
    page_subtitle: "docs.sub",
    openapi_path: "/api/v1/openapi.json",
    api_base: "/api/v1",
    example_path: "/api/v1/monitors",
    metrics_path: Some("/metrics"),
};

fn csrf_exempt_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    matches!(
        parts.as_slice(),
        ["", "status", slug, "subscribe", "unsubscribe", token] if !slug.is_empty() && !token.is_empty()
    )
}

macro_rules! locales {
    ($($code:literal),*) => {
        &[$(($code, include_str!(concat!("../../locales/", $code, ".toml")))),*]
    };
}

pub static CONFIG: KitConfig = KitConfig {
    product: "Vigilant Vole",
    slug: "vigilant-vole",
    short_name: "Vole",
    session_cookie: "vv_session",
    pending_2fa_cookie: "vv_2fa",
    token_prefix: "vv_",
    other_token_prefixes: &["vva_"],
    version: env!("CARGO_PKG_VERSION"),
    user_agent: "VigilantVole/test",
    backup_prefix: "vigilant-vole-before-",
    flash_cookie: "vv_flash",
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
    migrations: &MIGRATIONS,
    embeddable_prefixes: &["/status/", "/api/status-page/"],
    static_file: |_| None,
    machine_path_prefixes: &["/static/", "/api/"],
    machine_paths: &["/metrics", "/healthz"],
    browser_language_prefixes: &["/status/"],
    docs: &DOCS,
    locales: locales!(
        "en", "nb", "ar", "cs", "da", "de", "es", "fa", "fi", "fr", "he", "hu", "it", "ja", "ko",
        "nl", "pl", "pt-BR", "pt-PT", "ro", "ru", "sv", "tr", "uk", "zh-CN", "zh-TW"
    ),
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
            SettingsTab {
                id: "general",
                href: "/settings/general",
                label: "nav.general",
                icon: "sliders",
                access: TabAccess::Admins,
            },
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
};
