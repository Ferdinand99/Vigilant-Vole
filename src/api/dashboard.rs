//! `/` dashboard. Owned by the monitors agent.
//!
//! All view state (search, status/type/tag filters, sort, collapsed groups,
//! select mode) lives in the query string, so it survives reloads and back
//! navigation, and the SSE stream (`/events?<same query>`) renders exactly
//! what the page shows. htmx requests that target `#dash-results` get just
//! the results fragment.

use crate::i18n::Tr as _;
use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::cookie::SignedCookieJar;

use super::monitors::view::format_pct;
use super::{ApiResult, AppState, HtmlTemplate, PageCtx};
use crate::auth::CurrentUser;
use crate::db::models::{HeartbeatStatus, MonitorType, MonitorWithStatus};
use crate::db::monitors as db_monitors;
use crate::db::tags::{self as db_tags, MonitorTag, Tag};

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(dashboard))
}

/// Element id the filter controls target with htmx.
pub const RESULTS_ID: &str = "dash-results";

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Problems first (down, pending, maintenance, up, paused), then name.
    #[default]
    Status,
    Name,
    /// Lowest 24 h uptime first.
    Uptime,
    /// Slowest first.
    Response,
    /// Most recently created first.
    Newest,
}

impl Sort {
    pub const ALL: [Sort; 5] = [
        Sort::Status,
        Sort::Name,
        Sort::Uptime,
        Sort::Response,
        Sort::Newest,
    ];

    pub fn parse(s: &str) -> Self {
        match s {
            "name" => Sort::Name,
            "uptime" => Sort::Uptime,
            "response" => Sort::Response,
            "newest" => Sort::Newest,
            _ => Sort::Status,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Sort::Status => "status",
            Sort::Name => "name",
            Sort::Uptime => "uptime",
            Sort::Response => "response",
            Sort::Newest => "newest",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Status => crate::i18n::t("dashboard.sort_status"),
            Sort::Name => crate::i18n::t("dashboard.sort_name"),
            Sort::Uptime => crate::i18n::t("dashboard.sort_uptime"),
            Sort::Response => crate::i18n::t("dashboard.sort_response"),
            Sort::Newest => crate::i18n::t("dashboard.sort_newest"),
        }
    }
}

/// Status filter values (`status=` in the URL).
pub const STATUS_FILTERS: [(&str, &str); 6] = [
    ("", "All"),
    ("down", "Down"),
    ("pending", "Pending"),
    ("up", "Up"),
    ("maintenance", "Maintenance"),
    ("paused", "Paused"),
];

const MAX_QUERY_LEN: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashQuery {
    pub q: String,
    /// "" | up | down | pending | maintenance | paused
    pub status: String,
    /// Monitor type (`MonitorType::as_str`) or "".
    pub kind: String,
    pub tag: Option<i64>,
    pub sort: Sort,
    /// Group ids whose sections are collapsed.
    pub collapsed: Vec<i64>,
    /// Bulk-select mode.
    pub select: bool,
}

