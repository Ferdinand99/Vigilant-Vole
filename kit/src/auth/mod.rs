//! Authentication primitives and the request-level auth contract.
//! `CurrentUser` / `Role` are a shared contract.
//!
//! - [`session`]: session + pending-2FA cookies, proxy awareness, `?next=` sanitising
//! - [`csrf`]: the same-origin check behind `api::auth::csrf_guard`
//! - [`rate_limit`]: in-memory login throttling with exponential backoff
//! - [`reauth`]: the fresh-confirmation grant of `CredentialMode::External`
//! - [`totp`]: TOTP 2FA (enrolment, verification with replay protection, recovery codes)
//! - [`webauthn`]: passkeys, the WebAuthn registration and sign-in ceremonies
//! - [`tokens`]: API tokens (generation, hashing, header parsing)
//! - [`guard`]: the middleware and helpers that need the database
//!   (`require_auth`, `csrf_guard`, `session_user`, `authenticate_token`)

use std::path::Path;
use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::request::Parts;
use axum_extra::extract::cookie::Key;

pub mod csrf;
#[cfg(test)]
mod flow_tests;
pub mod guard;
pub mod pages;
pub mod rate_limit;
pub mod reauth;
pub mod session;
pub mod tokens;
pub mod totp;
pub mod webauthn;

pub use crate::config::ExtraScope;
pub use crate::db::api_tokens::TokenScope;
pub use crate::db::role::Role;
use crate::http::{ApiError, ApiResult};
pub use guard::{authenticate_headers, authenticate_token};

/// The authenticated user for this request.
///
/// Inserted into the request extensions by the auth middleware
/// (`guard::require_auth`) on every protected route, so handlers just
/// take it as an argument:
///
/// ```ignore
/// async fn handler(user: CurrentUser, ...) -> Result<impl IntoResponse, ApiError> {
///     user.require(Role::Editor)?;
///     ...
/// }
/// ```
///
/// Extracting `CurrentUser` on a route without the middleware yields
/// `ApiError::Unauthorized`; `Option<CurrentUser>` yields `None` instead.
///
/// For API-token requests `role` is already capped by the token's access
/// (read → viewer); `account_role` is the owner's own role, and `scopes` the
/// product scopes the token carries. How the request authenticated is in
/// [`AuthMethod`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    /// Falls back to `username` when the user has no display name.
    pub display_name: String,
    /// What this request may do: the account's role, capped for an API
    /// token by its access (a read-only token acts as a viewer).
    pub role: Role,
    /// The account's own role, whatever a token caps. The same as `role`
    /// for sessions. See [`Self::may_act_as`].
    pub account_role: Role,
    /// The product scopes ([`KitConfig::token_scopes`](crate::config::KitConfig::token_scopes))
    /// this request carries. See [`Self::has_scope`].
    pub scopes: Scopes,
}

/// The product scopes a request carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Scopes {
    /// A session or a read-and-write token: every declared scope (the
    /// role still applies).
    #[default]
    All,
    /// A read-only API token: the scopes it was given that the product
    /// still declares (none for a plain read-only token).
    Only(Vec<&'static str>),
}

impl CurrentUser {
    /// `Ok(())` when this user's role is at least `role`, otherwise `Forbidden`.
    pub fn require(&self, role: Role) -> Result<(), ApiError> {
        if self.role >= role {
            Ok(())
        } else {
            Err(ApiError::Forbidden)
        }
    }

    /// Shorthand for `require(Role::Admin)`.
    pub fn require_admin(&self) -> Result<(), ApiError> {
        self.require(Role::Admin)
    }

    /// Shorthand for `require(Role::Editor)`.
    pub fn require_editor(&self) -> Result<(), ApiError> {
        self.require(Role::Editor)
    }

    pub fn can_edit(&self) -> bool {
        self.role.can_edit()
    }

    pub fn is_admin(&self) -> bool {
        self.role.is_admin()
    }

