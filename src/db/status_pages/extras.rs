//! Status page extras: uploaded logo and favicon (`status_page_assets`),
//! password protection, analytics settings and the built-in daily view
//! counters (`status_page_views`, `status_page_referrers`).
//!
//! The validation here is pure and shared by the editor, the REST API,
//! config-as-code and the Kuma importer, so every way in has the same rules.

use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Largest accepted logo or favicon.
pub const MAX_ASSET_BYTES: usize = 1024 * 1024;
/// Shortest page password.
pub const PASSWORD_MIN: usize = 8;
/// Longest analytics site ID or URL.
const ANALYTICS_MAX: usize = 500;

// ===========================================================================
// Assets (logo / favicon)
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetKind {
    Logo,
    Favicon,
}

impl AssetKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "logo" => Some(AssetKind::Logo),
            "favicon" => Some(AssetKind::Favicon),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AssetKind::Logo => "logo",
            AssetKind::Favicon => "favicon",
        }
    }
}

/// Public, cache-busting path of an uploaded asset.
pub fn asset_path(slug: &str, kind: AssetKind, sha: &str) -> String {
    format!("/status/{slug}/{}?v={}", kind.as_str(), &sha[..sha.len().min(16)])
}

/// A validated image, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// From the magic bytes, one of [`IMAGE_TYPES`].
    pub mime: &'static str,
    pub bytes: Vec<u8>,
    /// Lowercase hex SHA-256 of `bytes`.
    pub sha256: String,
}

/// The image formats we accept (and the only `Content-Type`s we serve).
pub const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/x-icon"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetError {
    Empty,
    TooLarge,
    /// SVG can carry scripts; we don't try to sanitise it.
    Svg,
    Unsupported,
    /// Not a `data:image/…;base64,…` URL.
    BadDataUrl,
}

impl AssetError {
    pub fn message(self) -> &'static str {
        match self {
            AssetError::Empty => crate::i18n::t("spx.err_image_empty"),
            AssetError::TooLarge => crate::i18n::t("spx.err_image_large"),
            AssetError::Svg => crate::i18n::t("spx.err_image_svg"),
            AssetError::Unsupported => crate::i18n::t("spx.err_image_type"),
            AssetError::BadDataUrl => crate::i18n::t("spx.err_image_data_url"),
        }
    }
}

/// The image type, judged only by the file's first bytes.
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.len() >= 4 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // ICONDIR: reserved 0, type 1 (icon), at least one image.
    if bytes.len() >= 6 && bytes[..4] == [0, 0, 1, 0] && u16::from_le_bytes([bytes[4], bytes[5]]) > 0 {
        return Some("image/x-icon");
    }
    None
}

/// Only used to give SVG uploads a clearer message; they're refused either way.
fn looks_like_svg(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(&bytes[..bytes.len().min(1024)])
        .to_ascii_lowercase()
        .contains("<svg")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Accepts PNG, JPEG, WebP and ICO up to [`MAX_ASSET_BYTES`], by magic bytes.
pub fn validate_image(bytes: Vec<u8>) -> Result<Asset, AssetError> {
    if bytes.is_empty() {
        return Err(AssetError::Empty);
    }
    if bytes.len() > MAX_ASSET_BYTES {
        return Err(AssetError::TooLarge);
    }
    let Some(mime) = sniff_image(&bytes) else {
        return Err(if looks_like_svg(&bytes) { AssetError::Svg } else { AssetError::Unsupported });
    };
    Ok(Asset {
        mime,
        sha256: sha256_hex(&bytes),
        bytes,
    })
}

/// `data:image/png;base64,…` → validated asset (the declared type is ignored;
/// the bytes decide).
pub fn parse_data_url(url: &str) -> Result<Asset, AssetError> {
    let rest = url.trim().strip_prefix("data:").ok_or(AssetError::BadDataUrl)?;
    let (meta, data) = rest.split_once(',').ok_or(AssetError::BadDataUrl)?;
    if !meta.to_ascii_lowercase().ends_with(";base64") {
        return Err(AssetError::BadDataUrl);
    }
    if meta.to_ascii_lowercase().starts_with("image/svg") {
        return Err(AssetError::Svg);
    }
    // Base64 is 4/3 the size; refuse obviously oversized input before decoding.
    if data.len() > MAX_ASSET_BYTES / 3 * 4 + 8 {
        return Err(AssetError::TooLarge);
    }
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|_| AssetError::BadDataUrl)?;
    validate_image(bytes)
}

pub fn to_data_url(asset: &Asset) -> String {
    format!("data:{};base64,{}", asset.mime, base64::engine::general_purpose::STANDARD.encode(&asset.bytes))
}

