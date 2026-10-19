//! Status pages, sections and their monitors (`status_pages`, `status_page_sections`,
//! `status_page_monitors`). Owned by the statuspages agent.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// A row of `status_pages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusPage {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub custom_domain: Option<String>,
    pub published: bool,
    /// "auto" | "light" | "dark".
    pub theme: String,
    pub show_tags: bool,
    pub show_uptime: bool,
    pub show_response: bool,
    pub show_powered_by: bool,
    pub footer_text: Option<String>,
    pub custom_css: Option<String>,
    pub refresh_seconds: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Fields written by create/update (already validated, see [`validate_slug`] etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewStatusPage {
    pub slug: String,
    pub title: String,
    pub description: String,
    pub custom_domain: Option<String>,
    pub published: bool,
    pub theme: String,
    pub show_tags: bool,
    pub show_uptime: bool,
    pub show_response: bool,
    pub show_powered_by: bool,
    pub footer_text: Option<String>,
    pub custom_css: Option<String>,
    pub refresh_seconds: i64,
}

impl Default for NewStatusPage {
    fn default() -> Self {
        Self {
            slug: String::new(),
            title: String::new(),
            description: String::new(),
            custom_domain: None,
            published: true,
            theme: "auto".into(),
            show_tags: false,
            show_uptime: true,
            show_response: true,
            show_powered_by: true,
            footer_text: None,
            custom_css: None,
            refresh_seconds: 60,
        }
    }
}

impl StatusPage {
    pub fn to_new(&self) -> NewStatusPage {
        NewStatusPage {
            slug: self.slug.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            custom_domain: self.custom_domain.clone(),
            published: self.published,
            theme: self.theme.clone(),
            show_tags: self.show_tags,
            show_uptime: self.show_uptime,
            show_response: self.show_response,
            show_powered_by: self.show_powered_by,
            footer_text: self.footer_text.clone(),
            custom_css: self.custom_css.clone(),
            refresh_seconds: self.refresh_seconds,
        }
    }
}

/// List row: a page plus how many monitors it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusPageSummary {
    pub page: StatusPage,
    pub monitor_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Section {
    pub id: i64,
    pub name: String,
    pub sort_order: i64,
    pub monitors: Vec<SectionMonitor>,
}

/// A monitor placed in a section (only what the editor and public page need).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionMonitor {
    pub monitor_id: i64,
    pub name: String,
    pub monitor_type: String,
    /// `false` = paused.
    pub active: bool,
    pub sort_order: i64,
}

impl SectionMonitor {
    pub fn type_label(&self) -> &'static str {
        crate::db::models::MonitorType::parse(&self.monitor_type).map_or(crate::i18n::t("form.back_monitor"), |t| t.label())
    }
}

const COLUMNS: &str = "id, slug, title, description, custom_domain, published, theme, show_tags, show_uptime, \
                       show_response, show_powered_by, footer_text, custom_css, refresh_seconds, created_at, updated_at";

/// Slugs that would shadow app routes or read confusingly.
pub const RESERVED_SLUGS: &[&str] = &[
    "admin",
    "api",
    "assets",
    "badge",
    "badges",
    "dashboard",
    "edit",
    "events",
    "fragment",
    "healthz",
    "login",
    "logout",
    "maintenance",
    "metrics",
    "monitors",
    "new",
    "settings",
    "setup",
    "static",
    "status",
    "status-pages",
];

pub const REFRESH_MIN: i64 = 10;
pub const REFRESH_MAX: i64 = 3600;
pub const THEMES: &[&str] = &["auto", "light", "dark"];

