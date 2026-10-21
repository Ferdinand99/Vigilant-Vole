//! Status page subscribers (`status_page_subscribers`) and the per-page
//! subscription settings.
//!
//! Anyone can ask to subscribe from the public page, so nothing is trusted
//! until the owner of the address clicked the confirmation link
//! (`confirmed_at`). Unconfirmed rows expire; a subscriber whose deliveries
//! keep failing is dropped.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// Most subscribers (confirmed or pending) one page keeps; stops a flood of
/// sign-ups from growing the table without bound.
pub const MAX_PER_PAGE: i64 = 5_000;
/// Unconfirmed sign-ups are forgotten after this many days.
pub const PENDING_DAYS: i64 = 3;
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
    #[serde(skip)]
    pub token: String,
    pub lang: String,
    #[serde(skip)]
    pub base_url: String,
    pub created_at: String,
    pub confirmed_at: Option<String>,
    pub failures: i64,
}

impl Subscriber {
    pub fn is_confirmed(&self) -> bool {
        self.confirmed_at.is_some()
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
    "id, page_id, kind, target, token, lang, base_url, created_at, confirmed_at, failures";

fn row(r: &Row) -> rusqlite::Result<Subscriber> {
    Ok(Subscriber {
        id: r.get("id")?,
        page_id: r.get("page_id")?,
        kind: r.get("kind")?,
        target: r.get("target")?,
        token: r.get("token")?,
        lang: r.get("lang")?,
        base_url: r.get("base_url")?,
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

/// What [`add`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    /// New (or still unconfirmed) sign-up: send the confirmation to this token.
    Pending(String),
    /// Already confirmed: nothing to send.
    AlreadySubscribed,
    /// The page is full.
    Full,
}

/// Records a sign-up. An unconfirmed duplicate keeps its token (the
/// confirmation is simply sent again) and takes the new language.
pub fn add(
    conn: &Connection,
    page_id: i64,
    kind: &str,
    target: &str,
    lang: &str,
    base_url: &str,
) -> rusqlite::Result<Added> {
    purge_pending(conn)?;
    let existing: Option<(String, bool)> = conn
        .query_row(
            "SELECT token, confirmed_at IS NOT NULL FROM status_page_subscribers
             WHERE page_id = ?1 AND kind = ?2 AND target = ?3",
            params![page_id, kind, target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((_, true)) => return Ok(Added::AlreadySubscribed),
        Some((token, false)) => {
            conn.execute(
                "UPDATE status_page_subscribers SET lang = ?2, base_url = ?3 WHERE token = ?1",
                params![token, lang, base_url],
            )?;
            return Ok(Added::Pending(token));
        }
        None => {}
    }
    if count(conn, page_id)? >= MAX_PER_PAGE {
        return Ok(Added::Full);
    }
    let token = crate::auth::tokens::generate();
    conn.execute(
        "INSERT INTO status_page_subscribers (page_id, kind, target, token, lang, base_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![page_id, kind, target, token, lang, base_url],
    )?;
    Ok(Added::Pending(token))
}

pub fn by_token(conn: &Connection, token: &str) -> rusqlite::Result<Option<Subscriber>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM status_page_subscribers WHERE token = ?1"),
        params![token],
        row,
    )
    .optional()
}

/// Confirms the sign-up behind `token`. `None` if the token is unknown
/// (or expired).
pub fn confirm(conn: &Connection, token: &str) -> rusqlite::Result<Option<Subscriber>> {
    purge_pending(conn)?;
    conn.execute(
        "UPDATE status_page_subscribers SET confirmed_at = COALESCE(confirmed_at, datetime('now')), failures = 0
         WHERE token = ?1",
        params![token],
    )?;
    by_token(conn, token)
}

/// Removes the subscriber behind `token`; `false` if there was none.
pub fn unsubscribe(conn: &Connection, token: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM status_page_subscribers WHERE token = ?1",
        params![token],
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

/// Forgets sign-ups nobody confirmed.
pub fn purge_pending(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM status_page_subscribers
         WHERE confirmed_at IS NULL AND created_at < datetime('now', ?1)",
        params![format!("-{PENDING_DAYS} days")],
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

    #[test]
    fn sign_up_confirm_unsubscribe() {
        let conn = test_conn();
        let p = page(&conn);
        let Added::Pending(token) =
            add(&conn, p, "email", "a@example.com", "en", "https://x").unwrap()
        else {
            panic!("expected pending");
        };
        assert!(list_confirmed(&conn, p).unwrap().is_empty());
        // The same address again keeps the token.
        assert_eq!(
            add(&conn, p, "email", "a@example.com", "nb", "https://x").unwrap(),
            Added::Pending(token.clone())
        );
        assert_eq!(by_token(&conn, &token).unwrap().unwrap().lang, "nb");
        let sub = confirm(&conn, &token).unwrap().unwrap();
        assert!(sub.is_confirmed());
        assert_eq!(list_confirmed(&conn, p).unwrap().len(), 1);
        assert_eq!(
            add(&conn, p, "email", "a@example.com", "en", "").unwrap(),
            Added::AlreadySubscribed
        );
        assert!(unsubscribe(&conn, &token).unwrap());
        assert!(!unsubscribe(&conn, &token).unwrap());
        assert_eq!(count(&conn, p).unwrap(), 0);
    }

    #[test]
    fn pending_expire_and_failures_drop() {
        let conn = test_conn();
        let p = page(&conn);
        let Added::Pending(old) = add(&conn, p, "email", "old@example.com", "en", "").unwrap()
        else {
            panic!()
        };
        conn.execute(
            "UPDATE status_page_subscribers SET created_at = datetime('now', '-4 days')",
            [],
        )
        .unwrap();
        assert!(
            confirm(&conn, &old).unwrap().is_none(),
            "expired sign-ups can't be confirmed"
        );

        let Added::Pending(t) =
            add(&conn, p, "webhook", "https://example.com/h", "en", "").unwrap()
        else {
            panic!()
        };
        let sub = confirm(&conn, &t).unwrap().unwrap();
        for _ in 0..MAX_FAILURES {
            record_delivery(&conn, sub.id, false).unwrap();
        }
        assert!(by_token(&conn, &t).unwrap().is_none());
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