pub fn get_asset(conn: &Connection, page_id: i64, kind: AssetKind) -> rusqlite::Result<Option<Asset>> {
    conn.query_row(
        "SELECT mime, bytes, sha256 FROM status_page_assets WHERE page_id = ?1 AND kind = ?2",
        params![page_id, kind.as_str()],
        |r| {
            let mime: String = r.get(0)?;
            Ok(Asset {
                // Only ever stored from `sniff_image`; anything else is served as a download.
                mime: IMAGE_TYPES.iter().find(|t| **t == mime).copied().unwrap_or("application/octet-stream"),
                bytes: r.get(1)?,
                sha256: r.get(2)?,
            })
        },
    )
    .optional()
}

pub fn set_asset(conn: &Connection, page_id: i64, kind: AssetKind, asset: &Asset) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO status_page_assets (page_id, kind, mime, bytes, sha256) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (page_id, kind) DO UPDATE SET mime = excluded.mime, bytes = excluded.bytes,
             sha256 = excluded.sha256, updated_at = datetime('now')",
        params![page_id, kind.as_str(), asset.mime, asset.bytes, asset.sha256],
    )?;
    touch(conn, page_id)
}

pub fn delete_asset(conn: &Connection, page_id: i64, kind: AssetKind) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM status_page_assets WHERE page_id = ?1 AND kind = ?2",
        params![page_id, kind.as_str()],
    )?;
    touch(conn, page_id)?;
    Ok(n > 0)
}

fn touch(conn: &Connection, page_id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE status_pages SET updated_at = datetime('now') WHERE id = ?1", params![page_id])?;
    Ok(())
}

// ===========================================================================
// Password
// ===========================================================================

/// Why `password` can't protect a page, or `None`. Page passwords are shared
/// with whoever should see the page, so only length is enforced.
pub fn password_problem(password: &str) -> Option<&'static str> {
    if password.chars().count() < PASSWORD_MIN {
        return Some(crate::i18n::t("spx.err_password_short"));
    }
    if password.len() > crate::auth::MAX_PASSWORD_LEN {
        return Some(crate::i18n::t("spx.err_password_long"));
    }
    None
}

/// Sets (`Some(hash)`) or removes (`None`) the page password. Either way the
/// version moves on, so every unlock cookie issued so far stops working.
pub fn set_password_hash(conn: &Connection, page_id: i64, hash: Option<&str>) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE status_pages SET password_hash = ?2, password_version = password_version + 1,
             updated_at = datetime('now') WHERE id = ?1",
        params![page_id, hash],
    )? > 0)
}

/// Whether `hash` is an Argon2 PHC string we can verify against.
pub fn is_argon2_hash(hash: &str) -> bool {
    hash.starts_with("$argon2") && argon2::password_hash::phc::PasswordHash::new(hash).is_ok()
}

// ===========================================================================
// Analytics settings
// ===========================================================================

/// Third-party analytics provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    None,
    Plausible,
    Umami,
    Matomo,
    Google,
}

impl Provider {
    pub const ALL: [Provider; 5] = [Provider::None, Provider::Plausible, Provider::Umami, Provider::Matomo, Provider::Google];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "none" => Some(Provider::None),
            "plausible" => Some(Provider::Plausible),
            "umami" => Some(Provider::Umami),
            "matomo" => Some(Provider::Matomo),
            "google" | "google_analytics" | "ga" | "ga4" => Some(Provider::Google),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::None => "none",
            Provider::Plausible => "plausible",
            Provider::Umami => "umami",
            Provider::Matomo => "matomo",
            Provider::Google => "google",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::None => crate::i18n::t("common.none"),
            Provider::Plausible => "Plausible",
            Provider::Umami => "Umami",
            Provider::Matomo => "Matomo",
            Provider::Google => "Google Analytics",
        }
    }

    /// Whether the provider needs a script/host URL besides the site ID.
    pub fn needs_url(self) -> bool {
        matches!(self, Provider::Plausible | Provider::Umami | Provider::Matomo)
    }
}

/// A page's analytics settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Analytics {
    /// Built-in, cookie-free view counting.
    pub enabled: bool,
    /// Also count unique visitors (daily-rotating salted hash, kept in memory).
    pub unique: bool,
    pub provider: Provider,
    /// Plausible: domain(s); Umami: website UUID; Matomo: numeric site ID; Google: measurement ID.
    pub site_id: String,
    /// Plausible/Umami: script URL; Matomo: tracker base URL. Always https.
    pub url: String,
}

impl Default for Analytics {
    fn default() -> Self {
        Analytics {
            enabled: true,
            unique: true,
            provider: Provider::None,
            site_id: String::new(),
            url: String::new(),
        }
    }
}

/// Field name + message for an invalid analytics setting.
pub type FieldError = (&'static str, &'static str);

/// Characters never allowed in an ID or URL that ends up in markup or a CSP.
fn has_unsafe_chars(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '<' | '>' | '\\' | '`' | ';' | ','))
}

