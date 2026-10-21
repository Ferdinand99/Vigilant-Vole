//! Status page incidents (`incidents`). Owned by the statuspages agent.
//!
//! An incident is an announcement written by a person (as opposed to the
//! automatic outages in `monitor_events`). Open incidents (`resolved_at IS
//! NULL`) that are pinned sit at the top of the public page; resolved ones
//! show up in the page's history.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

pub const SEVERITIES: &[&str] = &["info", "warning", "danger", "success"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Incident {
    pub id: i64,
    pub status_page_id: i64,
    pub title: String,
    /// Markdown subset, rendered by `api::public_status::markdown`.
    pub body: String,
    /// "info" | "warning" | "danger" | "success".
    pub severity: String,
    pub pinned: bool,
    pub created_at: String,
    pub updated_at: String,
    pub resolved_at: Option<String>,
}

impl Incident {
    pub fn is_resolved(&self) -> bool {
        self.resolved_at.is_some()
    }

    /// Design-system icon for the severity.
    pub fn icon(&self) -> &'static str {
        severity_icon(&self.severity)
    }

    pub fn severity_label(&self) -> &'static str {
        severity_label(&self.severity)
    }
}

pub fn severity_icon(severity: &str) -> &'static str {
    match severity {
        "danger" => "alert-circle",
        "warning" => "alert",
        "success" => "check-circle",
        _ => "info",
    }
}

pub fn severity_label(severity: &str) -> &'static str {
    match severity {
        "danger" => crate::i18n::t("inc.sev_danger"),
        "warning" => crate::i18n::t("inc.sev_warning"),
        "success" => crate::i18n::t("inc.sev_success"),
        _ => crate::i18n::t("inc.sev_information"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewIncident {
    pub title: String,
    pub body: String,
    pub severity: String,
    pub pinned: bool,
}

const COLUMNS: &str =
    "id, status_page_id, title, body, severity, pinned, created_at, updated_at, resolved_at";

pub fn row_to_incident(row: &Row) -> rusqlite::Result<Incident> {
    Ok(Incident {
        id: row.get("id")?,
        status_page_id: row.get("status_page_id")?,
        title: row.get("title")?,
        body: row.get("body")?,
        severity: row.get("severity")?,
        pinned: row.get("pinned")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        resolved_at: row.get("resolved_at")?,
    })
}

/// All incidents of a page: open first, then newest first.
pub fn list_for_page(conn: &Connection, page_id: i64) -> rusqlite::Result<Vec<Incident>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM incidents WHERE status_page_id = ?1
         ORDER BY (pinned = 1 AND resolved_at IS NULL) DESC, resolved_at IS NOT NULL, created_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map(params![page_id], row_to_incident)?;
    rows.collect()
}

/// Open, pinned incidents (top of the public page), newest first.
pub fn list_pinned_open(conn: &Connection, page_id: i64) -> rusqlite::Result<Vec<Incident>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM incidents WHERE status_page_id = ?1 AND pinned = 1 AND resolved_at IS NULL
         ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map(params![page_id], row_to_incident)?;
    rows.collect()
}

/// Incidents for the history section: everything not pinned-and-open that
/// was created or resolved at/after `since` (UTC text).
pub fn list_history(
    conn: &Connection,
    page_id: i64,
    since: &str,
) -> rusqlite::Result<Vec<Incident>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM incidents
         WHERE status_page_id = ?1 AND NOT (pinned = 1 AND resolved_at IS NULL)
           AND (created_at >= ?2 OR resolved_at >= ?2)
         ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map(params![page_id, since], row_to_incident)?;
    rows.collect()
}

pub fn get(conn: &Connection, page_id: i64, id: i64) -> rusqlite::Result<Option<Incident>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM incidents WHERE id = ?1 AND status_page_id = ?2"),
        params![id, page_id],
        row_to_incident,
    )
    .optional()
}

