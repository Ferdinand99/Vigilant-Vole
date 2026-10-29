//! One-time sign-in codes and users' email addresses (`sign_in_codes` and
//! `user_emails`, the [`SIGN_IN_CODES`](super::migrate::SIGN_IN_CODES)
//! migration;
//! [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes)).
//!
//! A code is looked up by the SHA-256 of its text. It is *pending* until it
//! is used, revoked or past `expires_at`; every function here that takes a
//! code by hash only sees pending ones, so an expired, used, revoked and
//! unknown code are indistinguishable.
//!
//! A pending code is revoked by everything that settles how its user signs
//! in: a new password (their own, an admin's, or the one a redeemed code
//! sets), a reset of their two-factor, disabling or enabling the account and
//! a successful sign-in. [`revoke_if_enabled`] is what those call.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{api_tokens, users};

/// How long a code works.
pub const LIFETIME_HOURS: i64 = 24;

/// The longest email address stored (RFC 5321).
pub const MAX_EMAIL_LEN: usize = 254;

const PENDING: &str = "used_at IS NULL AND revoked_at IS NULL AND expires_at > datetime('now')";

/// Stores a new code for `user_id` (valid for [`LIFETIME_HOURS`]) and
/// revokes the user's earlier pending ones, in one transaction. Also forgets
/// the finished codes that are more than 30 days old. `code_hash` is the
/// SHA-256 of the code. Returns the new code's `expires_at` (UTC,
/// `YYYY-MM-DD HH:MM:SS`).
pub fn insert(
    conn: &mut Connection,
    user_id: i64,
    code_hash: &[u8],
    created_by: i64,
) -> rusqlite::Result<String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM sign_in_codes
         WHERE COALESCE(used_at, revoked_at, expires_at) < datetime('now', '-30 days')",
        [],
    )?;
    tx.execute(
        &format!(
            "UPDATE sign_in_codes SET revoked_at = datetime('now') WHERE user_id = ?1 AND {PENDING}"
        ),
        params![user_id],
    )?;
    let expires: String = tx.query_row(
        "INSERT INTO sign_in_codes (user_id, code_hash, created_by, expires_at)
         VALUES (?1, ?2, ?3, datetime('now', '+' || ?4 || ' hours'))
         RETURNING expires_at",
        params![user_id, code_hash, created_by, LIFETIME_HOURS],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(expires)
}

/// Uses the pending code with this hash for `user_id`, a user that is
/// active, and signs the user out everywhere (their `session_version` moves
/// on) and deletes all of the user's API tokens: a code is a recovery path,
/// so a token that may have leaked must not outlive it. With
/// `password_hash` the user's password is set in the same transaction. One
/// transaction, and the claim is a single statement: of two
/// callers racing for the same code exactly one gets `Some`. The code must
/// belong to `user_id`: a valid code for somebody else is left alone, and
/// the answer is `None` exactly as for a code that does not exist. Returns
/// the user's new `session_version`.
pub fn redeem(
    conn: &mut Connection,
    code_hash: &[u8],
    user_id: i64,
    password_hash: Option<&str>,
) -> rusqlite::Result<Option<i64>> {
    // The claim is a write: an immediate transaction takes the database's
    // write lock, so a second attempt waits and then finds nothing pending.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let claimed = tx.execute(
        &format!(
            "UPDATE sign_in_codes SET used_at = datetime('now')
             WHERE code_hash = ?1 AND user_id = ?2 AND {PENDING}
               AND EXISTS (SELECT 1 FROM users WHERE id = ?2 AND active = 1)"
        ),
        params![code_hash, user_id],
    )?;
    if claimed != 1 {
        return Ok(None);
    }
    api_tokens::delete_all_for_user(&tx, user_id)?;
    let version = match password_hash {
        Some(hash) => users::set_password(&tx, user_id, hash)?,
        None => users::bump_session_version(&tx, user_id)?,
    };
    tx.commit()?;
    Ok(Some(version))
}

