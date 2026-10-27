//! User storage. Owned by the auth agent.
//!
//! Invariant enforced here (not only in handlers): an instance always keeps at
//! least one *active admin*. Every change that could remove the last one —
//! demoting, disabling or deleting — runs inside an `IMMEDIATE` transaction
//! and returns [`UserChangeError::LastAdmin`] instead of applying.

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use super::role::Role;

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub role: Role,
    pub display_name: String,
    pub totp_enabled: bool,
    pub active: bool,
    pub last_login_at: Option<String>,
    pub created_at: String,
    /// Bumped to invalidate every session cookie issued for this user.
    pub session_version: i64,
    /// Interface language code ("en", "nb"); `None` follows the site default.
    pub language: Option<String>,
}

impl User {
    /// Display name, falling back to the username.
    pub fn name(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.username
        } else {
            &self.display_name
        }
    }
}

/// TOTP state for one user (kept out of [`User`] so secrets aren't passed around).
#[derive(Debug, Clone, Default)]
pub struct TotpState {
    /// Base32 secret; present while enrolling (`enabled == false`) and once enabled.
    pub secret: Option<String>,
    pub enabled: bool,
    /// Last accepted time step (replay protection).
    pub last_step: Option<i64>,
    /// Argon2 PHC hashes of the unused recovery codes.
    pub recovery_codes: Vec<String>,
}

/// Why a guarded user change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserChangeError {
    NotFound,
    /// The change would leave no active admin.
    LastAdmin,
}

pub type ChangeResult = rusqlite::Result<Result<(), UserChangeError>>;

const USER_COLUMNS: &str = "id, username, password_hash, role, display_name, totp_enabled, active, \
     last_login_at, created_at, session_version, language";

fn row_to_user(row: &Row) -> rusqlite::Result<User> {
    let role: String = row.get("role")?;
    Ok(User {
        id: row.get("id")?,
        username: row.get("username")?,
        password_hash: row.get("password_hash")?,
        // An unknown role must never grant more than read access.
        role: Role::parse(&role).unwrap_or(Role::Viewer),
        display_name: row.get("display_name")?,
        totp_enabled: row.get("totp_enabled")?,
        active: row.get("active")?,
        last_login_at: row.get("last_login_at")?,
        created_at: row.get("created_at")?,
        session_version: row.get("session_version")?,
        language: row.get("language")?,
    })
}

pub fn count_users(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
}

pub fn count_active_admins(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND active = 1",
        [],
        |row| row.get(0),
    )
}

pub fn insert_user(
    conn: &Connection,
    username: &str,
    password_hash: &str,
    role: Role,
) -> rusqlite::Result<i64> {
    insert_user_full(conn, username, password_hash, role, "")
}

/// A random starting `session_version` (1 to 2^32) for a new user. SQLite
/// hands a deleted user's id to the next one created; with every user
/// starting at 1, the old user's cookie would then sign in as the new one.
/// A random start makes a cookie match only the user it was issued for.
fn new_session_version() -> i64 {
    i64::from(rand::random::<u32>()) + 1
}

pub fn insert_user_full(
    conn: &Connection,
    username: &str,
    password_hash: &str,
    role: Role,
    display_name: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO users (username, password_hash, role, display_name, session_version)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            username,
            password_hash,
            role.as_str(),
            display_name,
            new_session_version()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Creates the first admin atomically: returns `None` when any user already
/// exists (two racing setup submissions can't both succeed).
pub fn insert_first_admin(
    conn: &Connection,
    username: &str,
    password_hash: &str,
    display_name: &str,
) -> rusqlite::Result<Option<i64>> {
    let inserted = conn.execute(
        "INSERT INTO users (username, password_hash, role, display_name, session_version)
         SELECT ?1, ?2, 'admin', ?3, ?4 WHERE NOT EXISTS (SELECT 1 FROM users)",
        params![username, password_hash, display_name, new_session_version()],
    )?;
    Ok((inserted == 1).then(|| conn.last_insert_rowid()))
}

/// Case-insensitive lookup (usernames are unique case-insensitively by
/// convention; exact matches win if legacy data has case variants).
pub fn get_user_by_username(conn: &Connection, username: &str) -> rusqlite::Result<Option<User>> {
    conn.query_row(
        &format!(
            "SELECT {USER_COLUMNS} FROM users WHERE username = ?1 COLLATE NOCASE
             ORDER BY (username = ?1) DESC, id LIMIT 1"
        ),
        params![username],
        row_to_user,
    )
    .optional()
}

pub fn username_taken(conn: &Connection, username: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM users WHERE username = ?1 COLLATE NOCASE)",
        params![username],
        |r| r.get(0),
    )
}

