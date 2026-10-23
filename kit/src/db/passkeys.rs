//! Passkeys (WebAuthn credentials) and their challenges
//! (`kit/0003_passkeys.sql`). The ceremonies themselves are in
//! [`crate::auth::webauthn`]; the pages in `auth::pages::passkeys`.

use rusqlite::{Connection, OptionalExtension, Row, params};

/// How long a challenge may be answered.
pub const CHALLENGE_TTL_SECS: i64 = 5 * 60;

/// Pending challenges kept at most; beyond it the oldest go first (a flood
/// of sign-in starts can't grow the table without bound).
const MAX_CHALLENGES: i64 = 10_000;

/// What a challenge is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Adding a passkey, for one signed-in user.
    Register,
    /// Signing in; nobody is known yet.
    SignIn,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Purpose::Register => "register",
            Purpose::SignIn => "sign_in",
        }
    }
}

/// One stored passkey.
#[derive(Debug, Clone)]
pub struct Passkey {
    pub id: i64,
    pub user_id: i64,
    pub credential_id: Vec<u8>,
    /// The COSE_Key as the authenticator sent it.
    pub public_key: Vec<u8>,
    pub alg: i64,
    pub user_handle: Vec<u8>,
    pub name: String,
    pub sign_count: u32,
    /// Comma-separated transport hints (`internal,hybrid`).
    pub transports: String,
    pub backup_eligible: bool,
    pub backed_up: bool,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

/// A passkey to store after a verified registration.
#[derive(Debug, Clone)]
pub struct NewPasskey<'a> {
    pub user_id: i64,
    pub credential_id: &'a [u8],
    pub public_key: &'a [u8],
    pub alg: i64,
    pub user_handle: &'a [u8],
    pub name: &'a str,
    pub sign_count: u32,
    pub transports: &'a str,
    pub backup_eligible: bool,
    pub backed_up: bool,
}

const COLUMNS: &str = "id, user_id, credential_id, public_key, alg, user_handle, name, sign_count, \
     transports, backup_eligible, backed_up, created_at, last_used_at";

fn row_to_passkey(row: &Row) -> rusqlite::Result<Passkey> {
    let count: i64 = row.get("sign_count")?;
    Ok(Passkey {
        id: row.get("id")?,
        user_id: row.get("user_id")?,
        credential_id: row.get("credential_id")?,
        public_key: row.get("public_key")?,
        alg: row.get("alg")?,
        user_handle: row.get("user_handle")?,
        name: row.get("name")?,
        sign_count: u32::try_from(count).unwrap_or(u32::MAX),
        transports: row.get("transports")?,
        backup_eligible: row.get("backup_eligible")?,
        backed_up: row.get("backed_up")?,
        created_at: row.get("created_at")?,
        last_used_at: row.get("last_used_at")?,
    })
}

/// A user's passkeys, oldest first.
pub fn list_for_user(conn: &Connection, user_id: i64) -> rusqlite::Result<Vec<Passkey>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM passkeys WHERE user_id = ?1 ORDER BY id"
    ))?;
    stmt.query_map(params![user_id], row_to_passkey)?.collect()
}

/// How many passkeys each user has (users without any are left out).
pub fn counts_by_user(conn: &Connection) -> rusqlite::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare("SELECT user_id, COUNT(*) FROM passkeys GROUP BY user_id")?;
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

pub fn by_credential_id(
    conn: &Connection,
    credential_id: &[u8],
) -> rusqlite::Result<Option<Passkey>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM passkeys WHERE credential_id = ?1"),
        params![credential_id],
        row_to_passkey,
    )
    .optional()
}

/// The user handle of `user_id`'s passkeys, if it has any. All of a user's
/// passkeys share one, so an authenticator keeps one passkey per account.
pub fn user_handle(conn: &Connection, user_id: i64) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT user_handle FROM passkeys WHERE user_id = ?1 ORDER BY id LIMIT 1",
        params![user_id],
        |r| r.get(0),
    )
    .optional()
}