/// Revokes the pending codes of a user; returns how many there were.
pub fn revoke_for_user(conn: &Connection, user_id: i64) -> rusqlite::Result<usize> {
    conn.execute(
        &format!(
            "UPDATE sign_in_codes SET revoked_at = datetime('now') WHERE user_id = ?1 AND {PENDING}"
        ),
        params![user_id],
    )
}

/// [`revoke_for_user`] for code that runs whether or not the feature is on
/// (the password and account functions of `db::users`): the `sign_in_codes`
/// table only exists with the feature, so without it this does nothing.
pub fn revoke_if_enabled(conn: &Connection, user_id: i64) -> rusqlite::Result<usize> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sign_in_codes')",
        [],
        |r| r.get(0),
    )?;
    if exists {
        revoke_for_user(conn, user_id)
    } else {
        Ok(0)
    }
}

/// The users that have a pending code, `(user id, expires_at)`.
pub fn pending_users(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT user_id, MAX(expires_at) FROM sign_in_codes WHERE {PENDING} GROUP BY user_id"
    ))?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// How many codes of this user are pending.
pub fn count_pending(conn: &Connection, user_id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM sign_in_codes WHERE user_id = ?1 AND {PENDING}"),
        params![user_id],
        |r| r.get(0),
    )
}

// ---------------------------------------------------------------------------
// Email addresses
// ---------------------------------------------------------------------------

/// The user's email address, if one is set.
pub fn email(conn: &Connection, user_id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT email FROM user_emails WHERE user_id = ?1",
        params![user_id],
        |r| r.get(0),
    )
    .optional()
}