    /// Whether the account behind the request has at least `role`, however
    /// a token caps [`Self::role`]. Combine it with [`Self::has_scope`]: a
    /// read-only token with an upload scope may upload when its owner may
    /// edit (`user.has_scope("difs:write") && user.may_act_as(Role::Editor)`).
    pub fn may_act_as(&self, role: Role) -> bool {
        self.account_role >= role
    }

    /// Whether the request carries the product scope `scope`: a session or
    /// a read-and-write token carries every declared scope, a read-only
    /// token the ones it was given. Always false for a name the product
    /// doesn't declare (debug builds panic, to catch typos). The role is
    /// checked separately ([`Self::may_act_as`], [`Self::require`]).
    pub fn has_scope(&self, scope: &str) -> bool {
        let declared = crate::config::get().token_scope(scope).is_some();
        debug_assert!(
            declared,
            "token scope {scope:?} is not in KitConfig::token_scopes"
        );
        declared
            && match &self.scopes {
                Scopes::All => true,
                Scopes::Only(given) => given.contains(&scope),
            }
    }

    /// Whether the request may read, as every session and API token may,
    /// except a token whose scopes all leave reading out
    /// ([`ExtraScope::implies_read`](crate::config::ExtraScope::implies_read)).
    /// `require_auth` refuses those outside their scopes' paths.
    pub fn can_read(&self) -> bool {
        match &self.scopes {
            Scopes::All => true,
            Scopes::Only(given) => {
                given.is_empty()
                    || given.iter().any(|name| {
                        crate::config::get()
                            .token_scope(name)
                            .is_some_and(|s| s.implies_read)
                    })
            }
        }
    }

    /// Whether `path` is under one of the [`paths`](crate::config::ExtraScope::paths)
    /// of the token's scopes (false for sessions and tokens without scopes).
    /// Whole segments, like the CSRF exemptions: `/api/push` covers
    /// `/api/push` and `/api/push/x`, never `/api/pushover`.
    pub fn scope_allows_path(&self, path: &str) -> bool {
        let Scopes::Only(given) = &self.scopes else {
            return false;
        };
        given.iter().any(|name| {
            crate::config::get()
                .token_scope(name)
                .is_some_and(|s| s.paths.iter().any(|p| path_under(p, path)))
        })
    }
}

/// Whether `path` is `prefix` or below it, in whole segments (a prefix
/// that ends in `/` already says where its segment ends).
fn path_under(prefix: &str, path: &str) -> bool {
    if prefix.ends_with('/') {
        path.starts_with(prefix)
    } else {
        csrf::prefix_matches(prefix, path)
    }
}

/// The scopes of a token with `access` that was given `stored` (its rows in
/// `api_token_scopes`), against the scopes the product declares. `None`
/// when it was given scopes of which the product declares none any more:
/// such a token stops working rather than turning into a read-only one.
pub fn token_scopes(
    access: TokenScope,
    stored: &[String],
    declared: &'static [crate::config::ExtraScope],
) -> Option<Scopes> {
    if access == TokenScope::Write {
        return Some(Scopes::All);
    }
    let known: Vec<&'static str> = stored
        .iter()
        .filter_map(|s| declared.iter().find(|d| d.name == s).map(|d| d.name))
        .collect();
    if known.is_empty() && !stored.is_empty() {
        return None;
    }
    Some(Scopes::Only(known))
}

/// How the current request authenticated. Inserted next to [`CurrentUser`]
/// by `require_auth`; extract it as `Option<Extension<AuthMethod>>` if a
/// handler needs to tell sessions and tokens apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Session,
    Token { token_id: i64, scope: TokenScope },
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .ok_or(ApiError::Unauthorized)
    }
}

impl<S: Send + Sync> OptionalFromRequestParts<S> for CurrentUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<CurrentUser>().cloned())
    }
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

pub const MIN_PASSWORD_LEN: usize = 10;
/// Upper bound so nobody can make us hash megabytes.
pub const MAX_PASSWORD_LEN: usize = 1024;

/// Argon2id (default parameters) PHC string.
pub fn try_hash_password(password: &str) -> Result<String, ApiError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| ApiError::internal(format!("password hashing failed: {e}")))
}

