//! Maintenance window evaluation. Owned by the statuspages agent.
//!
//! - Pure schedule math: [`Schedule`], [`window_is_active`], [`next_occurrence`],
//!   [`window_state`] (manual / once / daily / weekly / every N days / days of
//!   the month / cron, IANA time zones,
//!   overnight windows, DST gaps and folds).
//! - [`is_under_maintenance`]: called by the engine on every check. Backed by a
//!   per-database in-memory snapshot of "monitors covered right now" that is
//!   rebuilt at most every 30 s, at the next window start/end, or right after
//!   any maintenance edit ([`invalidate`]).
//! - [`spawn`]: background job that notices windows starting/ending, sends
//!   `MaintenanceStart` / `MaintenanceEnd` notifications, re-checks affected
//!   monitors so their status flips immediately, and pokes live dashboards.

mod cron;

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;

pub use cron::CronSpec;
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
/// Day-of-month and cron windows can skip months or years (29 February).
const LONG_LOOKAHEAD_DAYS: i64 = 4 * 366;
/// Longest cron window, in minutes (31 days).
pub const MAX_DURATION_MINUTES: i64 = 31 * 24 * 60;
/// Most a wall clock jumps at a DST change (and then some), in minutes: a
/// cron time whose local start plus duration plus this is still before the
/// local "now" has certainly ended, without a time-zone conversion.
const DST_SLACK_MINUTES: i64 = 3 * 60;

#[cfg(test)]
thread_local! {
    /// Local-time → instant conversions done by [`starts_on`] (tests only).
    static CONVERSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn count_conversion() {
    #[cfg(test)]
    CONVERSIONS.with(|c| c.set(c.get() + 1));
}

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
    /// Every `interval_days` days, counted from the first date, from
    /// `time_from` to `time_to` (local time).
    Interval,
    /// On the listed days of the month (negative = counted from the end:
    /// -1 is the last day) from `time_from` to `time_to` (local time).
    Monthly,
    /// Whenever the cron expression fires, for `duration_minutes` (local time).
    Cron,
}

impl Strategy {
    pub const ALL: [Strategy; 7] = [
        Strategy::Manual,
        Strategy::Once,
        Strategy::Daily,
        Strategy::Weekly,
        Strategy::Interval,
        Strategy::Monthly,
        Strategy::Cron,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "manual" => Strategy::Manual,
            "once" => Strategy::Once,
            "daily" => Strategy::Daily,
            "weekly" => Strategy::Weekly,
            "interval" => Strategy::Interval,
            "monthly" => Strategy::Monthly,
            "cron" => Strategy::Cron,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Manual => "manual",
            Strategy::Once => "once",
            Strategy::Daily => "daily",
            Strategy::Weekly => "weekly",
            Strategy::Interval => "interval",
            Strategy::Monthly => "monthly",
            Strategy::Cron => "cron",
        }
    }

    pub fn is_recurring(self) -> bool {
        !matches!(self, Strategy::Manual | Strategy::Once)
    }

    /// Recurring strategies that run between two times of day.
    pub fn uses_times(self) -> bool {
        matches!(
            self,
            Strategy::Daily | Strategy::Weekly | Strategy::Interval | Strategy::Monthly
        )
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
    /// Interval: run every this many days, counted from the local date of `start_at`.
    pub interval_days: i64,
    /// Monthly: days of the month; negative values count from the end.
    pub month_days: Vec<i8>,
    /// Cron: when occurrences start (`None` for an expression that doesn't parse).
    pub cron: Option<CronSpec>,
    /// Cron: how long each occurrence lasts.
    pub duration_minutes: i64,
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
            weekdays: w
                .weekdays
                .as_deref()
                .map(parse_weekdays)
                .unwrap_or_default(),
            time_from: w.time_from.as_deref().and_then(parse_hhmm),
            time_to: w.time_to.as_deref().and_then(parse_hhmm),
            interval_days: w.interval_days.unwrap_or(0),
            month_days: w
                .month_days
                .as_deref()
                .map(parse_month_days)
                .unwrap_or_default(),
            cron: w.cron.as_deref().and_then(|c| CronSpec::parse(c).ok()),
            duration_minutes: w.duration_minutes.unwrap_or(0),
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
    let start = date
        .to_datetime(from)
        .to_zoned(s.tz.clone())
        .ok()?
        .timestamp();
    let end_date = if to > from {
        date
    } else {
        date.tomorrow().ok()?
    };
    let end = end_date
        .to_datetime(to)
        .to_zoned(s.tz.clone())
        .ok()?
        .timestamp();
    Some((start, end))
}