impl DashQuery {
    pub fn parse(raw: Option<&str>) -> Self {
        let mut q = DashQuery::default();
        for (k, v) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
            let v = v.trim();
            match k.as_ref() {
                "q" => q.q = v.chars().take(MAX_QUERY_LEN).collect(),
                "status" if STATUS_FILTERS.iter().any(|(s, _)| *s == v) => q.status = v.to_string(),
                "type" if MonitorType::parse(v).is_some() => q.kind = v.to_string(),
                "tag" => q.tag = v.parse().ok(),
                "sort" => q.sort = Sort::parse(v),
                "collapsed" => {
                    for id in v.split(',').filter_map(|s| s.trim().parse::<i64>().ok()) {
                        if !q.collapsed.contains(&id) {
                            q.collapsed.push(id);
                        }
                    }
                }
                "select" => q.select = matches!(v, "1" | "true" | "on"),
                _ => {}
            }
        }
        q.collapsed.sort_unstable();
        q
    }

    /// Search or any filter active → flat result list instead of groups.
    pub fn is_filtered(&self) -> bool {
        !self.q.trim().is_empty()
            || !self.status.is_empty()
            || !self.kind.is_empty()
            || self.tag.is_some()
    }

    /// Query string (without `?`) for this state; empty values are left out.
    pub fn to_query_string(&self) -> String {
        let mut s = url::form_urlencoded::Serializer::new(String::new());
        if !self.q.trim().is_empty() {
            s.append_pair("q", self.q.trim());
        }
        if !self.status.is_empty() {
            s.append_pair("status", &self.status);
        }
        if !self.kind.is_empty() {
            s.append_pair("type", &self.kind);
        }
        if let Some(tag) = self.tag {
            s.append_pair("tag", &tag.to_string());
        }
        if self.sort != Sort::default() {
            s.append_pair("sort", self.sort.as_str());
        }
        if !self.collapsed.is_empty() {
            let ids: Vec<String> = self.collapsed.iter().map(i64::to_string).collect();
            s.append_pair("collapsed", &ids.join(","));
        }
        if self.select {
            s.append_pair("select", "1");
        }
        s.finish()
    }

    /// `/` plus the query string.
    pub fn href(&self) -> String {
        let qs = self.to_query_string();
        if qs.is_empty() {
            "/".into()
        } else {
            format!("/?{qs}")
        }
    }

    fn with_toggled(&self, group_id: i64) -> Self {
        let mut next = self.clone();
        if let Some(pos) = next.collapsed.iter().position(|id| *id == group_id) {
            next.collapsed.remove(pos);
        } else {
            next.collapsed.push(group_id);
            next.collapsed.sort_unstable();
        }
        next
    }
}

// ---------------------------------------------------------------------------
// View model
// ---------------------------------------------------------------------------

/// One monitor row.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: i64,
    pub name: String,
    pub type_label: &'static str,
    /// Shown after the type ("" for push/group).
    pub target: String,
    /// `up` | `down` | `pending` | `maintenance` | `paused` (for `status-*` classes).
    pub status: &'static str,
    pub status_label: &'static str,
    pub recent: Vec<Option<HeartbeatStatus>>,
    pub uptime_label: String,
    pub response_ms: Option<i64>,
    pub tags: Vec<MonitorTag>,
    /// Tags beyond [`ROW_TAGS`] (shown as "+N" with the names in a tooltip).
    pub more_tags: usize,
    pub more_tags_tip: String,
    /// Every tag id (including hidden ones), for the tag filter.
    tag_ids: Vec<i64>,
    /// Name of the parent group (shown in flat/filtered lists).
    pub group_name: String,
    /// Name of the agent that checks the monitor ("" = this server).
    pub checked_by: String,
    pub is_group: bool,
    uptime: Option<f64>,
    created_at: String,
}

/// A group monitor and its (active) children.
#[derive(Debug, Clone)]
pub struct GroupSection {
    pub id: i64,
    /// "Parent › Group" for nested groups.
    pub title: String,
    pub status: &'static str,
    pub status_label: &'static str,
    pub children: Vec<Row>,
    pub summary: String,
    pub collapsed: bool,
    /// URL that toggles this section's collapsed state.
    pub toggle_href: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub total: usize,
    pub up: usize,
    pub down: usize,
    pub pending: usize,
    pub maintenance: usize,
    pub paused: usize,
}

/// Everything `monitor_list.html` (the live region) renders.
#[derive(Debug, Clone)]
pub struct DashboardData {
    pub counts: Counts,
    /// Every monitor, groups included (0 → first-run empty state).
    pub total_all: usize,
    /// Filtered (or select-mode) view: one flat `rows` list, no sections.
    pub flat: bool,
    pub rows: Vec<Row>,
    pub groups: Vec<GroupSection>,
    pub paused: Vec<Row>,
    /// Rows shown in total (for "N monitors" and empty filter results).
    pub shown: usize,
    pub query: DashQuery,
    /// `/?…` for the current state (used by links and the SSE URL).
    pub href: String,
    /// The current state's query string (may be empty).
    pub qs: String,
    /// "Clear filters" target that keeps sort and collapsed groups.
    pub clear_href: String,
    pub select_mode: bool,
    /// Viewer may create monitors (empty-state action).
    pub can_edit: bool,
}