/// Like [`try_hash_password`] but panics on failure (only the OS RNG can fail).
/// Kept for callers outside request handlers; handlers use [`hash_password_async`].
pub fn hash_password(password: &str) -> String {
    try_hash_password(password).expect("failed to hash password")
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Hash on the blocking pool (Argon2 is deliberately slow).
pub async fn hash_password_async(password: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || try_hash_password(&password))
        .await
        .map_err(ApiError::internal)?
}

/// Verify on the blocking pool.
pub async fn verify_password_async(hash: String, password: String) -> ApiResult<bool> {
    tokio::task::spawn_blocking(move || verify_password(&hash, &password))
        .await
        .map_err(ApiError::internal)
}

/// A real Argon2 hash nobody knows the password for. Logins for unknown or
/// disabled users verify against it, so they take as long as real failures
/// and usernames can't be enumerated by timing.
pub static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    try_hash_password(&format!(
        "{}/no-such-user/timing-equaliser",
        crate::config::get().slug
    ))
    .unwrap_or_default()
});

const COMMON_PASSWORDS: &[&str] = &[
    "password",
    "password1",
    "password12",
    "password123",
    "passw0rd",
    "1234567890",
    "12345678910",
    "0123456789",
    "qwertyuiop",
    "qwerty1234",
    "1q2w3e4r5t",
    "iloveyou12",
    "letmein123",
    "welcome123",
    "admin12345",
    "administrator",
    "changeme123",
    "abcdefghij",
    "1111111111",
];

/// Why `password` is not acceptable, or `None`. Same rules everywhere a
/// password is chosen (setup, change, admin reset, new user).
pub fn password_problem(password: &str, username: &str) -> Option<&'static str> {
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_LEN {
        return Some(crate::i18n::t("users.err_password_short"));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Some(crate::i18n::t("users.err_password_long"));
    }
    let lower = password.to_lowercase();
    if !username.is_empty() && lower.contains(&username.to_lowercase()) {
        return Some(crate::i18n::t("users.err_password_username"));
    }
    let distinct: std::collections::HashSet<char> = password.chars().collect();
    if distinct.len() < 4
        || COMMON_PASSWORDS.contains(&lower.as_str())
        || crate::config::get()
            .extra_common_passwords
            .contains(&lower.as_str())
    {
        return Some(crate::i18n::t("users.err_password_weak"));
    }
    None
}

/// Why `username` is not acceptable, or `None`.
pub fn username_problem(username: &str) -> Option<&'static str> {
    if username.is_empty() {
        return Some(crate::i18n::t("users.err_username_empty"));
    }
    if username.chars().count() > 64 {
        return Some(crate::i18n::t("users.err_username_long"));
    }
    if !username
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
    {
        return Some(crate::i18n::t("users.err_username_chars"));
    }
    None
}

/// Loads the signed-cookie session key from `<data_dir>/session.key`, creating
/// and persisting a new one on first run so sessions survive container
/// restarts. Panics with a message that says what to do when the key can't
/// be loaded or created; see [`try_load_or_create_key`].
pub fn load_or_create_key(data_dir: &Path) -> Key {
    match try_load_or_create_key(data_dir) {
        Ok(key) => key,
        Err(message) => panic!("{message}"),
    }
}

/// Bytes of the key file: the cookie crate's master key.
const KEY_LEN: usize = 64;

