//! Public status pages. Owned by the statuspages agent.
//!
//! Routes: `/status/{slug}` (full page), `/status/{slug}/fragment` (htmx
//! auto-refresh of the live region), `/api/status-page/{slug}` (JSON summary
//! for integrations), plus custom-domain serving via [`custom_domain`].
//!
//! Nothing here exposes monitor targets, URLs or check messages: only names,
//! statuses, uptime and (optionally) response times.
//!
//! Password protection is in [`access`], logo/favicon uploads in [`assets`]
//! and analytics (built-in counters, third-party providers) in [`analytics`].

pub mod access;
pub mod analytics;
pub mod assets;
#[cfg(test)]
mod extras_tests;
pub mod feed;
#[cfg(test)]
mod incident_updates_tests;
pub mod subscribe;
#[cfg(test)]
mod subscribe_tests;

use crate::i18n::Tr as _;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use askama::Template;
use axum::Router;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum_extra::extract::cookie::SignedCookieJar;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use rusqlite::{Connection, params};
use serde::Serialize;

use super::{ApiError, ApiResult, AppState, HtmlTemplate};
use crate::db::incidents::{self as db_incidents, Incident};
use crate::db::models::HeartbeatStatus;
use crate::db::rollups::{self, DayStat};
use crate::db::settings as db_settings;
use crate::db::status_pages::{self as db_pages, StatusPage};
use crate::db::{maintenance as db_maint, status_pages};
use crate::maintenance::{self, Schedule, WindowState};

/// Days shown in the uptime bar.
pub const UPTIME_DAYS: i64 = 90;
/// Days of history under "Past incidents".
pub const HISTORY_DAYS: i64 = 14;
/// Scheduled maintenance shows up this many days ahead.
const UPCOMING_DAYS: i64 = 14;

/// Unauthenticated routes.
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/status/{slug}", get(page))
        .route("/status/{slug}/fragment", get(fragment))
        .route("/status/{slug}/feed.xml", get(feed::feed))
        .route("/status/{slug}/unlock", axum::routing::post(access::unlock))
        .route(
            "/status/{slug}/subscribe",
            get(subscribe::form).post(subscribe::sign_up),
        )
        .route(
            "/status/{slug}/subscribe/confirm/{token}",
            get(subscribe::confirm_page).post(subscribe::confirm),
        )
        .route(
            "/status/{slug}/subscribe/unsubscribe/{token}",
            get(subscribe::unsubscribe_page).post(subscribe::unsubscribe),
        )
        .route("/status/{slug}/analytics.js", get(analytics::analytics_js))
        .route("/api/status-page/{slug}", get(json_summary))
        .merge(assets::public_routes())
}

// ---------------------------------------------------------------------------
// View model
// ---------------------------------------------------------------------------

/// A monitor's status as shown publicly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicStatus {
    Up,
    Down,
    Degraded,
    Maintenance,
    Paused,
    Unknown,
}

impl PublicStatus {
    pub fn from_heartbeat(
        status: Option<HeartbeatStatus>,
        active: bool,
        in_maintenance: bool,
    ) -> Self {
        if in_maintenance {
            return PublicStatus::Maintenance;
        }
        if !active {
            return PublicStatus::Paused;
        }
        match status {
            Some(HeartbeatStatus::Up) => PublicStatus::Up,
            Some(HeartbeatStatus::Down) => PublicStatus::Down,
            Some(HeartbeatStatus::Pending) => PublicStatus::Degraded,
            Some(HeartbeatStatus::Maintenance) => PublicStatus::Maintenance,
            None => PublicStatus::Unknown,
        }
    }

    /// Design-system status class suffix.
    pub fn class(self) -> &'static str {
        match self {
            PublicStatus::Up => "up",
            PublicStatus::Down => "down",
            PublicStatus::Degraded => "pending",
            PublicStatus::Maintenance => "maintenance",
            PublicStatus::Paused => "paused",
            PublicStatus::Unknown => "unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PublicStatus::Up => crate::i18n::t("pub.operational"),
            PublicStatus::Down => crate::i18n::t("status.down"),
            PublicStatus::Degraded => crate::i18n::t("pub.degraded"),
            PublicStatus::Maintenance => crate::i18n::t("status.maintenance"),
            PublicStatus::Paused => crate::i18n::t("status.paused"),
            PublicStatus::Unknown => crate::i18n::t("detail.no_data"),
        }
    }
}

/// The big banner at the top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overall {
    /// "operational" | "degraded" | "partial_outage" | "major_outage" | "maintenance" | "unknown".
    pub indicator: &'static str,
    #[serde(skip)]
    pub class: &'static str,
    #[serde(skip)]
    pub icon: &'static str,
    pub title: &'static str,
    pub detail: String,
}

fn services(n: usize) -> String {
    crate::i18n::tn("pub.services", n as i64)
}

/// Overall status from the monitors' public statuses. Paused and no-data
/// monitors don't count. `maintenance_shown`: a maintenance window linked to
/// the page is in progress.
pub fn overall(statuses: &[PublicStatus], maintenance_shown: bool) -> Overall {
    let counted: Vec<PublicStatus> = statuses
        .iter()
        .copied()
        .filter(|s| !matches!(s, PublicStatus::Paused | PublicStatus::Unknown))
        .collect();
    let total = counted.len();
    let count = |want: PublicStatus| counted.iter().filter(|s| **s == want).count();
    let (down, degraded, maint) = (
        count(PublicStatus::Down),
        count(PublicStatus::Degraded),
        count(PublicStatus::Maintenance),
    );

    if total == 0 {
        if maintenance_shown {
            return Overall {
                indicator: "maintenance",
                class: "maintenance",
                icon: "wrench",
                title: crate::i18n::t("pub.under_maintenance"),
                detail: crate::i18n::t("pub.maint_in_progress").into(),
            };
        }
        return Overall {
            indicator: "unknown",
            class: "unknown",
            icon: "circle-dashed",
            title: crate::i18n::t("pub.unavailable"),
            detail: crate::i18n::t("pub.nothing_to_report").into(),
        };
    }
    if down > 0 && down + maint == total {
        return Overall {
            indicator: "major_outage",
            class: "down",
            icon: "x",
            title: crate::i18n::t("pub.major_outage"),
            detail: if down == 1 {
                crate::i18n::t("pub.only_service_down").into()
            } else {
                crate::i18n::tf("pub.all_down", &[("n", &down.to_string())])
            },
        };
    }
    if down > 0 {
        return Overall {
            indicator: "partial_outage",
            class: "warning",
            icon: "alert",
            title: crate::i18n::t("pub.partial_outage"),
            detail: crate::i18n::tnf(
                "pub.some_down",
                down as i64,
                &[("total", &crate::i18n::fmt_int(total as i64))],
            ),
        };
    }
    if degraded > 0 {
        return Overall {
            indicator: "degraded",
            class: "warning",
            icon: "alert",
            title: crate::i18n::t("pub.degraded_performance"),
            detail: crate::i18n::tn("pub.having_trouble", degraded as i64),
        };
    }
    if maint > 0 || maintenance_shown {
        return Overall {
            indicator: "maintenance",
            class: "maintenance",
            icon: "wrench",
            title: crate::i18n::t("pub.under_maintenance"),
            detail: if maint > 0 {
                crate::i18n::tf("pub.maint_on", &[("services", &services(maint))])
            } else {
                crate::i18n::t("pub.maint_in_progress").into()
            },
        };
    }
    Overall {
        indicator: "operational",
        class: "up",
        icon: "check",
        title: crate::i18n::t("pub.all_operational"),
        detail: if total == 1 {
            crate::i18n::t("pub.running_normally_one").into()
        } else {
            crate::i18n::tf("pub.running_normally", &[("n", &total.to_string())])
        },
    }
}