pub fn row_to_page(row: &Row) -> rusqlite::Result<StatusPage> {
    Ok(StatusPage {
        id: row.get("id")?,
        slug: row.get("slug")?,
        title: row.get("title")?,
        description: row.get("description")?,
        custom_domain: row.get("custom_domain")?,
        published: row.get("published")?,
        theme: row.get("theme")?,
        show_tags: row.get("show_tags")?,
        show_uptime: row.get("show_uptime")?,
        show_response: row.get("show_response")?,
        show_powered_by: row.get("show_powered_by")?,
        footer_text: row.get("footer_text")?,
        custom_css: row.get("custom_css")?,
        refresh_seconds: row.get("refresh_seconds")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// Validation (pure)
// ---------------------------------------------------------------------------

/// Lowercase letters, digits and hyphens; 1–64 chars; starts and ends with a
/// letter or digit; not reserved. Returns a user-facing message on failure.
pub fn validate_slug(slug: &str) -> Result<(), &'static str> {
    if slug.is_empty() {
        return Err(crate::i18n::t("sp.err_slug"));
    }
    if slug.len() > 64 {
        return Err(crate::i18n::t("sp.err_slug_long"));
    }
    if !slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(crate::i18n::t("sp.err_slug_chars"));
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err(crate::i18n::t("sp.err_slug_ends"));
    }
    if RESERVED_SLUGS.contains(&slug) {
        return Err(crate::i18n::t("sp.err_slug_reserved"));
    }
    Ok(())
}

/// Turns a title into a slug candidate ("Acme Cloud!" → "acme-cloud").
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 64 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// Normalises and validates a custom domain: lowercase, no scheme, path,
/// port or trailing dot; at least two labels of letters/digits/hyphens.
/// Empty input → `Ok(None)`.
pub fn normalize_domain(input: &str) -> Result<Option<String>, &'static str> {
    let mut d = input.trim().to_ascii_lowercase();
    if d.is_empty() {
        return Ok(None);
    }
    for scheme in ["https://", "http://"] {
        if let Some(rest) = d.strip_prefix(scheme) {
            d = rest.to_string();
        }
    }
    let d = d.trim_end_matches('/').trim_end_matches('.');
    if d.contains(['/', ':', '@', '?', '#']) || d.contains(char::is_whitespace) {
        return Err(crate::i18n::t("sp.err_domain_host"));
    }
    if d.len() > 253 {
        return Err(crate::i18n::t("sp.err_domain_long"));
    }
    let labels: Vec<&str> = d.split('.').collect();
    if labels.len() < 2 {
        return Err(crate::i18n::t("sp.err_domain_full"));
    }
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if !labels.iter().all(label_ok) {
        return Err(crate::i18n::t("sp.err_domain_chars"));
    }
    if labels.last().is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit())) {
        return Err(crate::i18n::t("sp.err_domain_ip"));
    }
    Ok(Some(d.to_string()))
}

/// Longest footer, in characters (rendered as Markdown on every public view).
pub const FOOTER_MAX: usize = 5_000;

/// The host part of a `Host` header value or URL authority: lowercase,
/// without port or trailing dot.
fn bare_host(value: &str) -> String {
    let v = value.trim().to_ascii_lowercase();
    let host = if v.starts_with('[') {
        v.split_once(']').map_or(v.clone(), |(h, _)| format!("{h}]"))
    } else {
        v.rsplit_once(':').map_or(v.clone(), |(h, _)| h.to_string())
    };
    host.trim_end_matches('.').to_string()
}

/// Whether `domain` (already normalized) is this instance's own address —
/// the host of the configured base URL or of the current request(s). A page
/// there would take over `/` and hide the dashboard from everyone.
pub fn is_own_host(domain: &str, base_url: Option<&str>, request_hosts: &[&str]) -> bool {
    let base = base_url
        .and_then(|b| url::Url::parse(b.trim()).ok())
        .and_then(|u| u.host_str().map(bare_host));
    base.iter()
        .cloned()
        .chain(request_hosts.iter().map(|h| bare_host(h)))
        .any(|h| !h.is_empty() && h == domain)
}

/// Custom CSS is rendered inside `<style>`; refuse anything that could end it.
pub fn validate_css(css: &str) -> Result<(), &'static str> {
    let lower = css.to_ascii_lowercase();
    if lower.contains("</style") || lower.contains("<!--") || lower.contains("<script") {
        return Err(crate::i18n::t("sp.err_css"));
    }
    if css.len() > 64 * 1024 {
        return Err(crate::i18n::t("sp.err_css_long"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

pub fn list(conn: &Connection) -> rusqlite::Result<Vec<StatusPageSummary>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS},
                (SELECT COUNT(DISTINCT spm.monitor_id) FROM status_page_monitors spm
                 JOIN status_page_sections s ON s.id = spm.section_id WHERE s.status_page_id = p.id) AS monitor_count
         FROM status_pages p ORDER BY title COLLATE NOCASE, id"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(StatusPageSummary {
            page: row_to_page(r)?,
            monitor_count: r.get("monitor_count")?,
        })
    })?;
    rows.collect()
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<StatusPage>> {
    conn.query_row(&format!("SELECT {COLUMNS} FROM status_pages WHERE id = ?1"), params![id], row_to_page)
        .optional()
}

