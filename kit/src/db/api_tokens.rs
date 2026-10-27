//! API tokens (`api_tokens`). Owned by the auth agent.
//!
//! Only the SHA-256 of a token is stored; the plaintext is shown once at
//! creation. Generation/parsing lives in `tokens`.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::role::Role;

/// What a token may do. `Read` caps the bearer at viewer powers; `Write`
/// grants the owning user's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenScope {
    Read,
    Write,
}

impl TokenScope {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenScope::Read => "read",
            TokenScope::Write => "write",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(TokenScope::Read),
            "write" => Some(TokenScope::Write),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TokenScope::Read => crate::i18n::t("tokens.scope_read"),
            TokenScope::Write => crate::i18n::t("tokens.scope_write"),
        }
    }

    /// The effective role of a request authenticated with this token.
    pub fn cap(self, user_role: Role) -> Role {
        match self {
            TokenScope::Read => Role::Viewer.min(user_role),
            TokenScope::Write => user_role,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiToken {
    pub id: i64,
    pub user_id: i64,
    /// Owner's username (joined for the admin view).
    pub username: String,
    pub name: String,
    /// First characters of the token, for recognising it in the list.
    pub prefix: String,
    pub scope: TokenScope,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    /// `expires_at` is in the past.
    pub expired: bool,
    /// The product scopes it was given (`api_token_scopes`), sorted. Empty
    /// unless filled with [`fill_scopes`].
    pub scopes: Vec<String>,
}

/// A token lookup hit: token + the owner's current state.
#[derive(Debug, Clone)]
pub struct TokenOwner {
    pub token_id: i64,
    pub scope: TokenScope,
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub needs_touch: bool,
}

const TOKEN_SELECT: &str =
    "SELECT t.id, t.user_id, u.username, t.name, t.prefix, t.scope, t.created_at,
        t.last_used_at, t.expires_at,
        (t.expires_at IS NOT NULL AND t.expires_at <= datetime('now')) AS expired
     FROM api_tokens t JOIN users u ON u.id = t.user_id";

fn row_to_token(row: &Row) -> rusqlite::Result<ApiToken> {
    let scope: String = row.get("scope")?;
    Ok(ApiToken {
        id: row.get("id")?,
        user_id: row.get("user_id")?,
        username: row.get("username")?,
        name: row.get("name")?,
        prefix: row.get("prefix")?,
        // Unknown scope never grants write.
        scope: TokenScope::parse(&scope).unwrap_or(TokenScope::Read),
        created_at: row.get("created_at")?,
        last_used_at: row.get("last_used_at")?,
        expires_at: row.get("expires_at")?,
        expired: row.get("expired")?,
        scopes: Vec::new(),
    })
}

/// Inserts a token. `expires_in_days = None` never expires.
pub fn insert(
    conn: &Connection,
    user_id: i64,
    name: &str,
    prefix: &str,
    token_hash: &str,
    scope: TokenScope,
    expires_in_days: Option<i64>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO api_tokens (user_id, name, prefix, token_hash, scope, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5,
                 CASE WHEN ?6 IS NULL THEN NULL ELSE datetime('now', '+' || ?6 || ' days') END)",
        params![
            user_id,
            name,
            prefix,
            token_hash,
            scope.as_str(),
            expires_in_days
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_for_user(conn: &Connection, user_id: i64) -> rusqlite::Result<Vec<ApiToken>> {
    let mut stmt = conn.prepare(&format!(
        "{TOKEN_SELECT} WHERE t.user_id = ?1 ORDER BY t.created_at DESC, t.id DESC"
    ))?;
    let rows = stmt.query_map(params![user_id], row_to_token)?;
    rows.collect()
}

pub fn list_all(conn: &Connection) -> rusqlite::Result<Vec<ApiToken>> {
    let mut stmt = conn.prepare(&format!(
        "{TOKEN_SELECT} ORDER BY u.username COLLATE NOCASE, t.created_at DESC, t.id DESC"
    ))?;
    let rows = stmt.query_map([], row_to_token)?;
    rows.collect()
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<ApiToken>> {
    conn.query_row(
        &format!("{TOKEN_SELECT} WHERE t.id = ?1"),
        params![id],
        row_to_token,
    )
    .optional()
}

pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM api_tokens WHERE id = ?1", params![id])? == 1)
}

/// Revokes every token of a user (their scopes go with them). Returns how
/// many there were.
pub fn delete_all_for_user(conn: &Connection, user_id: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM api_tokens WHERE user_id = ?1",
        params![user_id],
    )
}

/// Resolves a token hash to an unexpired token of an active user.
/// `needs_touch` is set when `last_used_at` is older than a minute (throttles writes).
pub fn find_valid_by_hash(
    conn: &Connection,
    token_hash: &str,
) -> rusqlite::Result<Option<TokenOwner>> {
    conn.query_row(
        "SELECT t.id, t.scope, u.id, u.username, u.display_name, u.role,
                (t.last_used_at IS NULL OR t.last_used_at < datetime('now', '-60 seconds'))
         FROM api_tokens t JOIN users u ON u.id = t.user_id
         WHERE t.token_hash = ?1 AND u.active = 1
           AND (t.expires_at IS NULL OR t.expires_at > datetime('now'))",
        params![token_hash],
        |r| {
            let scope: String = r.get(1)?;
            let role: String = r.get(5)?;
            Ok(TokenOwner {
                token_id: r.get(0)?,
                scope: TokenScope::parse(&scope).unwrap_or(TokenScope::Read),
                user_id: r.get(2)?,
                username: r.get(3)?,
                display_name: r.get(4)?,
                role: Role::parse(&role).unwrap_or(Role::Viewer),
                needs_touch: r.get(6)?,
            })
        },
    )
    .optional()
}

// The product scopes (`api_token_scopes`, core migration
// `kit/0002_token_scopes.sql`). Only products that declare token scopes have
// the table; callers check `KitConfig::token_scopes` before using these.

/// The product scopes token `token_id` was given, sorted.
pub fn scopes_of(conn: &Connection, token_id: i64) -> rusqlite::Result<Vec<String>> {
    conn.prepare_cached("SELECT scope FROM api_token_scopes WHERE token_id = ?1 ORDER BY scope")?
        .query_map(params![token_id], |r| r.get(0))?
        .collect()
}

/// Gives token `token_id` the product scope `scope` (again is harmless).
pub fn add_scope(conn: &Connection, token_id: i64, scope: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO api_token_scopes (token_id, scope) VALUES (?1, ?2)",
        params![token_id, scope],
    )?;
    Ok(())
}

/// Fills [`ApiToken::scopes`] of every token in `tokens`.
pub fn fill_scopes(conn: &Connection, tokens: &mut [ApiToken]) -> rusqlite::Result<()> {
    for token in tokens {
        token.scopes = scopes_of(conn, token.id)?;
    }
    Ok(())
}

/// The ids of the tokens that have product scope `scope`, newest first.
pub fn with_scope(conn: &Connection, scope: &str) -> rusqlite::Result<Vec<i64>> {
    conn.prepare(
        "SELECT s.token_id FROM api_token_scopes s JOIN api_tokens t ON t.id = s.token_id
         WHERE s.scope = ?1 ORDER BY t.created_at DESC, t.id DESC",
    )?
    .query_map(params![scope], |r| r.get(0))?
    .collect()
}

pub fn touch(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE api_tokens SET last_used_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_conn;
    use crate::db::users;

    fn setup() -> (Connection, i64) {
        let conn = test_conn();
        let uid = users::insert_user(&conn, "u", "h", Role::Editor).unwrap();
        (conn, uid)
    }

    #[test]
    fn read_scope_caps_at_viewer() {
        assert_eq!(TokenScope::Read.cap(Role::Admin), Role::Viewer);
    }

    #[test]
    fn write_scope_keeps_user_role() {
        assert_eq!(TokenScope::Write.cap(Role::Editor), Role::Editor);
    }

    #[test]
    fn valid_token_resolves_to_owner() {
        let (conn, uid) = setup();
        insert(&conn, uid, "ci", "vv_abc", "hash1", TokenScope::Write, None).unwrap();
        let owner = find_valid_by_hash(&conn, "hash1").unwrap().unwrap();
        assert_eq!(
            (owner.user_id, owner.scope, owner.role, owner.needs_touch),
            (uid, TokenScope::Write, Role::Editor, true)
        );
    }

    #[test]
    fn expired_token_is_rejected() {
        let (conn, uid) = setup();
        let id = insert(
            &conn,
            uid,
            "ci",
            "vv_abc",
            "hash1",
            TokenScope::Read,
            Some(30),
        )
        .unwrap();
        conn.execute(
            "UPDATE api_tokens SET expires_at = datetime('now', '-1 second') WHERE id = ?1",
            [id],
        )
        .unwrap();
        assert!(find_valid_by_hash(&conn, "hash1").unwrap().is_none());
        assert!(get(&conn, id).unwrap().unwrap().expired);
    }

    #[test]
    fn disabled_owner_invalidates_token() {
        let (conn, uid) = setup();
        insert(&conn, uid, "ci", "vv_abc", "hash1", TokenScope::Read, None).unwrap();
        conn.execute("UPDATE users SET active = 0 WHERE id = ?1", [uid])
            .unwrap();
        assert!(find_valid_by_hash(&conn, "hash1").unwrap().is_none());
    }

    #[test]
    fn touch_throttles_until_a_minute_passes() {
        let (conn, uid) = setup();
        let id = insert(&conn, uid, "ci", "vv_abc", "hash1", TokenScope::Read, None).unwrap();
        touch(&conn, id).unwrap();
        assert!(
            !find_valid_by_hash(&conn, "hash1")
                .unwrap()
                .unwrap()
                .needs_touch
        );
    }

    #[test]
    fn product_scopes_are_stored_per_token_and_go_with_it() {
        let (conn, uid) = setup();
        let id = insert(&conn, uid, "ci", "vv_abc", "hash1", TokenScope::Read, None).unwrap();
        let other = insert(&conn, uid, "x", "vv_def", "hash2", TokenScope::Read, None).unwrap();
        add_scope(&conn, id, "uploads:write").unwrap();
        add_scope(&conn, id, "uploads:write").unwrap();
        add_scope(&conn, id, "a:b").unwrap();
        assert_eq!(scopes_of(&conn, id).unwrap(), ["a:b", "uploads:write"]);
        assert!(scopes_of(&conn, other).unwrap().is_empty());
        assert_eq!(with_scope(&conn, "uploads:write").unwrap(), [id]);
        let mut all = list_for_user(&conn, uid).unwrap();
        fill_scopes(&conn, &mut all).unwrap();
        let mine = all.iter().find(|t| t.id == id).unwrap();
        assert_eq!(mine.scopes, ["a:b", "uploads:write"]);
        assert!(delete(&conn, id).unwrap());
        assert!(scopes_of(&conn, id).unwrap().is_empty());
    }

    #[test]
    fn deleting_user_removes_tokens() {
        let mut conn = test_conn();
        users::insert_user(&conn, "admin", "h", Role::Admin).unwrap();
        let uid = users::insert_user(&conn, "u", "h", Role::Viewer).unwrap();
        insert(&conn, uid, "ci", "vv_abc", "hash1", TokenScope::Read, None).unwrap();
        users::delete_user(&mut conn, uid).unwrap().unwrap();
        assert!(list_all(&conn).unwrap().is_empty());
    }
}
