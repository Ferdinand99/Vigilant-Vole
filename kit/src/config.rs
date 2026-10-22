//! What the kit needs to know about the product it runs inside.
//!
//! The product describes itself once, with a static [`KitConfig`], and
//! hands it over with [`install`] when it starts, before any kit code runs.
//! The kit reads it through [`get`] (or `Core::cfg`). Using the kit before a
//! configuration is installed is a bug and panics with a message that says
//! so; the kit's own tests fall back to their test configuration.

use std::sync::OnceLock;

/// Everything product specific that the kit uses.
pub struct KitConfig {
    /// Display name used in texts and API documents ("Vigilant Vole").
    pub product: &'static str,
    /// Lower-case name for file names and the like ("vigilant-vole").
    pub slug: &'static str,
    /// Name of the signed session cookie.
    pub session_cookie: &'static str,
    /// Name of the short-lived cookie between password and second factor.
    pub pending_2fa_cookie: &'static str,
    /// Prefix of API tokens ("vv_"); the kit generates and recognises these.
    pub token_prefix: &'static str,
    /// Further token prefixes the API documents mention besides
    /// [`Self::token_prefix`] (agent tokens).
    pub other_token_prefixes: &'static [&'static str],
    /// The product's version, reported by the API documents and the MCP
    /// server (the kit's own `CARGO_PKG_VERSION` is the kit's, not the
    /// product's).
    pub version: &'static str,
    /// `User-Agent` of outgoing requests.
    pub user_agent: &'static str,
    /// Prefix of the database copy taken before a migration
    /// ("vigilant-vole-before-").
    pub backup_prefix: &'static str,
    /// Name of the one-shot flash message cookie.
    pub flash_cookie: &'static str,
    /// Prefix of the product's environment variables ("VV" for
    /// `VV_TRUST_PROXY` and `VV_SETUP_TOKEN`).
    pub env_prefix: &'static str,
    /// Paths (prefixes) whose requests authenticate with tokens, not
    /// cookies, and so need no CSRF proof.
    pub csrf_exempt_prefixes: &'static [&'static str],
    /// Further exempt paths that a prefix can't describe.
    pub csrf_exempt_path: fn(&str) -> bool,
    /// Passwords (lower case) the product refuses besides the kit's list.
    pub extra_common_passwords: &'static [&'static str],
    /// The product's translations: `(language code, locale file text)`,
    /// added to the kit's core catalogs ([`super::i18n::LANGS`] lists the
    /// languages). A key may not be in both.
    /// The product's database migrations and how to run them.
    pub migrations: &'static super::db::migrate::MigrationSet,
    /// Path prefixes that may be framed by other sites (public pages);
    /// everything else is served with `frame-ancestors 'none'`.
    pub embeddable_prefixes: &'static [&'static str],
    /// Looks up one of the product's embedded static files by path.
    pub static_file: fn(&str) -> Option<super::http::assets::StaticFile>,
    /// Machine endpoints that stay English: path prefixes ...
    pub machine_path_prefixes: &'static [&'static str],
    /// ... and exact paths.
    pub machine_paths: &'static [&'static str],
    /// Paths where the visitor's `Accept-Language` beats the site language
    /// (public pages).
    pub browser_language_prefixes: &'static [&'static str],
    /// The product's API reference.
    pub docs: &'static super::docs::DocsSpec,
    pub locales: &'static [(&'static str, &'static str)],
}

static CONFIG: OnceLock<&'static KitConfig> = OnceLock::new();

/// Installs the product's configuration. Call it once at start-up (and in
/// test set-up). Installing the same configuration again does nothing;
/// installing a different one panics, because the kit would otherwise behave
/// as two products at once.
///
/// The first installation also checks the translations: every catalog
/// parses and no key is defined by both the kit and the product. A failure
/// panics, so a broken build doesn't start.
pub fn install(config: &'static KitConfig) {
    let mut first = false;
    let current = *CONFIG.get_or_init(|| {
        first = true;
        config
    });
    assert!(
        std::ptr::eq(current, config),
        "a different KitConfig is already installed"
    );
    if first && let Err(e) = crate::i18n::catalog::validate() {
        panic!("invalid translations: {e}");
    }
}

/// The product's configuration.
pub fn get() -> &'static KitConfig {
    match CONFIG.get() {
        Some(config) => config,
        None => missing(),
    }
}

#[cfg(not(test))]
#[cold]
fn missing() -> &'static KitConfig {
    panic!(
        "no KitConfig installed: call vole_kit::config::install(&YOUR_CONFIG) at start-up, \
         before any kit code runs (tests: in their shared set-up)"
    )
}

/// The kit's own tests run with the test configuration unless a test
/// installed another one.
#[cfg(test)]
fn missing() -> &'static KitConfig {
    install(&crate::testing::CONFIG);
    get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_the_same_config_again_is_harmless() {
        install(&crate::testing::CONFIG);
        install(&crate::testing::CONFIG);
        assert!(std::ptr::eq(get(), &crate::testing::CONFIG));
    }

    #[test]
    fn installing_a_different_config_panics_and_keeps_the_first() {
        static OTHER: KitConfig = KitConfig {
            product: "Other",
            ..crate::testing::CONFIG
        };
        install(&crate::testing::CONFIG);
        assert!(std::panic::catch_unwind(|| install(&OTHER)).is_err());
        assert_eq!(get().product, crate::testing::CONFIG.product);
    }
}