impl DashboardData {
    /// Type/tag filters and a non-default sort (shown as a badge on the Filter button).
    pub fn extra_filter_count(&self) -> usize {
        usize::from(!self.query.kind.is_empty())
            + usize::from(self.query.tag.is_some())
            + usize::from(self.query.sort != Sort::default())
    }

    pub fn tag_selected(&self, id: &i64) -> bool {
        self.query.tag == Some(*id)
    }

    /// Rendered in select mode: link to leave it.
    pub fn exit_select_href(&self) -> String {
        DashQuery {
            select: false,
            ..self.query.clone()
        }
        .href()
    }

    pub fn status_href(&self, status: &str) -> String {
        DashQuery {
            status: status.to_string(),
            select: false,
            ..self.query.clone()
        }
        .href()
    }
}

fn status_key(m: &MonitorWithStatus) -> &'static str {
    if !m.monitor.active {
        return "paused";
    }
    m.status.map(HeartbeatStatus::as_str).unwrap_or("pending")
}

fn status_label(key: &str) -> &'static str {
    match key {
        "up" => crate::i18n::t("status.up"),
        "down" => crate::i18n::t("status.down"),
        "maintenance" => crate::i18n::t("status.maintenance"),
        "paused" => crate::i18n::t("status.paused"),
        _ => crate::i18n::t("status.pending"),
    }
}

fn status_rank(key: &str) -> u8 {
    match key {
        "down" => 0,
        "pending" => 1,
        "maintenance" => 2,
        "up" => 3,
        _ => 4,
    }
}

/// Tags shown per dashboard row; the rest collapse into "+N".
pub const ROW_TAGS: usize = 3;

