//! Maintenance window evaluation. Owned by the statuspages agent.
//!
//! - Pure schedule math: [`Schedule`], [`window_is_active`], [`next_occurrence`],
//!   [`window_state`] (manual / once / daily / weekly, IANA time zones,
//!   overnight windows, DST gaps and folds).
//! - [`is_under_maintenance`]: called by the engine on every check. Backed by a
//!   per-database in-memory snapshot of "monitors covered right now" that is
//!   rebuilt at most every 30 s, at the next window start/end, or right after
//!   any maintenance edit ([`invalidate`]).
//! - [`spawn`]: background job that notices windows starting/ending, sends
//!   `MaintenanceStart` / `MaintenanceEnd` notifications, re-checks affected
//!   monitors so their status flips immediately, and pokes live dashboards.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use rusqlite::Connection;
use tokio::sync::Notify;

use crate::api::AppState;
use crate::db::maintenance::{self as db_maint, MaintenanceWindow};
use crate::db::models::{EventKind, NotificationChannel, NotificationEvent};
use crate::db::{self, notifications as db_notifications, settings as db_settings};

/// How long a coverage snapshot may be reused.
const SNAPSHOT_TTL: Duration = Duration::from_secs(30);
/// How far ahead [`next_occurrence`] looks for recurring windows (days).
const LOOKAHEAD_DAYS: i64 = 9;

// ---------------------------------------------------------------------------
// Schedule math (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// On while `active`, until someone ends it.
    Manual,
    /// One fixed `[start_at, end_at)` range.
    Once,
    /// Every day from `time_from` to `time_to` (local time).
    Daily,
    /// On the listed ISO weekdays from `time_from` to `time_to` (local time).
    Weekly,
}

impl Strategy {
    pub const ALL: [Strategy; 4] = [Strategy::Manual, Strategy::Once, Strategy::Daily, Strategy::Weekly];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "manual" => Strategy::Manual,
            "once" => Strategy::Once,
            "daily" => Strategy::Daily,
            "weekly" => Strategy::Weekly,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Manual => "manual",
            Strategy::Once => "once",
            Strategy::Daily => "daily",
            Strategy::Weekly => "weekly",
        }
    }

    pub fn is_recurring(self) -> bool {
        matches!(self, Strategy::Daily | Strategy::Weekly)
    }
}

/// A window's schedule, parsed and ready for evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub strategy: Strategy,
    /// Manual: in effect. Scheduled: enabled (not paused).
    pub active: bool,
    /// Once: start. Recurring: occurrences must start at or after this.
    pub start_at: Option<Timestamp>,
    /// Once: end. Recurring: occurrences must start before this.
    pub end_at: Option<Timestamp>,
    /// Weekly: ISO weekdays (Mon = 1 … Sun = 7) on which an occurrence starts.
    pub weekdays: Vec<i8>,
    pub time_from: Option<Time>,
    /// Earlier than (or equal to) `time_from` means the window runs past midnight.
    pub time_to: Option<Time>,
    pub tz: TimeZone,
    /// Occurrences that started before this were ended early ("End now").
    pub skip_until: Option<Timestamp>,
}

impl Schedule {
    /// Parses a DB row. Unparseable parts become `None` (the window then
    /// simply never matches) and an unknown time zone falls back to UTC.
    pub fn from_window(w: &MaintenanceWindow) -> Self {
        Schedule {
            strategy: Strategy::parse(&w.strategy).unwrap_or(Strategy::Manual),
            active: w.active,
            start_at: w.start_at.as_deref().and_then(parse_db_ts),
            end_at: w.end_at.as_deref().and_then(parse_db_ts),
            weekdays: w.weekdays.as_deref().map(parse_weekdays).unwrap_or_default(),
            time_from: w.time_from.as_deref().and_then(parse_hhmm),
            time_to: w.time_to.as_deref().and_then(parse_hhmm),
            tz: TimeZone::get(&w.timezone).unwrap_or(TimeZone::UTC),
            skip_until: w.skip_until.as_deref().and_then(parse_db_ts),
        }
    }
}

/// Where a window stands relative to `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    /// In effect right now.
    Active,
    /// Will start later.
    Scheduled,
    /// Scheduled window switched off.
    Paused,
    /// Manual window that isn't on.
    Off,
    /// No occurrence left.
    Ended,
}

