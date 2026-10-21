//! Status page subscribers (`status_page_subscribers`) and the per-page
//! subscription settings.
//!
//! Anyone can ask to subscribe from the public page, so nothing is trusted
//! until it is confirmed (`confirmed_at`): an email address by its owner
//! clicking the confirmation link, a webhook by an editor approving it.
//! Unconfirmed rows expire; a subscriber whose deliveries keep failing is
//! dropped.
//!
//! Only the SHA-256 of a subscriber's link token is stored (`token_hash`);
//! the token itself is derived from `nonce` with the server's secret key in
//! [`crate::subscribers::link_token`].

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// Most confirmed subscribers one page keeps.
pub const MAX_PER_PAGE: i64 = 5_000;
/// Most unconfirmed sign-ups one page keeps, so a flood of sign-ups can't
/// take the room confirmed subscribers need.
pub const MAX_PENDING_PER_PAGE: i64 = 1_000;
/// Most webhook subscribers (confirmed or not) of one page on one host.
pub const MAX_WEBHOOKS_PER_HOST: i64 = 10;
/// Unconfirmed email sign-ups are forgotten after this many days.
pub const PENDING_DAYS: i64 = 3;
/// Webhook sign-ups wait this many days for an editor's approval.
pub const PENDING_WEBHOOK_DAYS: i64 = 14;
/// Consecutive failed deliveries after which a subscriber is dropped.
pub const MAX_FAILURES: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    /// SMTP channel used to send mails; `None` = no email subscriptions.
    pub channel_id: Option<i64>,
    pub webhooks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subscriber {
    pub id: i64,
    pub page_id: i64,
    /// "email" | "webhook".
    pub kind: String,
    pub target: String,
    /// SHA-256 (hex) of the link token.
    #[serde(skip)]
    pub token_hash: String,
    /// Input of the link token's derivation; empty for rows from before it.
    #[serde(skip)]
    pub nonce: String,
    pub lang: String,
    pub created_at: String,
    pub confirmed_at: Option<String>,
    pub failures: i64,
}

impl Subscriber {
    pub fn is_confirmed(&self) -> bool {
        self.confirmed_at.is_some()
    }

    /// A webhook sign-up that waits for an editor.
    pub fn awaits_approval(&self) -> bool {
        self.kind == "webhook" && !self.is_confirmed()
    }

    /// The target with the middle hidden, for places that list subscribers
    /// to someone who is not the page's editor.
    pub fn masked_target(&self) -> String {
        mask(&self.kind, &self.target)
    }
}

/// `jo***@example.com`, or `https://host/…` for webhooks.
pub fn mask(kind: &str, target: &str) -> String {
    if kind == "email" {
        match target.split_once('@') {
            Some((local, domain)) => {
                let head: String = local.chars().take(2).collect();
                format!("{head}***@{domain}")
            }
            None => "***".into(),
        }
    } else {
        match url::Url::parse(target) {
            Ok(u) => format!("{}://{}/…", u.scheme(), u.host_str().unwrap_or("")),
            Err(_) => "***".into(),
        }
    }
}

const COLUMNS: &str =
    "id, page_id, kind, target, token_hash, nonce, lang, created_at, confirmed_at, failures";

fn row(r: &Row) -> rusqlite::Result<Subscriber> {
    Ok(Subscriber {
        id: r.get("id")?,
        page_id: r.get("page_id")?,
        kind: r.get("kind")?,
        target: r.get("target")?,
        token_hash: r.get("token_hash")?,
        nonce: r.get("nonce")?,
        lang: r.get("lang")?,
        created_at: r.get("created_at")?,
        confirmed_at: r.get("confirmed_at")?,
        failures: r.get("failures")?,
    })
}

pub fn settings(conn: &Connection, page_id: i64) -> rusqlite::Result<Settings> {
    conn.query_row(
        "SELECT subscribe_enabled, subscribe_channel_id, subscribe_webhooks FROM status_pages WHERE id = ?1",
        params![page_id],
        |r| {
            Ok(Settings {
                enabled: r.get(0)?,
                channel_id: r.get(1)?,
                webhooks: r.get(2)?,
            })
        },
    )
}

