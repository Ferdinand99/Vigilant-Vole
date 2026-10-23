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
    /// One short word for tight spots, like an SMS sender name ("Vole").
    pub short_name: &'static str,
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
    /// They match whole segments, and a `*` segment stands for any one
    /// segment (`/api/*/envelope`); see [`crate::auth::csrf::prefix_matches`].
    pub csrf_exempt_prefixes: &'static [&'static str],
    /// Further exempt paths that a prefix can't describe.
    pub csrf_exempt_path: fn(&str) -> bool,
    /// Passwords (lower case) the product refuses besides the kit's list.
    pub extra_common_passwords: &'static [&'static str],
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
    /// The product's translations: `(language code, locale file text)`,
    /// added to the kit's core catalogs ([`super::i18n::LANGS`] lists the
    /// languages). A key may not be in both.
    pub locales: &'static [(&'static str, &'static str)],
    /// What the shared layouts show: logo, icons, navigation, settings tabs.
    pub ui: &'static super::page::Ui,
    /// Where an admin sets the instance's public URL, for messages that
    /// point there ("Settings → General"). `None` if the product has no such
    /// place; the messages then say "the instance's public URL".
    pub public_url_setting: Option<&'static str>,
    /// How the webhook provider names what alerts are about: the default
    /// body's fields, the body template's placeholders, the URL example
    /// (`Wording::NEUTRAL` for `subject` and `duration`).
    pub webhook: super::notify::webhook::Wording,
    /// Permissions the product adds to API tokens besides "read only" and
    /// "read and write" (see [`ExtraScope`]); `&[]` for none. A product with
    /// any runs the core migration
    /// [`TOKEN_SCOPES`](crate::db::migrate::TOKEN_SCOPES) (part of
    /// [`CORE`](crate::db::migrate::CORE)).
    pub token_scopes: &'static [ExtraScope],
    /// The kit's General settings page (`/settings/general`: site name,
    /// public URL, time zone, language and the product's own fields).
    /// `None` when the product has no such page, or its own.
    pub general: Option<&'static crate::settings::GeneralSettings>,
}

/// A permission a product adds to API tokens ([`KitConfig::token_scopes`]),
/// such as Keen Kestrel's `difs:write` (upload debug files). Users pick it
/// on the API tokens page like "Read only" and "Read and write"; handlers
/// check it with [`CurrentUser::has_scope`](crate::auth::CurrentUser::has_scope).
#[derive(Debug)]
pub struct ExtraScope {
    /// The name stored with the token and checked by the product
    /// (`difs:write`): 1–40 characters of `a-z`, `0-9`, `:`, `.`, `_` and
    /// `-`, and not `read` or `write`.
    pub name: &'static str,
    /// Translation key (the product's catalog) of its title on the API
    /// tokens page; it is also the token's Access badge.
    pub label: &'static str,
    /// Translation key of the line under the title: what such a token may do.
    pub help: &'static str,
    /// Whether such a token also reads, like a read-only token. Without it,
    /// the token is refused everywhere except under [`Self::paths`].
    pub implies_read: bool,
    /// For a scope that doesn't imply read: the path prefixes where such a
    /// token is accepted (`/api/v1/uploads/`). Ignored when it implies read.
    pub paths: &'static [&'static str],
}

impl ExtraScope {
    /// Whether `name` is a valid scope name (see [`Self::name`]).
    pub fn valid_name(name: &str) -> bool {
        (1..=40).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b":._-".contains(&b))
            && !matches!(name, "read" | "write")
    }
}

impl KitConfig {
    /// The declared token scope called `name`.
    pub fn token_scope(&self, name: &str) -> Option<&'static ExtraScope> {
        self.token_scopes.iter().find(|s| s.name == name)
    }
}

static CONFIG: OnceLock<&'static KitConfig> = OnceLock::new();

/// Installs the product's configuration. Call it once at start-up (and in
/// test set-up). Installing the same configuration again does nothing;
/// installing a different one panics, because the kit would otherwise behave
/// as two products at once.
///
/// The first installation also checks the translations (every catalog
/// parses and no key is defined by both the kit and the product) and, with
/// [`DocsSpec::strict_access`](crate::docs::DocsSpec::strict_access), that
/// every documented route names a known access. A failure panics, so a
/// broken build doesn't start.
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
    if !first {
        return;
    }
    if let Err(e) = crate::i18n::catalog::validate() {
        panic!("invalid translations: {e}");
    }
    if config.docs.strict_access {
        let undeclared = crate::docs::undeclared_access(config.docs);
        assert!(
            undeclared.is_empty(),
            "API reference: these routes name an access that is neither a role, `public`, \
             `metrics token` nor one of DocsSpec::credentials: {}",
            undeclared.join(", ")
        );
    }
    if let Err(e) = check(config) {
        panic!("invalid KitConfig: {e}");
    }
}

/// What [`install`] checks besides the catalogs: the token scopes (valid,
/// distinct names with English texts, and their core migration) and the
/// General page (the product's texts and fields).
pub fn check(config: &KitConfig) -> Result<(), String> {
    use crate::i18n::{Lang, catalog};
    let source = Lang::EN.info().source;
    let product = catalog::parse(source.product()).map_err(|e| format!("product catalog: {e}"))?;
    let core = catalog::parse(source.core()).map_err(|e| format!("kit catalog: {e}"))?;
    let has = |key: &str| product.contains_key(key) || core.contains_key(key);
    let mut problems = Vec::new();
    for (i, scope) in config.token_scopes.iter().enumerate() {
        let name = scope.name;
        if !ExtraScope::valid_name(name) {
            problems.push(format!("token scope {name:?}: invalid name"));
        }
        if config.token_scopes[..i].iter().any(|s| s.name == name) {
            problems.push(format!("token scope {name:?}: declared twice"));
        }
        for key in [scope.label, scope.help] {
            if !has(key) {
                problems.push(format!("token scope {name:?}: no English text for {key}"));
            }
        }
    }
    let migration = crate::db::migrate::TOKEN_SCOPES.0;
    if !config.token_scopes.is_empty()
        && !config.migrations.core.iter().any(|(n, _)| *n == migration)
    {
        problems.push(format!(
            "token scopes need the core migration {migration} in MigrationSet::core"
        ));
    }
    if let Some(general) = config.general {
        for key in catalog::GENERAL_PRODUCT_KEYS {
            if !product.contains_key(*key) {
                problems.push(format!("the General page needs the product text {key}"));
            }
        }
        problems.extend(general.problems(&has));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
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
