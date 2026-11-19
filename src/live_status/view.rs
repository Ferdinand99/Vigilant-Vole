//! What a board shows, read from the database.
//!
//! The words come from the language of the board: call [`load`] inside
//! `i18n::sync_scope`.
//!
//! Privacy: a status page board is built from the status page's own view
//! ([`crate::api::public_status`]), so it shows what the page shows and
//! nothing else (its sections, order, names, visibility, and the figures the
//! page itself shows). A board of picked monitors shows only those, by name.
//! Neither carries a URL, host, target or check message. A status page that
//! is not published, or is behind a password, is not shown at all.

use std::collections::HashSet;

use jiff::Timestamp;
use rusqlite::Connection;

use super::layout::{BoardView, Notice, Row, Section};
use crate::api::public_status::{self as ps, PublicStatus};
use crate::db::live_boards::Board;
use crate::db::models::{Monitor, MonitorType};
use crate::db::{
    maintenance as db_maint, monitors as db_monitors, rollups, status_pages as db_pages,
};
use crate::maintenance::{self, Schedule, WindowState};

/// Why a board can't be built.
#[derive(Debug)]
pub enum LoadError {
    /// The board follows a status page that is gone.
    PageGone,
    /// The status page is a draft or behind a password.
    PageNotPublic,
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for LoadError {
    fn from(e: rusqlite::Error) -> Self {
        LoadError::Db(e)
    }
}

impl LoadError {
    pub fn message(&self) -> String {
        match self {
            LoadError::PageGone => "the status page of this board was deleted".into(),
            LoadError::PageNotPublic => {
                "the status page of this board is not public (a draft, or behind a password)".into()
            }
            LoadError::Db(_) => "could not read the database".into(),
        }
    }
}

/// A board's content and the monitors that are down in it.
pub struct Loaded {
    pub view: BoardView,
    /// Monitors that are down, in order, once each.
    pub down: Vec<(i64, String)>,
}

fn status_text(s: PublicStatus) -> String {
    s.label().to_string()
}

struct Figures {
    uptime_24h: bool,
    uptime_30d: bool,
    response: bool,
}

/// One monitor's line.
fn row(
    conn: &Connection,
    id: i64,
    name: &str,
    status: PublicStatus,
    response_ms: Option<i64>,
    show: &Figures,
) -> rusqlite::Result<Row> {
    let mut parts: Vec<String> = Vec::new();
    if show.uptime_24h
        && let Some(p) = rollups::uptime_24h_cached(conn, id)?
    {
        parts.push(crate::i18n::tf(
            "live.uptime_24h",
            &[("value", &ps::fmt_pct(p))],
        ));
    }
    if show.uptime_30d
        && let Some(p) = rollups::uptime_pct(conn, id, 30)?
    {
        parts.push(crate::i18n::tf(
            "live.uptime_30d",
            &[("value", &ps::fmt_pct(p))],
        ));
    }
    if show.response
        && status == PublicStatus::Up
        && let Some(ms) = response_ms
    {
        parts.push(if ms < 1 {
            "< 1 ms".to_string()
        } else {
            crate::i18n::fmt_ms(ms)
        });
    }
    Ok(Row {
        class: status.class(),
        name: name.to_string(),
        status: status_text(status),
        details: parts.join(" \u{b7} "),
    })
}

fn rfc3339(now: Timestamp) -> String {
    maintenance::fmt_db_ts(now).replacen(' ', "T", 1) + "Z"
}

/// Builds the board's content at `now`.
pub fn load(conn: &Connection, board: &Board, now: Timestamp) -> Result<Loaded, LoadError> {
    match (board.source.as_str(), board.status_page_id) {
        ("page", Some(page_id)) => from_page(conn, board, page_id, now),
        ("page", None) => Err(LoadError::PageGone),
        _ => Ok(from_items(conn, board, now)?),
    }
}

fn from_page(
    conn: &Connection,
    board: &Board,
    page_id: i64,
    now: Timestamp,
) -> Result<Loaded, LoadError> {
    let page = db_pages::get(conn, page_id)?.ok_or(LoadError::PageGone)?;
    if !page.published || page.password_hash.is_some() {
        return Err(LoadError::PageNotPublic);
    }
    let page_title = page.title.clone();
    // What the page itself shows of uptime and response time bounds what a
    // board may show.
    let show = Figures {
        uptime_24h: board.show_uptime_24h && page.show_uptime,
        uptime_30d: board.show_uptime_30d && page.show_uptime,
        response: board.show_response && page.show_response,
    };
    let view = ps::load_view_data(conn, page, now)?;

    let mut down: Vec<(i64, String)> = Vec::new();
    let mut sections = Vec::new();
    for s in &view.sections {
        let mut rows = Vec::new();
        for m in &s.monitors {
            if m.status == PublicStatus::Down && !down.iter().any(|(id, _)| *id == m.id) {
                down.push((m.id, m.name.clone()));
            }
            rows.push(row(conn, m.id, &m.name, m.status, m.response_ms, &show)?);
        }
        sections.push(Section {
            name: Some(s.name.clone()),
            rows,
        });
    }

    let mut notices = Vec::new();
    if board.show_incidents {
        for n in &view.pinned {
            // The newest update says where things stand.
            let (body, label) = match n.updates.first() {
                Some(u) => (u.body.clone(), u.label.to_string()),
                None => (n.body.clone(), String::new()),
            };
            notices.push(Notice {
                class: n.class.clone(),
                title: n.title.clone(),
                body,
                meta: if label.is_empty() {
                    n.meta.clone()
                } else {
                    format!("{label} \u{b7} {}", n.meta)
                },
            });
        }
        for n in view.maintenance.iter().filter(|n| n.class == "maintenance") {
            notices.push(Notice {
                class: "maintenance".into(),
                title: n.title.clone(),
                body: n.body.clone(),
                meta: n.meta.clone(),
            });
        }
    }

    Ok(Loaded {
        view: BoardView {
            title: if board.title.trim().is_empty() {
                page_title
            } else {
                board.title.clone()
            },
            description: board.description.clone(),
            overall_class: view.overall.class.to_string(),
            overall_title: view.overall.title.to_string(),
            overall_detail: view.overall.detail.clone(),
            sections,
            notices,
            updated_at: rfc3339(now),
            footer: crate::product::NAME.to_string(),
            more_label: crate::i18n::t("live.more").to_string(),
            colour: board.color.clone(),
        },
        down,
    })
}

/// The monitors below a group (not groups themselves), in the group's order.
fn leaves(conn: &Connection, group: &Monitor, seen: &mut HashSet<i64>, out: &mut Vec<Monitor>) {
    if !seen.insert(group.id) {
        return;
    }
    for child in db_monitors::children(conn, group.id).unwrap_or_default() {
        if child.monitor_type == MonitorType::Group {
            leaves(conn, &child, seen, out);
        } else if seen.insert(child.id) {
            out.push(child);
        }
    }
}

fn from_items(conn: &Connection, board: &Board, now: Timestamp) -> rusqlite::Result<Loaded> {
    let show = Figures {
        uptime_24h: board.show_uptime_24h,
        uptime_30d: board.show_uptime_30d,
        response: board.show_response,
    };
    let covered = maintenance::covered_monitors(conn)?;
    let mut statuses = Vec::new();
    let mut down: Vec<(i64, String)> = Vec::new();
    let mut shown: HashSet<i64> = HashSet::new();
    let mut sections: Vec<Section> = Vec::new();

    let mut line = |m: &Monitor, statuses: &mut Vec<PublicStatus>| -> rusqlite::Result<Row> {
        let (hb, ms) = ps::latest_local(conn, m.id)?;
        let status = PublicStatus::from_heartbeat(hb, m.active, covered.contains(&m.id));
        statuses.push(status);
        if status == PublicStatus::Down && !down.iter().any(|(id, _)| *id == m.id) {
            down.push((m.id, m.name.clone()));
        }
        shown.insert(m.id);
        row(conn, m.id, &m.name, status, ms, &show)
    };

    for id in db_boards_items(conn, board.id)? {
        let Some(m) = db_monitors::get_monitor(conn, id)? else {
            continue;
        };
        if m.monitor_type == MonitorType::Group {
            let mut found = Vec::new();
            leaves(conn, &m, &mut HashSet::new(), &mut found);
            let mut rows = Vec::new();
            if found.is_empty() {
                rows.push(line(&m, &mut statuses)?);
            } else {
                for leaf in &found {
                    rows.push(line(leaf, &mut statuses)?);
                }
            }
            sections.push(Section {
                name: Some(m.name.clone()),
                rows,
            });
        } else {
            let r = line(&m, &mut statuses)?;
            match sections.last_mut() {
                Some(last) if last.name.is_none() => last.rows.push(r),
                _ => sections.push(Section {
                    name: None,
                    rows: vec![r],
                }),
            }
        }
    }

    // Active maintenance that covers what the board shows.
    let mut notices = Vec::new();
    let mut maintenance_shown = false;
    if !shown.is_empty() {
        let tz = ps::site_tz(&crate::db::settings::load(conn)?.timezone);
        for w in db_maint::list_enabled(conn)? {
            let sched = Schedule::from_window(&w);
            if maintenance::window_state(&sched, now) != WindowState::Active {
                continue;
            }
            let mut ids: HashSet<i64> = HashSet::new();
            for id in db_maint::monitor_ids(conn, w.id)? {
                ids.insert(id);
                ids.extend(db_monitors::descendant_ids(conn, id)?);
            }
            if ids.is_disjoint(&shown) {
                continue;
            }
            maintenance_shown = true;
            if board.show_incidents {
                let meta = match maintenance::next_occurrence(&sched, now) {
                    Some((_, end)) => {
                        crate::i18n::tf("pub.maint_until", &[("when", &ps::fmt_ts(end, &tz))])
                    }
                    None => crate::i18n::t("mw.state_active").to_string(),
                };
                notices.push(Notice {
                    class: "maintenance".into(),
                    title: w.title,
                    body: w.description,
                    meta,
                });
            }
        }
    }

    let overall = ps::overall(&statuses, maintenance_shown);
    Ok(Loaded {
        view: BoardView {
            title: if board.title.trim().is_empty() {
                board.name.clone()
            } else {
                board.title.clone()
            },
            description: board.description.clone(),
            overall_class: overall.class.to_string(),
            overall_title: overall.title.to_string(),
            overall_detail: overall.detail,
            sections,
            notices,
            updated_at: rfc3339(now),
            footer: crate::product::NAME.to_string(),
            more_label: crate::i18n::t("live.more").to_string(),
            colour: board.color.clone(),
        },
        down,
    })
}

fn db_boards_items(conn: &Connection, id: i64) -> rusqlite::Result<Vec<i64>> {
    crate::db::live_boards::items(conn, id)
}

/// The monitors and groups that exist, for a board's picker: `(id, name, is_group)`.
pub fn pickable(conn: &Connection) -> rusqlite::Result<Vec<(i64, String, bool)>> {
    Ok(db_monitors::list_monitors(conn)?
        .into_iter()
        .map(|m| (m.id, m.name, m.monitor_type == MonitorType::Group))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::live_boards::{self as boards, NewBoard};
    use rusqlite::params;

    pub(crate) fn new_board(name: &str, source: &str) -> NewBoard {
        NewBoard {
            name: name.into(),
            webhook_url: "x".into(),
            platform: "discord".into(),
            source: source.into(),
            status_page_id: None,
            show_uptime_24h: true,
            show_uptime_30d: true,
            show_response: true,
            show_incidents: true,
            title: String::new(),
            description: String::new(),
            color: String::new(),
            refresh_seconds: 300,
            notify_down: false,
            mention: String::new(),
            lang: "en".into(),
            items: Vec::new(),
        }
    }

    pub(crate) fn add_monitor(
        conn: &Connection,
        id: i64,
        name: &str,
        kind: &str,
        parent: Option<i64>,
        status: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO monitors (id, name, monitor_type, target, parent_id) VALUES (?1, ?2, ?3, 'https://secret-host.internal/path?token=abc', ?4)",
            params![id, name, kind, parent],
        )
        .unwrap();
        if let Some(s) = status {
            conn.execute(
                "INSERT INTO heartbeats (monitor_id, status, response_time_ms, message) VALUES (?1, ?2, 120, 'connect to secret-host.internal failed')",
                params![id, s],
            )
            .unwrap();
        }
    }

    fn all_text(v: &BoardView) -> String {
        let mut out = format!(
            "{} {} {} {}",
            v.title, v.description, v.overall_title, v.overall_detail
        );
        for s in &v.sections {
            out.push_str(s.name.as_deref().unwrap_or_default());
            for r in &s.rows {
                out.push_str(&format!(" {} {} {}", r.name, r.status, r.details));
            }
        }
        for n in &v.notices {
            out.push_str(&format!(" {} {} {}", n.title, n.body, n.meta));
        }
        out
    }

    #[test]
    fn a_monitor_list_shows_picked_monitors_and_group_children_without_targets() {
        let conn = crate::db::test_conn();
        add_monitor(&conn, 1, "Website", "http", None, Some("up"));
        add_monitor(&conn, 2, "Backend", "group", None, None);
        add_monitor(&conn, 3, "API", "http", Some(2), Some("down"));
        add_monitor(&conn, 4, "Queue", "http", Some(2), Some("up"));
        add_monitor(&conn, 5, "Hidden", "http", None, Some("up"));
        let mut nb = new_board("A", "items");
        nb.items = vec![2, 1];
        let id = boards::insert(&conn, &nb).unwrap();
        let board = boards::get(&conn, id).unwrap().unwrap();
        let l = load(&conn, &board, Timestamp::now()).unwrap();
        let names: Vec<(Option<&str>, Vec<&str>)> = l
            .view
            .sections
            .iter()
            .map(|s| {
                (
                    s.name.as_deref(),
                    s.rows.iter().map(|r| r.name.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            names,
            vec![
                (Some("Backend"), vec!["API", "Queue"]),
                (None, vec!["Website"]),
            ]
        );
        assert_eq!(l.down, vec![(3, "API".to_string())]);
        assert_eq!(l.view.overall_class, "warning");
        let text = all_text(&l.view);
        for secret in ["secret-host", "token=abc", "https://", "Hidden"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        // The figures follow the options.
        let api = &l.view.sections[0].rows[0];
        assert!(
            api.details.contains("(24 h)") && api.details.contains("(30 d)"),
            "{}",
            api.details
        );
    }

    #[test]
    fn a_status_page_board_shows_only_what_the_page_shows() {
        let mut conn = crate::db::test_conn();
        add_monitor(&conn, 1, "Shown", "http", None, Some("up"));
        add_monitor(&conn, 2, "Not on the page", "http", None, Some("down"));
        conn.execute(
            "INSERT INTO status_pages (id, slug, title, published, show_uptime, show_response) VALUES (1, 'sylo', 'Sylo status', 1, 0, 0)",
            [],
        )
        .unwrap();
        let section = db_pages::add_section(&conn, 1, "Core").unwrap();
        db_pages::add_monitors(&mut conn, 1, section, &[1]).unwrap();
        let mut nb = new_board("Sylo", "page");
        nb.status_page_id = Some(1);
        let id = boards::insert(&conn, &nb).unwrap();
        let board = boards::get(&conn, id).unwrap().unwrap();
        let l = load(&conn, &board, Timestamp::now()).unwrap();
        assert_eq!(l.view.title, "Sylo status");
        assert_eq!(l.view.sections.len(), 1);
        assert_eq!(l.view.sections[0].name.as_deref(), Some("Core"));
        assert_eq!(l.view.sections[0].rows.len(), 1);
        // The page hides uptime and response time, so the board does too.
        assert_eq!(l.view.sections[0].rows[0].details, "");
        assert!(l.down.is_empty());
        assert_eq!(l.view.overall_class, "up");
        let text = all_text(&l.view);
        assert!(
            !text.contains("Not on the page") && !text.contains("secret-host"),
            "{text}"
        );
    }

    #[test]
    fn drafts_password_pages_and_deleted_pages_are_not_shown() {
        let conn = crate::db::test_conn();
        conn.execute(
            "INSERT INTO status_pages (id, slug, title, published) VALUES (1, 'draft', 'Draft', 0)",
            [],
        )
        .unwrap();
        let mut nb = new_board("A", "page");
        nb.status_page_id = Some(1);
        let id = boards::insert(&conn, &nb).unwrap();
        let board = boards::get(&conn, id).unwrap().unwrap();
        assert!(matches!(
            load(&conn, &board, Timestamp::now()),
            Err(LoadError::PageNotPublic)
        ));
        conn.execute(
            "UPDATE status_pages SET published = 1, password_hash = 'x' WHERE id = 1",
            [],
        )
        .unwrap();
        assert!(matches!(
            load(&conn, &board, Timestamp::now()),
            Err(LoadError::PageNotPublic)
        ));
        conn.execute("DELETE FROM status_pages WHERE id = 1", [])
            .unwrap();
        let board = boards::get(&conn, id).unwrap().unwrap();
        assert!(matches!(
            load(&conn, &board, Timestamp::now()),
            Err(LoadError::PageGone)
        ));
    }
}