/// Sets (or with `None` clears) a user's own interface language.
pub fn set_language(conn: &Connection, id: i64, language: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE users SET language = ?2 WHERE id = ?1",
        params![id, language],
    )?;
    Ok(())
}

pub fn get_user(conn: &Connection, id: i64) -> rusqlite::Result<Option<User>> {
    conn.query_row(
        &format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"),
        params![id],
        row_to_user,
    )
    .optional()
}

pub fn list_users(conn: &Connection) -> rusqlite::Result<Vec<User>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {USER_COLUMNS} FROM users ORDER BY active DESC, username COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map([], row_to_user)?;
    rows.collect()
}

pub fn touch_last_login(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE users SET last_login_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Invalidates every session of this user. Returns the new version.
pub fn bump_session_version(conn: &Connection, id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "UPDATE users SET session_version = session_version + 1 WHERE id = ?1 RETURNING session_version",
        params![id],
        |r| r.get(0),
    )
}

/// Sets a new password hash and signs the user out everywhere. Returns the new
/// session version (so the caller can re-issue its own cookie).
pub fn set_password(conn: &Connection, id: i64, password_hash: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "UPDATE users SET password_hash = ?2, session_version = session_version + 1
         WHERE id = ?1 RETURNING session_version",
        params![id, password_hash],
        |r| r.get(0),
    )
}

pub fn set_display_name(conn: &Connection, id: i64, display_name: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE users SET display_name = ?2 WHERE id = ?1",
        params![id, display_name],
    )?;
    Ok(())
}

/// Would removing admin powers from `user` leave no active admin?
fn is_last_active_admin(conn: &Connection, user: &User) -> rusqlite::Result<bool> {
    if user.role != Role::Admin || !user.active {
        return Ok(false);
    }
    Ok(count_active_admins(conn)? <= 1)
}

/// Changes role and display name. Refuses to demote the last active admin.
pub fn update_profile(
    conn: &mut Connection,
    id: i64,
    display_name: &str,
    role: Role,
) -> ChangeResult {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(user) = get_user(&tx, id)? else {
        return Ok(Err(UserChangeError::NotFound));
    };
    if role != Role::Admin && is_last_active_admin(&tx, &user)? {
        return Ok(Err(UserChangeError::LastAdmin));
    }
    tx.execute(
        "UPDATE users SET display_name = ?2, role = ?3 WHERE id = ?1",
        params![id, display_name, role.as_str()],
    )?;
    tx.commit()?;
    Ok(Ok(()))
}

/// Enables or disables a user. Disabling signs them out everywhere and is
/// refused for the last active admin.
pub fn set_active(conn: &mut Connection, id: i64, active: bool) -> ChangeResult {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(user) = get_user(&tx, id)? else {
        return Ok(Err(UserChangeError::NotFound));
    };
    if !active && is_last_active_admin(&tx, &user)? {
        return Ok(Err(UserChangeError::LastAdmin));
    }
    tx.execute(
        "UPDATE users SET active = ?2,
             session_version = session_version + CASE WHEN ?2 = 0 THEN 1 ELSE 0 END
         WHERE id = ?1",
        params![id, active],
    )?;
    tx.commit()?;
    Ok(Ok(()))
}

/// Deletes a user (their API tokens go with them). Refused for the last active admin.
pub fn delete_user(conn: &mut Connection, id: i64) -> ChangeResult {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(user) = get_user(&tx, id)? else {
        return Ok(Err(UserChangeError::NotFound));
    };
    if is_last_active_admin(&tx, &user)? {
        return Ok(Err(UserChangeError::LastAdmin));
    }
    tx.execute("DELETE FROM users WHERE id = ?1", params![id])?;
    tx.commit()?;
    Ok(Ok(()))
}

// ---------------------------------------------------------------------------
// TOTP
// ---------------------------------------------------------------------------

pub fn get_totp(conn: &Connection, id: i64) -> rusqlite::Result<Option<TotpState>> {
    conn.query_row(
        "SELECT totp_secret, totp_enabled, totp_last_step, recovery_codes FROM users WHERE id = ?1",
        params![id],
        |r| {
            let codes: Option<String> = r.get(3)?;
            Ok(TotpState {
                secret: r.get(0)?,
                enabled: r.get(1)?,
                last_step: r.get(2)?,
                recovery_codes: parse_codes(codes.as_deref()),
            })
        },
    )
    .optional()
}

