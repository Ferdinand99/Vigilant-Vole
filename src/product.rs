//! What makes this build Vigilant Vole, as far as the shared kit
//! (`crate::kit`) is concerned.

use crate::kit::config::KitConfig;

pub static KIT: KitConfig = KitConfig {
    product: "Vigilant Vole",
    slug: "vigilant-vole",
    session_cookie: "vv_session",
    pending_2fa_cookie: "vv_2fa",
    token_prefix: "vv_",
    other_token_prefixes: &["vva_"],
    user_agent: concat!("VigilantVole/", env!("CARGO_PKG_VERSION")),
    backup_prefix: "vigilant-vole-before-",
    migrations: &crate::db::MIGRATIONS,
    embeddable_prefixes: &["/status/", "/api/status-page/"],
    static_file: crate::api::static_file,
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
};

crate::register_config!(KIT);