/// One bar of the 90-day strip.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayView {
    pub day: String,
    #[serde(skip)]
    pub class: &'static str,
    #[serde(skip)]
    pub tip: String,
    pub uptime: Option<f64>,
    pub outages: usize,
    pub outage_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TagView {
    pub name: String,
    #[serde(skip)]
    pub color: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonitorView {
    pub id: i64,
    pub name: String,
    pub status: PublicStatus,
    pub tags: Vec<TagView>,
    pub uptime: Option<f64>,
    pub response_ms: Option<i64>,
    pub days: Vec<DayView>,
}

impl MonitorView {
    /// "123 ms" / "< 1 ms" for the public row.
    pub fn response_label(&self) -> Option<String> {
        self.response_ms.map(|ms| {
            if ms < 1 {
                "< 1 ms".into()
            } else {
                crate::i18n::fmt_ms(ms)
            }
        })
    }

    pub fn uptime_label(&self) -> String {
        self.uptime.map_or_else(
            || crate::i18n::t("detail.no_data").into(),
            |p| crate::i18n::tf("detail.day_uptime", &[("value", &fmt_pct(p))]),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SectionView {
    pub name: String,
    pub monitors: Vec<MonitorView>,
}

/// A pinned announcement, maintenance notice, or past incident card.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoticeView {
    pub title: String,
    /// Status class suffix ("info", "warning", "danger", "success", "maintenance").
    pub class: String,
    #[serde(skip)]
    pub icon: &'static str,
    pub meta: String,
    #[serde(skip)]
    pub body_html: String,
    pub body: String,
    /// Incident timeline, newest first (empty for maintenance notices).
    pub updates: Vec<UpdateView>,
}

/// One entry of an incident's timeline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateView {
    #[serde(skip)]
    pub id: i64,
    /// "investigating" | "identified" | "monitoring" | "resolved".
    pub status: String,
    #[serde(skip)]
    pub label: &'static str,
    pub posted_at: String,
    #[serde(skip)]
    pub time: String,
    #[serde(skip)]
    pub body_html: String,
    pub body: String,
}

/// An automatic outage (from `monitor_events`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutageView {
    pub monitor: String,
    pub ongoing: bool,
    pub started_at: String,
    #[serde(skip)]
    pub time: String,
    #[serde(skip)]
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryDay {
    pub label: String,
    pub incidents: Vec<NoticeView>,
    pub outages: Vec<OutageView>,
}

/// Everything the public page (and its JSON twin) shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PublicView {
    #[serde(skip)]
    pub page: StatusPage,
    pub overall: Overall,
    pub pinned: Vec<NoticeView>,
    pub maintenance: Vec<NoticeView>,
    pub sections: Vec<SectionView>,
    pub history: Vec<HistoryDay>,
    /// UTC `YYYY-MM-DD HH:MM:SS`.
    pub updated_at: String,
    #[serde(skip)]
    pub updated_label: String,
    #[serde(skip)]
    pub footer_html: String,
    /// The page lets visitors subscribe to announcements.
    #[serde(skip)]
    pub subscribe: bool,
    /// Logged-in editor viewing an unpublished page.
    #[serde(skip)]
    pub preview: bool,
    /// Logged-in editor (shows an "Edit page" link).
    #[serde(skip)]
    pub can_edit: bool,
    /// Path of the htmx refresh endpoint.
    #[serde(skip)]
    pub fragment_url: String,
}

impl PublicView {
    pub fn has_monitors(&self) -> bool {
        self.sections.iter().any(|s| !s.monitors.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

pub fn fmt_pct(p: f64) -> String {
    // Never rounds 99.996 up to a misleading "100.00%".
    crate::i18n::fmt_pct2(p)
}

/// "45 sec", "14 min", "2 h 5 min", "3 d 4 h" (localized).
pub fn fmt_duration(secs: i64) -> String {
    crate::i18n::fmt_duration(secs)
}

/// Site time zone from settings (UTC when unset or unknown).
pub fn site_tz(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap_or(TimeZone::UTC)
}

/// UTC DB text → "Oct 18, 14:03 CEST" in `tz` (input echoed back if unparseable).
pub fn fmt_local(utc: &str, tz: &TimeZone) -> String {
    match maintenance::parse_db_ts(utc) {
        Some(ts) => fmt_ts(ts, tz),
        None => utc.to_string(),
    }
}

pub fn fmt_ts(ts: Timestamp, tz: &TimeZone) -> String {
    let z = ts.to_zoned(tz.clone());
    format!(
        "{} {}",
        crate::i18n::fmt_short_datetime(&z),
        z.strftime("%Z")
    )
}

/// "2026-10-03" → "Oct 3" / "3. okt.".
pub fn fmt_day(day: &str) -> String {
    day.parse::<jiff::civil::Date>()
        .map(crate::i18n::fmt_day_month)
        .unwrap_or_else(|_| day.to_string())
}

fn day_class(d: &DayStat) -> &'static str {
    match d.uptime_pct {
        None if d.maintenance > 0 => "status-maintenance",
        None => "status-none",
        Some(_) if d.down == 0 => "status-up",
        Some(p) if p >= 90.0 => "status-pending",
        Some(_) => "status-down",
    }
}

fn day_tip(d: &DayStat, outages: usize, outage_secs: i64) -> String {
    let mut tip = fmt_day(&d.day);
    match d.uptime_pct {
        Some(p) => {
            tip.push_str(" · ");
            tip.push_str(&crate::i18n::tf(
                "detail.day_uptime",
                &[("value", &fmt_pct(p))],
            ));
        }
        None if d.maintenance > 0 => {
            tip.push_str(" · ");
            tip.push_str(crate::i18n::t("status.maintenance"));
        }
        None => {
            tip.push_str(" · ");
            tip.push_str(crate::i18n::t("detail.no_data"));
        }
    }
    if outages > 0 {
        tip.push_str(&format!(
            " · {}, {}",
            crate::i18n::tn("count.outages", outages as i64),
            fmt_duration(outage_secs)
        ));
    } else if d.uptime_pct.is_some() {
        tip.push_str(" · ");
        tip.push_str(crate::i18n::t("pub.no_incidents_day"));
    }
    tip
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

struct DownEvent {
    monitor_id: i64,
    monitor_name: String,
    started_at: String,
    ended_at: Option<String>,
    duration: Option<i64>,
}

fn down_events_since(
    conn: &Connection,
    page_id: i64,
    since: &str,
) -> rusqlite::Result<Vec<DownEvent>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT e.id, e.monitor_id, m.name, e.started_at, e.ended_at, e.duration_seconds
         FROM monitor_events e
         JOIN monitors m ON m.id = e.monitor_id
         JOIN status_page_monitors spm ON spm.monitor_id = e.monitor_id
         JOIN status_page_sections s ON s.id = spm.section_id
         WHERE s.status_page_id = ?1 AND e.kind = 'down' AND (e.started_at >= ?2 OR e.ended_at IS NULL OR e.ended_at >= ?2)
         ORDER BY e.started_at DESC",
    )?;
    let rows = stmt.query_map(params![page_id, since], |r| {
        Ok(DownEvent {
            monitor_id: r.get(1)?,
            monitor_name: r.get(2)?,
            started_at: r.get(3)?,
            ended_at: r.get(4)?,
            duration: r.get(5)?,
        })
    })?;
    rows.collect()
}

fn event_seconds(e: &DownEvent, now: Timestamp) -> i64 {
    if let Some(d) = e.duration {
        return d;
    }
    let start = maintenance::parse_db_ts(&e.started_at);
    let end = e
        .ended_at
        .as_deref()
        .and_then(maintenance::parse_db_ts)
        .unwrap_or(now);
    start.map_or(0, |s| end.duration_since(s).as_secs().max(0))
}

fn page_tags(conn: &Connection, page_id: i64) -> rusqlite::Result<HashMap<i64, Vec<TagView>>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT mt.monitor_id, t.name, t.color, COALESCE(mt.value, '')
         FROM monitor_tags mt
         JOIN tags t ON t.id = mt.tag_id
         JOIN status_page_monitors spm ON spm.monitor_id = mt.monitor_id
         JOIN status_page_sections s ON s.id = spm.section_id
         WHERE s.status_page_id = ?1
         ORDER BY t.name COLLATE NOCASE",
    )?;
    let mut map: HashMap<i64, Vec<TagView>> = HashMap::new();
    let rows = stmt.query_map(params![page_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            TagView {
                name: r.get(1)?,
                color: r.get(2)?,
                value: r.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (id, tag) = row?;
        map.entry(id).or_default().push(tag);
    }
    Ok(map)
}

fn latest_local(
    conn: &Connection,
    monitor_id: i64,
) -> rusqlite::Result<(Option<HeartbeatStatus>, Option<i64>)> {
    let mut stmt = conn.prepare_cached(
        "SELECT status, response_time_ms FROM heartbeats WHERE monitor_id = ?1 AND agent_id IS NULL
         ORDER BY checked_at DESC, id DESC LIMIT 1",
    )?;
    let mut rows = stmt.query(params![monitor_id])?;
    match rows.next()? {
        Some(r) => {
            let s: String = r.get(0)?;
            Ok((HeartbeatStatus::parse(&s), r.get(1)?))
        }
        None => Ok((None, None)),
    }
}

fn incident_notice(
    i: &Incident,
    tz: &TimeZone,
    updates: &[db_incidents::IncidentUpdate],
) -> NoticeView {
    let mut meta = crate::i18n::tf("inc.posted", &[("when", &fmt_local(&i.created_at, tz))]);
    if let Some(r) = &i.resolved_at {
        meta = crate::i18n::tf("inc.resolved_at", &[("when", &fmt_local(r, tz))]);
    } else if i.updated_at != i.created_at {
        meta.push_str(" · ");
        meta.push_str(&crate::i18n::tf(
            "pub.updated_at",
            &[("when", &fmt_local(&i.updated_at, tz))],
        ));
    }
    NoticeView {
        title: i.title.clone(),
        class: i.severity.clone(),
        icon: i.icon(),
        meta,
        body_html: String::new(),
        body: i.body.clone(),
        updates: updates
            .iter()
            .map(|u| UpdateView {
                id: u.id,
                status: u.status.clone(),
                label: u.status_label(),
                posted_at: u.created_at.clone(),
                time: fmt_local(&u.created_at, tz),
                body_html: String::new(),
                body: u.body.clone(),
            })
            .collect(),
    }
}

/// Builds the whole public view for `page` at `now`, Markdown included.
pub fn load_view(
    conn: &Connection,
    page: StatusPage,
    now: Timestamp,
) -> rusqlite::Result<PublicView> {
    load_view_data(conn, page, now).map(render_markdown)
}

/// Fills in every `*_html` field. Runs outside `with_conn` so a slow render
/// never holds a pooled database connection.
fn render_markdown(mut view: PublicView) -> PublicView {
    let notices = view
        .pinned
        .iter_mut()
        .chain(view.maintenance.iter_mut())
        .chain(view.history.iter_mut().flat_map(|d| d.incidents.iter_mut()));
    for n in notices {
        n.body_html = markdown::render(&n.body);
        for u in &mut n.updates {
            u.body_html = markdown::render(&u.body);
        }
    }
    view.footer_html = view
        .page
        .footer_text
        .as_deref()
        .map(markdown::render)
        .unwrap_or_default();
    view
}

/// The database half of [`load_view`]: everything except rendered Markdown.
fn load_view_data(
    conn: &Connection,
    page: StatusPage,
    now: Timestamp,
) -> rusqlite::Result<PublicView> {
    let settings = db_settings::load(conn)?;
    let tz = site_tz(&settings.timezone);
    let covered = maintenance::covered_monitors(conn)?;
    let sections = status_pages::sections(conn, page.id)?;
    let tags = if page.show_tags {
        page_tags(conn, page.id)?
    } else {
        HashMap::new()
    };

    // Outages per (monitor, UTC day) over the uptime window.
    let since_90 = maintenance::fmt_db_ts(now - jiff::SignedDuration::from_hours(24 * UPTIME_DAYS));
    let events = down_events_since(conn, page.id, &since_90)?;
    let mut per_day: HashMap<(i64, String), (usize, i64)> = HashMap::new();
    for e in &events {
        let day = e.started_at.get(..10).unwrap_or_default().to_string();
        let entry = per_day.entry((e.monitor_id, day)).or_default();
        entry.0 += 1;
        entry.1 += event_seconds(e, now);
    }

    let mut statuses = Vec::new();
    let mut section_views = Vec::new();
    let mut seen: HashMap<i64, MonitorView> = HashMap::new();
    for s in sections {
        let mut monitors = Vec::new();
        for m in s.monitors {
            if let Some(v) = seen.get(&m.monitor_id) {
                monitors.push(v.clone());
                continue;
            }
            let (hb, ms) = latest_local(conn, m.monitor_id)?;
            let status =
                PublicStatus::from_heartbeat(hb, m.active, covered.contains(&m.monitor_id));
            statuses.push(status);
            let days = if page.show_uptime {
                rollups::daily_uptime(conn, m.monitor_id, UPTIME_DAYS)?
                    .into_iter()
                    .map(|d| {
                        let (n, secs) = per_day
                            .get(&(m.monitor_id, d.day.clone()))
                            .copied()
                            .unwrap_or_default();
                        DayView {
                            class: day_class(&d),
                            tip: day_tip(&d, n, secs),
                            uptime: d.uptime_pct,
                            outages: n,
                            outage_seconds: secs,
                            day: d.day,
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let uptime = if page.show_uptime {
                rollups::uptime_pct(conn, m.monitor_id, UPTIME_DAYS)?
            } else {
                None
            };
            let view = MonitorView {
                id: m.monitor_id,
                name: m.name,
                status,
                tags: tags.get(&m.monitor_id).cloned().unwrap_or_default(),
                uptime,
                response_ms: if page.show_response && status == PublicStatus::Up {
                    ms
                } else {
                    None
                },
                days,
            };
            seen.insert(view.id, view.clone());
            monitors.push(view);
        }
        section_views.push(SectionView {
            name: s.name,
            monitors,
        });
    }

    // Maintenance linked to this page.
    let mut maint_views = Vec::new();
    let mut maintenance_shown = false;
    let horizon = now + jiff::SignedDuration::from_hours(24 * UPCOMING_DAYS);
    for w in db_maint::list_for_status_page(conn, page.id)? {
        let sched = Schedule::from_window(&w);
        let state = maintenance::window_state(&sched, now);
        let meta = match (state, maintenance::next_occurrence(&sched, now)) {
            (WindowState::Active, Some((_, end))) => {
                crate::i18n::tf("pub.maint_until", &[("when", &fmt_ts(end, &tz))])
            }
            (WindowState::Active, None) => crate::i18n::t("mw.state_active").to_string(),
            (WindowState::Scheduled, Some((start, end))) if start <= horizon => crate::i18n::tf(
                "pub.maint_scheduled",
                &[("start", &fmt_ts(start, &tz)), ("end", &fmt_ts(end, &tz))],
            ),
            _ => continue,
        };
        if state == WindowState::Active {
            maintenance_shown = true;
        }
        maint_views.push(NoticeView {
            title: w.title,
            class: if state == WindowState::Active {
                "maintenance"
            } else {
                "info"
            }
            .into(),
            icon: if state == WindowState::Active {
                "wrench"
            } else {
                "calendar"
            },
            meta,
            body_html: String::new(),
            body: w.description,
            updates: Vec::new(),
        });
    }

    let pinned_open = db_incidents::list_pinned_open(conn, page.id)?;
    let history_incidents = db_incidents::list_history(
        conn,
        page.id,
        &maintenance::fmt_db_ts(now - jiff::SignedDuration::from_hours(24 * HISTORY_DAYS)),
    )?;
    let update_ids: Vec<i64> = pinned_open
        .iter()
        .chain(&history_incidents)
        .map(|i| i.id)
        .collect();
    let timelines = db_incidents::updates_for(conn, &update_ids)?;
    let updates_of = |i: &Incident| {
        timelines
            .get(&i.id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .to_vec()
    };
    let pinned = pinned_open
        .iter()
        .map(|i| incident_notice(i, &tz, &updates_of(i)))
        .collect();

    // History: past announcements + automatic outages, grouped by local day.
    let since_14 =
        maintenance::fmt_db_ts(now - jiff::SignedDuration::from_hours(24 * HISTORY_DAYS));
    let mut buckets: Vec<(jiff::civil::Date, HistoryDay)> = Vec::new();
    let mut bucket = |ts_text: &str| -> Option<usize> {
        let ts = maintenance::parse_db_ts(ts_text)?;
        let date = ts.to_zoned(tz.clone()).date();
        if let Some(i) = buckets.iter().position(|(d, _)| *d == date) {
            return Some(i);
        }
        buckets.push((
            date,
            HistoryDay {
                label: crate::i18n::fmt_day_month_long(date),
                incidents: Vec::new(),
                outages: Vec::new(),
            },
        ));
        Some(buckets.len() - 1)
    };
    let mut history_items: Vec<(usize, Result<NoticeView, OutageView>)> = Vec::new();
    for i in history_incidents {
        if let Some(b) = bucket(&i.created_at) {
            history_items.push((b, Ok(incident_notice(&i, &tz, &updates_of(&i)))));
        }
    }
    for e in events
        .iter()
        .filter(|e| e.started_at.as_str() >= since_14.as_str())
    {
        let secs = event_seconds(e, now);
        let ongoing = e.ended_at.is_none();
        let start_local = maintenance::parse_db_ts(&e.started_at)
            .map(|t| {
                crate::i18n::local_digits(
                    crate::i18n::current(),
                    t.to_zoned(tz.clone()).strftime("%H:%M").to_string(),
                )
            })
            .unwrap_or_default();
        let text = if ongoing {
            crate::i18n::tf(
                "pub.down_since",
                &[("time", &start_local), ("duration", &fmt_duration(secs))],
            )
        } else {
            crate::i18n::tf("pub.down_for", &[("duration", &fmt_duration(secs))])
        };
        if let Some(b) = bucket(&e.started_at) {
            history_items.push((
                b,
                Err(OutageView {
                    monitor: e.monitor_name.clone(),
                    ongoing,
                    started_at: e.started_at.clone(),
                    time: start_local,
                    text,
                }),
            ));
        }
    }
    for (b, item) in history_items {
        match item {
            Ok(n) => buckets[b].1.incidents.push(n),
            Err(o) => buckets[b].1.outages.push(o),
        }
    }
    buckets.sort_by_key(|(date, _)| std::cmp::Reverse(*date));
    let history = buckets.into_iter().map(|(_, h)| h).collect();

    Ok(PublicView {
        overall: overall(&statuses, maintenance_shown),
        pinned,
        maintenance: maint_views,
        sections: section_views,
        history,
        updated_at: maintenance::fmt_db_ts(now),
        updated_label: fmt_ts(now, &tz),
        footer_html: String::new(),
        subscribe: crate::db::subscribers::settings(conn, page.id).is_ok_and(|s| s.enabled),
        preview: !page.published,
        can_edit: false,
        fragment_url: format!("/status/{}/fragment", page.slug),
        page,
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "public_status.html")]
struct PublicPageTemplate {
    page_title: String,
    theme: String,
    custom_css: Option<String>,
    meta_description: String,
    v: PublicView,
    /// Drafts and password-protected pages stay out of search engines.
    noindex: bool,
    /// Third-party analytics script (published pages only).
    analytics: Option<analytics::Snippet>,
}

#[derive(Template)]
#[template(path = "partials/status_public_live.html")]
struct LiveFragment {
    v: PublicView,
}

/// Resolves a page for a public request: published pages for everyone,
/// unpublished ones only for logged-in editors (as a preview). The access
/// says whether a password-protected page's content may be shown.
async fn resolve(
    state: &AppState,
    slug: String,
    jar: &SignedCookieJar,
) -> ApiResult<(PublicView, access::Access)> {
    let user = super::auth::session_user(state, jar).await.unwrap_or(None);
    let can_edit = user.as_ref().is_some_and(|u| u.can_edit());
    let view = cached_view(state, slug)
        .await?
        .filter(|v| v.page.published || can_edit)
        .ok_or(ApiError::NotFound)?;
    let access = access::access_for(jar, &view.page, user.is_some());
    Ok((PublicView { can_edit, ..view }, access))
}

// ---------------------------------------------------------------------------
// View cache
// ---------------------------------------------------------------------------

/// Public pages are unauthenticated and auto-refresh, so each (page,
/// language) is built at most once per this interval.
const VIEW_TTL: Duration = Duration::from_secs(5);
/// More distinct pages than this and the cache starts over.
const VIEW_CACHE_MAX: usize = 256;

/// (data dir, slug, language). The data dir tells app instances apart (tests
/// run several at once) without the address reuse a pointer key would risk.
type ViewKey = (std::path::PathBuf, String, crate::i18n::Lang);

static VIEWS: LazyLock<Mutex<HashMap<ViewKey, (Instant, PublicView)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Forget cached public views (after a page, incident or maintenance change),
/// and rendered badges, whose visibility depends on the pages.
pub fn invalidate_views() {
    if let Ok(mut m) = VIEWS.lock() {
        m.clear();
    }
    super::badges::invalidate_cache();
}

/// The page's view (published or not — callers decide who may see it),
/// from the cache when fresh. Markdown renders outside the connection.
async fn cached_view(state: &AppState, slug: String) -> ApiResult<Option<PublicView>> {
    let key: ViewKey = (state.config.data_dir.clone(), slug, crate::i18n::current());
    if let Ok(cache) = VIEWS.lock()
        && let Some((at, view)) = cache.get(&key)
        && at.elapsed() < VIEW_TTL
    {
        return Ok(Some(view.clone()));
    }
    let slug = key.1.clone();
    let loaded = state
        .with_conn(move |c| match db_pages::get_by_slug(c, &slug)? {
            Some(page) => load_view_data(c, page, Timestamp::now()).map(Some),
            None => Ok(None),
        })
        .await?;
    let Some(view) = loaded else {
        return Ok(None);
    };
    let view = render_markdown(view);
    if let Ok(mut cache) = VIEWS.lock() {
        if cache.len() >= VIEW_CACHE_MAX {
            cache.retain(|_, (at, _)| at.elapsed() < VIEW_TTL);
            if cache.len() >= VIEW_CACHE_MAX {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), view.clone()));
    }
    Ok(Some(view))
}

/// Middleware for routers that edit what public pages show (pages,
/// incidents, maintenance): any successful write drops the cached views.
pub async fn invalidate_after_writes(request: Request, next: Next) -> Response {
    let write = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let response = next.run(request).await;
    if write && !response.status().is_client_error() && !response.status().is_server_error() {
        invalidate_views();
    }
    response
}

/// A published page's view (cached), or `None`.
async fn published_view(state: &AppState, slug: String) -> ApiResult<Option<PublicView>> {
    Ok(cached_view(state, slug).await?.filter(|v| v.page.published))
}

/// Response extension marking a public status page, wherever it's served
/// (custom domains serve it at `/`), so `security_headers` lets it be framed.
#[derive(Debug, Clone, Copy)]
pub struct PublicPage;

fn page_response(view: PublicView) -> Response {
    let page = &view.page;
    let meta_description = if page.description.trim().is_empty() {
        format!("{} — {}", view.overall.title, page.title)
    } else {
        page.description.clone()
    };
    let protected = page.is_protected();
    // Third-party analytics only on the real, published page (not previews).
    let snippet = if view.preview {
        None
    } else {
        analytics::snippet(page)
    };
    let csp = snippet
        .as_ref()
        .and_then(|_| analytics::csp(&page.analytics));
    let tpl = PublicPageTemplate {
        page_title: page.title.clone(),
        theme: page.theme.clone(),
        custom_css: page.custom_css.clone().filter(|c| !c.trim().is_empty()),
        meta_description,
        noindex: view.preview || protected,
        analytics: snippet,
        v: view,
    };
    let noindex = tpl.noindex;
    let mut resp = HtmlTemplate(tpl).into_response();
    resp.extensions_mut().insert(PublicPage);
    let h = resp.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if protected {
            "private, no-store"
        } else {
            "no-cache"
        }),
    );
    if noindex {
        h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    }
    if let Some(policy) = csp.and_then(|p| HeaderValue::from_str(&p).ok()) {
        h.insert(header::CONTENT_SECURITY_POLICY, policy);
    }
    resp
}

async fn page(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    peer: super::auth::PeerAddr,
    Path(slug): Path<String>,
) -> ApiResult<Response> {
    let (view, access) = resolve(&state, slug, &jar).await?;
    if !access.allows(&view.page) {
        return Ok(access::locked_response(
            &view.page,
            None,
            StatusCode::UNAUTHORIZED,
        ));
    }
    analytics::count_view(&state, &view.page, &headers, peer.0, access.logged_in).await;
    Ok(page_response(view))
}

async fn fragment(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(slug): Path<String>,
) -> ApiResult<Response> {
    let (v, access) = resolve(&state, slug, &jar).await?;
    if !access.allows(&v.page) {
        return Ok((
            StatusCode::UNAUTHORIZED,
            [(header::CACHE_CONTROL, "no-store")],
        )
            .into_response());
    }
    let protected = v.page.is_protected();
    let mut resp = HtmlTemplate(LiveFragment { v }).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if protected {
            "private, no-store"
        } else {
            "no-cache"
        }),
    );
    Ok(resp)
}

#[derive(Serialize)]
struct JsonSummary {
    slug: String,
    title: String,
    description: String,
    #[serde(flatten)]
    view: PublicView,
}

/// `GET /api/status-page/{slug}`: public summary for integrations (published
/// pages only). Password-protected pages answer 401 unless the request
/// carries an unlock cookie or a session.
async fn json_summary(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(slug): Path<String>,
) -> ApiResult<Response> {
    let view = published_view(&state, slug)
        .await?
        .ok_or(ApiError::NotFound)?;
    if view.page.is_protected() {
        let logged_in = super::auth::session_user(&state, &jar)
            .await
            .unwrap_or(None)
            .is_some();
        if !access::access_for(&jar, &view.page, logged_in).allows(&view.page) {
            let body = serde_json::json!({ "error": "password_required", "message": "This status page is password protected." });
            return Ok((
                StatusCode::UNAUTHORIZED,
                [(header::CACHE_CONTROL, "no-store")],
                Json(body),
            )
                .into_response());
        }
    }
    let body = JsonSummary {
        slug: view.page.slug.clone(),
        title: view.page.title.clone(),
        description: view.page.description.clone(),
        view,
    };
    let protected = body.view.page.is_protected();
    let mut resp = Json(body).into_response();
    let h = resp.headers_mut();
    if protected {
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
    } else {
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=30"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Custom domains
// ---------------------------------------------------------------------------

const DOMAIN_TTL: Duration = Duration::from_secs(30);

struct DomainCache {
    loaded: Instant,
    /// custom_domain → slug (published pages only).
    map: HashMap<String, String>,
}

/// Keyed by the app instance (the `Config` allocation), so tests with
/// separate databases don't share entries.
static DOMAINS: LazyLock<Mutex<HashMap<usize, DomainCache>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Forget cached custom domains (call after a status page is saved or deleted).
pub fn invalidate_domains() {
    if let Ok(mut m) = DOMAINS.lock() {
        m.clear();
    }
    invalidate_views();
}

fn instance_key(state: &AppState) -> usize {
    Arc::as_ptr(&state.config) as usize
}

/// The request's host name: lowercase, without port or trailing dot.
fn request_host(request: &Request) -> Option<String> {
    let raw = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().host().map(str::to_owned))?;
    let host = if raw.starts_with('[') {
        raw
    } else {
        raw.rsplit_once(':')
            .map_or(raw.clone(), |(h, _)| h.to_string())
    };
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

async fn slug_for_host(state: &AppState, host: &str) -> Option<String> {
    let key = instance_key(state);
    if let Ok(cache) = DOMAINS.lock()
        && let Some(entry) = cache.get(&key)
        && entry.loaded.elapsed() < DOMAIN_TTL
    {
        return entry.map.get(host).cloned();
    }
    let map: HashMap<String, String> =
        match state.with_conn(|c| db_pages::published_domains(c)).await {
            Ok(rows) => rows.into_iter().collect(),
            Err(err) => {
                tracing::warn!(%err, "custom domain lookup failed");
                return None;
            }
        };
    let slug = map.get(host).cloned();
    if let Ok(mut cache) = DOMAINS.lock() {
        cache.insert(
            key,
            DomainCache {
                loaded: Instant::now(),
                map,
            },
        );
    }
    slug
}

/// Middleware wrapping the whole app (outside auth): when the request `Host`
/// matches a published status page's `custom_domain`, `GET /` serves that
/// page instead of the dashboard (and `/favicon.ico` its uploaded favicon).
/// Every other path passes through (static assets, the
/// `/status/{slug}/fragment` refresh, logo, unlock, badges, …).
pub async fn custom_domain(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let get = matches!(*request.method(), Method::GET | Method::HEAD);
    let is_root = get && request.uri().path() == "/";
    let is_favicon = get && request.uri().path() == "/favicon.ico";
    if !is_root && !is_favicon {
        return next.run(request).await;
    }
    let Some(host) = request_host(&request) else {
        return next.run(request).await;
    };
    let Some(slug) = slug_for_host(&state, &host).await else {
        return next.run(request).await;
    };
    if is_favicon {
        return match assets::custom_domain_favicon(&state, slug, request.headers()).await {
            Some(resp) => resp,
            None => next.run(request).await,
        };
    }
    // Public pages follow the visitor's browser language.
    let lang = crate::i18n::negotiate(request.headers()).unwrap_or_else(crate::i18n::current);
    crate::i18n::scope(lang, serve_custom_domain(state, slug, request, next)).await
}

async fn serve_custom_domain(
    state: AppState,
    slug: String,
    request: Request,
    next: Next,
) -> Response {
    match published_view(&state, slug).await {
        Ok(Some(view)) => {
            let jar = SignedCookieJar::from_headers(request.headers(), state.session_key.clone());
            let logged_in = super::auth::session_user(&state, &jar)
                .await
                .unwrap_or(None)
                .is_some();
            let access = access::access_for(&jar, &view.page, logged_in);
            if !access.allows(&view.page) {
                return access::locked_response(&view.page, None, StatusCode::UNAUTHORIZED);
            }
            let peer = request
                .extensions()
                .get::<ConnectInfo<std::net::SocketAddr>>()
                .map(|c| c.0);
            analytics::count_view(&state, &view.page, request.headers(), peer, logged_in).await;
            page_response(view)
        }
        Ok(None) => next.run(request).await,
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Markdown subset
// ---------------------------------------------------------------------------

/// A deliberately tiny, escaping Markdown renderer for incident bodies and
/// footers: paragraphs (single newlines become `<br>`), `**bold**`,
/// `*italic*` / `_italic_`, `` `code` ``, `[text](url)` and bare http(s)
/// links, `-`/`*` bullet lists and `1.` numbered lists. Everything else is
/// escaped text. Links only allow http(s), mailto, `/path` and `#anchor`.
pub mod markdown {
    use crate::api::error::escape_html;

    enum Block {
        Para(Vec<String>),
        List { ordered: bool, items: Vec<String> },
    }

    fn list_item(line: &str) -> Option<(bool, &str)> {
        let t = line.trim_start();
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = t.strip_prefix(marker) {
                return Some((false, rest));
            }
        }
        let digits = t.bytes().take_while(u8::is_ascii_digit).count();
        if (1..=9).contains(&digits) {
            let rest = &t[digits..];
            if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
                return Some((true, rest));
            }
        }
        None
    }

    pub fn render(src: &str) -> String {
        let src = src.replace("\r\n", "\n").replace('\r', "\n");
        let mut blocks: Vec<Block> = Vec::new();
        let mut current: Option<Block> = None;
        for line in src.lines() {
            if line.trim().is_empty() {
                blocks.extend(current.take());
                continue;
            }
            match (list_item(line), &mut current) {
                (Some((ordered, text)), Some(Block::List { ordered: o, items }))
                    if *o == ordered =>
                {
                    items.push(text.to_string());
                }
                (Some((ordered, text)), _) => {
                    blocks.extend(current.take());
                    current = Some(Block::List {
                        ordered,
                        items: vec![text.to_string()],
                    });
                }
                (None, Some(Block::Para(lines))) => lines.push(line.trim().to_string()),
                (None, Some(Block::List { items, .. })) if line.starts_with([' ', '\t']) => {
                    // Indented continuation of the last item.
                    if let Some(last) = items.last_mut() {
                        last.push(' ');
                        last.push_str(line.trim());
                    }
                }
                (None, _) => {
                    blocks.extend(current.take());
                    current = Some(Block::Para(vec![line.trim().to_string()]));
                }
            }
        }
        blocks.extend(current);

        let mut out = String::new();
        for block in blocks {
            match block {
                Block::Para(lines) => {
                    out.push_str("<p>");
                    for (i, l) in lines.iter().enumerate() {
                        if i > 0 {
                            out.push_str("<br>");
                        }
                        inline(&l.chars().collect::<Vec<_>>(), &mut out);
                    }
                    out.push_str("</p>");
                }
                Block::List { ordered, items } => {
                    out.push_str(if ordered { "<ol>" } else { "<ul>" });
                    for item in items {
                        out.push_str("<li>");
                        inline(&item.chars().collect::<Vec<_>>(), &mut out);
                        out.push_str("</li>");
                    }
                    out.push_str(if ordered { "</ol>" } else { "</ul>" });
                }
            }
        }
        out
    }

    /// Whether a link target is safe to put in `href`.
    pub fn safe_url(url: &str) -> bool {
        // Browsers ignore whitespace/control chars inside schemes ("java\tscript:").
        let cleaned: String = url
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_control())
            .collect::<String>()
            .to_ascii_lowercase();
        if cleaned.is_empty() || cleaned.contains(['"', '<', '>', '\\']) {
            return false;
        }
        if cleaned.starts_with("//") {
            return false;
        }
        cleaned.starts_with("http://")
            || cleaned.starts_with("https://")
            || cleaned.starts_with("mailto:")
            || cleaned.starts_with('/')
            || cleaned.starts_with('#')
    }

    fn push_escaped(out: &mut String, s: &[char]) {
        for &c in s {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                _ => out.push(c),
            }
        }
    }

    /// A forward search that remembers its last answer. Within one
    /// `inline_with` call the starting points of each kind of search only
    /// grow, so a remembered "found at j" answers every start up to `j` and a
    /// remembered "not found" answers every later start: each position is
    /// scanned at most once per kind, keeping rendering linear (an unclosed
    /// `[` or `**` repeated thousands of times used to rescan to the end each
    /// time).
    #[derive(Default)]
    struct Finder {
        from: usize,
        result: Option<Option<usize>>,
    }

    impl Finder {
        fn find(
            &mut self,
            start: usize,
            end: usize,
            pred: impl Fn(usize) -> bool,
        ) -> Option<usize> {
            if start >= self.from {
                match self.result {
                    Some(None) => return None,
                    Some(Some(j)) if start <= j => return Some(j),
                    _ => {}
                }
            }
            let found = (start..end).find(|&j| pred(j));
            self.from = start;
            self.result = Some(found);
            found
        }
    }

    /// Longer link targets stay plain text (and are never copied per character).
    const MAX_URL: usize = 1000;

    #[derive(Default)]
    struct Finders {
        backtick: Finder,
        double_star: Finder,
        star: Finder,
        underscore: Finder,
        bracket_paren: Finder,
        close_paren: Finder,
        whitespace: Finder,
    }

    fn link(out: &mut String, text: &[char], url: &str) {
        out.push_str("<a href=\"");
        out.push_str(&escape_html(url.trim()));
        out.push_str("\" rel=\"nofollow noopener noreferrer\">");
        // No links inside links.
        inline_with(text, out, false);
        out.push_str("</a>");
    }

    fn inline(chars: &[char], out: &mut String) {
        inline_with(chars, out, true);
    }

    fn inline_with(chars: &[char], out: &mut String, links: bool) {
        let n = chars.len();
        let mut f = Finders::default();
        let mut i = 0;
        while i < n {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            match c {
                '\\' if next.is_some_and(|x| x.is_ascii_punctuation()) => {
                    push_escaped(out, &chars[i + 1..i + 2]);
                    i += 2;
                    continue;
                }
                '`' => {
                    if let Some(j) = f.backtick.find(i + 1, n, |j| chars[j] == '`')
                        && j > i + 1
                    {
                        out.push_str("<code>");
                        push_escaped(out, &chars[i + 1..j]);
                        out.push_str("</code>");
                        i = j + 1;
                        continue;
                    }
                }
                '*' if next == Some('*') => {
                    if let Some(j) = f.double_star.find(i + 3, n.saturating_sub(1), |j| {
                        chars[j] == '*' && chars[j + 1] == '*'
                    }) && !chars[i + 2].is_whitespace()
                    {
                        out.push_str("<strong>");
                        inline_with(&chars[i + 2..j], out, links);
                        out.push_str("</strong>");
                        i = j + 2;
                        continue;
                    }
                }
                '*' | '_' => {
                    let left_ok = c == '*' || prev.is_none_or(|p| !p.is_alphanumeric());
                    let finder = if c == '*' {
                        &mut f.star
                    } else {
                        &mut f.underscore
                    };
                    if left_ok
                        && next.is_some_and(|x| !x.is_whitespace() && x != c)
                        && let Some(j) = finder.find(i + 2, n, |j| {
                            chars[j] == c
                                && !chars[j - 1].is_whitespace()
                                && (c == '*'
                                    || chars.get(j + 1).is_none_or(|x| !x.is_alphanumeric()))
                        })
                    {
                        out.push_str("<em>");
                        inline_with(&chars[i + 1..j], out, links);
                        out.push_str("</em>");
                        i = j + 1;
                        continue;
                    }
                }
                '[' if links => {
                    if let Some(j) = f.bracket_paren.find(i + 1, n.saturating_sub(1), |j| {
                        chars[j] == ']' && chars[j + 1] == '('
                    }) && let Some(k) = f.close_paren.find(j + 2, n, |k| chars[k] == ')')
                        && k - (j + 2) <= MAX_URL
                    {
                        let url: String = chars[j + 2..k].iter().collect();
                        if j > i + 1 && safe_url(&url) {
                            link(out, &chars[i + 1..j], &url);
                            i = k + 1;
                            continue;
                        }
                    }
                }
                'h' if links && prev.is_none_or(|p| p.is_whitespace() || p == '(') => {
                    let rest: String = chars[i..n.min(i + 8)].iter().collect();
                    if rest.starts_with("https://") || rest.starts_with("http://") {
                        let mut end = f
                            .whitespace
                            .find(i, n, |k| chars[k].is_whitespace())
                            .unwrap_or(n);
                        while end > i
                            && matches!(chars[end - 1], '.' | ',' | ';' | ':' | '!' | '?' | ')')
                        {
                            end -= 1;
                        }
                        let url: String = if end - i <= MAX_URL {
                            chars[i..end].iter().collect()
                        } else {
                            String::new()
                        };
                        if url.len() > 8 && safe_url(&url) {
                            link(out, &chars[i..end], &url);
                            i = end;
                            continue;
                        }
                    }
                }
                _ => {}
            }
            push_escaped(out, &chars[i..i + 1]);
            i += 1;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn pathological_input_renders_in_linear_time() {
            let n = 100_000;
            let inputs = [
                "[".repeat(n),
                "[a](".repeat(n / 4),
                "[a]".repeat(n / 3),
                "**a ".repeat(n / 4),
                "*".repeat(n),
                "_a".repeat(n / 2),
                "* a".repeat(n / 3),
                "`".repeat(n),
                "(http://\"".repeat(n / 9),
                " http://x".repeat(n / 9),
                "*_`[**](".repeat(n / 8),
                "a_*".repeat(n / 3),
                "[a](x".repeat(n / 5) + ")",
                "_*`".repeat(n / 3) + "**",
            ];
            for input in &inputs {
                // Best of three, so a busy test machine doesn't fail it.
                let took = (0..3)
                    .map(|_| {
                        let started = std::time::Instant::now();
                        assert!(!render(input).is_empty());
                        started.elapsed()
                    })
                    .min()
                    .unwrap_or_default();
                assert!(
                    took < std::time::Duration::from_millis(50),
                    "{:?}… took {took:?}",
                    &input[..12]
                );
            }
        }

        #[test]
        fn paragraphs_and_line_breaks() {
            assert_eq!(
                render("Hello\nworld\n\nSecond"),
                "<p>Hello<br>world</p><p>Second</p>"
            );
            assert_eq!(render(""), "");
            assert_eq!(render("\r\n\r\n  \n"), "");
        }

        #[test]
        fn emphasis_code_and_lists() {
            assert_eq!(
                render("**bold** and *it* and _it_"),
                "<p><strong>bold</strong> and <em>it</em> and <em>it</em></p>"
            );
            assert_eq!(render("`a<b`"), "<p><code>a&lt;b</code></p>");
            assert_eq!(
                render("- one\n- **two**"),
                "<ul><li>one</li><li><strong>two</strong></li></ul>"
            );
            assert_eq!(render("1. a\n2) b"), "<ol><li>a</li><li>b</li></ol>");
            assert_eq!(
                render("Intro\n- a\n- b\nOutro"),
                "<p>Intro</p><ul><li>a</li><li>b</li></ul><p>Outro</p>"
            );
            assert_eq!(render("- a\n  continued"), "<ul><li>a continued</li></ul>");
        }

        #[test]
        fn unmatched_markers_stay_literal() {
            assert_eq!(render("2 * 3 * 4"), "<p>2 * 3 * 4</p>");
            assert_eq!(render("snake_case_name"), "<p>snake_case_name</p>");
            assert_eq!(render("**open"), "<p>**open</p>");
            assert_eq!(render("\\*not italic\\*"), "<p>*not italic*</p>");
            assert_eq!(render("[text] (x)"), "<p>[text] (x)</p>");
        }

        #[test]
        fn links() {
            assert_eq!(
                render("[Docs](https://example.com/a?b=1&c=2)"),
                "<p><a href=\"https://example.com/a?b=1&amp;c=2\" rel=\"nofollow noopener noreferrer\">Docs</a></p>"
            );
            assert_eq!(
                render("See https://example.com/x."),
                "<p>See <a href=\"https://example.com/x\" rel=\"nofollow noopener noreferrer\">https://example.com/x</a>.</p>"
            );
            assert!(render("[home](/status/acme)").contains("href=\"/status/acme\""));
            assert!(
                render("[mail](mailto:ops@example.com)")
                    .contains("href=\"mailto:ops@example.com\"")
            );
            // Never nests links.
            assert_eq!(
                render("[https://a.example](https://a.example)")
                    .matches("<a ")
                    .count(),
                1
            );
        }

        #[test]
        fn escapes_html_everywhere() {
            assert_eq!(
                render("<script>alert(1)</script>"),
                "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>"
            );
            assert_eq!(
                render("**<img src=x onerror=alert(1)>**"),
                "<p><strong>&lt;img src=x onerror=alert(1)&gt;</strong></p>"
            );
            assert!(render("[<b>x</b>](https://a.b)").contains(">&lt;b&gt;x&lt;/b&gt;</a>"));
            assert_eq!(render("\"'&"), "<p>&quot;&#39;&amp;</p>");
        }

        #[test]
        fn rejects_dangerous_links() {
            for evil in [
                "[x](javascript:alert(1))",
                "[x](JaVaScRiPt:alert(1))",
                "[x]( javascript:alert(1))",
                "[x](java\tscript:alert(1))",
                "[x](data:text/html;base64,PHNjcmlwdD4=)",
                "[x](vbscript:msgbox)",
                "[x](//evil.example)",
                "[x](\" onmouseover=\"alert(1))",
                "[x](https://ok.example\" onclick=\"alert(1))",
            ] {
                let html = render(evil);
                assert!(!html.contains("<a "), "{evil} produced a link: {html}");
                assert!(
                    !html.contains("\" on"),
                    "{evil} broke out of an attribute: {html}"
                );
            }
            assert!(!render("javascript:alert(1)").contains("<a "));
        }

        #[test]
        fn safe_url_allowlist() {
            assert!(safe_url("https://a.b"));
            assert!(safe_url("HTTP://A.B"));
            assert!(safe_url("/x"));
            assert!(safe_url("#top"));
            assert!(!safe_url("ftp://a.b"));
            assert!(!safe_url("x"));
            assert!(!safe_url(""));
        }
    }
}

/// Router-level tests (public pages, custom domains, badges, editor access).
#[cfg(test)]
mod router_tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum_extra::extract::cookie::{Cookie, Key};
    use tower::ServiceExt;

    use super::*;
    use crate::config::Config;
    use crate::db::models::{NewMonitor, Role};
    use crate::db::status_pages::NewStatusPage;
    use crate::db::{self, heartbeats, monitors, users};
    use crate::monitor::Scheduler;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    pub(super) struct TestApp {
        pub(super) state: AppState,
        pub(super) router: Router,
        dir: std::path::PathBuf,
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(super) async fn app() -> TestApp {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!(
            "vv-sp-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::build_pool(&dir.join("t.db"));
        db::run_migrations(&pool, &dir).await;
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let state = AppState {
            scheduler: Scheduler::new(pool.clone(), tx.clone(), reqwest::Client::new()),
            db: pool,
            session_key: Key::generate(),
            update_tx: tx,
            http: reqwest::Client::new(),
            config: Arc::new(Config {
                data_dir: dir.clone(),
                bind_addr: "127.0.0.1:0".into(),
            }),
        };
        TestApp {
            router: crate::api::build_router(state.clone()),
            state,
            dir,
        }
    }

    /// Page "acme" (published as given) with one up monitor whose target is secret.
    pub(super) async fn seed(t: &TestApp, published: bool, domain: Option<&str>) -> (i64, i64) {
        let domain = domain.map(str::to_owned);
        t.state
            .with_conn(move |c| {
                let mid = monitors::insert_monitor(
                    c,
                    &NewMonitor {
                        name: "Website".into(),
                        target: "https://secret-target.internal.example/health".into(),
                        ..NewMonitor::default()
                    },
                )?;
                heartbeats::insert(
                    c,
                    mid,
                    None,
                    HeartbeatStatus::Up,
                    Some(123),
                    Some("secret-message"),
                )?;
                let pid = status_pages::insert(
                    c,
                    &NewStatusPage {
                        slug: "acme".into(),
                        title: "Acme status".into(),
                        description: "All about Acme".into(),
                        published,
                        custom_domain: domain,
                        ..NewStatusPage::default()
                    },
                )?;
                let sid = status_pages::sections(c, pid)?[0].id;
                status_pages::add_monitors(c, pid, sid, &[mid])?;
                db_incidents::insert(
                    c,
                    pid,
                    &db_incidents::NewIncident {
                        title: "Heads up".into(),
                        body: "<script>alert(1)</script> **bold**".into(),
                        severity: "warning".into(),
                        pinned: true,
                    },
                )?;
                Ok((pid, mid))
            })
            .await
            .unwrap()
    }

    pub(super) async fn get(
        t: &TestApp,
        uri: &str,
        host: Option<&str>,
        cookie: Option<&str>,
    ) -> (StatusCode, HeaderMapLite, String) {
        let mut req = Request::builder().uri(uri);
        if let Some(h) = host {
            req = req.header(header::HOST, h);
        }
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        let resp = t
            .router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = HeaderMapLite(
            resp.headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect(),
        );
        let body = String::from_utf8_lossy(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
            .to_string();
        (status, headers, body)
    }

    pub(super) struct HeaderMapLite(pub(super) Vec<(String, String)>);
    impl HeaderMapLite {
        pub(super) fn get(&self, k: &str) -> Option<&str> {
            self.0.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
        }
    }

    pub(super) fn assert_no_leaks(body: &str) {
        assert!(!body.contains("secret-target"), "monitor target leaked");
        assert!(!body.contains("secret-message"), "check message leaked");
    }

    #[tokio::test]
    async fn published_page_fragment_and_json() {
        let t = app().await;
        seed(&t, true, None).await;

        let (status, headers, body) = get(&t, "/status/acme", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Acme status"));
        assert!(body.contains("All systems operational"));
        assert!(body.contains("Website"));
        assert!(body.contains("123 ms"));
        assert!(body.contains("Heads up"));
        assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt; <strong>bold</strong>"));
        assert!(!body.contains("<script>alert(1)"));
        assert!(body.contains("<meta name=\"description\" content=\"All about Acme\">"));
        assert!(body.contains("hx-get=\"/status/acme/fragment\""));
        assert!(body.contains("Powered by"));
        assert_eq!(headers.get("cache-control"), Some("no-cache"));
        assert_no_leaks(&body);

        let (status, _, body) = get(&t, "/status/acme/fragment", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("<div class=\"stack\" id=\"sp-live\""));
        assert!(!body.contains("<html"));
        assert_no_leaks(&body);

        let (status, _, body) = get(&t, "/api/status-page/acme", None, None).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["slug"], "acme");
        assert_eq!(json["overall"]["indicator"], "operational");
        assert_eq!(json["sections"][0]["monitors"][0]["name"], "Website");
        assert_eq!(json["sections"][0]["monitors"][0]["status"], "up");
        assert_eq!(
            json["sections"][0]["monitors"][0]["days"]
                .as_array()
                .map(Vec::len),
            Some(90)
        );
        assert_no_leaks(&body);

        let (status, _, _) = get(&t, "/status/nope", None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unpublished_page_is_hidden_from_visitors() {
        let t = app().await;
        seed(&t, false, Some("status.hidden.test")).await;
        for uri in [
            "/status/acme",
            "/status/acme/fragment",
            "/api/status-page/acme",
        ] {
            let (status, _, _) = get(&t, uri, None, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
        // Its custom domain isn't served either.
        let (_, _, body) = get(&t, "/", Some("status.hidden.test"), None).await;
        assert!(!body.contains("Acme status"));
    }

    #[tokio::test]
    async fn editors_get_a_preview_of_unpublished_pages() {
        let t = app().await;
        seed(&t, false, None).await;
        let cookie = login(&t, "ed", Role::Editor).await;
        let (status, headers, body) = get(&t, "/status/acme", None, Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Draft preview"));
        assert!(body.contains("/status-pages/"));
        assert_eq!(headers.get("x-robots-tag"), Some("noindex"));
        // The JSON API never previews.
        let (status, _, _) = get(&t, "/api/status-page/acme", None, Some(&cookie)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn custom_domain_serves_the_page_at_root() {
        let t = app().await;
        seed(&t, true, Some("status.acme.test")).await;
        invalidate_domains();
        let (status, headers, body) = get(&t, "/", Some("Status.Acme.Test:8443"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Acme status"));
        assert_no_leaks(&body);
        // Embeddable like /status/{slug}, not locked down like the app.
        assert!(headers.get("x-frame-options").is_none());
        assert!(
            !headers
                .get("content-security-policy")
                .unwrap_or_default()
                .contains("frame-ancestors 'none'")
        );
        // Other hosts still get the app (here: redirect to setup, no users yet).
        let (status, _, body) = get(&t, "/", Some("localhost:3001"), None).await;
        assert_ne!(status, StatusCode::OK);
        assert!(!body.contains("Acme status"));
        // Other paths on the custom domain pass through.
        let (status, _, _) = get(&t, "/status/acme/fragment", Some("status.acme.test"), None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn badges_require_a_published_page_or_the_setting() {
        let t = app().await;
        let (_, mid) = seed(&t, true, None).await;
        let other = t
            .state
            .with_conn(|c| {
                monitors::insert_monitor(
                    c,
                    &NewMonitor {
                        name: "Hidden".into(),
                        ..NewMonitor::default()
                    },
                )
            })
            .await
            .unwrap();

        let (status, headers, body) = get(
            &t,
            &format!("/api/badge/{mid}/status?label=web&style=flat-square"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-type"),
            Some("image/svg+xml; charset=utf-8")
        );
        assert_eq!(headers.get("cache-control"), Some("public, max-age=60"));
        assert!(body.starts_with("<svg"));
        assert!(body.contains(">web</text>"));
        assert!(body.contains(">up</text>"));

        let (status, _, body) = get(
            &t,
            &format!("/api/badge/{mid}/uptime?duration=7d"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("uptime (7d)"));
        assert!(body.contains("100%"));
        let (status, _, body) = get(&t, &format!("/api/badge/{mid}/ping"), None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("123 ms"));
        let (status, _, body) = get(&t, &format!("/api/badge/{mid}/cert-exp"), None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("no data"));

        let (status, _, _) = get(&t, &format!("/api/badge/{other}/status"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = get(&t, &format!("/api/badge/{mid}/bogus"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = get(&t, "/api/badge/99999/status", None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        t.state
            .with_conn(|c| db_settings::set_bool(c, db_settings::BADGES_PUBLIC, true))
            .await
            .unwrap();
        let (status, _, body) = get(&t, &format!("/api/badge/{other}/status"), None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("unknown"));
    }

    pub(super) async fn login(t: &TestApp, name: &str, role: Role) -> String {
        let name = name.to_string();
        let user = t
            .state
            .with_conn(move |c| {
                let id = users::insert_user(c, &name, "x", role)?;
                users::get_user(c, id)
            })
            .await
            .unwrap()
            .unwrap();
        let claims = crate::auth::session::SessionClaims::new(
            user.id,
            user.session_version,
            crate::auth::session::now_unix(),
        );
        let jar = SignedCookieJar::new(t.state.session_key.clone())
            .add(Cookie::new(crate::auth::SESSION_COOKIE, claims.encode()));
        let resp = (jar, ()).into_response();
        let set_cookie = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        set_cookie.split(';').next().unwrap().to_string()
    }

    pub(super) async fn send(
        t: &TestApp,
        method: &str,
        uri: &str,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, HeaderMapLite, String) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost")
            .header("hx-request", "true")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = t.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = HeaderMapLite(
            resp.headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect(),
        );
        let body = String::from_utf8_lossy(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
            .to_string();
        (status, headers, body)
    }

    #[tokio::test]
    async fn editor_flow_renders_and_saves() {
        let t = app().await;
        let (pid, mid) = seed(&t, true, None).await;
        let ed = login(&t, "ed", Role::Editor).await;

        for uri in [
            "/status-pages".to_string(),
            "/status-pages/new".to_string(),
            format!("/status-pages/{pid}/edit"),
            format!("/status-pages/{pid}/edit?tab=settings"),
            format!("/status-pages/{pid}/incidents"),
            "/maintenance".to_string(),
            "/maintenance?tab=past".to_string(),
            "/maintenance/new".to_string(),
            "/maintenance/monitor-picker?q=web".to_string(),
        ] {
            let (status, _, body) = get(&t, &uri, None, Some(&ed)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        }
        let sid = t
            .state
            .with_conn(move |c| status_pages::sections(c, pid))
            .await
            .unwrap()[0]
            .id;
        let (status, _, body) = get(
            &t,
            &format!("/status-pages/{pid}/sections/{sid}/picker"),
            None,
            Some(&ed),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Already on this section"));

        // Create a page; slug derived from the title.
        let (status, headers, _) = send(
            &t,
            "POST",
            "/status-pages/new",
            &ed,
            "title=Docs+%26+API&slug=",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            headers
                .get("location")
                .is_some_and(|l| l.ends_with("/edit"))
        );
        let (status, _, body) = send(
            &t,
            "POST",
            "/status-pages/new",
            &ed,
            "title=Again&slug=docs-api",
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("already uses this slug"));

        // Settings: CSS breakout is refused, valid save redirects.
        let bad = "title=Acme&slug=acme&theme=dark&refresh_seconds=60&custom_css=%3C%2Fstyle%3E";
        let (status, _, body) =
            send(&t, "POST", &format!("/status-pages/{pid}/edit"), &ed, bad).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("aria-invalid=\"true\""));
        let good = "title=Acme&slug=acme&theme=dark&refresh_seconds=30&published=true&show_uptime=true&custom_domain=status.acme.test";
        let (status, _, _) =
            send(&t, "POST", &format!("/status-pages/{pid}/edit"), &ed, good).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let page = t
            .state
            .with_conn(move |c| status_pages::get(c, pid))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((page.theme.as_str(), page.refresh_seconds), ("dark", 30));
        assert_eq!(page.custom_domain.as_deref(), Some("status.acme.test"));

        // Sections: add, reorder (moving the monitor), rename, add monitors.
        let (status, headers, body) = send(
            &t,
            "POST",
            &format!("/status-pages/{pid}/sections"),
            &ed,
            "name=APIs",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("value=\"APIs\""));
        assert!(
            headers
                .get("hx-trigger")
                .is_some_and(|h| h.contains("vv:toast"))
        );
        let secs = t
            .state
            .with_conn(move |c| status_pages::sections(c, pid))
            .await
            .unwrap();
        let s2 = secs[1].id;
        let order = format!("sections%5B%5D={s2}&sections%5B%5D={sid}&section_{s2}%5B%5D={mid}");
        let (status, _, _) = send(
            &t,
            "POST",
            &format!("/status-pages/{pid}/order"),
            &ed,
            &order,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let secs = t
            .state
            .with_conn(move |c| status_pages::sections(c, pid))
            .await
            .unwrap();
        assert_eq!(secs[0].id, s2);
        assert_eq!(secs[0].monitors[0].monitor_id, mid);
        assert!(secs[1].monitors.is_empty());
        let (status, _, _) = send(
            &t,
            "POST",
            &format!("/status-pages/{pid}/sections/{s2}"),
            &ed,
            &format!("name_{s2}=Public+APIs"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, body) = send(
            &t,
            "POST",
            &format!("/status-pages/{pid}/sections/{sid}/monitors"),
            &ed,
            &format!("monitor_id={mid}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Public APIs"));

        // Incidents.
        let (status, _, _) = send(
            &t,
            "POST",
            &format!("/status-pages/{pid}/incidents"),
            &ed,
            "title=Planned&severity=info&body=Hi",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (_, _, body) = get(&t, "/status/acme", None, None).await;
        assert!(
            body.contains("Planned"),
            "unpinned open incidents show in the history"
        );

        // Maintenance: a manual window covering the monitor flips the public page.
        let (status, _, body) = send(
            &t,
            "POST",
            "/maintenance/new",
            &ed,
            &format!(
                "title=Upgrade&strategy=manual&active=true&monitor_id={mid}&status_page_id={pid}"
            ),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        let covered = t
            .state
            .with_conn(move |c| crate::maintenance::is_under_maintenance(c, mid))
            .await
            .unwrap();
        assert!(covered);
        let (_, _, body) = get(&t, "/status/acme", None, None).await;
        assert!(body.contains("Under maintenance"));
        assert!(body.contains("Upgrade"));
        let wid: i64 = t
            .state
            .with_conn(|c| c.query_row("SELECT id FROM maintenance_windows", [], |r| r.get(0)))
            .await
            .unwrap();
        let (status, _, _) = get(&t, &format!("/maintenance/{wid}/edit"), None, Some(&ed)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, headers, _) =
            send(&t, "POST", &format!("/maintenance/{wid}/end"), &ed, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("hx-refresh"), Some("true"));
        let covered = t
            .state
            .with_conn(move |c| crate::maintenance::is_under_maintenance(c, mid))
            .await
            .unwrap();
        assert!(!covered);
        let (status, _, _) = send(&t, "POST", &format!("/maintenance/{wid}/pause"), &ed, "").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "manual windows can't be paused"
        );
        let (status, headers, _) =
            send(&t, "DELETE", &format!("/maintenance/{wid}"), &ed, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("hx-redirect"), Some("/maintenance"));

        // Delete the page.
        let (status, headers, _) =
            send(&t, "DELETE", &format!("/status-pages/{pid}"), &ed, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("hx-redirect"), Some("/status-pages"));
    }

    #[tokio::test]
    async fn viewers_are_read_only() {
        let t = app().await;
        let (pid, _) = seed(&t, true, None).await;
        let viewer = login(&t, "vi", Role::Viewer).await;
        let (status, _, body) = get(&t, "/status-pages", None, Some(&viewer)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("New status page"));
        assert!(body.contains("href=\"/status/acme\""));
        let (status, _, body) = get(&t, "/maintenance", None, Some(&viewer)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("Schedule maintenance"));
        for uri in [
            format!("/status-pages/{pid}/edit"),
            "/status-pages/new".into(),
            "/maintenance/new".into(),
        ] {
            let (status, _, _) = get(&t, &uri, None, Some(&viewer)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
        let (status, _, _) = send(
            &t,
            "POST",
            &format!("/status-pages/{pid}/sections"),
            &viewer,
            "name=x",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = send(&t, "DELETE", &format!("/status-pages/{pid}"), &viewer, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn editor_routes_require_login() {
        let t = app().await;
        seed(&t, true, None).await;
        t.state
            .with_conn(|c| users::insert_user(c, "admin", "x", Role::Admin))
            .await
            .unwrap();
        for uri in [
            "/status-pages",
            "/status-pages/1/edit",
            "/maintenance",
            "/maintenance/new",
        ] {
            let (status, headers, _) = get(&t, uri, None, None).await;
            assert!(status.is_redirection(), "{uri}: {status}");
            assert!(
                headers
                    .get("location")
                    .is_some_and(|l| l.starts_with("/login")),
                "{uri}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overall_status_rules() {
        use PublicStatus::*;
        assert_eq!(overall(&[], false).indicator, "unknown");
        assert_eq!(overall(&[], true).indicator, "maintenance");
        assert_eq!(overall(&[Up, Up], false).indicator, "operational");
        assert_eq!(
            overall(&[Up, Paused, Unknown], false).indicator,
            "operational"
        );
        assert_eq!(overall(&[Up, Degraded], false).indicator, "degraded");
        assert_eq!(
            overall(&[Up, Down, Degraded], false).indicator,
            "partial_outage"
        );
        assert_eq!(overall(&[Down, Down], false).indicator, "major_outage");
        assert_eq!(
            overall(&[Down, Maintenance], false).indicator,
            "major_outage"
        );
        assert_eq!(overall(&[Up, Maintenance], false).indicator, "maintenance");
        assert_eq!(overall(&[Up], true).indicator, "maintenance");
        assert_eq!(
            overall(&[Up, Down, Up], false).detail,
            "1 of 3 services is down."
        );
        assert_eq!(
            overall(&[Down, Down, Up], false).detail,
            "2 of 3 services are down."
        );
    }

    #[test]
    fn public_status_mapping() {
        use HeartbeatStatus as H;
        assert_eq!(
            PublicStatus::from_heartbeat(Some(H::Up), true, false),
            PublicStatus::Up
        );
        assert_eq!(
            PublicStatus::from_heartbeat(Some(H::Down), true, true),
            PublicStatus::Maintenance
        );
        assert_eq!(
            PublicStatus::from_heartbeat(Some(H::Down), false, false),
            PublicStatus::Paused
        );
        assert_eq!(
            PublicStatus::from_heartbeat(Some(H::Pending), true, false),
            PublicStatus::Degraded
        );
        assert_eq!(
            PublicStatus::from_heartbeat(None, true, false),
            PublicStatus::Unknown
        );
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(fmt_pct(100.0), "100%");
        assert_eq!(fmt_pct(99.996), "99.99%");
        assert_eq!(fmt_pct(12.345), "12.34%");
        assert_eq!(fmt_duration(42), "42 sec");
        assert_eq!(fmt_duration(14 * 60 + 5), "14 min");
        assert_eq!(fmt_duration(2 * 3600), "2 h");
        assert_eq!(fmt_duration(2 * 3600 + 5 * 60), "2 h 5 min");
        assert_eq!(fmt_duration(3 * 86400 + 4 * 3600), "3 d 4 h");
        assert_eq!(fmt_day("2026-10-03"), "Oct 3");
        let oslo = site_tz("Europe/Oslo");
        assert_eq!(fmt_local("2026-07-01 08:30:00", &oslo), "Jul 1, 10:30 CEST");
        assert_eq!(fmt_local("nonsense", &oslo), "nonsense");
        assert_eq!(site_tz("Not/AZone"), TimeZone::UTC);
    }

    #[test]
    fn day_bars() {
        let mut d = DayStat {
            day: "2026-10-03".into(),
            up: 100,
            down: 0,
            pending: 0,
            maintenance: 0,
            avg_ms: None,
            uptime_pct: Some(100.0),
        };
        assert_eq!(day_class(&d), "status-up");
        assert_eq!(day_tip(&d, 0, 0), "Oct 3 · 100% uptime · No incidents");
        d.down = 2;
        d.uptime_pct = Some(98.0);
        assert_eq!(day_class(&d), "status-pending");
        assert_eq!(
            day_tip(&d, 1, 600),
            "Oct 3 · 98.00% uptime · 1 outage, 10 min"
        );
        d.uptime_pct = Some(40.0);
        assert_eq!(day_class(&d), "status-down");
        d.uptime_pct = None;
        assert_eq!(day_class(&d), "status-none");
        assert_eq!(day_tip(&d, 0, 0), "Oct 3 · No data");
        d.maintenance = 5;
        assert_eq!(day_class(&d), "status-maintenance");
    }
}
