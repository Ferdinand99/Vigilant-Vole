//! Invitation links ([`KitFeatures::invites`](crate::config::KitFeatures::invites)).
//!
//! An admin creates an invitation on the Users page and gets a link,
//! `{base_url}/invite/{token}`, once. The token is 256 random bits in base62;
//! only its SHA-256 is stored, so a leaked database holds no usable link. The
//! link works once, for seven days, and until the admin revokes it.
//!
//! In the kit's own mode the link opens a page where the invitee picks a user
//! name and a password. A product with
//! [`CredentialMode::External`](crate::config::CredentialMode::External)
//! gets the visitor on its setup page instead
//! (`{setup_path}?invite={token}`) and finishes the job itself with the
//! functions below:
//!
//! ```ignore
//! // On the product's setup page, with ?invite=<token>:
//! let Some(invite) = invites::lookup(&core, &token).await? else {
//!     return not_valid(); // used, revoked, expired or unknown: the same
//! };
//! // ... the product's own registration; then one transaction that makes
//! // the account with the invited role and uses up the invitation:
//! match invites::accept(&core, &token, &username, "", "!external").await? {
//!     Ok(user_id) => { /* sign the new user in with guard::login_jar */ }
//!     Err(AcceptError::Invalid) => not_valid(),
//!     Err(AcceptError::UsernameTaken) => ask_for_another_name(),
//! }
//! ```
//!
//! A product that creates the account some other way calls [`create_user`]
//! and then [`consume`]; if `consume` says the invitation is gone it should
//! remove the account again ([`accept`] avoids that window).

use sha2::{Digest, Sha256};

pub use crate::db::invites::{AcceptError, Invite, LIFETIME_DAYS, MAX_PENDING};

use crate::db::invites as db_invites;
use crate::db::role::Role;
use crate::db::users as db_users;
use crate::http::ApiResult;
use crate::state::Core;

/// Characters in a token: 62^43 > 2^256.
pub const TOKEN_LEN: usize = 43;

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A new random token: 43 base62 characters, about 256 bits. Show it once;
/// store [`hash`].
pub fn generate() -> String {
    // Rejection sampling: 248 = 4 x 62, so `b % 62` is uniform for b < 248.
    let mut out = String::with_capacity(TOKEN_LEN);
    let mut buf = [0u8; 64];
    while out.len() < TOKEN_LEN {
        rand::fill(&mut buf);
        for b in buf {
            if out.len() == TOKEN_LEN {
                break;
            }
            if b < 248 {
                out.push(char::from(BASE62[usize::from(b % 62)]));
            }
        }
    }
    out
}

/// Cheap shape check before touching the database.
pub fn is_well_formed(token: &str) -> bool {
    token.len() == TOKEN_LEN && token.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The SHA-256 of a token (the `invites.token_hash` column).
pub fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The pending invitation for this token. `None` for a token that is
/// unknown, used, revoked or expired, so the caller can't tell those apart.
/// The lookup is by hash, so no comparison of secrets takes place.
pub async fn lookup(core: &Core, token: &str) -> ApiResult<Option<Invite>> {
    if !is_well_formed(token) {
        return Ok(None);
    }
    let hash = hash(token);
    core.with_conn(move |c| db_invites::find_pending(c, &hash))
        .await
}

/// The invitation was not pending (used, revoked, expired or unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotPending;

impl std::fmt::Display for NotPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the invitation is no longer valid")
    }
}

impl std::error::Error for NotPending {}

/// Uses up the invitation for the account `user_id` the product has
/// created. One statement: of two calls racing for the same invitation, one
/// succeeds; it fails when the invitation was used, revoked or has expired
/// in the meantime.
pub async fn consume(core: &Core, token: &str, user_id: i64) -> ApiResult<Result<(), NotPending>> {
    if !is_well_formed(token) {
        return Ok(Err(NotPending));
    }
    let hash = hash(token);
    let used = core
        .with_conn(move |c| db_invites::consume(c, &hash, user_id))
        .await?;
    Ok(if used { Ok(()) } else { Err(NotPending) })
}

/// Creates an account: the user name must pass
/// [`username_problem`](super::username_problem), and `password_hash` is
/// stored as given (the kit never looks inside it; for an account that
/// signs in some other way it can be a marker no password verifies against,
/// such as `"!external"`). `None` when the user name is taken
/// (case-insensitively). Prefer [`accept`], which also uses up the invitation.
pub async fn create_user(
    core: &Core,
    username: &str,
    display_name: &str,
    role: Role,
    password_hash: &str,
) -> ApiResult<Option<i64>> {
    let (username, display_name) = (username.to_string(), display_name.to_string());
    let password_hash = password_hash.to_string();
    core.with_conn(move |c| {
        let tx = c.transaction()?;
        if db_users::username_taken(&tx, &username)? {
            return Ok(None);
        }
        let id = db_users::insert_user_full(&tx, &username, &password_hash, role, &display_name)?;
        tx.commit()?;
        Ok(Some(id))
    })
    .await
}

/// Creates the account with the invitation's role and uses up the
/// invitation, in one transaction: either both happen or neither (a taken
/// user name leaves the invitation pending). `display_name` replaces the one
/// the invitation suggests unless it is empty. Check the user name
/// ([`username_problem`](super::username_problem)) first. Returns the new
/// user's id.
pub async fn accept(
    core: &Core,
    token: &str,
    username: &str,
    display_name: &str,
    password_hash: &str,
) -> ApiResult<Result<i64, AcceptError>> {
    if !is_well_formed(token) {
        return Ok(Err(AcceptError::Invalid));
    }
    let hash = hash(token);
    let (username, display_name) = (username.to_string(), display_name.to_string());
    let password_hash = password_hash.to_string();
    core.with_conn(move |c| db_invites::accept(c, &hash, &username, &display_name, &password_hash))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bits_of_base62() {
        let a = generate();
        let b = generate();
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_LEN);
        assert!(is_well_formed(&a));
        // Every character is used eventually; none is out of the alphabet.
        let many: String = (0..200).map(|_| generate()).collect();
        assert!(many.bytes().all(|c| BASE62.contains(&c)));
        for c in BASE62 {
            assert!(many.as_bytes().contains(c), "{}", char::from(*c));
        }
    }

    #[test]
    fn the_shape_check_rejects_the_wrong_length_and_characters() {
        assert!(!is_well_formed(""));
        assert!(!is_well_formed(&"a".repeat(TOKEN_LEN - 1)));
        assert!(!is_well_formed(&"a".repeat(TOKEN_LEN + 1)));
        assert!(!is_well_formed(&format!("{}-", "a".repeat(TOKEN_LEN - 1))));
        assert!(!is_well_formed(&format!("{}é", "a".repeat(TOKEN_LEN - 2))));
        assert!(is_well_formed(&"a".repeat(TOKEN_LEN)));
    }

    #[test]
    fn the_stored_value_is_the_sha256() {
        // SHA-256("abc")
        assert_eq!(
            hex::encode(hash("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