/// Strict https URL check: no user info, no fragment, no unsafe characters.
pub fn validate_https_url(input: &str) -> Option<url::Url> {
    let s = input.trim();
    if s.is_empty() || s.len() > ANALYTICS_MAX || has_unsafe_chars(s) || s.contains(['{', '}', '|', '^']) {
        return None;
    }
    let url = url::Url::parse(s).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return None;
    }
    let host = url.host_str()?;
    if host.is_empty() {
        return None;
    }
    Some(url)
}

fn valid_ga_id(id: &str) -> bool {
    let ga4 = |prefix: &str| {
        id.strip_prefix(prefix)
            .is_some_and(|rest| (4..=20).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
    };
    let ua = || {
        let parts: Vec<&str> = id.split('-').collect();
        parts.len() == 3
            && parts[0] == "UA"
            && (4..=10).contains(&parts[1].len())
            && (1..=4).contains(&parts[2].len())
            && parts[1..].iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    };
    ["G-", "GT-", "AW-", "DC-"].iter().any(|p| ga4(p)) || ua()
}

fn valid_domain_list(ids: &str) -> bool {
    !ids.is_empty()
        && ids.len() <= ANALYTICS_MAX
        && ids.split(',').all(|d| {
            let d = d.trim();
            !d.is_empty()
                && d.len() <= 253
                && !d.starts_with(['.', '-'])
                && !d.ends_with(['.', '-'])
                && d.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        })
}

fn valid_uuid(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.iter().map(|g| g.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
        && groups.iter().all(|g| g.bytes().all(|b| b.is_ascii_hexdigit()))
}

impl Analytics {
    /// Normalises and validates. IDs and URLs end up in markup and in the
    /// Content-Security-Policy, so they are checked strictly against what
    /// each provider uses; anything with quotes, brackets or spaces is refused.
    pub fn validated(mut self) -> Result<Analytics, Vec<FieldError>> {
        let mut errors = Vec::new();
        self.site_id = self.site_id.trim().to_string();
        self.url = self.url.trim().to_string();
        match self.provider {
            Provider::None => {
                self.site_id.clear();
                self.url.clear();
            }
            Provider::Google => {
                self.site_id = self.site_id.to_ascii_uppercase();
                self.url.clear();
                if !valid_ga_id(&self.site_id) {
                    errors.push(("analytics_site_id", crate::i18n::t("spx.err_ga_id")));
                }
            }
            Provider::Plausible | Provider::Umami | Provider::Matomo => {
                let id_ok = match self.provider {
                    Provider::Plausible => {
                        self.site_id = self.site_id.to_ascii_lowercase().replace(", ", ",");
                        valid_domain_list(&self.site_id)
                    }
                    Provider::Umami => {
                        self.site_id = self.site_id.to_ascii_lowercase();
                        valid_uuid(&self.site_id)
                    }
                    _ => (1..=10).contains(&self.site_id.len()) && self.site_id.bytes().all(|b| b.is_ascii_digit()),
                };
                if !id_ok {
                    errors.push((
                        "analytics_site_id",
                        match self.provider {
                            Provider::Plausible => crate::i18n::t("spx.err_plausible_id"),
                            Provider::Umami => crate::i18n::t("spx.err_umami_id"),
                            _ => crate::i18n::t("spx.err_matomo_id"),
                        },
                    ));
                }
                match validate_https_url(&self.url) {
                    _ if self.url.is_empty() => errors.push(("analytics_url", crate::i18n::t("spx.err_analytics_url_required"))),
                    Some(mut u) => {
                        if self.provider == Provider::Matomo {
                            // A base URL: matomo.js and matomo.php are appended.
                            u.set_query(None);
                            if !u.path().ends_with('/') {
                                let path = format!("{}/", u.path());
                                u.set_path(&path);
                            }
                        }
                        self.url = u.to_string();
                    }
                    None => errors.push(("analytics_url", crate::i18n::t("spx.err_analytics_url"))),
                }
            }
        }
        if errors.is_empty() { Ok(self) } else { Err(errors) }
    }

    /// Origin (`https://host[:port]`) of the provider's script/tracker, for the CSP.
    pub fn origin(&self) -> Option<String> {
        match self.provider {
            Provider::None => None,
            Provider::Google => Some("https://www.googletagmanager.com".into()),
            _ => validate_https_url(&self.url).map(|u| u.origin().ascii_serialization()),
        }
    }

    /// Extra CSP sources this provider needs: `(script-src, connect-src, img-src)`.
    pub fn csp_sources(&self) -> Option<(Vec<String>, Vec<String>, Vec<String>)> {
        match self.provider {
            Provider::None => None,
            Provider::Google => Some((
                vec!["https://www.googletagmanager.com".into()],
                vec![
                    "https://www.googletagmanager.com".into(),
                    "https://*.google-analytics.com".into(),
                    "https://*.analytics.google.com".into(),
                ],
                vec!["https://www.googletagmanager.com".into(), "https://*.google-analytics.com".into()],
            )),
            Provider::Plausible | Provider::Umami | Provider::Matomo => {
                let origin = self.origin()?;
                let mut connect = vec![origin.clone()];
                // Umami Cloud's script reports to a separate gateway.
                if self.provider == Provider::Umami && origin == "https://cloud.umami.is" {
                    connect.push("https://api-gateway.umami.dev".into());
                }
                let img = if self.provider == Provider::Matomo { vec![origin.clone()] } else { Vec::new() };
                Some((vec![origin], connect, img))
            }
        }
    }
}

pub fn set_analytics(conn: &Connection, page_id: i64, a: &Analytics) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE status_pages SET analytics_enabled = ?2, analytics_unique = ?3, analytics_provider = ?4,
             analytics_site_id = ?5, analytics_url = ?6, updated_at = datetime('now') WHERE id = ?1",
        params![page_id, a.enabled, a.unique, a.provider.as_str(), a.site_id, a.url],
    )? > 0)
}