pub fn insert(conn: &Connection, page_id: i64, i: &NewIncident) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO incidents (status_page_id, title, body, severity, pinned) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![page_id, i.title, i.body, i.severity, i.pinned],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update(conn: &Connection, page_id: i64, id: i64, i: &NewIncident) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE incidents SET title = ?3, body = ?4, severity = ?5, pinned = ?6, updated_at = datetime('now')
         WHERE id = ?1 AND status_page_id = ?2",
        params![id, page_id, i.title, i.body, i.severity, i.pinned],
    )? > 0)
}

/// Resolve (`true`) or reopen (`false`).
pub fn set_resolved(
    conn: &Connection,
    page_id: i64,
    id: i64,
    resolved: bool,
) -> rusqlite::Result<bool> {
    let sql = if resolved {
        "UPDATE incidents SET resolved_at = datetime('now'), updated_at = datetime('now')
         WHERE id = ?1 AND status_page_id = ?2 AND resolved_at IS NULL"
    } else {
        "UPDATE incidents SET resolved_at = NULL, updated_at = datetime('now') WHERE id = ?1 AND status_page_id = ?2"
    };
    Ok(conn.execute(sql, params![id, page_id])? > 0)
}

pub fn delete(conn: &Connection, page_id: i64, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM incidents WHERE id = ?1 AND status_page_id = ?2",
        params![id, page_id],
    )? > 0)
}

// ---------------------------------------------------------------------------
// Updates (the timeline of an incident)
// ---------------------------------------------------------------------------

/// Where an incident stands, as announced in an update.
pub const UPDATE_STATUSES: &[&str] = &["investigating", "identified", "monitoring", "resolved"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IncidentUpdate {
    pub id: i64,
    pub incident_id: i64,
    /// "investigating" | "identified" | "monitoring" | "resolved".
    pub status: String,
    /// Markdown subset, rendered like the incident body.
    pub body: String,
    pub created_at: String,
}

impl IncidentUpdate {
    pub fn status_label(&self) -> &'static str {
        update_status_label(&self.status)
    }
}

pub fn update_status_label(status: &str) -> &'static str {
    match status {
        "identified" => crate::i18n::t("inc.st_identified"),
        "monitoring" => crate::i18n::t("inc.st_monitoring"),
        "resolved" => crate::i18n::t("inc.st_resolved"),
        _ => crate::i18n::t("inc.st_investigating"),
    }
}

fn row_to_update(row: &Row) -> rusqlite::Result<IncidentUpdate> {
    Ok(IncidentUpdate {
        id: row.get("id")?,
        incident_id: row.get("incident_id")?,
        status: row.get("status")?,
        body: row.get("body")?,
        created_at: row.get("created_at")?,
    })
}

/// One incident's updates, newest first.
pub fn list_updates(conn: &Connection, incident_id: i64) -> rusqlite::Result<Vec<IncidentUpdate>> {
    let mut stmt = conn.prepare(
        "SELECT id, incident_id, status, body, created_at FROM incident_updates
         WHERE incident_id = ?1 ORDER BY created_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![incident_id], row_to_update)?;
    rows.collect()
}

/// The updates of several incidents at once, newest first per incident.
pub fn updates_for(
    conn: &Connection,
    incident_ids: &[i64],
) -> rusqlite::Result<std::collections::HashMap<i64, Vec<IncidentUpdate>>> {
    let mut map: std::collections::HashMap<i64, Vec<IncidentUpdate>> =
        std::collections::HashMap::new();
    if incident_ids.is_empty() {
        return Ok(map);
    }
    let marks = vec!["?"; incident_ids.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, incident_id, status, body, created_at FROM incident_updates
         WHERE incident_id IN ({marks}) ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(incident_ids), row_to_update)?;
    for row in rows {
        let u = row?;
        map.entry(u.incident_id).or_default().push(u);
    }
    Ok(map)
}