/// [`load_or_create_key`] with the failure as a message.
///
/// A file that exists is never replaced: one that can't be read or is
/// shorter than 64 bytes is an error, because a silently made new key would
/// sign everyone out and make every sealed secret (notification channels)
/// unreadable. A missing file is created exclusively, with mode 0600 from
/// the start on Unix, and written once.
pub fn try_load_or_create_key(data_dir: &Path) -> Result<Key, String> {
    let path = data_dir.join("session.key");
    let unusable = |why: String| {
        format!(
            "the session key {} {why}. Restore it from a backup; delete the file only if you accept              that everyone is signed out and saved channel secrets must be entered again",
            path.display()
        )
    };
    for _ in 0..2 {
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() >= KEY_LEN => return Ok(Key::from(&bytes)),
            Ok(bytes) => {
                return Err(unusable(format!(
                    "is only {} bytes long (it needs {KEY_LEN})",
                    bytes.len()
                )));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(unusable(format!("can't be read ({err})"))),
        }
        let key = Key::generate();
        match write_new_key_file(&path, key.master()) {
            Ok(()) => return Ok(key),
            // Another process made it between our read and our create: read theirs.
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "could not create the session key {}: {err}",
                    path.display()
                ));
            }
        }
    }
    Err(unusable("keeps changing while it is being read".into()))
}