/// Every user's email address, `(user id, address)`.
pub fn emails(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT user_id, email FROM user_emails")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Sets the user's email address, or with `None` (or blank) removes it. A
/// changed address is not verified.
pub fn set_email(conn: &Connection, user_id: i64, email: Option<&str>) -> rusqlite::Result<()> {
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        Some(email) => conn.execute(
            "INSERT INTO user_emails (user_id, email) VALUES (?1, ?2)
             ON CONFLICT(user_id) DO UPDATE SET
                 verified_at = CASE WHEN email = excluded.email THEN verified_at ELSE NULL END,
                 email = excluded.email",
            params![user_id, email],
        )?,
        None => conn.execute(
            "DELETE FROM user_emails WHERE user_id = ?1",
            params![user_id],
        )?,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        for (_, sql) in crate::db::migrate::CORE {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch(crate::db::migrate::SIGN_IN_CODES.1)
            .unwrap();
        for (id, name) in [(1, "ada"), (2, "bob"), (3, "eve")] {
            conn.execute(
                "INSERT INTO users (id, username, password_hash, role, session_version)
                 VALUES (?1, ?2, 'x', 'admin', 10)",
                params![id, name],
            )
            .unwrap();
        }
        conn
    }

    fn hash(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn a_code_works_once_and_signs_the_user_out() {
        let mut c = conn();
        insert(&mut c, 2, &hash(1), 1).unwrap();
        assert_eq!(count_pending(&c, 2).unwrap(), 1);
        assert_eq!(redeem(&mut c, &hash(1), 2, Some("new")).unwrap(), Some(11));
        let (stored, version): (String, i64) = c
            .query_row(
                "SELECT password_hash, session_version FROM users WHERE id = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((stored.as_str(), version), ("new", 11));
        assert_eq!(redeem(&mut c, &hash(1), 2, Some("again")).unwrap(), None);
        let stored: String = c
            .query_row("SELECT password_hash FROM users WHERE id = 2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, "new", "the second try changed nothing");
        assert_eq!(count_pending(&c, 2).unwrap(), 0);
    }

    #[test]
    fn without_a_password_only_the_session_version_moves() {
        let mut c = conn();
        insert(&mut c, 2, &hash(1), 1).unwrap();
        assert_eq!(redeem(&mut c, &hash(1), 2, None).unwrap(), Some(11));
        let stored: String = c
            .query_row("SELECT password_hash FROM users WHERE id = 2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, "x");
    }

    #[test]
    fn a_code_of_another_user_is_left_alone() {
        let mut c = conn();
        insert(&mut c, 2, &hash(1), 1).unwrap();
        assert_eq!(redeem(&mut c, &hash(1), 3, Some("evil")).unwrap(), None);
        assert_eq!(redeem(&mut c, &hash(1), -1, Some("evil")).unwrap(), None);
        assert_eq!(count_pending(&c, 2).unwrap(), 1, "still pending");
        assert_eq!(redeem(&mut c, &hash(1), 2, None).unwrap(), Some(11));
    }

    #[test]
    fn a_new_code_revokes_the_earlier_ones() {
        let mut c = conn();
        insert(&mut c, 2, &hash(1), 1).unwrap();
        insert(&mut c, 3, &hash(3), 1).unwrap();
        insert(&mut c, 2, &hash(2), 1).unwrap();
        assert_eq!(redeem(&mut c, &hash(1), 2, None).unwrap(), None, "revoked");
        assert_eq!(redeem(&mut c, &hash(2), 2, None).unwrap(), Some(11));
        assert_eq!(count_pending(&c, 3).unwrap(), 1, "other users keep theirs");
    }

    #[test]
    fn expired_revoked_and_disabled_do_not_redeem() {
        let mut c = conn();
        insert(&mut c, 2, &hash(1), 1).unwrap();
        c.execute(
            "UPDATE sign_in_codes SET expires_at = datetime('now', '-1 second')",
            [],
        )
        .unwrap();
        assert_eq!(redeem(&mut c, &hash(1), 2, None).unwrap(), None, "expired");
        assert_eq!(count_pending(&c, 2).unwrap(), 0);

        insert(&mut c, 2, &hash(2), 1).unwrap();
        assert_eq!(revoke_for_user(&c, 2).unwrap(), 1);
        assert_eq!(redeem(&mut c, &hash(2), 2, None).unwrap(), None, "revoked");

        insert(&mut c, 2, &hash(3), 1).unwrap();
        c.execute("UPDATE users SET active = 0 WHERE id = 2", [])
            .unwrap();
        assert_eq!(redeem(&mut c, &hash(3), 2, None).unwrap(), None, "disabled");
        let version: i64 = c
            .query_row("SELECT session_version FROM users WHERE id = 2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, 10, "a refused code changes nothing");
    }

    #[test]
    fn redeeming_deletes_the_users_api_tokens_and_only_theirs() {
        let mut c = conn();
        for (user, hash) in [(2, "h2"), (3, "h3")] {
            c.execute(
                "INSERT INTO api_tokens (user_id, name, prefix, token_hash, scope)
                 VALUES (?1, 't', 'vv_x', ?2, 'read')",
                params![user, hash],
            )
            .unwrap();
        }
        insert(&mut c, 2, &hash(1), 1).unwrap();
        // A refused redeem leaves them.
        assert_eq!(redeem(&mut c, &hash(9), 2, None).unwrap(), None);
        let tokens = |c: &Connection, user: i64| -> i64 {
            c.query_row(
                "SELECT COUNT(*) FROM api_tokens WHERE user_id = ?1",
                [user],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!((tokens(&c, 2), tokens(&c, 3)), (1, 1));
        assert!(redeem(&mut c, &hash(1), 2, None).unwrap().is_some());
        assert_eq!((tokens(&c, 2), tokens(&c, 3)), (0, 1));
    }

    #[test]
    fn password_changes_two_factor_resets_and_account_changes_revoke_pending_codes() {
        let mut c = conn();
        let pending = |c: &Connection| count_pending(c, 2).unwrap();
        insert(&mut c, 2, &hash(1), 1).unwrap();
        users::set_password(&c, 2, "new").unwrap();
        assert_eq!(pending(&c), 0, "a new password");

        insert(&mut c, 2, &hash(2), 1).unwrap();
        users::disable_totp(&c, 2).unwrap();
        assert_eq!(pending(&c), 0, "a two-factor reset");

        insert(&mut c, 2, &hash(3), 1).unwrap();
        users::set_active(&mut c, 2, false).unwrap().unwrap();
        assert_eq!(pending(&c), 0, "disabling");
        insert(&mut c, 2, &hash(4), 1).unwrap();
        users::set_active(&mut c, 2, true).unwrap().unwrap();
        assert_eq!(pending(&c), 0, "enabling");
        // The code from before the disable does not come back.
        assert_eq!(redeem(&mut c, &hash(3), 2, None).unwrap(), None);

        // Another user's code is left alone.
        insert(&mut c, 3, &hash(5), 1).unwrap();
        users::set_password(&c, 2, "again").unwrap();
        assert_eq!(count_pending(&c, 3).unwrap(), 1);
        assert_eq!(pending_users(&c).unwrap().len(), 1);
    }

    #[test]
    fn without_the_feature_revoking_does_nothing() {
        // The tables of the feature are not there.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        for (_, sql) in crate::db::migrate::CORE {
            c.execute_batch(sql).unwrap();
        }
        c.execute(
            "INSERT INTO users (id, username, password_hash, role) VALUES (1, 'ada', 'x', 'admin')",
            [],
        )
        .unwrap();
        assert_eq!(revoke_if_enabled(&c, 1).unwrap(), 0);
        users::set_password(&c, 1, "y").unwrap();
        users::disable_totp(&c, 1).unwrap();
    }

    #[test]
    fn a_code_lasts_a_day() {
        let mut c = conn();
        let expires = insert(&mut c, 2, &hash(1), 1).unwrap();
        let hours: f64 = c
            .query_row(
                "SELECT (julianday(expires_at) - julianday(created_at)) * 24 FROM sign_in_codes",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!((hours - 24.0).abs() < 0.01, "{hours}");
        assert!(expires.len() == 19, "{expires}");
    }

    #[test]
    fn emails_are_set_changed_and_cleared() {
        let c = conn();
        assert_eq!(email(&c, 2).unwrap(), None);
        set_email(&c, 2, Some(" bob@example.com ")).unwrap();
        assert_eq!(email(&c, 2).unwrap().as_deref(), Some("bob@example.com"));
        c.execute("UPDATE user_emails SET verified_at = datetime('now')", [])
            .unwrap();
        set_email(&c, 2, Some("bob@example.com")).unwrap();
        let verified: Option<String> = c
            .query_row("SELECT verified_at FROM user_emails", [], |r| r.get(0))
            .unwrap();
        assert!(verified.is_some(), "the same address stays verified");
        set_email(&c, 2, Some("robert@example.com")).unwrap();
        let verified: Option<String> = c
            .query_row("SELECT verified_at FROM user_emails", [], |r| r.get(0))
            .unwrap();
        assert!(verified.is_none(), "a new address is not");
        assert_eq!(emails(&c).unwrap().len(), 1);
        set_email(&c, 2, Some("  ")).unwrap();
        assert_eq!(email(&c, 2).unwrap(), None);
        set_email(&c, 2, None).unwrap();
    }

    #[test]
    fn deleting_the_user_removes_codes_and_address() {
        let mut c = conn();
        insert(&mut c, 2, &hash(1), 1).unwrap();
        set_email(&c, 2, Some("bob@example.com")).unwrap();
        c.execute("DELETE FROM users WHERE id = 2", []).unwrap();
        let rows: i64 = c
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sign_in_codes) + (SELECT COUNT(*) FROM user_emails)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn deleting_the_creator_keeps_the_code() {
        let mut c = conn();
        insert(&mut c, 2, &hash(1), 3).unwrap();
        c.execute("DELETE FROM users WHERE id = 3", []).unwrap();
        assert_eq!(redeem(&mut c, &hash(1), 2, None).unwrap(), Some(11));
    }
}