fn parse_codes(json: Option<&str>) -> Vec<String> {
    json.and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default()
}

fn codes_json(codes: &[String]) -> String {
    serde_json::to_string(codes).unwrap_or_else(|_| "[]".into())
}

/// Stores a fresh, not-yet-confirmed secret. No-op when 2FA is already on.
pub fn set_pending_totp_secret(conn: &Connection, id: i64, secret: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE users SET totp_secret = ?2, totp_last_step = NULL WHERE id = ?1 AND totp_enabled = 0",
        params![id, secret],
    )?;
    Ok(n == 1)
}

/// Turns 2FA on (after the user confirmed a code) with fresh recovery codes,
/// recording the confirming step so that code can't be replayed at login.
pub fn enable_totp(
    conn: &Connection,
    id: i64,
    step: i64,
    recovery_hashes: &[String],
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE users SET totp_enabled = 1, totp_last_step = ?2, recovery_codes = ?3
         WHERE id = ?1 AND totp_enabled = 0 AND totp_secret IS NOT NULL",
        params![id, step, codes_json(recovery_hashes)],
    )?;
    Ok(n == 1)
}

/// Turns 2FA off and forgets the secret and recovery codes.
pub fn disable_totp(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE users SET totp_enabled = 0, totp_secret = NULL, totp_last_step = NULL, recovery_codes = NULL
         WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn replace_recovery_codes(
    conn: &Connection,
    id: i64,
    recovery_hashes: &[String],
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE users SET recovery_codes = ?2 WHERE id = ?1 AND totp_enabled = 1",
        params![id, codes_json(recovery_hashes)],
    )?;
    Ok(())
}

/// Atomically records `step` as used. `false` means the step (or a later one)
/// was already accepted: a replayed code.
pub fn consume_totp_step(conn: &Connection, id: i64, step: i64) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE users SET totp_last_step = ?2
         WHERE id = ?1 AND (totp_last_step IS NULL OR totp_last_step < ?2)",
        params![id, step],
    )?;
    Ok(n == 1)
}