pub fn set_settings(conn: &Connection, page_id: i64, s: &Settings) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE status_pages SET subscribe_enabled = ?2, subscribe_channel_id = ?3, subscribe_webhooks = ?4,
         updated_at = datetime('now') WHERE id = ?1",
        params![page_id, s.enabled, s.channel_id, s.webhooks],
    )? > 0)
}

/// A row's link secret: the derivation input and the hash of the token it
/// gives (see [`crate::subscribers::new_secret`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Secret {
    pub nonce: String,
    pub token_hash: String,
}

/// What [`add`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    /// New (or still unconfirmed) sign-up with this id.
    Pending(i64),
    /// Already confirmed: nothing to send.
    AlreadySubscribed,
    /// The page (or, for a webhook, its host) has no room left.
    Full,
}

/// The host part of a webhook URL, lowercased.
fn webhook_host(target: &str) -> Option<String> {
    url::Url::parse(target)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

/// Records a sign-up. An unconfirmed duplicate keeps its token (the
/// confirmation is simply sent again) and takes the new language; `secret`
/// is only used for a new row.
pub fn add(
    conn: &Connection,
    page_id: i64,
    kind: &str,
    target: &str,
    lang: &str,
    secret: &Secret,
) -> rusqlite::Result<Added> {
    purge_pending(conn)?;
    let existing: Option<(i64, bool)> = conn
        .query_row(
            "SELECT id, confirmed_at IS NOT NULL FROM status_page_subscribers
             WHERE page_id = ?1 AND kind = ?2 AND target = ?3",
            params![page_id, kind, target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((_, true)) => return Ok(Added::AlreadySubscribed),
        Some((id, false)) => {
            conn.execute(
                "UPDATE status_page_subscribers SET lang = ?2 WHERE id = ?1",
                params![id, lang],
            )?;
            return Ok(Added::Pending(id));
        }
        None => {}
    }
    let (confirmed, pending): (i64, i64) = conn.query_row(
        "SELECT COUNT(confirmed_at), COUNT(*) - COUNT(confirmed_at)
         FROM status_page_subscribers WHERE page_id = ?1",
        params![page_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if confirmed >= MAX_PER_PAGE || pending >= MAX_PENDING_PER_PAGE {
        return Ok(Added::Full);
    }
    if kind == "webhook" {
        let host = webhook_host(target);
        let mut stmt = conn.prepare(
            "SELECT target FROM status_page_subscribers WHERE page_id = ?1 AND kind = 'webhook'",
        )?;
        let mut same_host = 0;
        for t in stmt.query_map(params![page_id], |r| r.get::<_, String>(0))? {
            if webhook_host(&t?) == host {
                same_host += 1;
            }
        }
        if same_host >= MAX_WEBHOOKS_PER_HOST {
            return Ok(Added::Full);
        }
    }
    conn.execute(
        "INSERT INTO status_page_subscribers (page_id, kind, target, token_hash, nonce, lang)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![page_id, kind, target, secret.token_hash, secret.nonce, lang],
    )?;
    Ok(Added::Pending(conn.last_insert_rowid()))
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Subscriber>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM status_page_subscribers WHERE id = ?1"),
        params![id],
        row,
    )
    .optional()
}

/// Stores a (new) link secret for subscriber `id`.
pub fn set_secret(conn: &Connection, id: i64, secret: &Secret) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE status_page_subscribers SET nonce = ?2, token_hash = ?3 WHERE id = ?1",
        params![id, secret.nonce, secret.token_hash],
    )?;
    Ok(())
}

/// Rows from before link tokens were hashed hold the plain `vv_…` token in
/// `token_hash`; hash them so the links already sent keep working. Runs after
/// the migrations and does nothing once every row is hashed.
pub fn hash_legacy_tokens(conn: &Connection) -> rusqlite::Result<usize> {
    let migrated: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('status_page_subscribers') WHERE name = 'token_hash')",
        [],
        |r| r.get(0),
    )?;
    if !migrated {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT id, token_hash FROM status_page_subscribers WHERE substr(token_hash, 1, 3) = 'vv_'",
    )?;
    let legacy: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, token) in &legacy {
        conn.execute(
            "UPDATE status_page_subscribers SET token_hash = ?2 WHERE id = ?1",
            params![id, crate::auth::tokens::hash(token)],
        )?;
    }
    Ok(legacy.len())
}