/// Stores a passkey; `None` when its credential id is already stored.
pub fn insert(conn: &Connection, new: &NewPasskey) -> rusqlite::Result<Option<i64>> {
    let inserted = conn.execute(
        "INSERT INTO passkeys (user_id, credential_id, public_key, alg, user_handle, name,
             sign_count, transports, backup_eligible, backed_up)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (credential_id) DO NOTHING",
        params![
            new.user_id,
            new.credential_id,
            new.public_key,
            new.alg,
            new.user_handle,
            new.name,
            i64::from(new.sign_count),
            new.transports,
            new.backup_eligible,
            new.backed_up,
        ],
    )?;
    Ok((inserted == 1).then(|| conn.last_insert_rowid()))
}

/// Records a sign-in with passkey `id`: the new signature counter, the
/// backup state and the time. Only applies while the stored counter is
/// still `seen` (two racing sign-ins with one assertion can't both count).
pub fn record_use(
    conn: &Connection,
    id: i64,
    seen: u32,
    sign_count: u32,
    backed_up: bool,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE passkeys SET sign_count = ?3, backed_up = ?4, last_used_at = datetime('now')
         WHERE id = ?1 AND sign_count = ?2",
        params![id, i64::from(seen), i64::from(sign_count), backed_up],
    )?;
    Ok(n == 1)
}

/// Renames one of `user_id`'s passkeys.
pub fn rename(conn: &Connection, user_id: i64, id: i64, name: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE passkeys SET name = ?3 WHERE id = ?1 AND user_id = ?2",
        params![id, user_id, name],
    )?;
    Ok(n == 1)
}

/// Deletes one of `user_id`'s passkeys.
pub fn delete(conn: &Connection, user_id: i64, id: i64) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM passkeys WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )?;
    Ok(n == 1)
}

/// Deletes all of a user's passkeys (an admin's reset); returns how many.
pub fn delete_all_for_user(conn: &Connection, user_id: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM passkeys WHERE user_id = ?1", params![user_id])
}

// ---------------------------------------------------------------------------
// Challenges
// ---------------------------------------------------------------------------

/// Stores a fresh challenge (base64url) that expires
/// [`CHALLENGE_TTL_SECS`] after `now`, dropping expired ones first. A
/// registration's carries the user handle it offered, which the
/// authenticator keeps but doesn't send back.
pub fn put_challenge(
    conn: &Connection,
    challenge: &str,
    purpose: Purpose,
    user_id: Option<i64>,
    user_handle: Option<&[u8]>,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM passkey_challenges WHERE expires_at <= ?1",
        params![now],
    )?;
    let pending: i64 =
        conn.query_row("SELECT COUNT(*) FROM passkey_challenges", [], |r| r.get(0))?;
    if pending >= MAX_CHALLENGES {
        conn.execute(
            "DELETE FROM passkey_challenges WHERE challenge IN
                 (SELECT challenge FROM passkey_challenges ORDER BY expires_at LIMIT ?1)",
            params![pending - MAX_CHALLENGES + 1],
        )?;
    }
    conn.execute(
        "INSERT INTO passkey_challenges (challenge, purpose, user_id, user_handle, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            challenge,
            purpose.as_str(),
            user_id,
            user_handle,
            now + CHALLENGE_TTL_SECS
        ],
    )?;
    Ok(())
}