pub fn get_by_slug(conn: &Connection, slug: &str) -> rusqlite::Result<Option<StatusPage>> {
    conn.query_row(&format!("SELECT {COLUMNS} FROM status_pages WHERE slug = ?1"), params![slug], row_to_page)
        .optional()
}

/// `(custom_domain, slug)` of every published page with a custom domain.
pub fn published_domains(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT custom_domain, slug FROM status_pages WHERE published = 1 AND custom_domain IS NOT NULL AND custom_domain != ''",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Whether another page (not `except_id`) already uses this slug.
pub fn slug_taken(conn: &Connection, slug: &str, except_id: Option<i64>) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM status_pages WHERE slug = ?1 AND id != ?2)",
        params![slug, except_id.unwrap_or(-1)],
        |r| r.get(0),
    )
}

pub fn domain_taken(conn: &Connection, domain: &str, except_id: Option<i64>) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM status_pages WHERE custom_domain = ?1 AND id != ?2)",
        params![domain, except_id.unwrap_or(-1)],
        |r| r.get(0),
    )
}

/// Creates a page with one empty "Services" section.
pub fn insert(conn: &mut Connection, p: &NewStatusPage) -> rusqlite::Result<i64> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO status_pages (slug, title, description, custom_domain, published, theme, show_tags, show_uptime,
             show_response, show_powered_by, footer_text, custom_css, refresh_seconds)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            p.slug,
            p.title,
            p.description,
            p.custom_domain,
            p.published,
            p.theme,
            p.show_tags,
            p.show_uptime,
            p.show_response,
            p.show_powered_by,
            p.footer_text,
            p.custom_css,
            p.refresh_seconds
        ],
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO status_page_sections (status_page_id, name, sort_order) VALUES (?1, ?2, 0)",
        params![id, crate::i18n::t("sp.default_section")],
    )?;
    tx.commit()?;
    Ok(id)
}

pub fn update(conn: &Connection, id: i64, p: &NewStatusPage) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE status_pages SET slug = ?2, title = ?3, description = ?4, custom_domain = ?5, published = ?6, theme = ?7,
             show_tags = ?8, show_uptime = ?9, show_response = ?10, show_powered_by = ?11, footer_text = ?12,
             custom_css = ?13, refresh_seconds = ?14, updated_at = datetime('now')
         WHERE id = ?1",
        params![
            id,
            p.slug,
            p.title,
            p.description,
            p.custom_domain,
            p.published,
            p.theme,
            p.show_tags,
            p.show_uptime,
            p.show_response,
            p.show_powered_by,
            p.footer_text,
            p.custom_css,
            p.refresh_seconds
        ],
    )? > 0)
}

pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM status_pages WHERE id = ?1", params![id])? > 0)
}

fn touch(conn: &Connection, page_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE status_pages SET updated_at = datetime('now') WHERE id = ?1",
        params![page_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sections & monitors
// ---------------------------------------------------------------------------

/// Sections of a page in display order, each with its monitors in order.
pub fn sections(conn: &Connection, page_id: i64) -> rusqlite::Result<Vec<Section>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, sort_order FROM status_page_sections WHERE status_page_id = ?1 ORDER BY sort_order, id",
    )?;
    let mut sections: Vec<Section> = stmt
        .query_map(params![page_id], |r| {
            Ok(Section {
                id: r.get(0)?,
                name: r.get(1)?,
                sort_order: r.get(2)?,
                monitors: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut stmt = conn.prepare(
        "SELECT spm.section_id, spm.monitor_id, m.name, m.monitor_type, m.active, spm.sort_order
         FROM status_page_monitors spm
         JOIN status_page_sections s ON s.id = spm.section_id
         JOIN monitors m ON m.id = spm.monitor_id
         WHERE s.status_page_id = ?1
         ORDER BY spm.sort_order, m.name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map(params![page_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            SectionMonitor {
                monitor_id: r.get(1)?,
                name: r.get(2)?,
                monitor_type: r.get(3)?,
                active: r.get(4)?,
                sort_order: r.get(5)?,
            },
        ))
    })?;
    for row in rows {
        let (section_id, monitor) = row?;
        if let Some(s) = sections.iter_mut().find(|s| s.id == section_id) {
            s.monitors.push(monitor);
        }
    }
    Ok(sections)
}

/// Whether `section_id` belongs to `page_id`.
pub fn section_on_page(conn: &Connection, page_id: i64, section_id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM status_page_sections WHERE id = ?1 AND status_page_id = ?2)",
        params![section_id, page_id],
        |r| r.get(0),
    )
}

pub fn add_section(conn: &Connection, page_id: i64, name: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO status_page_sections (status_page_id, name, sort_order)
         SELECT ?1, ?2, COALESCE(MAX(sort_order) + 1, 0) FROM status_page_sections WHERE status_page_id = ?1",
        params![page_id, name],
    )?;
    let id = conn.last_insert_rowid();
    touch(conn, page_id)?;
    Ok(id)
}