/// The most recent incidents of a page (open first is not needed here), for feeds.
pub fn list_recent(
    conn: &Connection,
    page_id: i64,
    limit: usize,
) -> rusqlite::Result<Vec<Incident>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM incidents WHERE status_page_id = ?1 ORDER BY updated_at DESC, id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(
        params![page_id, i64::try_from(limit).unwrap_or(50)],
        row_to_incident,
    )?;
    rows.collect()
}

/// Posts an update to an incident of `page_id`. A `resolved` update resolves
/// the incident; any other status reopens it. `None` if the incident isn't
/// on that page.
pub fn add_update(
    conn: &Connection,
    page_id: i64,
    incident_id: i64,
    status: &str,
    body: &str,
) -> rusqlite::Result<Option<i64>> {
    if get(conn, page_id, incident_id)?.is_none() {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO incident_updates (incident_id, status, body) VALUES (?1, ?2, ?3)",
        params![incident_id, status, body],
    )?;
    let id = conn.last_insert_rowid();
    if status == "resolved" {
        conn.execute(
            "UPDATE incidents SET resolved_at = COALESCE(resolved_at, datetime('now')), updated_at = datetime('now') WHERE id = ?1",
            params![incident_id],
        )?;
    } else {
        conn.execute(
            "UPDATE incidents SET resolved_at = NULL, updated_at = datetime('now') WHERE id = ?1",
            params![incident_id],
        )?;
    }
    Ok(Some(id))
}