/// Creates `path` (which must not exist) with only the owner allowed to
/// read it, and writes `bytes`. A failed write leaves no half-written file.
fn write_new_key_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    if written.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A product whose users sign in some other way stores a marker in
    /// `password_hash` (`"!external"`). It is no PHC string: it never
    /// verifies, whatever is offered, and never panics.
    #[test]
    fn a_hash_that_is_not_a_hash_never_verifies() {
        let real = hash_password("Correct-Horse-Battery-2026");
        assert!(verify_password(&real, "Correct-Horse-Battery-2026"));
        for marker in [
            "!external",
            "",
            "!",
            "*",
            "$",
            "$argon2id$",
            "$argon2id$v=19$m=19456,t=2,p=1$",
            "$argon2id$v=19$m=19456,t=2,p=1$not-base64!$also-not",
            "$argon2id$v=19$m=0,t=0,p=0$AAAAAAAAAAA$AAAAAAAAAAA",
            "$unknown$v=1$x$y",
            "\u{0}\u{1}\u{2}",
            "!external\n",
        ] {
            for attempt in ["", "!external", marker, "Correct-Horse-Battery-2026"] {
                assert!(
                    !verify_password(marker, attempt),
                    "{marker:?} verified {attempt:?}"
                );
            }
        }
    }

    fn user(role: Role) -> CurrentUser {
        CurrentUser {
            id: 1,
            username: "u".into(),
            display_name: "u".into(),
            role,
            account_role: role,
            scopes: Scopes::All,
        }
    }

    static DECLARED: &[crate::config::ExtraScope] = &[
        crate::config::ExtraScope {
            name: "uploads:write",
            label: "x",
            help: "x",
            implies_read: true,
            paths: &[],
        },
        crate::config::ExtraScope {
            name: "push",
            label: "x",
            help: "x",
            implies_read: false,
            paths: &["/api/push/"],
        },
    ];

    #[test]
    fn existing_tokens_keep_their_access() {
        assert_eq!(
            token_scopes(TokenScope::Write, &[], DECLARED),
            Some(Scopes::All)
        );
        assert_eq!(
            token_scopes(TokenScope::Read, &[], DECLARED),
            Some(Scopes::Only(vec![]))
        );
        assert_eq!(
            token_scopes(TokenScope::Read, &[], &[]),
            Some(Scopes::Only(vec![]))
        );
    }

    #[test]
    fn stored_scopes_count_while_the_product_declares_them() {
        let stored = ["gone".to_string(), "uploads:write".to_string()];
        assert_eq!(
            token_scopes(TokenScope::Read, &stored, DECLARED),
            Some(Scopes::Only(vec!["uploads:write"]))
        );
        // Only scopes the product dropped: the token stops working.
        let gone = ["gone".to_string()];
        assert_eq!(token_scopes(TokenScope::Read, &gone, DECLARED), None);
        assert_eq!(
            token_scopes(TokenScope::Write, &gone, DECLARED),
            Some(Scopes::All)
        );
    }

    #[test]
    fn may_act_as_looks_past_the_tokens_cap() {
        let u = CurrentUser {
            account_role: Role::Editor,
            scopes: Scopes::Only(vec![]),
            ..user(Role::Viewer)
        };
        assert!(u.require_editor().is_err());
        assert!(u.may_act_as(Role::Editor) && !u.may_act_as(Role::Admin));
    }

    /// The test configuration declares no scopes, so no request has one.
    #[test]
    fn sessions_and_plain_tokens_read_and_carry_no_undeclared_scope() {
        assert!(user(Role::Admin).can_read());
        assert!(!user(Role::Admin).scope_allows_path("/api/push/x"));
        let read_only = CurrentUser {
            scopes: Scopes::Only(vec![]),
            ..user(Role::Viewer)
        };
        assert!(read_only.can_read());
        assert!(!read_only.scope_allows_path("/api/push/x"));
    }

    #[test]
    fn scope_paths_match_whole_segments() {
        assert!(path_under("/api/push", "/api/push"));
        assert!(path_under("/api/push", "/api/push/x"));
        assert!(!path_under("/api/push", "/api/pushover"));
        assert!(!path_under("/api/push", "/api/pus"));
        assert!(path_under("/api/push/", "/api/push/x"));
        assert!(!path_under("/api/push/", "/api/pushover"));
        assert!(!path_under("/api/push/", "/api/push"));
    }

    fn key_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vole-kit-key-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_session_key_is_created_once_and_then_kept() {
        let dir = key_dir("create");
        let first = try_load_or_create_key(&dir).unwrap();
        let bytes = std::fs::read(dir.join("session.key")).unwrap();
        assert_eq!(bytes.len(), KEY_LEN);
        assert_eq!(bytes, first.master());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join("session.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = try_load_or_create_key(&dir).unwrap();
        assert_eq!(again.master(), first.master());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_session_key_stops_startup_and_is_left_alone() {
        let dir = key_dir("short");
        let path = dir.join("session.key");
        for content in [&b""[..], &[7u8; 63][..]] {
            std::fs::write(&path, content).unwrap();
            let err = try_load_or_create_key(&dir).unwrap_err();
            assert!(
                err.contains("session key") && err.contains("backup"),
                "{err}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), content, "never overwritten");
        }
        // Something that can't be read as a file at all.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = try_load_or_create_key(&dir).unwrap_err();
        assert!(err.contains("can't be read"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn require_allows_equal_or_higher_roles() {
        assert!(user(Role::Editor).require(Role::Editor).is_ok());
        assert!(user(Role::Admin).require(Role::Editor).is_ok());
    }

    #[test]
    fn require_rejects_lower_roles() {
        assert!(matches!(
            user(Role::Viewer).require(Role::Editor),
            Err(ApiError::Forbidden)
        ));
    }

    #[test]
    fn require_admin_rejects_editors() {
        assert!(user(Role::Editor).require_admin().is_err());
    }

    #[test]
    fn password_policy_rejects_short_passwords() {
        assert!(password_problem("Sh0rt!", "u").is_some());
    }

    #[test]
    fn password_policy_counts_characters_not_bytes() {
        assert!(password_problem("ææææøøøøåå", "").is_some_and(|m| m.contains("guess")));
        assert!(password_problem("blåbærsyltetøy", "").is_none());
    }

    #[test]
    fn password_policy_rejects_username_inside_password() {
        assert!(password_problem("my-alice-password", "Alice").is_some());
    }

    #[test]
    fn password_policy_rejects_common_and_repetitive_passwords() {
        assert!(password_problem("Password123", "u").is_some());
        assert!(password_problem("abababababab", "u").is_some());
    }

    #[test]
    fn password_policy_accepts_passphrases() {
        assert_eq!(password_problem("correct horse battery", "admin"), None);
    }

    #[test]
    fn username_policy() {
        assert!(username_problem("ada.lovelace@example").is_none());
        assert!(username_problem("has space").is_some());
        assert!(username_problem("").is_some());
        assert!(username_problem("<script>").is_some());
    }

    #[test]
    fn hashes_verify_and_reject() {
        let h = try_hash_password("correct horse").unwrap();
        assert!(verify_password(&h, "correct horse"));
        assert!(!verify_password(&h, "wrong horse"));
        assert!(!verify_password("not a hash", "x"));
    }
}