// ===========================================================================
// Changes from config-as-code and the REST API
// ===========================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PasswordChange {
    #[default]
    Keep,
    Remove,
    /// Argon2 PHC string.
    Set(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AssetChange {
    #[default]
    Keep,
    Remove,
    Set(Asset),
}

impl AssetChange {
    /// From a document value: `None` keeps, `""` removes, a data URL sets.
    pub fn from_doc(value: Option<&str>) -> Result<Self, AssetError> {
        match value.map(str::trim) {
            None => Ok(AssetChange::Keep),
            Some("") => Ok(AssetChange::Remove),
            Some(url) => parse_data_url(url).map(AssetChange::Set),
        }
    }

    /// Whether applying this to a page whose current hash is `current` changes anything.
    pub fn differs(&self, current: Option<&str>) -> bool {
        match self {
            AssetChange::Keep => false,
            AssetChange::Remove => current.is_some(),
            AssetChange::Set(a) => current != Some(a.sha256.as_str()),
        }
    }
}

/// Everything besides the core settings that a config document or API call may change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtrasChange {
    pub password: PasswordChange,
    /// `None` keeps the current settings.
    pub analytics: Option<Analytics>,
    pub logo: AssetChange,
    pub favicon: AssetChange,
}

pub fn apply_extras(conn: &Connection, page_id: i64, change: &ExtrasChange) -> rusqlite::Result<()> {
    match &change.password {
        PasswordChange::Keep => {}
        PasswordChange::Remove => {
            conn.execute(
                "UPDATE status_pages SET password_hash = NULL, password_version = password_version + 1
                 WHERE id = ?1 AND password_hash IS NOT NULL",
                params![page_id],
            )?;
        }
        PasswordChange::Set(hash) => {
            set_password_hash(conn, page_id, Some(hash))?;
        }
    }
    if let Some(a) = &change.analytics {
        set_analytics(conn, page_id, a)?;
    }
    for (kind, c) in [(AssetKind::Logo, &change.logo), (AssetKind::Favicon, &change.favicon)] {
        match c {
            AssetChange::Keep => {}
            AssetChange::Remove => {
                delete_asset(conn, page_id, kind)?;
            }
            AssetChange::Set(asset) => set_asset(conn, page_id, kind, asset)?,
        }
    }
    Ok(())
}

// ===========================================================================
// Config-as-code / REST API documents
// ===========================================================================

fn yes() -> bool {
    true
}

/// Analytics settings as they appear in a config document or API body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyticsDoc {
    /// Built-in, cookie-free view counting.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "yes")]
    pub unique_visitors: bool,
    /// `none` | `plausible` | `umami` | `matomo` | `google`.
    #[serde(default)]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub site_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub script_url: String,
}

impl AnalyticsDoc {
    pub fn from_settings(a: &Analytics) -> Self {
        AnalyticsDoc {
            enabled: a.enabled,
            unique_visitors: a.unique,
            provider: a.provider.as_str().into(),
            site_id: a.site_id.clone(),
            script_url: a.url.clone(),
        }
    }

    /// `None` for the defaults (exports stay short).
    pub fn exported(a: &Analytics) -> Option<Self> {
        (a != &Analytics::default()).then(|| Self::from_settings(a))
    }

    pub fn to_settings(&self) -> Result<Analytics, Vec<(String, String)>> {
        let provider = Provider::parse(&self.provider).ok_or_else(|| {
            vec![("analytics.provider".to_string(), crate::i18n::t("spx.err_analytics_provider").to_string())]
        })?;
        Analytics {
            enabled: self.enabled,
            unique: self.unique_visitors,
            provider,
            site_id: self.site_id.clone(),
            url: self.script_url.clone(),
        }
        .validated()
        .map_err(|errors| {
            errors
                .into_iter()
                .map(|(field, msg)| {
                    let field = if field == "analytics_url" { "analytics.script_url" } else { "analytics.site_id" };
                    (field.to_string(), msg.to_string())
                })
                .collect()
        })
    }
}