fn make_row(m: &MonitorWithStatus, mut tags: Vec<MonitorTag>, group_name: String) -> Row {
    let tag_ids = tags.iter().map(|t| t.tag_id).collect();
    let hidden: Vec<MonitorTag> = if tags.len() > ROW_TAGS {
        tags.split_off(ROW_TAGS)
    } else {
        Vec::new()
    };
    let more_tags_tip = hidden
        .iter()
        .map(|t| {
            if t.value.is_empty() {
                t.name.clone()
            } else {
                format!("{}: {}", t.name, t.value)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let status = status_key(m);
    let target = match m.monitor.monitor_type {
        MonitorType::Push | MonitorType::Group => String::new(),
        _ => m.monitor.target.clone(),
    };
    Row {
        id: m.monitor.id,
        name: m.monitor.name.clone(),
        type_label: m.monitor.monitor_type.label(),
        target,
        status,
        status_label: status_label(status),
        recent: m.recent.clone(),
        uptime_label: m.uptime_24h.map(format_pct).unwrap_or_else(|| "—".into()),
        response_ms: m.response_time_ms,
        more_tags: hidden.len(),
        more_tags_tip,
        tag_ids,
        tags,
        group_name,
        checked_by: m.checked_by.clone().unwrap_or_default(),
        is_group: m.monitor.monitor_type == MonitorType::Group,
        uptime: m.uptime_24h,
        created_at: m.monitor.created_at.clone(),
    }
}

fn sort_rows(rows: &mut [Row], sort: Sort) {
    let name = |r: &Row| r.name.to_lowercase();
    match sort {
        Sort::Status => rows.sort_by(|a, b| {
            status_rank(a.status)
                .cmp(&status_rank(b.status))
                .then_with(|| name(a).cmp(&name(b)))
        }),
        Sort::Name => rows.sort_by_key(|a| (name(a), a.id)),
        Sort::Uptime => rows.sort_by(|a, b| {
            // No data sorts last.
            let ua = a.uptime.unwrap_or(f64::INFINITY);
            let ub = b.uptime.unwrap_or(f64::INFINITY);
            ua.total_cmp(&ub).then_with(|| name(a).cmp(&name(b)))
        }),
        Sort::Response => rows.sort_by(|a, b| {
            b.response_ms
                .unwrap_or(-1)
                .cmp(&a.response_ms.unwrap_or(-1))
                .then_with(|| name(a).cmp(&name(b)))
        }),
        Sort::Newest => rows.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        }),
    }
}

fn matches_search(row: &Row, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay = |s: &str| s.to_lowercase().contains(needle);
    hay(&row.name)
        || hay(&row.target)
        || hay(row.type_label)
        || hay(&row.group_name)
        || hay(&row.checked_by)
        || row.tags.iter().any(|t| hay(&t.name) || hay(&t.value))
        || hay(&row.more_tags_tip)
}

/// Pure: turns the monitor list into the dashboard view for `query`.
pub fn build_dashboard(
    monitors: &[MonitorWithStatus],
    mut tags: HashMap<i64, Vec<MonitorTag>>,
    query: &DashQuery,
    can_edit: bool,
) -> DashboardData {
    let by_id: HashMap<i64, &MonitorWithStatus> =
        monitors.iter().map(|m| (m.monitor.id, m)).collect();
    let group_name = |id: Option<i64>| -> String {
        id.and_then(|id| by_id.get(&id))
            .filter(|g| g.monitor.monitor_type == MonitorType::Group)
            .map(|g| g.monitor.name.clone())
            .unwrap_or_default()
    };

    let mut counts = Counts::default();
    for m in monitors
        .iter()
        .filter(|m| m.monitor.monitor_type != MonitorType::Group)
    {
        counts.total += 1;
        match status_key(m) {
            "up" => counts.up += 1,
            "down" => counts.down += 1,
            "maintenance" => counts.maintenance += 1,
            "paused" => counts.paused += 1,
            _ => counts.pending += 1,
        }
    }

    let rows: Vec<Row> = monitors
        .iter()
        .map(|m| {
            let t = tags.remove(&m.monitor.id).unwrap_or_default();
            make_row(m, t, group_name(m.monitor.parent_id))
        })
        .collect();

    let flat = query.is_filtered() || query.select;
    let qs = {
        let mut q = query.clone();
        q.collapsed.retain(|id| by_id.contains_key(id));
        q
    };
    let mut data = DashboardData {
        counts,
        total_all: monitors.len(),
        flat,
        rows: Vec::new(),
        groups: Vec::new(),
        paused: Vec::new(),
        shown: 0,
        href: qs.href(),
        qs: qs.to_query_string(),
        clear_href: DashQuery {
            sort: query.sort,
            collapsed: qs.collapsed.clone(),
            select: query.select,
            ..DashQuery::default()
        }
        .href(),
        query: qs,
        select_mode: query.select,
        can_edit,
    };

    if flat {
        let needle = query.q.trim().to_lowercase();
        let mut list: Vec<Row> = rows
            .into_iter()
            .filter(|r| query.status.is_empty() || r.status == query.status)
            .filter(|r| {
                query.kind.is_empty()
                    || by_id
                        .get(&r.id)
                        .is_some_and(|m| m.monitor.monitor_type.as_str() == query.kind)
            })
            .filter(|r| query.tag.is_none_or(|tag| r.tag_ids.contains(&tag)))
            .filter(|r| matches_search(r, &needle))
            .collect();
        sort_rows(&mut list, query.sort);
        data.shown = list.len();
        data.rows = list;
        return data;
    }

    // Grouped view: active groups get sections; paused monitors go last.
    let active_group = |id: Option<i64>| -> Option<i64> {
        let id = id?;
        by_id
            .get(&id)
            .filter(|g| g.monitor.monitor_type == MonitorType::Group && g.monitor.active)
            .map(|_| id)
    };
    let mut top = Vec::new();
    let mut paused = Vec::new();
    let mut children: HashMap<i64, Vec<Row>> = HashMap::new();
    let mut group_rows: Vec<Row> = Vec::new();
    for row in rows {
        let Some(m) = by_id.get(&row.id) else {
            continue;
        };
        if !m.monitor.active {
            paused.push(row);
            continue;
        }
        let parent = active_group(m.monitor.parent_id);
        if row.is_group {
            group_rows.push(row.clone());
        }
        match parent {
            Some(p) => children.entry(p).or_default().push(row),
            None if !row.is_group => top.push(row),
            None => {}
        }
    }

    let title_of = |id: i64| -> String {
        // "Grandparent › Parent › Group", guarding against cycles.
        let mut parts = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            let Some(m) = by_id.get(&c) else { break };
            if parts.len() >= 4 || parts.contains(&m.monitor.name) {
                break;
            }
            parts.push(m.monitor.name.clone());
            cur = active_group(m.monitor.parent_id);
        }
        parts.reverse();
        parts.join(" › ")
    };

    sort_rows(&mut group_rows, query.sort);
    let mut shown = 0;
    let mut groups = Vec::new();
    for g in group_rows {
        let mut kids = children.remove(&g.id).unwrap_or_default();
        sort_rows(&mut kids, query.sort);
        let problems = kids.iter().filter(|k| k.status == "down").count();
        let summary = match (kids.len(), problems) {
            (0, _) => crate::i18n::t("dashboard.group_empty_summary").to_string(),
            (n, 0) => crate::i18n::tn("count.monitors", n as i64),
            (n, p) => crate::i18n::tf(
                "dashboard.group_down_summary",
                &[
                    ("monitors", &crate::i18n::tn("count.monitors", n as i64)),
                    ("n", &p.to_string()),
                ],
            ),
        };
        let collapsed = data.query.collapsed.contains(&g.id);
        shown += kids.len() + 1;
        groups.push(GroupSection {
            id: g.id,
            title: title_of(g.id),
            status: g.status,
            status_label: g.status_label,
            summary,
            collapsed,
            toggle_href: data.query.with_toggled(g.id).href(),
            children: kids,
        });
    }
    sort_rows(&mut top, query.sort);
    paused.sort_by_key(|r| (r.name.to_lowercase(), r.id));
    shown += top.len() + paused.len();
    data.rows = top;
    data.groups = groups;
    data.paused = paused;
    data.shown = shown;
    data
}