pub fn rename_section(conn: &Connection, page_id: i64, section_id: i64, name: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE status_page_sections SET name = ?3 WHERE id = ?1 AND status_page_id = ?2",
        params![section_id, page_id, name],
    )?;
    touch(conn, page_id)?;
    Ok(n > 0)
}

pub fn delete_section(conn: &Connection, page_id: i64, section_id: i64) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM status_page_sections WHERE id = ?1 AND status_page_id = ?2",
        params![section_id, page_id],
    )?;
    touch(conn, page_id)?;
    Ok(n > 0)
}

/// Appends monitors to a section (existing ones and unknown ids are skipped).
pub fn add_monitors(conn: &mut Connection, page_id: i64, section_id: i64, monitor_ids: &[i64]) -> rusqlite::Result<usize> {
    if !section_on_page(conn, page_id, section_id)? {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut added = 0;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO status_page_monitors (section_id, monitor_id, sort_order)
             SELECT ?1, m.id, (SELECT COALESCE(MAX(sort_order) + 1, 0) FROM status_page_monitors WHERE section_id = ?1)
             FROM monitors m WHERE m.id = ?2",
        )?;
        for id in monitor_ids {
            added += stmt.execute(params![section_id, id])?;
        }
    }
    touch(&tx, page_id)?;
    tx.commit()?;
    Ok(added)
}

pub fn remove_monitor(conn: &Connection, page_id: i64, section_id: i64, monitor_id: i64) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM status_page_monitors WHERE section_id = ?1 AND monitor_id = ?2
           AND section_id IN (SELECT id FROM status_page_sections WHERE status_page_id = ?3)",
        params![section_id, monitor_id, page_id],
    )?;
    touch(conn, page_id)?;
    Ok(n > 0)
}

/// Applies a drag-and-drop result: `section_order` sets the order of the
/// page's sections; each `(section_id, monitor_ids)` replaces that section's
/// monitors (in order). Sections that aren't on this page are ignored.
pub fn apply_order(
    conn: &mut Connection,
    page_id: i64,
    section_order: &[i64],
    section_monitors: &[(i64, Vec<i64>)],
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut set_order =
            tx.prepare("UPDATE status_page_sections SET sort_order = ?3 WHERE id = ?1 AND status_page_id = ?2")?;
        for (i, sid) in section_order.iter().enumerate() {
            set_order.execute(params![sid, page_id, i as i64])?;
        }
        let mut owned = tx.prepare("SELECT EXISTS(SELECT 1 FROM status_page_sections WHERE id = ?1 AND status_page_id = ?2)")?;
        let mut clear = tx.prepare("DELETE FROM status_page_monitors WHERE section_id = ?1")?;
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO status_page_monitors (section_id, monitor_id, sort_order)
             SELECT ?1, id, ?3 FROM monitors WHERE id = ?2",
        )?;
        for (sid, monitors) in section_monitors {
            let ok: bool = owned.query_row(params![sid, page_id], |r| r.get(0))?;
            if !ok {
                continue;
            }
            clear.execute(params![sid])?;
            for (i, mid) in monitors.iter().enumerate() {
                insert.execute(params![sid, mid, i as i64])?;
            }
        }
    }
    touch(&tx, page_id)?;
    tx.commit()
}

/// Distinct monitor ids shown on a page.
pub fn monitor_ids(conn: &Connection, page_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT spm.monitor_id FROM status_page_monitors spm
         JOIN status_page_sections s ON s.id = spm.section_id WHERE s.status_page_id = ?1",
    )?;
    let rows = stmt.query_map(params![page_id], |r| r.get(0))?;
    rows.collect()
}