/// The extra fields of a status page document (`None` = keep).
#[derive(Debug, Clone, Copy, Default)]
pub struct DocExtras<'a> {
    /// Plain password to set, `""` to remove, the secret mask to keep.
    pub password: Option<&'a str>,
    /// An Argon2 PHC string (full exports carry this instead of a password).
    pub password_hash: Option<&'a str>,
    pub analytics: Option<&'a AnalyticsDoc>,
    /// `data:image/…;base64,…`, or `""` to remove.
    pub logo: Option<&'a str>,
    pub favicon: Option<&'a str>,
}

/// A validated change plus the names of the fields it touches.
pub type PlannedExtras = (ExtrasChange, Vec<&'static str>);

/// Validates document extras against the page as stored (`None` for a new
/// page) and returns the change plus the names of the fields it touches.
/// Plain passwords are only hashed when they differ from the stored one, so
/// re-applying the same document is a no-op.
pub fn plan_doc_extras(doc: DocExtras<'_>, current: Option<&super::StatusPage>) -> Result<PlannedExtras, Vec<(String, String)>> {
    let mut errors: Vec<(String, String)> = Vec::new();
    let mut changed = Vec::new();
    let current_hash = current.and_then(|p| p.password_hash.as_deref());
    let mask = crate::notification::SECRET_MASK;
    let password = match (doc.password, doc.password_hash) {
        (Some(_), Some(_)) => {
            errors.push(("password".into(), "Use either password or password_hash, not both.".into()));
            PasswordChange::Keep
        }
        (None, Some(hash)) => {
            let hash = hash.trim();
            if !is_argon2_hash(hash) {
                errors.push(("password_hash".into(), "Use an Argon2 hash from a full export.".into()));
                PasswordChange::Keep
            } else if current_hash == Some(hash) {
                PasswordChange::Keep
            } else {
                PasswordChange::Set(hash.to_string())
            }
        }
        (Some(p), None) if p == mask => {
            if current_hash.is_none() {
                errors.push(("password".into(), "The password was exported without secrets; set it again.".into()));
            }
            PasswordChange::Keep
        }
        (Some(""), None) => {
            if current_hash.is_some() { PasswordChange::Remove } else { PasswordChange::Keep }
        }
        (Some(p), None) => match password_problem(p) {
            Some(msg) => {
                errors.push(("password".into(), msg.into()));
                PasswordChange::Keep
            }
            None if current_hash.is_some_and(|h| crate::auth::verify_password(h, p)) => PasswordChange::Keep,
            None => PasswordChange::Set(crate::auth::hash_password(p)),
        },
        (None, None) => PasswordChange::Keep,
    };
    if password != PasswordChange::Keep {
        changed.push("password");
    }
    let analytics = match doc.analytics.map(AnalyticsDoc::to_settings) {
        None => None,
        Some(Ok(a)) => {
            if current.is_some_and(|p| p.analytics != a) {
                changed.push("analytics");
            }
            Some(a)
        }
        Some(Err(e)) => {
            errors.extend(e);
            None
        }
    };
    let mut asset = |field: &'static str, value: Option<&str>, now: Option<&str>| match AssetChange::from_doc(value) {
        Ok(c) => {
            if c.differs(now) {
                changed.push(field);
            }
            c
        }
        Err(e) => {
            errors.push((field.into(), e.message().into()));
            AssetChange::Keep
        }
    };
    let logo = asset("logo", doc.logo, current.and_then(|p| p.logo_sha.as_deref()));
    let favicon = asset("favicon", doc.favicon, current.and_then(|p| p.favicon_sha.as_deref()));
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok((ExtrasChange { password, analytics, logo, favicon }, changed))
}

/// What an export says about a page's extras:
/// `(password, password_hash, analytics, logo, favicon)`. Without secrets a
/// protected page shows the secret mask and images are left out; a full
/// export carries the password hash and the images as data URLs.
#[allow(clippy::type_complexity)]
pub fn export_doc_extras(
    conn: &Connection,
    page: &super::StatusPage,
    include_secrets: bool,
) -> rusqlite::Result<(Option<String>, Option<String>, Option<AnalyticsDoc>, Option<String>, Option<String>)> {
    let (password, hash) = match (&page.password_hash, include_secrets) {
        (None, _) => (None, None),
        (Some(h), true) => (None, Some(h.clone())),
        (Some(_), false) => (Some(crate::notification::SECRET_MASK.to_string()), None),
    };
    let image = |kind| -> rusqlite::Result<Option<String>> {
        Ok(if include_secrets { get_asset(conn, page.id, kind)?.map(|a| to_data_url(&a)) } else { None })
    };
    Ok((password, hash, AnalyticsDoc::exported(&page.analytics), image(AssetKind::Logo)?, image(AssetKind::Favicon)?))
}

// ===========================================================================
// Built-in view counters
// ===========================================================================

