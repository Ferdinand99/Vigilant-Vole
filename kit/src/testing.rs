//! The product configuration the kit's own tests run with: Vigilant Vole's
//! names, translations and migrations, read from the repository (the kit is
//! a member of the Vigilant Vole workspace). Tests only.

use rusqlite::Connection;
use rust_embed::Embed;

use crate::config::KitConfig;
use crate::db::migrate::{Baseline, MigrationError, MigrationSet};
use crate::docs::{DocsSpec, RouteDoc};

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
    session_cookie: "vv_session",
    pending_2fa_cookie: "vv_2fa",
    token_prefix: "vv_",
    other_token_prefixes: &["vva_"],
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
    subject_type_label: |t| crate::i18n::t_or(&format!("monitor_type.{t}"), t).to_string(),
    locales: locales!(
        "en", "nb", "ar", "cs", "da", "de", "es", "fa", "fi", "fr", "he", "hu", "it", "ja", "ko",
        "nl", "pl", "pt-BR", "pt-PT", "ro", "ru", "sv", "tr", "uk", "zh-CN", "zh-TW"
    ),
};

crate::register_config!(CONFIG);