/// Removes one used recovery code. Compare-and-swap on the stored list, so two
/// concurrent logins with the same code can't both succeed.
pub fn consume_recovery_code(
    conn: &Connection,
    id: i64,
    before: &[String],
    used_hash: &str,
) -> rusqlite::Result<bool> {
    let remaining: Vec<String> = before
        .iter()
        .filter(|h| h.as_str() != used_hash)
        .cloned()
        .collect();
    if remaining.len() == before.len() {
        return Ok(false);
    }
    let n = conn.execute(
        "UPDATE users SET recovery_codes = ?3 WHERE id = ?1 AND recovery_codes = ?2",
        params![id, codes_json(before), codes_json(&remaining)],
    )?;
    Ok(n == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_conn;

    fn add(conn: &Connection, name: &str, role: Role) -> i64 {
        insert_user(conn, name, "hash", role).unwrap()
    }

    #[test]
    fn first_admin_is_only_created_once() {
        let conn = test_conn();
        assert!(insert_first_admin(&conn, "a", "h", "").unwrap().is_some());
        assert!(insert_first_admin(&conn, "b", "h", "").unwrap().is_none());
        assert_eq!(count_users(&conn).unwrap(), 1);
    }

    #[test]
    fn new_users_start_at_a_random_positive_session_version() {
        let conn = test_conn();
        let versions: Vec<i64> = (0..8)
            .map(|i| {
                let id = add(&conn, &format!("u{i}"), Role::Viewer);
                get_user(&conn, id).unwrap().unwrap().session_version
            })
            .collect();
        assert!(versions.iter().all(|v| (1..=1 << 32).contains(v)));
        assert!(versions.windows(2).any(|w| w[0] != w[1]), "{versions:?}");
        let first = test_conn();
        let id = insert_first_admin(&first, "a", "h", "").unwrap().unwrap();
        let v = get_user(&first, id).unwrap().unwrap().session_version;
        assert!((1..=1 << 32).contains(&v));
    }

    #[test]
    fn username_lookup_ignores_case() {
        let conn = test_conn();
        let id = add(&conn, "Alice", Role::Admin);
        assert_eq!(
            get_user_by_username(&conn, "alice").unwrap().map(|u| u.id),
            Some(id)
        );
        assert!(username_taken(&conn, "ALICE").unwrap());
    }

    #[test]
    fn last_admin_cannot_be_demoted() {
        let mut conn = test_conn();
        let id = add(&conn, "admin", Role::Admin);
        assert_eq!(
            update_profile(&mut conn, id, "", Role::Editor).unwrap(),
            Err(UserChangeError::LastAdmin)
        );
    }

    #[test]
    fn last_admin_cannot_be_disabled() {
        let mut conn = test_conn();
        let id = add(&conn, "admin", Role::Admin);
        assert_eq!(
            set_active(&mut conn, id, false).unwrap(),
            Err(UserChangeError::LastAdmin)
        );
    }

    #[test]
    fn last_admin_cannot_be_deleted() {
        let mut conn = test_conn();
        let id = add(&conn, "admin", Role::Admin);
        assert_eq!(
            delete_user(&mut conn, id).unwrap(),
            Err(UserChangeError::LastAdmin)
        );
        assert_eq!(count_users(&conn).unwrap(), 1);
    }

    #[test]
    fn disabled_admins_do_not_count_as_the_remaining_admin() {
        let mut conn = test_conn();
        let a = add(&conn, "a", Role::Admin);
        let b = add(&conn, "b", Role::Admin);
        set_active(&mut conn, b, false).unwrap().unwrap();
        assert_eq!(
            delete_user(&mut conn, a).unwrap(),
            Err(UserChangeError::LastAdmin)
        );
    }

    #[test]
    fn an_admin_can_be_removed_while_another_remains() {
        let mut conn = test_conn();
        let a = add(&conn, "a", Role::Admin);
        add(&conn, "b", Role::Admin);
        assert_eq!(delete_user(&mut conn, a).unwrap(), Ok(()));
    }

    #[test]
    fn missing_user_change_reports_not_found() {
        let mut conn = test_conn();
        assert_eq!(
            set_active(&mut conn, 99, true).unwrap(),
            Err(UserChangeError::NotFound)
        );
    }

    #[test]
    fn disabling_bumps_session_version() {
        let mut conn = test_conn();
        add(&conn, "admin", Role::Admin);
        let id = add(&conn, "v", Role::Viewer);
        let before = get_user(&conn, id).unwrap().unwrap().session_version;
        set_active(&mut conn, id, false).unwrap().unwrap();
        assert_eq!(
            get_user(&conn, id).unwrap().unwrap().session_version,
            before + 1
        );
    }

    #[test]
    fn password_change_bumps_session_version() {
        let conn = test_conn();
        let id = add(&conn, "u", Role::Viewer);
        let before = get_user(&conn, id).unwrap().unwrap().session_version;
        assert_eq!(set_password(&conn, id, "new").unwrap(), before + 1);
    }

    #[test]
    fn totp_step_cannot_be_consumed_twice() {
        let conn = test_conn();
        let id = add(&conn, "u", Role::Viewer);
        assert!(consume_totp_step(&conn, id, 100).unwrap());
        assert!(!consume_totp_step(&conn, id, 100).unwrap());
        assert!(!consume_totp_step(&conn, id, 99).unwrap());
        assert!(consume_totp_step(&conn, id, 101).unwrap());
    }

    #[test]
    fn enrolment_flow_round_trips() {
        let conn = test_conn();
        let id = add(&conn, "u", Role::Viewer);
        assert!(set_pending_totp_secret(&conn, id, "SECRET").unwrap());
        assert!(enable_totp(&conn, id, 5, &["h1".into(), "h2".into()]).unwrap());
        let state = get_totp(&conn, id).unwrap().unwrap();
        assert_eq!(
            (
                state.enabled,
                state.secret.as_deref(),
                state.last_step,
                state.recovery_codes.len()
            ),
            (true, Some("SECRET"), Some(5), 2)
        );
        // Re-enrolling while enabled must not overwrite the secret.
        assert!(!set_pending_totp_secret(&conn, id, "OTHER").unwrap());
        disable_totp(&conn, id).unwrap();
        assert!(get_totp(&conn, id).unwrap().unwrap().secret.is_none());
    }

    #[test]
    fn recovery_code_is_single_use() {
        let conn = test_conn();
        let id = add(&conn, "u", Role::Viewer);
        set_pending_totp_secret(&conn, id, "S").unwrap();
        let codes = vec!["h1".to_string(), "h2".to_string()];
        enable_totp(&conn, id, 1, &codes).unwrap();
        assert!(consume_recovery_code(&conn, id, &codes, "h1").unwrap());
        // A second consumer still holding the old list loses the race.
        assert!(!consume_recovery_code(&conn, id, &codes, "h1").unwrap());
        assert_eq!(
            get_totp(&conn, id).unwrap().unwrap().recovery_codes,
            vec!["h2".to_string()]
        );
    }
}