/// Counts one page view on `day` (UTC `YYYY-MM-DD`). `new_visitor`: the
/// first view of this visitor today. `referrer`: a foreign domain, if any.
pub fn record_view(conn: &Connection, page_id: i64, day: &str, new_visitor: bool, referrer: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO status_page_views (page_id, day, views, visitors) VALUES (?1, ?2, 1, ?3)
         ON CONFLICT (page_id, day) DO UPDATE SET views = views + 1, visitors = visitors + excluded.visitors",
        params![page_id, day, i64::from(new_visitor)],
    )?;
    if let Some(domain) = referrer {
        conn.execute(
            "INSERT INTO status_page_referrers (page_id, day, domain, views) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT (page_id, day, domain) DO UPDATE SET views = views + 1",
            params![page_id, day, domain],
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DayViews {
    pub day: String,
    pub views: i64,
    pub visitors: i64,
}

/// The last `days` days up to and including `today`, oldest first, zero-filled.
pub fn daily_views(conn: &Connection, page_id: i64, today: jiff::civil::Date, days: i64) -> rusqlite::Result<Vec<DayViews>> {
    let days = days.max(1);
    let first = today.saturating_sub(jiff::Span::new().days(days - 1));
    let mut stmt = conn.prepare(
        "SELECT day, views, visitors FROM status_page_views WHERE page_id = ?1 AND day >= ?2 AND day <= ?3",
    )?;
    let mut found = std::collections::HashMap::new();
    let rows = stmt.query_map(params![page_id, first.to_string(), today.to_string()], |r| {
        Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
    })?;
    for row in rows {
        let (day, counts) = row?;
        found.insert(day, counts);
    }
    let mut out = Vec::with_capacity(days as usize);
    let mut d = first;
    for _ in 0..days {
        let key = d.to_string();
        let (views, visitors) = found.get(&key).copied().unwrap_or_default();
        out.push(DayViews { day: key, views, visitors });
        d = d.tomorrow().unwrap_or(d);
    }
    Ok(out)
}

/// Referrer domains since `since_day`, most views first.
pub fn top_referrers(conn: &Connection, page_id: i64, since_day: &str, limit: i64) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT domain, SUM(views) AS n FROM status_page_referrers WHERE page_id = ?1 AND day >= ?2
         GROUP BY domain ORDER BY n DESC, domain LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![page_id, since_day, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Deletes counters from days before `before_day`. Returns rows removed.
pub fn prune_views(conn: &Connection, before_day: &str) -> rusqlite::Result<usize> {
    let a = conn.execute("DELETE FROM status_page_views WHERE day < ?1", params![before_day])?;
    let b = conn.execute("DELETE FROM status_page_referrers WHERE day < ?1", params![before_day])?;
    Ok(a + b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::status_pages::{self as pages, NewStatusPage};
    use crate::db::test_conn;

    fn png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0u8; 32]);
        v
    }

    fn page(conn: &mut Connection) -> i64 {
        pages::insert(
            conn,
            &NewStatusPage {
                slug: "acme".into(),
                title: "Acme".into(),
                ..NewStatusPage::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn images_are_judged_by_magic_bytes() {
        assert_eq!(sniff_image(&png()), Some("image/png"));
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image(&[0, 0, 1, 0, 1, 0, 16, 16]), Some("image/x-icon"));
        // Zero images, cursor type, GIF and text are refused.
        assert_eq!(sniff_image(&[0, 0, 1, 0, 0, 0]), None);
        assert_eq!(sniff_image(&[0, 0, 2, 0, 1, 0]), None);
        assert_eq!(sniff_image(b"GIF89a......"), None);
        assert_eq!(sniff_image(b"<html>"), None);
        // An extension or a declared type changes nothing: a PNG name with HTML inside fails.
        assert_eq!(validate_image(b"<html><script>alert(1)</script>".to_vec()), Err(AssetError::Unsupported));
    }

    #[test]
    fn upload_limits_and_svg_rejection() {
        assert_eq!(validate_image(Vec::new()), Err(AssetError::Empty));
        let mut big = png();
        big.resize(MAX_ASSET_BYTES + 1, 0);
        assert_eq!(validate_image(big), Err(AssetError::TooLarge));
        let mut max = png();
        max.resize(MAX_ASSET_BYTES, 0);
        assert!(validate_image(max).is_ok());
        for svg in [
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>",
            "\u{feff}  <?xml version=\"1.0\"?><svg/>",
            "<!DOCTYPE svg><svg/>",
        ] {
            assert_eq!(validate_image(svg.as_bytes().to_vec()), Err(AssetError::Svg), "{svg}");
        }
        let a = validate_image(png()).unwrap();
        assert_eq!(a.mime, "image/png");
        assert_eq!(a.sha256.len(), 64);
    }

    #[test]
    fn data_urls_round_trip_and_ignore_the_declared_type() {
        let a = validate_image(png()).unwrap();
        let url = to_data_url(&a);
        assert!(url.starts_with("data:image/png;base64,"));
        assert_eq!(parse_data_url(&url).unwrap(), a);
        // Declared JPEG, actually PNG: the bytes win.
        let lie = url.replace("image/png", "image/jpeg");
        assert_eq!(parse_data_url(&lie).unwrap().mime, "image/png");
        assert_eq!(parse_data_url("data:image/svg+xml;base64,PHN2Zz4="), Err(AssetError::Svg));
        assert_eq!(parse_data_url("data:image/png,raw"), Err(AssetError::BadDataUrl));
        assert_eq!(parse_data_url("/upload/logo1.png"), Err(AssetError::BadDataUrl));
        assert_eq!(parse_data_url("data:image/png;base64,!!!"), Err(AssetError::BadDataUrl));
    }

    #[test]
    fn assets_are_stored_hashed_and_cascade() {
        let mut conn = test_conn();
        let id = page(&mut conn);
        let a = validate_image(png()).unwrap();
        set_asset(&conn, id, AssetKind::Logo, &a).unwrap();
        assert_eq!(get_asset(&conn, id, AssetKind::Logo).unwrap(), Some(a.clone()));
        assert_eq!(get_asset(&conn, id, AssetKind::Favicon).unwrap(), None);
        let p = pages::get(&conn, id).unwrap().unwrap();
        assert_eq!(p.logo_sha.as_deref(), Some(a.sha256.as_str()));
        assert_eq!(p.logo_url(), Some(format!("/status/acme/logo?v={}", &a.sha256[..16])));
        assert_eq!(p.favicon_url(), None);
        // The list query carries the hashes too.
        assert_eq!(pages::list(&conn).unwrap()[0].page.logo_sha, p.logo_sha);
        assert!(delete_asset(&conn, id, AssetKind::Logo).unwrap());
        assert!(!delete_asset(&conn, id, AssetKind::Logo).unwrap());
        set_asset(&conn, id, AssetKind::Favicon, &a).unwrap();
        pages::delete(&conn, id).unwrap();
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM status_page_assets", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn password_changes_bump_the_version() {
        let mut conn = test_conn();
        let id = page(&mut conn);
        let p = pages::get(&conn, id).unwrap().unwrap();
        assert!(!p.is_protected());
        let v0 = p.password_version;
        set_password_hash(&conn, id, Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA")).unwrap();
        let p = pages::get(&conn, id).unwrap().unwrap();
        assert!(p.is_protected());
        assert_eq!(p.password_version, v0 + 1);
        apply_extras(&conn, id, &ExtrasChange { password: PasswordChange::Remove, ..Default::default() }).unwrap();
        let p = pages::get(&conn, id).unwrap().unwrap();
        assert!(!p.is_protected());
        assert_eq!(p.password_version, v0 + 2);
        // The hash is never serialized.
        assert!(!serde_json::to_string(&p).unwrap().contains("argon2"));
        assert!(password_problem("short").is_some());
        assert!(password_problem("long enough").is_none());
        assert!(is_argon2_hash(&crate::auth::hash_password("long enough")));
        assert!(!is_argon2_hash("$2b$10$abcdefghijklmnopqrstuv"));
    }

    fn analytics(provider: Provider, id: &str, url: &str) -> Result<Analytics, Vec<FieldError>> {
        Analytics {
            provider,
            site_id: id.into(),
            url: url.into(),
            ..Analytics::default()
        }
        .validated()
    }

    #[test]
    fn analytics_ids_and_urls_are_strict() {
        assert_eq!(analytics(Provider::Google, " g-abc123xyz ", "https://ignored.test").unwrap().site_id, "G-ABC123XYZ");
        assert_eq!(analytics(Provider::Google, "UA-12345-1", "").unwrap().url, "");
        assert!(analytics(Provider::Google, "G-ABC\"><script>", "").is_err());
        assert!(analytics(Provider::Google, "", "").is_err());

        let p = analytics(Provider::Plausible, "Status.Example.com, example.org", "https://plausible.io/js/script.js").unwrap();
        assert_eq!(p.site_id, "status.example.com,example.org");
        assert!(analytics(Provider::Plausible, "exa mple.com", "https://plausible.io/js/script.js").is_err());
        for bad in [
            "http://plausible.io/js/script.js",
            "https://plausible.io/js/script.js\"onload=\"x",
            "https://user:pw@plausible.io/s.js",
            "javascript:alert(1)",
            "https://plausible.io/s.js#x",
            "https://plausible.io/<s>.js",
            "//plausible.io/s.js",
            "https://a.test/x;script-src *",
        ] {
            assert!(analytics(Provider::Plausible, "example.com", bad).is_err(), "{bad}");
        }
        assert!(analytics(Provider::Plausible, "example.com", "").is_err());

        let u = analytics(Provider::Umami, "4FB7FA4C-5B46-438D-94B3-3A8FB9BC2E8B", "https://cloud.umami.is/script.js").unwrap();
        assert_eq!(u.site_id, "4fb7fa4c-5b46-438d-94b3-3a8fb9bc2e8b");
        assert!(analytics(Provider::Umami, "not-a-uuid", "https://cloud.umami.is/script.js").is_err());

        let m = analytics(Provider::Matomo, "7", "https://stats.example.com/matomo?x=1").unwrap();
        assert_eq!(m.url, "https://stats.example.com/matomo/");
        assert!(analytics(Provider::Matomo, "seven", "https://stats.example.com/").is_err());

        let none = analytics(Provider::None, "G-ABCDEF", "https://x.test").unwrap();
        assert_eq!((none.site_id.as_str(), none.url.as_str()), ("", ""));
    }

    #[test]
    fn csp_sources_cover_exactly_the_provider() {
        assert_eq!(Analytics::default().csp_sources(), None);
        let (script, connect, img) = analytics(Provider::Plausible, "example.com", "https://stats.example.com:8443/js/script.js")
            .unwrap()
            .csp_sources()
            .unwrap();
        assert_eq!(script, vec!["https://stats.example.com:8443"]);
        assert_eq!(connect, vec!["https://stats.example.com:8443"]);
        assert!(img.is_empty());
        let (script, connect, _) = analytics(Provider::Google, "G-ABCDEF12", "").unwrap().csp_sources().unwrap();
        assert_eq!(script, vec!["https://www.googletagmanager.com"]);
        assert!(connect.iter().any(|c| c == "https://*.google-analytics.com"));
        let (_, connect, _) = analytics(Provider::Umami, "4fb7fa4c-5b46-438d-94b3-3a8fb9bc2e8b", "https://cloud.umami.is/script.js")
            .unwrap()
            .csp_sources()
            .unwrap();
        assert!(connect.contains(&"https://api-gateway.umami.dev".to_string()));
    }

    #[test]
    fn analytics_settings_persist() {
        let mut conn = test_conn();
        let id = page(&mut conn);
        assert_eq!(pages::get(&conn, id).unwrap().unwrap().analytics, Analytics::default());
        let a = analytics(Provider::Matomo, "3", "https://m.example.com/").unwrap();
        set_analytics(&conn, id, &Analytics { unique: false, ..a.clone() }).unwrap();
        let stored = pages::get(&conn, id).unwrap().unwrap().analytics;
        assert_eq!(stored.provider, Provider::Matomo);
        assert!(!stored.unique);
        assert_eq!(stored.site_id, "3");
    }

    #[test]
    fn views_are_counted_per_day_and_pruned() {
        let mut conn = test_conn();
        let id = page(&mut conn);
        let today: jiff::civil::Date = "2026-10-20".parse().unwrap();
        record_view(&conn, id, "2026-10-20", true, Some("news.example")).unwrap();
        record_view(&conn, id, "2026-10-20", false, Some("news.example")).unwrap();
        record_view(&conn, id, "2026-10-20", true, None).unwrap();
        record_view(&conn, id, "2026-10-18", true, Some("other.example")).unwrap();
        record_view(&conn, id, "2025-01-01", true, None).unwrap();
        let days = daily_views(&conn, id, today, 30).unwrap();
        assert_eq!(days.len(), 30);
        assert_eq!(days[29], DayViews { day: "2026-10-20".into(), views: 3, visitors: 2 });
        assert_eq!(days[27].views, 1);
        assert_eq!(days[0].day, "2026-09-21");
        assert_eq!(
            top_referrers(&conn, id, "2026-09-21", 10).unwrap(),
            vec![("news.example".to_string(), 2), ("other.example".to_string(), 1)]
        );
        assert_eq!(prune_views(&conn, "2025-10-20").unwrap(), 1);
        assert_eq!(prune_views(&conn, "2025-10-20").unwrap(), 0);
    }

    #[test]
    fn migration_applies_fresh_and_upgrades_existing_pages_with_safe_defaults() {
        // Fresh: every migration, ours included.
        let all = crate::db::apply_migrations(&mut Connection::open_in_memory().unwrap()).unwrap();
        assert!(all.iter().any(|m| m.starts_with("0098_")), "{all:?}");

        // Upgrade: a database as the previous release left it, with a page in it.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::CONNECTION_PRAGMAS).unwrap();
        crate::db::prepare_tracking(&conn).unwrap();
        for (name, sql) in crate::db::embedded_migrations().into_iter().filter(|(n, _)| n.as_str() < "0098_") {
            crate::db::apply_one(&mut conn, &name, &sql).unwrap();
        }
        conn.execute("INSERT INTO status_pages (slug, title) VALUES ('old', 'Old')", []).unwrap();
        assert_eq!(crate::db::apply_migrations(&mut conn).unwrap().iter().filter(|m| m.starts_with("0098_")).count(), 1);
        let p = pages::get_by_slug(&conn, "old").unwrap().unwrap();
        assert!(!p.is_protected());
        assert_eq!(p.analytics, Analytics::default());
        assert_eq!((p.logo_sha, p.favicon_sha), (None, None));
    }
}