impl WindowState {
    pub fn label(self) -> &'static str {
        match self {
            WindowState::Active => crate::i18n::t("mw.state_active"),
            WindowState::Scheduled => crate::i18n::t("mw.state_scheduled"),
            WindowState::Paused => crate::i18n::t("status.paused"),
            WindowState::Off => crate::i18n::t("mw.state_off"),
            WindowState::Ended => crate::i18n::t("mw.state_ended"),
        }
    }

    /// Design-system status class suffix.
    pub fn status_class(self) -> &'static str {
        match self {
            WindowState::Active => "maintenance",
            WindowState::Scheduled => "info",
            WindowState::Paused | WindowState::Off => "paused",
            WindowState::Ended => "none",
        }
    }
}

/// The occurrence of a daily/weekly window that starts on local `date`
/// (ignores weekdays and the validity range). DST: a start or end inside a
/// spring-forward gap moves forward past the gap; inside a fall-back fold the
/// earlier instant is used.
fn occurrence_on(s: &Schedule, date: Date) -> Option<(Timestamp, Timestamp)> {
    let from = s.time_from?;
    let to = s.time_to?;
    let start = date.to_datetime(from).to_zoned(s.tz.clone()).ok()?.timestamp();
    let end_date = if to > from { date } else { date.tomorrow().ok()? };
    let end = end_date.to_datetime(to).to_zoned(s.tz.clone()).ok()?.timestamp();
    Some((start, end))
}

/// The occurrence in effect at `now`, or else the next one to start.
/// `None` for manual windows, paused windows and windows with nothing left.
pub fn next_occurrence(s: &Schedule, now: Timestamp) -> Option<(Timestamp, Timestamp)> {
    if !s.active {
        return None;
    }
    match s.strategy {
        Strategy::Manual => None,
        Strategy::Once => {
            let (start, end) = (s.start_at?, s.end_at?);
            (start < end && end > now).then_some((start, end))
        }
        Strategy::Daily | Strategy::Weekly => {
            if s.strategy == Strategy::Weekly && s.weekdays.is_empty() {
                return None;
            }
            let mut from = now;
            if let Some(start_at) = s.start_at {
                from = from.max(start_at);
            }
            // Begin a day early so an overnight occurrence that started
            // yesterday is still found.
            let mut date = from.to_zoned(s.tz.clone()).date().yesterday().ok()?;
            for _ in 0..=LOOKAHEAD_DAYS {
                let day_matches =
                    s.strategy == Strategy::Daily || s.weekdays.contains(&date.weekday().to_monday_one_offset());
                if day_matches && let Some((start, end)) = occurrence_on(s, date) {
                    let in_range = s.start_at.is_none_or(|a| start >= a) && s.end_at.is_none_or(|b| start < b);
                    let skipped = s.skip_until.is_some_and(|k| start < k);
                    if start < end && end > now && in_range && !skipped {
                        return Some((start, end));
                    }
                }
                date = date.tomorrow().ok()?;
            }
            None
        }
    }
}

/// Whether the window is in effect at `now`.
pub fn window_is_active(s: &Schedule, now: Timestamp) -> bool {
    match s.strategy {
        Strategy::Manual => s.active,
        _ => next_occurrence(s, now).is_some_and(|(start, _)| start <= now),
    }
}

pub fn window_state(s: &Schedule, now: Timestamp) -> WindowState {
    if s.strategy == Strategy::Manual {
        return if s.active { WindowState::Active } else { WindowState::Off };
    }
    if !s.active {
        return WindowState::Paused;
    }
    match next_occurrence(s, now) {
        Some((start, _)) if start <= now => WindowState::Active,
        Some(_) => WindowState::Scheduled,
        None => WindowState::Ended,
    }
}

/// The next instant after `now` at which [`window_is_active`] may change.
pub fn next_transition(s: &Schedule, now: Timestamp) -> Option<Timestamp> {
    let (start, end) = next_occurrence(s, now)?;
    Some(if start <= now { end } else { start })
}

/// "HH:MM" (or "HH:MM:SS") → time of day.
pub fn parse_hhmm(s: &str) -> Option<Time> {
    let mut parts = s.trim().split(':');
    let h: i8 = parts.next()?.parse().ok()?;
    let m: i8 = parts.next()?.parse().ok()?;
    let sec: i8 = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Time::new(h, m, sec, 0).ok()
}

/// "1,3,5" → [1, 3, 5] (invalid entries dropped, sorted, deduplicated).
pub fn parse_weekdays(s: &str) -> Vec<i8> {
    let mut days: Vec<i8> = s
        .split(',')
        .filter_map(|d| d.trim().parse::<i8>().ok())
        .filter(|d| (1..=7).contains(d))
        .collect();
    days.sort_unstable();
    days.dedup();
    days
}