/// The subscriber whose link token is `token`.
pub fn by_token(conn: &Connection, token: &str) -> rusqlite::Result<Option<Subscriber>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM status_page_subscribers WHERE token_hash = ?1"),
        params![crate::auth::tokens::hash(token)],
        row,
    )
    .optional()
}

/// Confirms the email sign-up behind `token`. `None` if the token is unknown
/// (or expired) or belongs to a webhook, which only an editor can approve.
pub fn confirm(conn: &Connection, token: &str) -> rusqlite::Result<Option<Subscriber>> {
    purge_pending(conn)?;
    conn.execute(
        "UPDATE status_page_subscribers SET confirmed_at = COALESCE(confirmed_at, datetime('now')), failures = 0
         WHERE token_hash = ?1 AND kind = 'email'",
        params![crate::auth::tokens::hash(token)],
    )?;
    Ok(by_token(conn, token)?.filter(|s| s.kind == "email"))
}

/// An editor approves webhook sign-up `id` of page `page_id`. `None` when
/// there is no such webhook waiting.
pub fn approve(conn: &Connection, page_id: i64, id: i64) -> rusqlite::Result<Option<Subscriber>> {
    purge_pending(conn)?;
    let changed = conn.execute(
        "UPDATE status_page_subscribers SET confirmed_at = datetime('now'), failures = 0
         WHERE id = ?1 AND page_id = ?2 AND kind = 'webhook' AND confirmed_at IS NULL",
        params![id, page_id],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get(conn, id)
}

/// Removes the subscriber behind `token`; `false` if there was none.
pub fn unsubscribe(conn: &Connection, token: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM status_page_subscribers WHERE token_hash = ?1",
        params![crate::auth::tokens::hash(token)],
    )? > 0)
}

pub fn count(conn: &Connection, page_id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM status_page_subscribers WHERE page_id = ?1",
        params![page_id],
        |r| r.get(0),
    )
}

pub fn list(conn: &Connection, page_id: i64) -> rusqlite::Result<Vec<Subscriber>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM status_page_subscribers WHERE page_id = ?1 ORDER BY created_at DESC, id DESC"
    ))?;
    stmt.query_map(params![page_id], row)?.collect()
}

pub fn list_confirmed(conn: &Connection, page_id: i64) -> rusqlite::Result<Vec<Subscriber>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM status_page_subscribers WHERE page_id = ?1 AND confirmed_at IS NOT NULL ORDER BY id"
    ))?;
    stmt.query_map(params![page_id], row)?.collect()
}

pub fn delete(conn: &Connection, page_id: i64, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM status_page_subscribers WHERE id = ?1 AND page_id = ?2",
        params![id, page_id],
    )? > 0)
}

/// Records a delivery result; a subscriber that keeps failing is removed.
pub fn record_delivery(conn: &Connection, id: i64, ok: bool) -> rusqlite::Result<()> {
    if ok {
        conn.execute(
            "UPDATE status_page_subscribers SET failures = 0 WHERE id = ?1",
            params![id],
        )?;
    } else {
        conn.execute(
            "UPDATE status_page_subscribers SET failures = failures + 1 WHERE id = ?1",
            params![id],
        )?;
        conn.execute(
            "DELETE FROM status_page_subscribers WHERE id = ?1 AND failures >= ?2",
            params![id, MAX_FAILURES],
        )?;
    }
    Ok(())
}

/// Forgets sign-ups nobody confirmed (or approved) in time.
pub fn purge_pending(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM status_page_subscribers
         WHERE confirmed_at IS NULL AND created_at < datetime('now', CASE kind WHEN 'webhook' THEN ?2 ELSE ?1 END)",
        params![
            format!("-{PENDING_DAYS} days"),
            format!("-{PENDING_WEBHOOK_DAYS} days")
        ],
    )
}