pub fn delete_update(
    conn: &Connection,
    page_id: i64,
    incident_id: i64,
    update_id: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM incident_updates WHERE id = ?3 AND incident_id = ?2
           AND incident_id IN (SELECT id FROM incidents WHERE id = ?2 AND status_page_id = ?1)",
        params![page_id, incident_id, update_id],
    )? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::status_pages::{self, NewStatusPage};
    use crate::db::test_conn;

    fn page(conn: &mut Connection, slug: &str) -> i64 {
        status_pages::insert(
            conn,
            &NewStatusPage {
                slug: slug.into(),
                title: slug.into(),
                ..NewStatusPage::default()
            },
        )
        .unwrap()
    }

    fn incident(title: &str, pinned: bool) -> NewIncident {
        NewIncident {
            title: title.into(),
            body: "Body".into(),
            severity: "warning".into(),
            pinned,
        }
    }

    #[test]
    fn crud_resolve_and_scoping() {
        let mut conn = test_conn();
        let p = page(&mut conn, "acme");
        let other = page(&mut conn, "other");
        let id = insert(&conn, p, &incident("Slow API", true)).unwrap();
        let unpinned = insert(&conn, p, &incident("FYI", false)).unwrap();

        assert_eq!(
            list_pinned_open(&conn, p)
                .unwrap()
                .iter()
                .map(|i| i.id)
                .collect::<Vec<_>>(),
            [id]
        );
        let history = list_history(&conn, p, "2000-01-01 00:00:00").unwrap();
        assert_eq!(history.iter().map(|i| i.id).collect::<Vec<_>>(), [unpinned]);

        // Scoped to its page.
        assert!(get(&conn, other, id).unwrap().is_none());
        assert!(!update(&conn, other, id, &incident("x", true)).unwrap());
        assert!(!delete(&conn, other, id).unwrap());

        let mut changed = incident("Slow API (fixed)", true);
        changed.severity = "success".into();
        assert!(update(&conn, p, id, &changed).unwrap());
        assert_eq!(get(&conn, p, id).unwrap().unwrap().severity, "success");

        assert!(set_resolved(&conn, p, id, true).unwrap());
        assert!(
            !set_resolved(&conn, p, id, true).unwrap(),
            "already resolved"
        );
        assert!(list_pinned_open(&conn, p).unwrap().is_empty());
        assert_eq!(
            list_history(&conn, p, "2000-01-01 00:00:00").unwrap().len(),
            2
        );
        let all = list_for_page(&conn, p).unwrap();
        assert_eq!(all[0].id, unpinned, "open incidents come first");

        assert!(set_resolved(&conn, p, id, false).unwrap());
        assert!(!get(&conn, p, id).unwrap().unwrap().is_resolved());
        assert!(delete(&conn, p, id).unwrap());
        assert_eq!(list_for_page(&conn, p).unwrap().len(), 1);
    }

    #[test]
    fn admin_list_puts_pinned_open_incidents_first() {
        let mut conn = test_conn();
        let p = page(&mut conn, "acme");
        let pinned_old = insert(&conn, p, &incident("Pinned, older", true)).unwrap();
        let open = insert(&conn, p, &incident("Open", false)).unwrap();
        let resolved = insert(&conn, p, &incident("Resolved", true)).unwrap();
        set_resolved(&conn, p, resolved, true).unwrap();
        let pinned_new = insert(&conn, p, &incident("Pinned, newer", true)).unwrap();
        let order: Vec<i64> = list_for_page(&conn, p)
            .unwrap()
            .iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(order, [pinned_new, pinned_old, open, resolved]);
    }

    #[test]
    fn severity_check_constraint_holds() {
        let mut conn = test_conn();
        let p = page(&mut conn, "acme");
        let mut bad = incident("x", true);
        bad.severity = "critical".into();
        assert!(insert(&conn, p, &bad).is_err());
    }

    #[test]
    fn icons_and_labels_cover_all_severities() {
        for s in SEVERITIES {
            assert!(!severity_icon(s).is_empty());
            assert!(!severity_label(s).is_empty());
        }
    }

    #[test]
    fn updates_form_a_timeline_and_drive_the_resolved_state() {
        let mut conn = test_conn();
        let p = page(&mut conn, "acme");
        let other = page(&mut conn, "other");
        let id = insert(&conn, p, &incident("Slow API", true)).unwrap();

        let first = add_update(&conn, p, id, "investigating", "Looking into it")
            .unwrap()
            .unwrap();
        let second = add_update(&conn, p, id, "identified", "A bad deploy")
            .unwrap()
            .unwrap();
        let timeline = list_updates(&conn, id).unwrap();
        assert_eq!(
            timeline.iter().map(|u| u.id).collect::<Vec<_>>(),
            [second, first],
            "newest first"
        );
        assert_eq!(timeline[0].status_label(), "Identified");
        assert!(!get(&conn, p, id).unwrap().unwrap().is_resolved());

        // A resolving update resolves the incident; another status reopens it.
        add_update(&conn, p, id, "resolved", "Fixed")
            .unwrap()
            .unwrap();
        let resolved_at = get(&conn, p, id).unwrap().unwrap().resolved_at;
        assert!(resolved_at.is_some());
        add_update(&conn, p, id, "resolved", "Still fixed")
            .unwrap()
            .unwrap();
        assert_eq!(
            get(&conn, p, id).unwrap().unwrap().resolved_at,
            resolved_at,
            "keeps the first resolution time"
        );
        add_update(&conn, p, id, "monitoring", "Watching")
            .unwrap()
            .unwrap();
        assert!(!get(&conn, p, id).unwrap().unwrap().is_resolved());

        // Scoped to the page; deleting the incident takes its updates along.
        assert_eq!(
            add_update(&conn, other, id, "monitoring", "x").unwrap(),
            None
        );
        assert!(!delete_update(&conn, other, id, first).unwrap());
        assert!(delete_update(&conn, p, id, first).unwrap());
        assert_eq!(list_updates(&conn, id).unwrap().len(), 4);
        let many = updates_for(&conn, &[id, 999]).unwrap();
        assert_eq!(many[&id].len(), 4);
        assert!(!many.contains_key(&999));
        assert!(updates_for(&conn, &[]).unwrap().is_empty());
        assert_eq!(list_recent(&conn, p, 10).unwrap().len(), 1);
        assert!(delete(&conn, p, id).unwrap());
        assert!(list_updates(&conn, id).unwrap().is_empty());
    }
}
