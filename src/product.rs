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
};

crate::register_config!(KIT);