/// DB UTC text (`YYYY-MM-DD HH:MM:SS`, `T` separator also accepted) → instant.
pub fn parse_db_ts(s: &str) -> Option<Timestamp> {
    let s = s.trim().replacen(' ', "T", 1);
    if let Ok(ts) = s.parse::<Timestamp>() {
        // RFC 3339 with an offset (e.g. expiry dates from checkers).
        return Some(ts);
    }
    let dt: jiff::civil::DateTime = s.parse().ok()?;
    dt.to_zoned(TimeZone::UTC).ok().map(|z| z.timestamp())
}

/// Instant → DB UTC text.
pub fn fmt_db_ts(ts: Timestamp) -> String {
    ts.strftime("%Y-%m-%d %H:%M:%S").to_string()
}

/// Local wall-clock text (from an `<input type="datetime-local">`, e.g.
/// `2026-10-19T14:30`) in `tz` → instant.
pub fn parse_local_datetime(s: &str, tz: &TimeZone) -> Option<Timestamp> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let dt: jiff::civil::DateTime = s.replacen(' ', "T", 1).parse().ok()?;
    dt.to_zoned(tz.clone()).ok().map(|z| z.timestamp())
}

/// Instant → `YYYY-MM-DDTHH:MM` in `tz`, for `datetime-local` inputs.
pub fn fmt_local_input(ts: Timestamp, tz: &TimeZone) -> String {
    ts.to_zoned(tz.clone()).strftime("%Y-%m-%dT%H:%M").to_string()
}

// ---------------------------------------------------------------------------
// Coverage snapshot (cheap per-check lookup)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Snapshot {
    valid_until: Instant,
    covered: HashSet<i64>,
}

/// Snapshots keyed by database file path (tests use several databases).
static SNAPSHOTS: LazyLock<Mutex<HashMap<String, Snapshot>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Wakes the background job early (after edits).
static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Drop every cached snapshot and wake the background job. Call after any
/// maintenance window (or group membership) change.
pub fn invalidate() {
    if let Ok(mut map) = SNAPSHOTS.lock() {
        map.clear();
    }
    crate::api::public_status::invalidate_views();
    WAKE.notify_one();
}

/// Windows in effect at `now`, each with the monitors it covers (groups
/// expanded to all their descendants).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveSet {
    pub windows: Vec<(i64, String, HashSet<i64>)>,
    /// Earliest upcoming start/end among enabled windows.
    pub next_change: Option<Timestamp>,
}

impl ActiveSet {
    pub fn covered(&self) -> HashSet<i64> {
        self.windows.iter().flat_map(|(_, _, m)| m.iter().copied()).collect()
    }
}

/// Evaluates every enabled window at `now`.
pub fn active_set(conn: &Connection, now: Timestamp) -> rusqlite::Result<ActiveSet> {
    let windows = db_maint::list_enabled(conn)?;
    let mut set = ActiveSet::default();
    let mut children: Option<HashMap<i64, Vec<i64>>> = None;
    for w in windows {
        let sched = Schedule::from_window(&w);
        if let Some(t) = next_transition(&sched, now) {
            set.next_change = Some(set.next_change.map_or(t, |c| c.min(t)));
        }
        if !window_is_active(&sched, now) {
            continue;
        }
        let direct = db_maint::monitor_ids(conn, w.id)?;
        let children = match &mut children {
            Some(c) => c,
            None => children.insert(child_map(conn)?),
        };
        set.windows.push((w.id, w.title, expand_groups(&direct, children)));
    }
    Ok(set)
}

fn child_map(conn: &Connection) -> rusqlite::Result<HashMap<i64, Vec<i64>>> {
    let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
    for (child, parent) in db_maint::group_edges(conn)? {
        map.entry(parent).or_default().push(child);
    }
    Ok(map)
}

/// `ids` plus every descendant of the groups among them (cycle-safe).
pub fn expand_groups(ids: &[i64], children: &HashMap<i64, Vec<i64>>) -> HashSet<i64> {
    let mut out: HashSet<i64> = HashSet::new();
    let mut stack: Vec<i64> = ids.to_vec();
    while let Some(id) = stack.pop() {
        if out.insert(id)
            && let Some(kids) = children.get(&id)
        {
            stack.extend(kids.iter().copied());
        }
    }
    out
}