/// Whether a recurring schedule has an occurrence starting on local `date`
/// (ignores the validity range).
fn day_matches(s: &Schedule, date: Date) -> bool {
    match s.strategy {
        Strategy::Daily => true,
        Strategy::Weekly => s.weekdays.contains(&date.weekday().to_monday_one_offset()),
        Strategy::Interval => {
            let Some(anchor) = s.start_at.map(|a| a.to_zoned(s.tz.clone()).date()) else {
                return false;
            };
            // Whole days between the two local dates.
            let days = date
                .since(anchor)
                .map_or(-1, |span| i64::from(span.get_days()));
            days >= 0 && s.interval_days > 0 && days % s.interval_days == 0
        }
        Strategy::Monthly => {
            let dim = date.days_in_month();
            s.month_days.iter().any(|d| {
                let day = if *d < 0 { dim + 1 + *d } else { *d };
                day >= 1 && day == date.day()
            })
        }
        Strategy::Cron => s.cron.as_ref().is_some_and(|c| c.matches_day(date)),
        Strategy::Manual | Strategy::Once => false,
    }
}

/// The occurrences that start on local `date`, earliest first, computed as
/// they are asked for. Cron times that certainly ended before `now_local`
/// (the local wall clock) are skipped without converting them.
fn starts_on<'a>(
    s: &'a Schedule,
    date: Date,
    now_local: jiff::civil::DateTime,
) -> Box<dyn Iterator<Item = (Timestamp, Timestamp)> + 'a> {
    if s.strategy != Strategy::Cron {
        count_conversion();
        return Box::new(occurrence_on(s, date).into_iter());
    }
    let Some(cron) = &s.cron else {
        return Box::new(std::iter::empty());
    };
    let duration = jiff::SignedDuration::from_mins(s.duration_minutes);
    let slack = jiff::SignedDuration::from_mins(s.duration_minutes + DST_SLACK_MINUTES);
    Box::new(cron.times().filter_map(move |(h, m)| {
        let local = date.at(h, m, 0, 0);
        if local.checked_add(slack).is_ok_and(|end| end < now_local) {
            return None;
        }
        count_conversion();
        let start = local.to_zoned(s.tz.clone()).ok()?.timestamp();
        let end = start.checked_add(duration).ok()?;
        Some((start, end))
    }))
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
        _ => {
            // Schedules that can't produce an occurrence at all.
            match s.strategy {
                Strategy::Weekly if s.weekdays.is_empty() => return None,
                Strategy::Interval if s.interval_days < 1 || s.start_at.is_none() => return None,
                Strategy::Monthly if s.month_days.is_empty() => return None,
                Strategy::Cron if s.cron.is_none() || s.duration_minutes < 1 => return None,
                _ => {}
            }
            let (back_days, ahead_days) = match s.strategy {
                Strategy::Cron => (s.duration_minutes / (24 * 60) + 1, LONG_LOOKAHEAD_DAYS),
                Strategy::Monthly => (1, 70),
                Strategy::Interval => (1, s.interval_days.max(LOOKAHEAD_DAYS) + 2),
                _ => (1, LOOKAHEAD_DAYS),
            };
            let mut from = now;
            if let Some(start_at) = s.start_at {
                from = from.max(start_at);
            }
            // Occurrences must start before `end_at`: none starts on a later
            // local date, so the search stops there instead of looking years
            // ahead for a window that is over.
            let last_date = s.end_at.map(|b| b.to_zoned(s.tz.clone()).date());
            let now_local = now.to_zoned(s.tz.clone()).datetime();
            // Begin early enough that an occurrence that started before `now`
            // (an overnight window, a long cron window) is still found.
            let mut date = from
                .to_zoned(s.tz.clone())
                .date()
                .checked_sub(jiff::Span::new().days(back_days))
                .ok()?;
            for _ in 0..=(ahead_days + back_days) {
                if last_date.is_some_and(|last| date > last) {
                    return None;
                }
                if day_matches(s, date) {
                    for (start, end) in starts_on(s, date, now_local) {
                        let in_range = s.start_at.is_none_or(|a| start >= a)
                            && s.end_at.is_none_or(|b| start < b);
                        let skipped = s.skip_until.is_some_and(|k| start < k);
                        if start < end && end > now && in_range && !skipped {
                            return Some((start, end));
                        }
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
        return if s.active {
            WindowState::Active
        } else {
            WindowState::Off
        };
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
/// Days of the month: `1`, `15`, `-1` (last day), `-2` (second to last) …;
/// `last` / `L` mean `-1`. Separated by commas or spaces.
pub fn parse_month_days_strict(s: &str) -> Result<Vec<i8>, String> {
    let mut days = Vec::new();
    for tok in s
        .split([',', ' ', ';'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let day = match tok.to_ascii_lowercase().as_str() {
            "last" | "l" => -1,
            other => other
                .parse::<i8>()
                .map_err(|_| format!("“{tok}” isn’t a day of the month."))?,
        };
        if day == 0 || !(-31..=31).contains(&day) {
            return Err(format!(
                "“{tok}” isn’t a day of the month (1–31, or -1 for the last day)."
            ));
        }
        days.push(day);
    }
    days.sort_unstable_by_key(|d| (*d < 0, d.abs()));
    days.dedup();
    Ok(days)
}

/// Like [`parse_month_days_strict`], dropping what doesn't parse.
pub fn parse_month_days(s: &str) -> Vec<i8> {
    let mut days: Vec<i8> = s
        .split([',', ' ', ';'])
        .filter_map(|t| parse_month_days_strict(t).ok())
        .flatten()
        .collect();
    days.sort_unstable_by_key(|d| (*d < 0, d.abs()));
    days.dedup();
    days
}

pub fn fmt_month_days(days: &[i8]) -> String {
    days.iter().map(i8::to_string).collect::<Vec<_>>().join(",")
}

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
    ts.to_zoned(tz.clone())
        .strftime("%Y-%m-%dT%H:%M")
        .to_string()
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
static SNAPSHOTS: LazyLock<Mutex<HashMap<String, Snapshot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
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
        self.windows
            .iter()
            .flat_map(|(_, _, m)| m.iter().copied())
            .collect()
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
        set.windows
            .push((w.id, w.title, expand_groups(&direct, children)));
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
                    let current: HashMap<i64, (String, HashSet<i64>)> = set
                        .windows
                        .into_iter()
                        .map(|(id, title, m)| (id, (title, m)))
                        .collect();
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
                let until =
                    Duration::try_from(Timestamp::now().duration_until(change)).unwrap_or_default();
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
        != current
            .values()
            .flat_map(|(_, m)| m)
            .collect::<HashSet<_>>();
    if started.is_empty() && ended.is_empty() && !monitors_changed {
        return;
    }
    invalidate_snapshots_only();

    let mut affected: HashSet<i64> = HashSet::new();
    for (kind, list) in [
        (EventKind::MaintenanceStart, &started),
        (EventKind::MaintenanceEnd, &ended),
    ] {
        for (id, (title, monitors)) in list {
            tracing::info!(window = id, %title, "maintenance {}", if kind == EventKind::MaintenanceStart { "started" } else { "ended" });
            affected.extend(monitors.iter().copied());
            notify_window(state, kind, *id, title, monitors).await;
            let ids: Vec<i64> = monitors.iter().copied().collect();
            let sub_kind = if kind == EventKind::MaintenanceStart {
                crate::subscribers::Kind::MaintenanceStart
            } else {
                crate::subscribers::Kind::MaintenanceEnd
            };
            crate::subscribers::announce_maintenance(state, sub_kind, title, &ids).await;
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

async fn notify_window(
    state: &AppState,
    kind: EventKind,
    window_id: i64,
    title: &str,
    monitors: &HashSet<i64>,
) {
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
                .query_row("SELECT name FROM monitors WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
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
            interval_days: 0,
            month_days: vec![],
            cron: None,
            duration_minutes: 0,
            tz: TimeZone::get(tz).unwrap(),
            skip_until: None,
        }
    }

    fn cron(expr: &str, minutes: i64, tz: &str) -> Schedule {
        Schedule {
            cron: Some(CronSpec::parse(expr).unwrap()),
            duration_minutes: minutes,
            ..sched(Strategy::Cron, tz)
        }
    }

    fn monthly(days: &[i8], from: &str, to: &str, tz: &str) -> Schedule {
        Schedule {
            month_days: days.to_vec(),
            strategy: Strategy::Monthly,
            ..daily(from, to, tz)
        }
    }

    fn every(days: i64, first: &str, from: &str, to: &str, tz: &str) -> Schedule {
        Schedule {
            interval_days: days,
            start_at: Some(ts(first)),
            strategy: Strategy::Interval,
            ..daily(from, to, tz)
        }
    }

    fn span(s: &Schedule, now: &str) -> Option<(String, String)> {
        next_occurrence(s, ts(now)).map(|(a, b)| (a.to_string(), b.to_string()))
    }

    fn pair(a: &str, b: &str) -> Option<(String, String)> {
        Some((a.to_string(), b.to_string()))
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
        assert_eq!(
            window_state(&s, ts("2026-10-19T09:00:00Z")),
            WindowState::Scheduled
        );
        assert!(window_is_active(&s, ts("2026-10-19T10:00:00Z")));
        assert!(window_is_active(&s, ts("2026-10-19T11:59:59Z")));
        assert!(!window_is_active(&s, ts("2026-10-19T12:00:00Z")));
        assert_eq!(
            window_state(&s, ts("2026-10-19T13:00:00Z")),
            WindowState::Ended
        );
        assert_eq!(
            next_transition(&s, ts("2026-10-19T09:00:00Z")),
            Some(ts("2026-10-19T10:00:00Z"))
        );
        assert_eq!(
            next_transition(&s, ts("2026-10-19T11:00:00Z")),
            Some(ts("2026-10-19T12:00:00Z"))
        );
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
        assert_eq!(
            window_state(&s, ts("2026-10-24T01:00:00Z")),
            WindowState::Ended
        );
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
        assert_eq!(
            parse_db_ts("2026-10-19 14:03:00"),
            Some(ts("2026-10-19T14:03:00Z"))
        );
        assert_eq!(
            parse_db_ts("2026-10-19T14:03:00"),
            Some(ts("2026-10-19T14:03:00Z"))
        );
        assert_eq!(
            parse_db_ts("2026-10-19T14:03:00Z"),
            Some(ts("2026-10-19T14:03:00Z"))
        );
        assert_eq!(
            parse_db_ts("2026-10-19T16:03:00+02:00"),
            Some(ts("2026-10-19T14:03:00Z"))
        );
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

    // --- cron ------------------------------------------------------------

    #[test]
    fn cron_runs_at_the_local_time_and_follows_dst() {
        // Sundays 02:00 Oslo for 90 minutes. Summer time (UTC+2) before 25 October.
        let s = cron("0 2 * * 0", 90, "Europe/Oslo");
        assert_eq!(
            span(&s, "2026-10-14T12:00:00Z"),
            pair("2026-10-18T00:00:00Z", "2026-10-18T01:30:00Z")
        );
        // 25 October: 02:00 happens twice; the earlier one (still UTC+2) is used.
        assert_eq!(
            span(&s, "2026-10-19T12:00:00Z"),
            pair("2026-10-25T00:00:00Z", "2026-10-25T01:30:00Z")
        );
        // The following Sunday is in winter time (UTC+1).
        assert_eq!(
            span(&s, "2026-10-26T12:00:00Z"),
            pair("2026-11-01T01:00:00Z", "2026-11-01T02:30:00Z")
        );
        // Spring forward 29 March 2026: 02:30 doesn't exist and moves to 03:30 (UTC+2).
        let gap = cron("30 2 * * 0", 30, "Europe/Oslo");
        assert_eq!(
            span(&gap, "2026-03-25T00:00:00Z"),
            pair("2026-03-29T01:30:00Z", "2026-03-29T02:00:00Z")
        );
    }

    #[test]
    fn cron_window_is_active_while_it_lasts() {
        let s = cron("0 2 * * 0", 90, "UTC");
        assert!(!window_is_active(&s, ts("2026-10-18T01:59:59Z")));
        assert!(window_is_active(&s, ts("2026-10-18T02:00:00Z")));
        assert!(window_is_active(&s, ts("2026-10-18T03:29:59Z")));
        assert!(!window_is_active(&s, ts("2026-10-18T03:30:00Z")));
        assert_eq!(
            window_state(&s, ts("2026-10-18T02:30:00Z")),
            WindowState::Active
        );
        assert_eq!(
            window_state(&s, ts("2026-10-18T05:00:00Z")),
            WindowState::Scheduled
        );
        assert_eq!(
            next_transition(&s, ts("2026-10-18T02:30:00Z")),
            Some(ts("2026-10-18T03:30:00Z"))
        );
        assert_eq!(
            next_transition(&s, ts("2026-10-18T05:00:00Z")),
            Some(ts("2026-10-25T02:00:00Z"))
        );
    }

    #[test]
    fn cron_windows_longer_than_a_day_are_found_mid_way() {
        // The first of the month at midnight, for three days.
        let s = cron("0 0 1 * *", 3 * 24 * 60, "UTC");
        assert!(window_is_active(&s, ts("2026-10-03T12:00:00Z")));
        assert!(!window_is_active(&s, ts("2026-10-04T00:00:00Z")));
        assert_eq!(
            span(&s, "2026-10-03T12:00:00Z"),
            pair("2026-10-01T00:00:00Z", "2026-10-04T00:00:00Z")
        );
        assert_eq!(
            span(&s, "2026-10-04T00:00:00Z"),
            pair("2026-11-01T00:00:00Z", "2026-11-04T00:00:00Z")
        );
    }

    #[test]
    fn cron_handles_leap_days_and_month_ends() {
        let leap = cron("0 0 29 2 *", 60, "UTC");
        assert_eq!(
            span(&leap, "2026-03-01T00:00:00Z"),
            pair("2028-02-29T00:00:00Z", "2028-02-29T01:00:00Z")
        );
        let end = cron("0 12 31 * *", 60, "UTC");
        // 30-day months are skipped.
        assert_eq!(
            span(&end, "2026-11-01T00:00:00Z"),
            pair("2026-12-31T12:00:00Z", "2026-12-31T13:00:00Z")
        );
        // An expression that can never fire has no occurrence.
        assert_eq!(
            next_occurrence(&cron("0 0 31 2 *", 60, "UTC"), ts("2026-01-01T00:00:00Z")),
            None
        );
    }

    #[test]
    fn cron_respects_the_date_range_and_early_end() {
        let mut s = cron("0 2 * * *", 60, "UTC");
        s.start_at = Some(ts("2026-10-20T00:00:00Z"));
        s.end_at = Some(ts("2026-10-22T00:00:00Z"));
        assert_eq!(
            span(&s, "2026-10-10T00:00:00Z"),
            pair("2026-10-20T02:00:00Z", "2026-10-20T03:00:00Z")
        );
        assert_eq!(next_occurrence(&s, ts("2026-10-21T04:00:00Z")), None);
        assert_eq!(next_occurrence(&s, ts("2026-10-22T00:00:00Z")), None);
        // "End now" during the 20th's window skips just that occurrence.
        s.skip_until = Some(ts("2026-10-20T02:30:00Z"));
        assert_eq!(
            span(&s, "2026-10-20T02:30:00Z"),
            pair("2026-10-21T02:00:00Z", "2026-10-21T03:00:00Z")
        );
    }

    fn conversions_for(f: impl FnOnce()) -> usize {
        CONVERSIONS.with(|c| c.set(0));
        f();
        CONVERSIONS.with(|c| c.get())
    }

    #[test]
    fn finished_cron_windows_are_not_searched_for_years() {
        // Every minute, but the range ended a year ago: the search used to
        // walk four years of days converting 1 440 times each (about two
        // million time-zone conversions per call, every 30 s).
        let mut s = cron("* * * * *", 1, "Europe/Oslo");
        s.start_at = Some(ts("2025-01-01T00:00:00Z"));
        s.end_at = Some(ts("2025-10-01T00:00:00Z"));
        let n = conversions_for(|| {
            assert_eq!(next_occurrence(&s, ts("2026-10-21T12:00:00Z")), None);
            assert_eq!(
                window_state(&s, ts("2026-10-21T12:00:00Z")),
                WindowState::Ended
            );
        });
        assert_eq!(n, 0, "nothing to convert after the end");
    }

    #[test]
    fn a_running_every_minute_cron_is_found_without_converting_the_whole_day() {
        let s = cron("* * * * *", 5, "America/New_York");
        let now = ts("2026-10-21T21:30:30Z");
        let mut found = None;
        let n = conversions_for(|| found = next_occurrence(&s, now));
        let (start, end) = found.unwrap();
        assert!(start <= now && now < end, "{start} {end}");
        // The minutes of today and yesterday that ended long ago are skipped
        // without a conversion; what's left is the DST slack plus the window.
        assert!(
            n <= 2 * (DST_SLACK_MINUTES as usize + 10),
            "{n} conversions"
        );
        // Still exact at the edge of the window.
        assert_eq!(start, ts("2026-10-21T21:26:00Z"));
    }

    #[test]
    fn a_cron_that_does_not_parse_or_has_no_length_never_matches() {
        let w = MaintenanceWindow {
            id: 1,
            title: String::new(),
            description: String::new(),
            strategy: "cron".into(),
            active: true,
            start_at: None,
            end_at: None,
            weekdays: None,
            time_from: None,
            time_to: None,
            interval_days: None,
            month_days: None,
            cron: Some("not a cron".into()),
            duration_minutes: Some(60),
            timezone: "UTC".into(),
            skip_until: None,
            created_at: String::new(),
        };
        let s = Schedule::from_window(&w);
        assert_eq!(s.strategy, Strategy::Cron);
        assert_eq!(next_occurrence(&s, ts("2026-10-19T00:00:00Z")), None);
        let ok = Schedule::from_window(&MaintenanceWindow {
            cron: Some("0 2 * * *".into()),
            duration_minutes: Some(0),
            ..w.clone()
        });
        assert_eq!(next_occurrence(&ok, ts("2026-10-19T00:00:00Z")), None);
        let ok = Schedule::from_window(&MaintenanceWindow {
            cron: Some("0 2 * * *".into()),
            duration_minutes: Some(30),
            ..w
        });
        assert!(next_occurrence(&ok, ts("2026-10-19T00:00:00Z")).is_some());
    }

    // --- days of the month -----------------------------------------------

    #[test]
    fn monthly_runs_on_the_listed_days_and_skips_short_months() {
        let s = monthly(&[1, 15], "02:00", "04:00", "UTC");
        assert_eq!(
            span(&s, "2026-10-10T00:00:00Z"),
            pair("2026-10-15T02:00:00Z", "2026-10-15T04:00:00Z")
        );
        assert_eq!(
            span(&s, "2026-10-15T03:00:00Z"),
            pair("2026-10-15T02:00:00Z", "2026-10-15T04:00:00Z")
        );
        assert_eq!(
            span(&s, "2026-10-16T00:00:00Z"),
            pair("2026-11-01T02:00:00Z", "2026-11-01T04:00:00Z")
        );
        let thirty_first = monthly(&[31], "02:00", "04:00", "UTC");
        assert_eq!(
            span(&thirty_first, "2026-11-01T00:00:00Z"),
            pair("2026-12-31T02:00:00Z", "2026-12-31T04:00:00Z")
        );
    }

    #[test]
    fn monthly_counts_from_the_end_including_leap_years() {
        let last = monthly(&[-1], "22:00", "23:00", "UTC");
        assert_eq!(
            span(&last, "2026-02-01T00:00:00Z"),
            pair("2026-02-28T22:00:00Z", "2026-02-28T23:00:00Z")
        );
        assert_eq!(
            span(&last, "2028-02-01T00:00:00Z"),
            pair("2028-02-29T22:00:00Z", "2028-02-29T23:00:00Z")
        );
        assert_eq!(
            span(&last, "2026-04-01T00:00:00Z"),
            pair("2026-04-30T22:00:00Z", "2026-04-30T23:00:00Z")
        );
        let second_last = monthly(&[-2], "22:00", "23:00", "UTC");
        assert_eq!(
            span(&second_last, "2026-02-01T00:00:00Z"),
            pair("2026-02-27T22:00:00Z", "2026-02-27T23:00:00Z")
        );
    }

    #[test]
    fn monthly_overnight_windows_cross_the_month_end() {
        let s = monthly(&[-1], "22:00", "02:00", "UTC");
        // Started 31 October at 22:00, still on at 1 November 01:00.
        assert!(window_is_active(&s, ts("2026-11-01T01:00:00Z")));
        assert!(!window_is_active(&s, ts("2026-11-01T02:00:00Z")));
    }

    #[test]
    fn month_day_lists_parse_strictly_and_leniently() {
        assert_eq!(
            parse_month_days_strict("15, 1; last 1"),
            Ok(vec![1, 15, -1])
        );
        assert_eq!(parse_month_days_strict("-2 L"), Ok(vec![-1, -2]));
        assert!(parse_month_days_strict("0").is_err());
        assert!(parse_month_days_strict("32").is_err());
        assert!(parse_month_days_strict("1, x").is_err());
        assert_eq!(parse_month_days_strict(""), Ok(vec![]));
        assert_eq!(parse_month_days("1, x, 40, 15"), vec![1, 15]);
        assert_eq!(fmt_month_days(&[1, 15, -1]), "1,15,-1");
    }

    // --- every N days ----------------------------------------------------

    #[test]
    fn interval_counts_days_from_the_first_date() {
        // Every 3rd day from 1 October, 22:00-23:00 UTC: 1, 4, 7, 10 …
        let s = every(3, "2026-10-01T00:00:00Z", "22:00", "23:00", "UTC");
        assert_eq!(
            span(&s, "2026-09-01T00:00:00Z"),
            pair("2026-10-01T22:00:00Z", "2026-10-01T23:00:00Z")
        );
        assert_eq!(
            span(&s, "2026-10-05T12:00:00Z"),
            pair("2026-10-07T22:00:00Z", "2026-10-07T23:00:00Z")
        );
        assert!(window_is_active(&s, ts("2026-10-07T22:30:00Z")));
        assert!(!window_is_active(&s, ts("2026-10-08T22:30:00Z")));
    }

    #[test]
    fn interval_crosses_month_and_year_ends() {
        let s = every(10, "2026-10-25T00:00:00Z", "01:00", "02:00", "UTC");
        assert_eq!(
            span(&s, "2026-10-26T00:00:00Z"),
            pair("2026-11-04T01:00:00Z", "2026-11-04T02:00:00Z")
        );
        let year = every(7, "2026-12-27T00:00:00Z", "01:00", "02:00", "UTC");
        assert_eq!(
            span(&year, "2026-12-28T00:00:00Z"),
            pair("2027-01-03T01:00:00Z", "2027-01-03T02:00:00Z")
        );
        // February in a leap year: 2028-02-22 + 7 = 2028-02-29, then 2028-03-07.
        let leap = every(7, "2028-02-22T00:00:00Z", "01:00", "02:00", "UTC");
        assert_eq!(
            span(&leap, "2028-02-23T00:00:00Z"),
            pair("2028-02-29T01:00:00Z", "2028-02-29T02:00:00Z")
        );
        assert_eq!(
            span(&leap, "2028-03-01T00:00:00Z"),
            pair("2028-03-07T01:00:00Z", "2028-03-07T02:00:00Z")
        );
    }

    #[test]
    fn interval_uses_local_dates_and_stays_on_wall_clock_time_across_dst() {
        // Every 2 days from 24 October (Oslo local midnight), 02:30-03:30 local. The clocks go
        // back on 25 October; 26 October is winter time.
        let s = every(2, "2026-10-23T22:00:00Z", "02:30", "03:30", "Europe/Oslo");
        // 24 Oct 02:30 CEST = 00:30Z, 26 Oct 02:30 CET = 01:30Z.
        assert_eq!(
            span(&s, "2026-10-24T12:00:00Z"),
            pair("2026-10-26T01:30:00Z", "2026-10-26T02:30:00Z")
        );
    }

    #[test]
    fn interval_without_a_first_date_or_count_never_matches() {
        let mut s = every(3, "2026-10-01T00:00:00Z", "22:00", "23:00", "UTC");
        s.start_at = None;
        assert_eq!(next_occurrence(&s, ts("2026-10-05T00:00:00Z")), None);
        let s = every(0, "2026-10-01T00:00:00Z", "22:00", "23:00", "UTC");
        assert_eq!(next_occurrence(&s, ts("2026-10-05T00:00:00Z")), None);
        let mut s = every(3, "2026-10-01T00:00:00Z", "22:00", "23:00", "UTC");
        s.end_at = Some(ts("2026-10-06T00:00:00Z"));
        assert_eq!(next_occurrence(&s, ts("2026-10-08T00:00:00Z")), None);
    }

    #[test]
    fn the_strategy_names_round_trip() {
        for s in Strategy::ALL {
            assert_eq!(Strategy::parse(s.as_str()), Some(s));
        }
        assert!(
            Strategy::Cron.is_recurring()
                && Strategy::Interval.is_recurring()
                && Strategy::Monthly.is_recurring()
        );
        assert!(!Strategy::Cron.uses_times() && Strategy::Monthly.uses_times());
    }
}
