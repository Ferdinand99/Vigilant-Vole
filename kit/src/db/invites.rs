//! Invitation links (`invites`, the [`INVITES`](super::migrate::INVITES)
//! migration; [`KitFeatures::invites`](crate::config::KitFeatures::invites)).
//!
//! An invitation is looked up by the SHA-256 of its token. It is *pending*
//! until it is used, revoked or past `expires_at`; every function here that
//! takes an invitation by token only sees pending ones, so an expired, used,
//! revoked and unknown token are indistinguishable.

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use super::role::Role;
use super::users;

/// How long an invitation lasts.
pub const LIFETIME_DAYS: i64 = 7;

/// How many invitations may be pending at once.
pub const MAX_PENDING: i64 = 50;

/// A pending invitation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub id: i64,
    /// The role the new account gets.
    pub role: Role,
    /// A name the admin suggested ("" for none).
    pub display_name: String,
    /// The admin who created it (`None` once that account is gone).
    pub created_by: Option<i64>,
    /// The creator's username, when it could be read.
    pub created_by_name: Option<String>,
    pub created_at: String,
    /// UTC, `YYYY-MM-DD HH:MM:SS`.
    pub expires_at: String,
}

const PENDING: &str = "used_at IS NULL AND revoked_at IS NULL AND expires_at > datetime('now')";

fn row_to_invite(row: &Row) -> rusqlite::Result<Invite> {
    let role: String = row.get("role")?;
    Ok(Invite {
        id: row.get("id")?,
        // The column's CHECK allows nothing else; never grant more than a
        // viewer for a value this build doesn't know.
        role: Role::parse(&role).unwrap_or(Role::Viewer),
        display_name: row.get("display_name")?,
        created_by: row.get("created_by")?,
        created_by_name: row.get("created_by_name")?,
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
    })
}

const SELECT: &str =
    "SELECT i.id, i.role, i.display_name, i.created_by, i.created_at, i.expires_at,
        (SELECT u.username FROM users u WHERE u.id = i.created_by) AS created_by_name
     FROM invites i";

/// Adds an invitation that lasts [`LIFETIME_DAYS`], and forgets the
/// finished ones that are more than 30 days old. `token_hash` is the SHA-256
/// of the token.
pub fn insert(
    conn: &Connection,
    token_hash: &[u8],
    role: Role,
    display_name: &str,
    created_by: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "DELETE FROM invites
         WHERE COALESCE(used_at, revoked_at, expires_at) < datetime('now', '-30 days')",
        [],
    )?;
    conn.execute(
        "INSERT INTO invites (token_hash, role, display_name, created_by, expires_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now', '+' || ?5 || ' days'))",
        params![
            token_hash,
            role.as_str(),
            display_name,
            created_by,
            LIFETIME_DAYS
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// How many invitations are pending.
pub fn count_pending(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM invites WHERE {PENDING}"),
        [],
        |r| r.get(0),
    )
}

/// The pending invitations, soonest to expire first.
pub fn list_pending(conn: &Connection) -> rusqlite::Result<Vec<Invite>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE i.used_at IS NULL AND i.revoked_at IS NULL
           AND i.expires_at > datetime('now')
         ORDER BY i.expires_at, i.id"
    ))?;
    let rows = stmt.query_map([], row_to_invite)?;
    rows.collect()
}

/// The pending invitation with this token hash.
pub fn find_pending(conn: &Connection, token_hash: &[u8]) -> rusqlite::Result<Option<Invite>> {
    conn.query_row(
        &format!(
            "{SELECT} WHERE i.token_hash = ?1 AND i.used_at IS NULL AND i.revoked_at IS NULL
               AND i.expires_at > datetime('now')"
        ),
        params![token_hash],
        row_to_invite,
    )
    .optional()
}

/// Revokes a pending invitation; `false` when there was none.
pub fn revoke(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        &format!("UPDATE invites SET revoked_at = datetime('now') WHERE id = ?1 AND {PENDING}"),
        params![id],
    )? == 1)
}

/// Marks the pending invitation with this token hash as used by `user_id`.
/// One statement, so of two callers racing for the same invitation exactly
/// one gets `true`. `false` when it was used, revoked or expired, or never
/// existed.
pub fn consume(conn: &Connection, token_hash: &[u8], user_id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        &format!(
            "UPDATE invites SET used_at = datetime('now'), used_by = ?2
             WHERE token_hash = ?1 AND {PENDING}"
        ),
        params![token_hash, user_id],
    )? == 1)
}

/// Why [`accept`] did not create the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptError {
    /// The invitation is not pending (used, revoked, expired or unknown).
    Invalid,
    /// The username is already in use. The invitation stays pending.
    UsernameTaken,
}