/// Whether the monitor appears on at least one published page (badge gate).
pub fn monitor_is_public(conn: &Connection, monitor_id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM status_page_monitors spm
             JOIN status_page_sections s ON s.id = spm.section_id
             JOIN status_pages p ON p.id = s.status_page_id
             WHERE spm.monitor_id = ?1 AND p.published = 1)",
        params![monitor_id],
        |r| r.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewMonitor;
    use crate::db::{monitors, test_conn};

    #[test]
    fn own_host_is_recognised_from_base_url_and_request() {
        let base = Some("https://Vole.Example.com:8443/app/");
        assert!(is_own_host("vole.example.com", base, &[]));
        assert!(is_own_host("status.acme.test", None, &["Status.Acme.Test:3000"]));
        assert!(is_own_host("status.acme.test", None, &["status.acme.test."]));
        assert!(!is_own_host("status.acme.test", base, &["vole.test"]));
        assert!(!is_own_host("status.acme.test", Some("not a url"), &[""]));
    }

    fn page(slug: &str) -> NewStatusPage {
        NewStatusPage {
            slug: slug.into(),
            title: format!("{slug} status"),
            ..NewStatusPage::default()
        }
    }

    fn monitor(conn: &Connection, name: &str) -> i64 {
        monitors::insert_monitor(
            conn,
            &NewMonitor {
                name: name.into(),
                ..NewMonitor::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn slug_validation() {
        assert!(validate_slug("acme").is_ok());
        assert!(validate_slug("acme-cloud-2").is_ok());
        assert!(validate_slug("a").is_ok());
        assert!(validate_slug("").is_err());
        assert!(validate_slug("Acme").is_err());
        assert!(validate_slug("acme cloud").is_err());
        assert!(validate_slug("-acme").is_err());
        assert!(validate_slug("acme-").is_err());
        assert!(validate_slug("ac/me").is_err());
        assert!(validate_slug("ünï").is_err());
        assert!(validate_slug(&"a".repeat(65)).is_err());
        for reserved in ["new", "api", "static", "status-pages", "fragment"] {
            assert!(validate_slug(reserved).is_err(), "{reserved} should be reserved");
        }
    }

    #[test]
    fn slugify_makes_valid_slugs() {
        assert_eq!(slugify("Acme Cloud!"), "acme-cloud");
        assert_eq!(slugify("  --Hello__World-- "), "hello-world");
        assert_eq!(slugify("Ünïcode"), "n-code");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn domain_validation() {
        assert_eq!(normalize_domain("").unwrap(), None);
        assert_eq!(normalize_domain("  Status.Example.COM ").unwrap().as_deref(), Some("status.example.com"));
        assert_eq!(normalize_domain("https://status.example.com/").unwrap().as_deref(), Some("status.example.com"));
        assert_eq!(normalize_domain("status.example.com.").unwrap().as_deref(), Some("status.example.com"));
        assert_eq!(normalize_domain("xn--bcher-kva.example").unwrap().as_deref(), Some("xn--bcher-kva.example"));
        assert!(normalize_domain("localhost").is_err());
        assert!(normalize_domain("status.example.com:8080").is_err());
        assert!(normalize_domain("status.example.com/path").is_err());
        assert!(normalize_domain("-bad.example.com").is_err());
        assert!(normalize_domain("bad-.example.com").is_err());
        assert!(normalize_domain("a..b").is_err());
        assert!(normalize_domain("exa mple.com").is_err());
        assert!(normalize_domain("under_score.example.com").is_err());
        assert!(normalize_domain("10.0.0.1").is_err());
        assert!(normalize_domain(&format!("{}.com", "a".repeat(64))).is_err());
    }

    #[test]
    fn css_validation_rejects_style_breakouts() {
        assert!(validate_css("body { color: red }").is_ok());
        assert!(validate_css("</style><script>alert(1)</script>").is_err());
        assert!(validate_css("</STYLE >").is_err());
        assert!(validate_css("a{} <!-- x").is_err());
        assert!(validate_css("<ScRiPt>").is_err());
    }

    #[test]
    fn page_crud_and_uniqueness() {
        let mut conn = test_conn();
        let id = insert(&mut conn, &page("acme")).unwrap();
        let p = get(&conn, id).unwrap().unwrap();
        assert_eq!(p.slug, "acme");
        assert!(p.published);
        assert!(p.show_powered_by);
        assert_eq!(p.refresh_seconds, 60);
        assert_eq!(get_by_slug(&conn, "acme").unwrap().map(|p| p.id), Some(id));
        // A new page starts with one section.
        assert_eq!(sections(&conn, id).unwrap().len(), 1);

        assert!(slug_taken(&conn, "acme", None).unwrap());
        assert!(!slug_taken(&conn, "acme", Some(id)).unwrap());
        assert!(insert(&mut conn, &page("acme")).is_err());

        let mut changed = p.to_new();
        changed.custom_domain = Some("status.acme.test".into());
        changed.published = false;
        assert!(update(&conn, id, &changed).unwrap());
        assert!(domain_taken(&conn, "status.acme.test", None).unwrap());
        assert!(!domain_taken(&conn, "status.acme.test", Some(id)).unwrap());
        // Unpublished pages don't serve their domain.
        assert!(published_domains(&conn).unwrap().is_empty());
        changed.published = true;
        update(&conn, id, &changed).unwrap();
        assert_eq!(
            published_domains(&conn).unwrap(),
            vec![("status.acme.test".to_string(), "acme".to_string())]
        );

        assert_eq!(list(&conn).unwrap().len(), 1);
        assert!(delete(&conn, id).unwrap());
        assert!(get(&conn, id).unwrap().is_none());
    }

    #[test]
    fn sections_monitors_and_ordering() {
        let mut conn = test_conn();
        let page_id = insert(&mut conn, &page("acme")).unwrap();
        let other_page = insert(&mut conn, &page("other")).unwrap();
        let s1 = sections(&conn, page_id).unwrap()[0].id;
        let s2 = add_section(&conn, page_id, "APIs").unwrap();
        let foreign = sections(&conn, other_page).unwrap()[0].id;
        let (a, b, c) = (monitor(&conn, "a"), monitor(&conn, "b"), monitor(&conn, "c"));

        assert_eq!(add_monitors(&mut conn, page_id, s1, &[a, b, a, 999]).unwrap(), 2);
        assert_eq!(add_monitors(&mut conn, page_id, foreign, &[c]).unwrap(), 0);
        add_monitors(&mut conn, page_id, s2, &[c]).unwrap();
        let secs = sections(&conn, page_id).unwrap();
        assert_eq!(secs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Services", "APIs"]);
        assert_eq!(secs[0].monitors.iter().map(|m| m.monitor_id).collect::<Vec<_>>(), [a, b]);

        // Swap sections, move `a` into APIs after `c`, drop nothing else.
        apply_order(&mut conn, page_id, &[s2, s1], &[(s2, vec![c, a]), (s1, vec![b]), (foreign, vec![a])]).unwrap();
        let secs = sections(&conn, page_id).unwrap();
        assert_eq!(secs[0].id, s2);
        assert_eq!(secs[0].monitors.iter().map(|m| m.monitor_id).collect::<Vec<_>>(), [c, a]);
        assert_eq!(secs[1].monitors.iter().map(|m| m.monitor_id).collect::<Vec<_>>(), [b]);
        // The other page's section was not touched.
        assert!(sections(&conn, other_page).unwrap()[0].monitors.is_empty());

        let mut ids = monitor_ids(&conn, page_id).unwrap();
        ids.sort();
        assert_eq!(ids, vec![a, b, c]);

        assert!(rename_section(&conn, page_id, s2, "Public APIs").unwrap());
        assert!(!rename_section(&conn, other_page, s2, "nope").unwrap());
        assert!(remove_monitor(&conn, page_id, s2, c).unwrap());
        assert!(!remove_monitor(&conn, other_page, s2, a).unwrap());
        assert!(!delete_section(&conn, other_page, s2).unwrap());
        assert!(delete_section(&conn, page_id, s2).unwrap());
        assert_eq!(sections(&conn, page_id).unwrap().len(), 1);
    }

    #[test]
    fn monitor_is_public_only_via_published_pages() {
        let mut conn = test_conn();
        let page_id = insert(&mut conn, &page("acme")).unwrap();
        let s = sections(&conn, page_id).unwrap()[0].id;
        let a = monitor(&conn, "a");
        assert!(!monitor_is_public(&conn, a).unwrap());
        add_monitors(&mut conn, page_id, s, &[a]).unwrap();
        assert!(monitor_is_public(&conn, a).unwrap());
        let mut p = get(&conn, page_id).unwrap().unwrap().to_new();
        p.published = false;
        update(&conn, page_id, &p).unwrap();
        assert!(!monitor_is_public(&conn, a).unwrap());
    }
}
