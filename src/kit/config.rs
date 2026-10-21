//! What the kit needs to know about the product it runs inside.
//!
//! The product describes itself once, with a [`KitConfig`] value, and hands
//! it over with [`register_config!`]. The kit reads it through [`get`] (or
//! `Core::cfg`). Registration is a link-time symbol rather than a runtime
//! `install()` call, so the configuration is present in every binary that
//! contains the kit (the server, unit tests, integration tests) without
//! anyone having to remember to initialise it first.

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
    /// The product's translations: `(language code, locale file text)`.
    /// The kit knows the languages ([`super::i18n::LANGS`]); the texts are
    /// the product's.
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
    pub locales: &'static [(&'static str, &'static str)],
}

unsafe extern "Rust" {
    /// Defined by the product through [`register_config!`].
    fn vole_kit_product_config() -> &'static KitConfig;
}

/// The product's configuration.
pub fn get() -> &'static KitConfig {
    // SAFETY: the symbol is defined by `register_config!` as a safe
    // function with exactly this signature; the build fails to link if the
    // product doesn't provide it.
    unsafe { vole_kit_product_config() }
}

/// Registers the product's [`KitConfig`]. Use once, in the product:
///
/// ```ignore
/// static KIT: KitConfig = KitConfig { /* … */ };
/// vole_kit::register_config!(KIT);
/// ```
#[macro_export]
macro_rules! register_config {
    ($config:expr) => {
        #[unsafe(no_mangle)]
        pub fn vole_kit_product_config() -> &'static $crate::kit::config::KitConfig {
            &$config
        }
    };
}