/// Monitors covered by a window in effect right now (uses the snapshot cache).
pub fn covered_monitors(conn: &Connection) -> rusqlite::Result<HashSet<i64>> {
    let key = conn.path().filter(|p| !p.is_empty()).map(str::to_owned);
    if let Some(key) = &key
        && let Ok(map) = SNAPSHOTS.lock()
        && let Some(snap) = map.get(key)
        && snap.valid_until > Instant::now()
    {
        return Ok(snap.covered.clone());
    }
    let now = Timestamp::now();
    let set = active_set(conn, now)?;
    let covered = set.covered();
    if let Some(key) = key {
        let mut ttl = SNAPSHOT_TTL;
        if let Some(change) = set.next_change {
            let until = now.duration_until(change);
            ttl = ttl.min(Duration::try_from(until).unwrap_or_default());
        }
        if let Ok(mut map) = SNAPSHOTS.lock() {
            map.insert(
                key,
                Snapshot {
                    valid_until: Instant::now() + ttl,
                    covered: covered.clone(),
                },
            );
        }
    }
    Ok(covered)
}

/// Whether `monitor_id` is inside an active maintenance window right now
/// (directly, or because one of its parent groups is). Called by the engine
/// on every check: answers from an in-memory snapshot.
pub fn is_under_maintenance(conn: &Connection, monitor_id: i64) -> rusqlite::Result<bool> {
    let key = conn.path().filter(|p| !p.is_empty()).map(str::to_owned);
    if let Some(key) = &key
        && let Ok(map) = SNAPSHOTS.lock()
        && let Some(snap) = map.get(key)
        && snap.valid_until > Instant::now()
    {
        return Ok(snap.covered.contains(&monitor_id));
    }
    Ok(covered_monitors(conn)?.contains(&monitor_id))
}

// ---------------------------------------------------------------------------
// Background job
// ---------------------------------------------------------------------------

/// Called once at server start (see `server::run`): watches windows start and
/// end, notifies, and refreshes affected monitors and dashboards.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut previous: Option<HashMap<i64, (String, HashSet<i64>)>> = None;
        loop {
            let now = Timestamp::now();
            let next_change = match db::interact(&state.db, move |c| active_set(c, now)).await {
                Ok(set) => {
                    let current: HashMap<i64, (String, HashSet<i64>)> =
                        set.windows.into_iter().map(|(id, title, m)| (id, (title, m))).collect();
                    if let Some(prev) = &previous {
                        handle_changes(&state, prev, &current).await;
                    }
                    previous = Some(current);
                    set.next_change
                }
                Err(err) => {
                    tracing::warn!(%err, "maintenance: failed to evaluate windows");
                    None
                }
            };
            let mut wait = SNAPSHOT_TTL;
            if let Some(change) = next_change {
                let until = Duration::try_from(Timestamp::now().duration_until(change)).unwrap_or_default();
                wait = wait.min(until + Duration::from_millis(250));
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = WAKE.notified() => {}
            }
        }
    });
}

async fn handle_changes(
    state: &AppState,
    prev: &HashMap<i64, (String, HashSet<i64>)>,
    current: &HashMap<i64, (String, HashSet<i64>)>,
) {
    let started: Vec<(i64, &(String, HashSet<i64>))> = current
        .iter()
        .filter(|(id, _)| !prev.contains_key(id))
        .map(|(id, v)| (*id, v))
        .collect();
    let ended: Vec<(i64, &(String, HashSet<i64>))> = prev
        .iter()
        .filter(|(id, _)| !current.contains_key(id))
        .map(|(id, v)| (*id, v))
        .collect();
    let monitors_changed = prev.values().flat_map(|(_, m)| m).collect::<HashSet<_>>()
        != current.values().flat_map(|(_, m)| m).collect::<HashSet<_>>();
    if started.is_empty() && ended.is_empty() && !monitors_changed {
        return;
    }
    invalidate_snapshots_only();

    let mut affected: HashSet<i64> = HashSet::new();
    for (kind, list) in [(EventKind::MaintenanceStart, &started), (EventKind::MaintenanceEnd, &ended)] {
        for (id, (title, monitors)) in list {
            tracing::info!(window = id, %title, "maintenance {}", if kind == EventKind::MaintenanceStart { "started" } else { "ended" });
            affected.extend(monitors.iter().copied());
            notify_window(state, kind, *id, title, monitors).await;
        }
    }
    if monitors_changed {
        // Membership edits while a window is running.
        for (_, m) in prev.values().chain(current.values()) {
            affected.extend(m.iter().copied());
        }
    }
    for id in affected {
        let scheduler = state.scheduler.clone();
        tokio::spawn(async move {
            scheduler.check_now(id).await;
        });
    }
    state.notify_update();
}

fn invalidate_snapshots_only() {
    if let Ok(mut map) = SNAPSHOTS.lock() {
        map.clear();
    }
}