/// Uses the invitation and creates the account with its role, in one
/// transaction: either both happen or neither. `display_name` replaces the
/// one the invitation suggests unless it is empty. Returns the new user id.
pub fn accept(
    conn: &mut Connection,
    token_hash: &[u8],
    username: &str,
    display_name: &str,
    password_hash: &str,
) -> rusqlite::Result<Result<i64, AcceptError>> {
    // The first statement is the claim, a write: with an immediate
    // transaction it takes the database's write lock, so a second attempt
    // waits and then finds nothing pending.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let claimed: Option<(String, String)> = tx
        .query_row(
            &format!(
                "UPDATE invites SET used_at = datetime('now')
                 WHERE token_hash = ?1 AND {PENDING}
                 RETURNING role, display_name"
            ),
            params![token_hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((role, suggested)) = claimed else {
        return Ok(Err(AcceptError::Invalid));
    };
    if users::username_taken(&tx, username)? {
        // Dropping the transaction undoes the claim.
        return Ok(Err(AcceptError::UsernameTaken));
    }
    let role = Role::parse(&role).unwrap_or(Role::Viewer);
    let display_name = if display_name.is_empty() {
        suggested.as_str()
    } else {
        display_name
    };
    let id = users::insert_user_full(&tx, username, password_hash, role, display_name)?;
    tx.execute(
        "UPDATE invites SET used_by = ?2 WHERE token_hash = ?1",
        params![token_hash, id],
    )?;
    tx.commit()?;
    Ok(Ok(id))
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
        conn.execute_batch(crate::db::migrate::INVITES.1).unwrap();
        conn.execute(
            "INSERT INTO users (id, username, password_hash, role) VALUES (1, 'ada', 'x', 'admin')",
            [],
        )
        .unwrap();
        conn
    }

    fn hash(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn a_pending_invitation_is_found_by_its_hash_only() {
        let c = conn();
        let id = insert(&c, &hash(1), Role::Editor, "Bob", 1).unwrap();
        let found = find_pending(&c, &hash(1)).unwrap().unwrap();
        assert_eq!(found.id, id);
        assert_eq!(found.role, Role::Editor);
        assert_eq!(found.display_name, "Bob");
        assert_eq!(found.created_by, Some(1));
        assert!(find_pending(&c, &hash(2)).unwrap().is_none());
        assert_eq!(count_pending(&c).unwrap(), 1);
        let listed = list_pending(&c).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].created_by_name.as_deref(), Some("ada"));
    }

    #[test]
    fn used_revoked_and_expired_invitations_are_not_pending() {
        let c = conn();
        insert(&c, &hash(1), Role::Viewer, "", 1).unwrap();
        insert(&c, &hash(2), Role::Viewer, "", 1).unwrap();
        let expired = insert(&c, &hash(3), Role::Viewer, "", 1).unwrap();
        assert!(consume(&c, &hash(1), 1).unwrap());
        assert!(!consume(&c, &hash(1), 1).unwrap(), "only once");
        let two = find_pending(&c, &hash(2)).unwrap().unwrap().id;
        assert!(revoke(&c, two).unwrap());
        assert!(!revoke(&c, two).unwrap(), "already revoked");
        c.execute(
            "UPDATE invites SET expires_at = datetime('now', '-1 second') WHERE id = ?1",
            params![expired],
        )
        .unwrap();
        for n in 1..=3 {
            assert!(find_pending(&c, &hash(n)).unwrap().is_none(), "{n}");
            assert!(!consume(&c, &hash(n), 1).unwrap(), "{n}");
        }
        assert!(!revoke(&c, expired).unwrap(), "expired");
        assert_eq!(count_pending(&c).unwrap(), 0);
        assert!(list_pending(&c).unwrap().is_empty());
    }

    #[test]
    fn an_invitation_lasts_seven_days() {
        let c = conn();
        insert(&c, &hash(1), Role::Viewer, "", 1).unwrap();
        let days: f64 = c
            .query_row(
                "SELECT julianday(expires_at) - julianday(created_at) FROM invites",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!((days - 7.0).abs() < 0.01, "{days}");
    }

    #[test]
    fn accepting_creates_the_user_with_the_invited_role_once() {
        let mut c = conn();
        insert(&c, &hash(1), Role::Editor, "Suggested", 1).unwrap();
        let id = accept(&mut c, &hash(1), "bob", "", "hash")
            .unwrap()
            .unwrap();
        let bob = users::get_user(&c, id).unwrap().unwrap();
        assert_eq!(bob.role, Role::Editor);
        assert_eq!(bob.display_name, "Suggested");
        assert_eq!(bob.username, "bob");
        let used_by: Option<i64> = c
            .query_row("SELECT used_by FROM invites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(used_by, Some(id));
        assert_eq!(
            accept(&mut c, &hash(1), "carol", "", "hash").unwrap(),
            Err(AcceptError::Invalid)
        );
        assert!(users::get_user_by_username(&c, "carol").unwrap().is_none());
    }

    #[test]
    fn a_taken_username_keeps_the_invitation_pending() {
        let mut c = conn();
        insert(&c, &hash(1), Role::Viewer, "", 1).unwrap();
        assert_eq!(
            accept(&mut c, &hash(1), "ADA", "", "hash").unwrap(),
            Err(AcceptError::UsernameTaken)
        );
        assert!(find_pending(&c, &hash(1)).unwrap().is_some());
        assert!(
            accept(&mut c, &hash(1), "bob", "Bob", "hash")
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn deleting_the_creator_keeps_the_invitation() {
        let mut c = conn();
        c.execute(
            "INSERT INTO users (id, username, password_hash, role) VALUES (2, 'eve', 'x', 'admin')",
            [],
        )
        .unwrap();
        insert(&c, &hash(1), Role::Viewer, "", 2).unwrap();
        c.execute("DELETE FROM users WHERE id = 2", []).unwrap();
        let found = find_pending(&c, &hash(1)).unwrap().unwrap();
        assert_eq!(found.created_by, None);
        assert!(accept(&mut c, &hash(1), "bob", "", "hash").unwrap().is_ok());
    }
}