/// Uses up a challenge: `Some` when it was issued for `purpose` (and, for
/// a registration, for `user_id`) and hasn't expired, with the user handle
/// stored with it. Either way it can't be used again: the row is gone once
/// taken.
pub fn take_challenge(
    conn: &Connection,
    challenge: &str,
    purpose: Purpose,
    user_id: Option<i64>,
    now: i64,
) -> rusqlite::Result<Option<Option<Vec<u8>>>> {
    type Taken = (String, Option<i64>, Option<Vec<u8>>, i64);
    let row: Option<Taken> = conn
        .query_row(
            "DELETE FROM passkey_challenges WHERE challenge = ?1
             RETURNING purpose, user_id, user_handle, expires_at",
            params![challenge],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(row
        .filter(|(p, user, _, expires_at)| {
            p == purpose.as_str() && *user == user_id && *expires_at > now
        })
        .map(|(_, _, handle, _)| handle))
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
        conn.execute(
            "INSERT INTO users (id, username, password_hash) VALUES (1, 'ada', 'x'), (2, 'bob', 'x')",
            [],
        )
        .unwrap();
        conn
    }

    fn new(user_id: i64, cred: &[u8]) -> NewPasskey<'_> {
        NewPasskey {
            user_id,
            credential_id: cred,
            public_key: b"key",
            alg: -7,
            user_handle: b"handle",
            name: "Passkey 1",
            sign_count: 0,
            transports: "internal",
            backup_eligible: true,
            backed_up: false,
        }
    }

    #[test]
    fn challenges_are_single_use_and_bound_to_purpose_user_and_time() {
        let c = conn();
        put_challenge(&c, "a", Purpose::SignIn, None, None, 1000).unwrap();
        assert_eq!(
            take_challenge(&c, "a", Purpose::SignIn, None, 1001).unwrap(),
            Some(None)
        );
        assert_eq!(
            take_challenge(&c, "a", Purpose::SignIn, None, 1001).unwrap(),
            None
        );

        put_challenge(&c, "b", Purpose::Register, Some(1), Some(b"h1"), 1000).unwrap();
        assert_eq!(
            take_challenge(&c, "b", Purpose::Register, Some(2), 1001).unwrap(),
            None
        );
        // A wrong guess uses it up too.
        assert_eq!(
            take_challenge(&c, "b", Purpose::Register, Some(1), 1001).unwrap(),
            None
        );

        put_challenge(&c, "b2", Purpose::Register, Some(1), Some(b"h1"), 1000).unwrap();
        assert_eq!(
            take_challenge(&c, "b2", Purpose::Register, Some(1), 1001).unwrap(),
            Some(Some(b"h1".to_vec()))
        );

        put_challenge(&c, "c", Purpose::Register, Some(1), None, 1000).unwrap();
        assert_eq!(
            take_challenge(&c, "c", Purpose::SignIn, None, 1001).unwrap(),
            None
        );

        put_challenge(&c, "d", Purpose::SignIn, None, None, 1000).unwrap();
        assert_eq!(
            take_challenge(&c, "d", Purpose::SignIn, None, 1000 + CHALLENGE_TTL_SECS).unwrap(),
            None
        );

        put_challenge(&c, "e", Purpose::SignIn, None, None, 1000).unwrap();
        // Expired rows go when the next challenge is stored.
        put_challenge(&c, "f", Purpose::SignIn, None, None, 2000).unwrap();
        let left: i64 = c
            .query_row("SELECT COUNT(*) FROM passkey_challenges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn passkeys_are_per_user_and_unique_by_credential() {
        let c = conn();
        let id = insert(&c, &new(1, b"cred-1")).unwrap().unwrap();
        assert_eq!(insert(&c, &new(2, b"cred-1")).unwrap(), None);
        assert_eq!(user_handle(&c, 1).unwrap().as_deref(), Some(&b"handle"[..]));
        assert_eq!(user_handle(&c, 2).unwrap(), None);
        assert!(!rename(&c, 2, id, "Mine").unwrap());
        assert!(rename(&c, 1, id, "Laptop").unwrap());
        let found = by_credential_id(&c, b"cred-1").unwrap().unwrap();
        assert_eq!((found.user_id, found.name.as_str()), (1, "Laptop"));
        assert!(found.last_used_at.is_none());

        assert!(record_use(&c, id, 0, 5, true).unwrap());
        // The counter moved on: a second use against the old value fails.
        assert!(!record_use(&c, id, 0, 5, true).unwrap());
        let found = by_credential_id(&c, b"cred-1").unwrap().unwrap();
        assert_eq!(found.sign_count, 5);
        assert!(found.backed_up && found.last_used_at.is_some());

        assert_eq!(counts_by_user(&c).unwrap(), vec![(1, 1)]);
        assert!(!delete(&c, 2, id).unwrap());
        insert(&c, &new(1, b"cred-2")).unwrap().unwrap();
        assert_eq!(delete_all_for_user(&c, 1).unwrap(), 2);
        assert!(list_for_user(&c, 1).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_user_deletes_their_passkeys() {
        let c = conn();
        insert(&c, &new(2, b"cred-1")).unwrap().unwrap();
        put_challenge(&c, "x", Purpose::Register, Some(2), Some(b"h"), 1000).unwrap();
        c.execute("DELETE FROM users WHERE id = 2", []).unwrap();
        assert!(by_credential_id(&c, b"cred-1").unwrap().is_none());
        assert_eq!(
            take_challenge(&c, "x", Purpose::Register, Some(2), 1001).unwrap(),
            None
        );
    }
}