/// Loads everything for the dashboard (also used by the SSE stream).
pub async fn load_dashboard_data(
    state: &AppState,
    query: &DashQuery,
    can_edit: bool,
) -> ApiResult<DashboardData> {
    let (monitors, tags) = state
        .with_conn(|c| {
            Ok((
                db_monitors::list_monitors_with_status(c)?,
                db_tags::tags_by_monitor(c)?,
            ))
        })
        .await?;
    Ok(build_dashboard(&monitors, tags, query, can_edit))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Filter-bar options (page only; not part of the live region).
pub struct FilterOptions {
    pub types: Vec<(&'static str, &'static str)>,
    pub tags: Vec<Tag>,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    ctx: PageCtx,
    data: DashboardData,
    filters: FilterOptions,
    statuses: [(&'static str, &'static str); 6],
    sorts: [Sort; 5],
    /// The "Edit…" sheet (select mode only).
    bulk: Option<super::monitors::BulkSheet>,
}

/// The live results region (wrapper with the SSE connection + list).
#[derive(Template)]
#[template(path = "partials/monitor_results.html")]
pub struct ResultsTemplate {
    pub data: DashboardData,
}

fn wants_fragment(headers: &HeaderMap) -> bool {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("hx-request") == Some("true")
        && header("hx-history-restore-request").is_none()
        && header("hx-target") == Some(RESULTS_ID)
}

async fn dashboard(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let mut query = DashQuery::parse(raw.as_deref());
    if !user.can_edit() {
        query.select = false;
    }
    let data = load_dashboard_data(&state, &query, user.can_edit()).await?;
    if wants_fragment(&headers) {
        return Ok(HtmlTemplate(ResultsTemplate { data }).into_response());
    }
    let tags = state.with_conn(|c| db_tags::list_tags(c)).await?;
    let bulk = if data.select_mode {
        Some(
            state
                .with_conn(|c| super::monitors::load_bulk_sheet(c))
                .await?,
        )
    } else {
        None
    };
    let mut present: Vec<MonitorType> = Vec::new();
    let monitors_types = state
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT DISTINCT monitor_type FROM monitors")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
        })
        .await?;
    for t in MonitorType::all() {
        if monitors_types.iter().any(|s| s == t.as_str()) {
            present.push(*t);
        }
    }
    let filters = FilterOptions {
        types: present.iter().map(|t| (t.as_str(), t.label())).collect(),
        tags,
    };
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("dashboard.page_title"),
        "dashboard",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(DashboardTemplate {
            ctx,
            data,
            filters,
            statuses: STATUS_FILTERS,
            sorts: Sort::ALL,
            bulk,
        }),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{Monitor, NewMonitor};

    fn monitor(
        id: i64,
        name: &str,
        kind: MonitorType,
        parent: Option<i64>,
        active: bool,
    ) -> Monitor {
        let n = NewMonitor {
            name: name.into(),
            monitor_type: kind,
            parent_id: parent,
            active,
            target: format!("https://{name}.example"),
            ..NewMonitor::default()
        };
        let mut json = serde_json::to_value(&n).unwrap();
        json["id"] = id.into();
        json["created_at"] = format!("2026-01-{:02} 00:00:00", id.clamp(1, 28)).into();
        json["updated_at"] = "2026-01-01 00:00:00".into();
        serde_json::from_value(json).unwrap()
    }

    fn row(m: Monitor, status: Option<HeartbeatStatus>, uptime: Option<f64>) -> MonitorWithStatus {
        MonitorWithStatus {
            monitor: m,
            status,
            response_time_ms: None,
            recent: vec![],
            uptime_24h: uptime,
            checked_by: None,
        }
    }

    fn fixture() -> Vec<MonitorWithStatus> {
        use HeartbeatStatus::*;
        vec![
            row(
                monitor(1, "Prod", MonitorType::Group, None, true),
                Some(Down),
                None,
            ),
            row(
                monitor(2, "api", MonitorType::Http, Some(1), true),
                Some(Down),
                Some(50.0),
            ),
            row(
                monitor(3, "Web", MonitorType::Http, Some(1), true),
                Some(Up),
                Some(100.0),
            ),
            row(monitor(4, "db", MonitorType::Tcp, None, true), None, None),
            row(
                monitor(5, "Old", MonitorType::Ping, None, false),
                Some(Up),
                None,
            ),
            row(
                monitor(6, "Sub", MonitorType::Group, Some(1), true),
                Some(Up),
                None,
            ),
            row(
                monitor(7, "cache", MonitorType::Tcp, Some(6), true),
                Some(Maintenance),
                None,
            ),
        ]
    }

    #[test]
    fn query_parses_and_round_trips() {
        let q = DashQuery::parse(Some(
            "q=+api+&status=down&type=http&tag=3&sort=uptime&collapsed=4,2&collapsed=4&select=1&junk=x",
        ));
        assert_eq!(q.q, "api");
        assert_eq!(
            (q.status.as_str(), q.kind.as_str(), q.tag),
            ("down", "http", Some(3))
        );
        assert_eq!(q.sort, Sort::Uptime);
        assert_eq!(q.collapsed, vec![2, 4]);
        assert!(q.select && q.is_filtered());
        assert_eq!(DashQuery::parse(Some(&q.to_query_string())), q);
        // Invalid values are dropped.
        let bad = DashQuery::parse(Some("status=nope&type=ftp&tag=x&sort=zzz"));
        assert_eq!(bad, DashQuery::default());
        assert_eq!(bad.href(), "/");
    }

    #[test]
    fn grouped_view_nests_children_and_separates_paused() {
        let data = build_dashboard(&fixture(), HashMap::new(), &DashQuery::default(), true);
        assert!(!data.flat);
        assert_eq!(
            data.counts,
            Counts {
                total: 5,
                up: 1,
                down: 1,
                pending: 1,
                maintenance: 1,
                paused: 1
            }
        );
        assert_eq!(data.rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![4]);
        assert_eq!(
            data.paused.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![5]
        );
        let prod = data.groups.iter().find(|g| g.id == 1).unwrap();
        // Problems first: api (down), then Sub/Web (up) by name.
        assert_eq!(
            prod.children.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 6, 3]
        );
        assert_eq!(prod.summary, "3 monitors · 1 down");
        let sub = data.groups.iter().find(|g| g.id == 6).unwrap();
        assert_eq!(sub.title, "Prod › Sub");
        assert_eq!(sub.children.len(), 1);
        assert_eq!(data.total_all, 7);
    }

    #[test]
    fn collapsing_toggles_through_the_url() {
        let q = DashQuery::parse(Some("collapsed=1,999"));
        let data = build_dashboard(&fixture(), HashMap::new(), &q, true);
        let prod = data.groups.iter().find(|g| g.id == 1).unwrap();
        assert!(prod.collapsed);
        assert_eq!(
            prod.toggle_href, "/",
            "unknown ids are dropped and toggling re-expands"
        );
        let sub = data.groups.iter().find(|g| g.id == 6).unwrap();
        assert!(!sub.collapsed);
        assert_eq!(sub.toggle_href, "/?collapsed=1%2C6");
    }

    #[test]
    fn filters_produce_a_flat_sorted_list() {
        let tags = HashMap::from([(
            3,
            vec![MonitorTag {
                tag_id: 9,
                name: "env".into(),
                color: "blue".into(),
                value: "production".into(),
            }],
        )]);
        let search = build_dashboard(
            &fixture(),
            tags.clone(),
            &DashQuery::parse(Some("q=PROD")),
            true,
        );
        assert!(search.flat);
        // Group "Prod", its children (group name matches) and the tag value match.
        let mut ids: Vec<i64> = search.rows.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3, 6]);

        let by_tag = build_dashboard(&fixture(), tags, &DashQuery::parse(Some("tag=9")), true);
        assert_eq!(
            by_tag.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![3]
        );

        let paused = build_dashboard(
            &fixture(),
            HashMap::new(),
            &DashQuery::parse(Some("status=paused")),
            true,
        );
        assert_eq!(
            paused.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![5]
        );

        let tcp = build_dashboard(
            &fixture(),
            HashMap::new(),
            &DashQuery::parse(Some("type=tcp&sort=name")),
            true,
        );
        assert_eq!(
            tcp.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["cache", "db"]
        );

        let uptime = build_dashboard(
            &fixture(),
            HashMap::new(),
            &DashQuery::parse(Some("type=http&sort=uptime")),
            true,
        );
        assert_eq!(
            uptime.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(uptime.rows[0].uptime_label, "50.00%");
        assert_eq!(uptime.clear_href, "/?sort=uptime");
    }

    #[test]
    fn rows_show_three_tags_but_filter_and_search_see_all() {
        let tags: Vec<MonitorTag> = (1..=5)
            .map(|i| MonitorTag {
                tag_id: i,
                name: format!("t{i}"),
                color: "blue".into(),
                value: if i == 5 {
                    "needle".into()
                } else {
                    String::new()
                },
            })
            .collect();
        let map = HashMap::from([(4, tags)]);
        let data = build_dashboard(&fixture(), map.clone(), &DashQuery::default(), true);
        let db = data.rows.iter().find(|r| r.id == 4).unwrap();
        assert_eq!((db.tags.len(), db.more_tags), (3, 2));
        assert_eq!(db.more_tags_tip, "t4, t5: needle");
        let by_hidden_tag = build_dashboard(
            &fixture(),
            map.clone(),
            &DashQuery::parse(Some("tag=5")),
            true,
        );
        assert_eq!(by_hidden_tag.rows.len(), 1);
        let search = build_dashboard(&fixture(), map, &DashQuery::parse(Some("q=needle")), true);
        assert_eq!(search.rows.len(), 1);
    }

    #[test]
    fn empty_dashboard() {
        let data = build_dashboard(&[], HashMap::new(), &DashQuery::default(), false);
        assert_eq!((data.total_all, data.shown), (0, 0));
        assert!(data.groups.is_empty() && data.rows.is_empty());
    }
}