async fn notify_window(state: &AppState, kind: EventKind, window_id: i64, title: &str, monitors: &HashSet<i64>) {
    let ids: Vec<i64> = monitors.iter().copied().collect();
    let loaded = db::interact(&state.db, move |c| {
        let mut channels: Vec<NotificationChannel> = Vec::new();
        let mut seen = HashSet::new();
        let mut names = Vec::new();
        for id in &ids {
            for ch in db_notifications::list_channels_for_monitor(c, *id, false)? {
                if seen.insert(ch.id) {
                    channels.push(ch);
                }
            }
            let name: Option<String> = c
                .query_row("SELECT name FROM monitors WHERE id = ?1", [id], |r| r.get(0))
                .ok();
            names.extend(name);
        }
        names.sort();
        let settings = db_settings::load(c)?;
        Ok((channels, names, settings))
    })
    .await;
    let (channels, names, settings) = match loaded {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(%err, "maintenance: failed to load channels");
            return;
        }
    };
    if channels.is_empty() {
        return;
    }
    let message = if names.is_empty() {
        None
    } else {
        let lang = crate::i18n::current_or_site();
        Some(crate::i18n::sync_scope(lang, || {
            crate::i18n::tf("mw.affects", &[("names", &names.join(", "))])
        }))
    };
    let event = NotificationEvent {
        kind,
        monitor_id: None,
        monitor_name: title.to_string(),
        monitor_type: String::new(),
        target: String::new(),
        message,
        downtime: None,
        expires_in_days: None,
        url: settings
            .base_url
            .as_deref()
            .map(|b| format!("{}/maintenance/{window_id}/edit", b.trim_end_matches('/'))),
        occurred_at: time::OffsetDateTime::now_utc(),
    };
    crate::notification::send_all(&state.http, &channels, &event).await;
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn sched(strategy: Strategy, tz: &str) -> Schedule {
        Schedule {
            strategy,
            active: true,
            start_at: None,
            end_at: None,
            weekdays: vec![],
            time_from: None,
            time_to: None,
            tz: TimeZone::get(tz).unwrap(),
            skip_until: None,
        }
    }

    fn daily(from: &str, to: &str, tz: &str) -> Schedule {
        Schedule {
            time_from: parse_hhmm(from),
            time_to: parse_hhmm(to),
            ..sched(Strategy::Daily, tz)
        }
    }

    fn weekly(days: &[i8], from: &str, to: &str, tz: &str) -> Schedule {
        Schedule {
            weekdays: days.to_vec(),
            strategy: Strategy::Weekly,
            ..daily(from, to, tz)
        }
    }

    // --- manual ---------------------------------------------------------

    #[test]
    fn manual_follows_the_active_flag() {
        let mut s = sched(Strategy::Manual, "UTC");
        let now = ts("2026-10-19T12:00:00Z");
        assert!(window_is_active(&s, now));
        assert_eq!(window_state(&s, now), WindowState::Active);
        assert_eq!(next_occurrence(&s, now), None);
        assert_eq!(next_transition(&s, now), None);
        s.active = false;
        assert!(!window_is_active(&s, now));
        assert_eq!(window_state(&s, now), WindowState::Off);
    }

    // --- once -----------------------------------------------------------

    #[test]
    fn once_is_active_inside_half_open_range() {
        let s = Schedule {
            start_at: Some(ts("2026-10-19T10:00:00Z")),
            end_at: Some(ts("2026-10-19T12:00:00Z")),
            ..sched(Strategy::Once, "UTC")
        };
        assert!(!window_is_active(&s, ts("2026-10-19T09:59:59Z")));
        assert_eq!(window_state(&s, ts("2026-10-19T09:00:00Z")), WindowState::Scheduled);
        assert!(window_is_active(&s, ts("2026-10-19T10:00:00Z")));
        assert!(window_is_active(&s, ts("2026-10-19T11:59:59Z")));
        assert!(!window_is_active(&s, ts("2026-10-19T12:00:00Z")));
        assert_eq!(window_state(&s, ts("2026-10-19T13:00:00Z")), WindowState::Ended);
        assert_eq!(next_transition(&s, ts("2026-10-19T09:00:00Z")), Some(ts("2026-10-19T10:00:00Z")));
        assert_eq!(next_transition(&s, ts("2026-10-19T11:00:00Z")), Some(ts("2026-10-19T12:00:00Z")));
    }

    #[test]
    fn once_with_missing_or_inverted_range_never_matches() {
        let mut s = Schedule {
            start_at: Some(ts("2026-10-19T12:00:00Z")),
            end_at: Some(ts("2026-10-19T10:00:00Z")),
            ..sched(Strategy::Once, "UTC")
        };
        assert_eq!(next_occurrence(&s, ts("2026-10-19T09:00:00Z")), None);
        s.end_at = None;
        assert_eq!(next_occurrence(&s, ts("2026-10-19T09:00:00Z")), None);
    }

    #[test]
    fn paused_window_is_never_active() {
        let s = Schedule {
            active: false,
            ..daily("00:00", "23:59", "UTC")
        };
        let now = ts("2026-10-19T12:00:00Z");
        assert!(!window_is_active(&s, now));
        assert_eq!(window_state(&s, now), WindowState::Paused);
    }

    // --- daily ----------------------------------------------------------

    #[test]
    fn daily_window_in_utc() {
        let s = daily("02:00", "04:00", "UTC");
        assert!(!window_is_active(&s, ts("2026-10-19T01:59:00Z")));
        assert!(window_is_active(&s, ts("2026-10-19T02:00:00Z")));
        assert!(window_is_active(&s, ts("2026-10-19T03:59:59Z")));
        assert!(!window_is_active(&s, ts("2026-10-19T04:00:00Z")));
        assert_eq!(
            next_occurrence(&s, ts("2026-10-19T05:00:00Z")),
            Some((ts("2026-10-20T02:00:00Z"), ts("2026-10-20T04:00:00Z")))
        );
    }

    #[test]
    fn daily_window_respects_time_zone() {
        // 02:00–04:00 in New York (EDT, UTC-4) = 06:00–08:00 UTC.
        let s = daily("02:00", "04:00", "America/New_York");
        assert!(!window_is_active(&s, ts("2026-07-01T03:00:00Z")));
        assert!(window_is_active(&s, ts("2026-07-01T06:30:00Z")));
    }

    #[test]
    fn overnight_window_spans_midnight() {
        let s = daily("22:00", "02:00", "UTC");
        assert!(window_is_active(&s, ts("2026-10-19T23:00:00Z")));
        assert!(window_is_active(&s, ts("2026-10-20T01:30:00Z")));
        assert!(!window_is_active(&s, ts("2026-10-20T02:00:00Z")));
        assert!(!window_is_active(&s, ts("2026-10-20T21:59:00Z")));
        assert_eq!(
            next_occurrence(&s, ts("2026-10-20T01:00:00Z")),
            Some((ts("2026-10-19T22:00:00Z"), ts("2026-10-20T02:00:00Z")))
        );
    }

    #[test]
    fn equal_from_and_to_means_all_day() {
        let s = daily("06:00", "06:00", "UTC");
        assert_eq!(
            next_occurrence(&s, ts("2026-10-19T12:00:00Z")),
            Some((ts("2026-10-19T06:00:00Z"), ts("2026-10-20T06:00:00Z")))
        );
    }

    #[test]
    fn daily_without_times_never_matches() {
        let s = sched(Strategy::Daily, "UTC");
        assert_eq!(next_occurrence(&s, ts("2026-10-19T12:00:00Z")), None);
    }

    #[test]
    fn recurring_respects_validity_range() {
        let mut s = daily("02:00", "04:00", "UTC");
        s.start_at = Some(ts("2026-10-21T00:00:00Z"));
        s.end_at = Some(ts("2026-10-23T00:00:00Z"));
        // Before the range: first occurrence is on the start day.
        assert_eq!(
            next_occurrence(&s, ts("2026-10-19T03:00:00Z")),
            Some((ts("2026-10-21T02:00:00Z"), ts("2026-10-21T04:00:00Z")))
        );
        assert!(!window_is_active(&s, ts("2026-10-20T03:00:00Z")));
        assert!(window_is_active(&s, ts("2026-10-22T03:00:00Z")));
        // After the range: nothing left.
        assert_eq!(next_occurrence(&s, ts("2026-10-23T01:00:00Z")), None);
        assert_eq!(window_state(&s, ts("2026-10-24T01:00:00Z")), WindowState::Ended);
    }

    #[test]
    fn recurring_start_far_in_the_future_is_found() {
        let mut s = daily("02:00", "04:00", "UTC");
        s.start_at = Some(ts("2027-03-01T00:00:00Z"));
        assert_eq!(
            next_occurrence(&s, ts("2026-10-19T03:00:00Z")),
            Some((ts("2027-03-01T02:00:00Z"), ts("2027-03-01T04:00:00Z")))
        );
    }

    #[test]
    fn end_now_skips_only_the_current_occurrence() {
        let mut s = daily("22:00", "02:00", "UTC");
        let now = ts("2026-10-19T23:00:00Z");
        assert!(window_is_active(&s, now));
        s.skip_until = Some(now);
        assert!(!window_is_active(&s, now));
        assert!(!window_is_active(&s, ts("2026-10-20T01:00:00Z")));
        assert_eq!(
            next_occurrence(&s, now),
            Some((ts("2026-10-20T22:00:00Z"), ts("2026-10-21T02:00:00Z")))
        );
        assert!(window_is_active(&s, ts("2026-10-20T22:30:00Z")));
    }

    // --- weekly ---------------------------------------------------------

    #[test]
    fn weekly_matches_listed_days_only() {
        // 2026-10-19 is a Monday.
        let s = weekly(&[1, 3], "09:00", "10:00", "UTC");
        assert!(window_is_active(&s, ts("2026-10-19T09:30:00Z"))); // Mon
        assert!(!window_is_active(&s, ts("2026-10-20T09:30:00Z"))); // Tue
        assert!(window_is_active(&s, ts("2026-10-21T09:30:00Z"))); // Wed
        assert_eq!(
            next_occurrence(&s, ts("2026-10-21T11:00:00Z")),
            Some((ts("2026-10-26T09:00:00Z"), ts("2026-10-26T10:00:00Z")))
        );
    }

    #[test]
    fn weekly_overnight_belongs_to_its_start_day() {
        // Friday 22:00 → Saturday 02:00.
        let s = weekly(&[5], "22:00", "02:00", "UTC");
        assert!(window_is_active(&s, ts("2026-10-23T23:00:00Z"))); // Fri night
        assert!(window_is_active(&s, ts("2026-10-24T01:00:00Z"))); // Sat early
        assert!(!window_is_active(&s, ts("2026-10-24T23:00:00Z"))); // Sat night
    }

    #[test]
    fn weekly_without_days_never_matches() {
        let s = weekly(&[], "09:00", "10:00", "UTC");
        assert_eq!(next_occurrence(&s, ts("2026-10-19T09:30:00Z")), None);
    }

    #[test]
    fn weekly_sunday_is_seven() {
        let s = weekly(&[7], "12:00", "13:00", "UTC");
        assert!(window_is_active(&s, ts("2026-10-25T12:30:00Z"))); // Sun
    }

    #[test]
    fn weekly_uses_local_weekday() {
        // Monday 01:00 in Tokyo is Sunday 16:00 UTC.
        let s = weekly(&[1], "01:00", "02:00", "Asia/Tokyo");
        assert!(window_is_active(&s, ts("2026-10-18T16:30:00Z")));
        assert!(!window_is_active(&s, ts("2026-10-19T16:30:00Z")));
    }

    // --- DST ------------------------------------------------------------

    #[test]
    fn window_keeps_local_time_across_dst() {
        // Oslo: CEST (UTC+2) until 2026-10-25, CET (UTC+1) after.
        let s = daily("03:00", "04:00", "Europe/Oslo");
        assert_eq!(
            next_occurrence(&s, ts("2026-10-24T00:00:00Z")),
            Some((ts("2026-10-24T01:00:00Z"), ts("2026-10-24T02:00:00Z")))
        );
        assert_eq!(
            next_occurrence(&s, ts("2026-10-26T00:00:00Z")),
            Some((ts("2026-10-26T02:00:00Z"), ts("2026-10-26T03:00:00Z")))
        );
    }

    #[test]
    fn start_in_spring_forward_gap_moves_forward() {
        // 2026-03-29 02:00 → 03:00 in Oslo. 02:30 doesn't exist.
        let s = daily("02:30", "05:00", "Europe/Oslo");
        let (start, end) = next_occurrence(&s, ts("2026-03-28T23:00:00Z")).unwrap();
        // 02:30 is pushed to 03:30 CEST (01:30 UTC); 05:00 CEST = 03:00 UTC.
        assert_eq!(start, ts("2026-03-29T01:30:00Z"));
        assert_eq!(end, ts("2026-03-29T03:00:00Z"));
    }

    #[test]
    fn window_inside_gap_shifts_forward() {
        // 02:10–02:50 doesn't exist on 2026-03-29 in Oslo: both ends land on 03:xx,
        // giving 03:10–03:50 CEST — still a 40 minute window, never empty or inverted.
        let s = daily("02:10", "02:50", "Europe/Oslo");
        let (start, end) = next_occurrence(&s, ts("2026-03-28T23:00:00Z")).unwrap();
        assert!(start < end);
        assert_eq!(end.duration_since(start), SignedDuration::from_mins(40));
    }

    #[test]
    fn fall_back_fold_uses_earlier_instant() {
        // 2026-10-25 03:00 → 02:00 in Oslo; 02:30 happens twice.
        let s = daily("02:30", "03:30", "Europe/Oslo");
        let (start, end) = next_occurrence(&s, ts("2026-10-24T23:00:00Z")).unwrap();
        assert_eq!(start, ts("2026-10-25T00:30:00Z")); // first 02:30 (CEST)
        assert_eq!(end, ts("2026-10-25T02:30:00Z")); // 03:30 CET
        assert_eq!(end.duration_since(start), SignedDuration::from_hours(2));
    }

    #[test]
    fn overnight_window_across_dst_end() {
        // 22:00 Sat → 06:00 Sun across the fall-back night is 9 real hours.
        let s = daily("22:00", "06:00", "Europe/Oslo");
        let (start, end) = next_occurrence(&s, ts("2026-10-24T19:00:00Z")).unwrap();
        assert_eq!(start, ts("2026-10-24T20:00:00Z"));
        assert_eq!(end, ts("2026-10-25T05:00:00Z"));
        assert!(window_is_active(&s, ts("2026-10-25T01:30:00Z")));
    }

    // --- parsing & helpers ---------------------------------------------

    #[test]
    fn parses_db_timestamps_and_times() {
        assert_eq!(parse_db_ts("2026-10-19 14:03:00"), Some(ts("2026-10-19T14:03:00Z")));
        assert_eq!(parse_db_ts("2026-10-19T14:03:00"), Some(ts("2026-10-19T14:03:00Z")));
        assert_eq!(parse_db_ts("2026-10-19T14:03:00Z"), Some(ts("2026-10-19T14:03:00Z")));
        assert_eq!(parse_db_ts("2026-10-19T16:03:00+02:00"), Some(ts("2026-10-19T14:03:00Z")));
        assert_eq!(parse_db_ts("garbage"), None);
        assert_eq!(fmt_db_ts(ts("2026-10-19T14:03:09Z")), "2026-10-19 14:03:09");
        assert_eq!(parse_hhmm("22:05"), Some(jiff::civil::time(22, 5, 0, 0)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("7"), None);
        assert_eq!(parse_weekdays("5, 1,1,9,x,7"), vec![1, 5, 7]);
    }

    #[test]
    fn local_input_round_trip() {
        let tz = TimeZone::get("Europe/Oslo").unwrap();
        let t = parse_local_datetime("2026-07-01T10:30", &tz).unwrap();
        assert_eq!(t, ts("2026-07-01T08:30:00Z"));
        assert_eq!(fmt_local_input(t, &tz), "2026-07-01T10:30");
        assert_eq!(parse_local_datetime("", &tz), None);
    }

    #[test]
    fn group_expansion_covers_descendants_and_survives_cycles() {
        let mut children = HashMap::new();
        children.insert(1, vec![2, 3]);
        children.insert(3, vec![4]);
        children.insert(4, vec![1]); // cycle
        let got = expand_groups(&[1], &children);
        assert_eq!(got, HashSet::from([1, 2, 3, 4]));
        assert_eq!(expand_groups(&[2], &children), HashSet::from([2]));
    }

    #[test]
    fn active_set_and_lookup_against_db() {
        use crate::db::maintenance::{NewWindow, insert, set_active};
        use crate::db::models::{MonitorType, NewMonitor};
        use crate::db::{monitors, test_conn};

        let mut conn = test_conn();
        let group = monitors::insert_monitor(
            &conn,
            &NewMonitor {
                name: "g".into(),
                monitor_type: MonitorType::Group,
                ..NewMonitor::default()
            },
        )
        .unwrap();
        let child = monitors::insert_monitor(
            &conn,
            &NewMonitor {
                name: "c".into(),
                parent_id: Some(group),
                ..NewMonitor::default()
            },
        )
        .unwrap();
        let other = monitors::insert_monitor(
            &conn,
            &NewMonitor {
                name: "o".into(),
                ..NewMonitor::default()
            },
        )
        .unwrap();
        let w = insert(
            &mut conn,
            &NewWindow {
                title: "w".into(),
                strategy: "manual".into(),
                active: true,
                timezone: "UTC".into(),
                monitor_ids: vec![group],
                ..NewWindow::default()
            },
        )
        .unwrap();
        // In-memory connections bypass the cache, so this reads straight through.
        assert!(is_under_maintenance(&conn, group).unwrap());
        assert!(is_under_maintenance(&conn, child).unwrap());
        assert!(!is_under_maintenance(&conn, other).unwrap());
        set_active(&conn, w, false).unwrap();
        assert!(!is_under_maintenance(&conn, child).unwrap());
    }
}