/// Ids of the published pages that show any of `monitor_ids`.
pub fn pages_with_monitors(conn: &Connection, monitor_ids: &[i64]) -> rusqlite::Result<Vec<i64>> {
    let mut pages = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT p.id FROM status_pages p
         JOIN status_page_sections s ON s.status_page_id = p.id
         JOIN status_page_monitors m ON m.section_id = s.id
         WHERE m.monitor_id = ?1 AND p.published = 1 AND p.subscribe_enabled = 1",
    )?;
    for id in monitor_ids {
        for page in stmt.query_map(params![id], |r| r.get::<_, i64>(0))? {
            let page = page?;
            if !pages.contains(&page) {
                pages.push(page);
            }
        }
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_conn;

    fn page(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO status_pages (slug, title) VALUES ('s', 'S')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// A secret whose token is `token` (tests don't need the key derivation).
    fn secret(token: &str) -> Secret {
        Secret {
            nonce: format!("n-{token}"),
            token_hash: crate::auth::tokens::hash(token),
        }
    }

    fn pending(added: Added) -> i64 {
        match added {
            Added::Pending(id) => id,
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[test]
    fn sign_up_confirm_unsubscribe() {
        let conn = test_conn();
        let p = page(&conn);
        let id = pending(add(&conn, p, "email", "a@example.com", "en", &secret("T1")).unwrap());
        assert!(list_confirmed(&conn, p).unwrap().is_empty());
        // The same address again keeps the row and its token.
        assert_eq!(
            add(&conn, p, "email", "a@example.com", "nb", &secret("T2")).unwrap(),
            Added::Pending(id)
        );
        let sub = by_token(&conn, "T1").unwrap().unwrap();
        assert_eq!((sub.id, sub.lang.as_str()), (id, "nb"));
        assert!(by_token(&conn, "T2").unwrap().is_none());
        // Only the hash is stored.
        assert_ne!(sub.token_hash, "T1");
        let sub = confirm(&conn, "T1").unwrap().unwrap();
        assert!(sub.is_confirmed());
        assert_eq!(list_confirmed(&conn, p).unwrap().len(), 1);
        assert_eq!(
            add(&conn, p, "email", "a@example.com", "en", &secret("T3")).unwrap(),
            Added::AlreadySubscribed
        );
        assert!(unsubscribe(&conn, "T1").unwrap());
        assert!(!unsubscribe(&conn, "T1").unwrap());
        assert_eq!(count(&conn, p).unwrap(), 0);
    }

    #[test]
    fn webhooks_are_approved_by_an_editor_not_by_their_link() {
        let conn = test_conn();
        let p = page(&conn);
        let id = pending(
            add(
                &conn,
                p,
                "webhook",
                "https://example.com/h",
                "en",
                &secret("W"),
            )
            .unwrap(),
        );
        let sub = get(&conn, id).unwrap().unwrap();
        assert!(sub.awaits_approval());
        // Whoever receives traffic at the URL can't confirm it.
        assert!(confirm(&conn, "W").unwrap().is_none());
        assert!(!get(&conn, id).unwrap().unwrap().is_confirmed());
        // Another page's editor can't approve it.
        assert!(approve(&conn, p + 1, id).unwrap().is_none());
        let sub = approve(&conn, p, id).unwrap().unwrap();
        assert!(sub.is_confirmed() && !sub.awaits_approval());
        assert!(approve(&conn, p, id).unwrap().is_none(), "only once");
        // Unsubscribing by link still works.
        assert!(unsubscribe(&conn, "W").unwrap());
    }

    #[test]
    fn pending_expire_and_failures_drop() {
        let conn = test_conn();
        let p = page(&conn);
        pending(add(&conn, p, "email", "old@example.com", "en", &secret("OLD")).unwrap());
        let hook = pending(
            add(
                &conn,
                p,
                "webhook",
                "https://example.com/w",
                "en",
                &secret("H"),
            )
            .unwrap(),
        );
        conn.execute(
            "UPDATE status_page_subscribers SET created_at = datetime('now', '-4 days')",
            [],
        )
        .unwrap();
        assert!(
            confirm(&conn, "OLD").unwrap().is_none(),
            "expired sign-ups can't be confirmed"
        );
        // A webhook waits longer for its editor.
        assert!(get(&conn, hook).unwrap().is_some());
        conn.execute(
            "UPDATE status_page_subscribers SET created_at = datetime('now', '-15 days')",
            [],
        )
        .unwrap();
        purge_pending(&conn).unwrap();
        assert!(get(&conn, hook).unwrap().is_none());

        let id = pending(
            add(
                &conn,
                p,
                "webhook",
                "https://example.com/h",
                "en",
                &secret("T"),
            )
            .unwrap(),
        );
        approve(&conn, p, id).unwrap().unwrap();
        for _ in 0..MAX_FAILURES {
            record_delivery(&conn, id, false).unwrap();
        }
        assert!(by_token(&conn, "T").unwrap().is_none());
    }

    #[test]
    fn a_flood_of_sign_ups_cannot_take_confirmed_room() {
        let conn = test_conn();
        let p = page(&conn);
        conn.execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {MAX_PENDING_PER_PAGE})
             INSERT INTO status_page_subscribers (page_id, kind, target, token_hash)
             SELECT {p}, 'email', 'u' || i || '@example.com', 'h' || i FROM n"
        ))
        .unwrap();
        assert_eq!(
            add(&conn, p, "email", "late@example.com", "en", &secret("L")).unwrap(),
            Added::Full,
            "the pending room is full"
        );
        // Confirmed subscribers don't count against the pending room.
        conn.execute(
            "UPDATE status_page_subscribers SET confirmed_at = datetime('now') WHERE id % 2 = 0",
            [],
        )
        .unwrap();
        pending(add(&conn, p, "email", "late@example.com", "en", &secret("L")).unwrap());
    }

    #[test]
    fn one_host_gets_a_limited_number_of_webhooks() {
        let conn = test_conn();
        let p = page(&conn);
        for n in 0..MAX_WEBHOOKS_PER_HOST {
            pending(
                add(
                    &conn,
                    p,
                    "webhook",
                    &format!("https://Hooks.Example.com/{n}"),
                    "en",
                    &secret(&format!("T{n}")),
                )
                .unwrap(),
            );
        }
        assert_eq!(
            add(
                &conn,
                p,
                "webhook",
                "https://hooks.example.com/x",
                "en",
                &secret("X")
            )
            .unwrap(),
            Added::Full
        );
        pending(
            add(
                &conn,
                p,
                "webhook",
                "https://other.example.com/x",
                "en",
                &secret("Y"),
            )
            .unwrap(),
        );
        // Email sign-ups are not affected.
        pending(add(&conn, p, "email", "a@hooks.example.com", "en", &secret("Z")).unwrap());
    }

    #[test]
    fn legacy_plain_tokens_are_hashed_once() {
        let conn = test_conn();
        let p = page(&conn);
        let legacy = crate::auth::tokens::generate();
        conn.execute(
            "INSERT INTO status_page_subscribers (page_id, kind, target, token_hash, confirmed_at)
             VALUES (?1, 'email', 'a@example.com', ?2, datetime('now'))",
            params![p, legacy],
        )
        .unwrap();
        assert_eq!(hash_legacy_tokens(&conn).unwrap(), 1);
        assert_eq!(hash_legacy_tokens(&conn).unwrap(), 0);
        let sub = by_token(&conn, &legacy).unwrap().unwrap();
        assert_eq!(sub.token_hash, crate::auth::tokens::hash(&legacy));
    }

    #[test]
    fn masks_targets() {
        assert_eq!(mask("email", "jonas@example.com"), "jo***@example.com");
        assert_eq!(
            mask("webhook", "https://hooks.example.com/secret/abc"),
            "https://hooks.example.com/…"
        );
    }

    #[test]
    fn settings_round_trip() {
        let conn = test_conn();
        let p = page(&conn);
        assert!(!settings(&conn, p).unwrap().enabled);
        let s = Settings {
            enabled: true,
            channel_id: None,
            webhooks: true,
        };
        assert!(set_settings(&conn, p, &s).unwrap());
        assert_eq!(settings(&conn, p).unwrap(), s);
    }
}
