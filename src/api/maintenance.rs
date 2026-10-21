//! Maintenance window pages. Owned by the statuspages agent.
//!
//! Routes: `/maintenance` (Active / Upcoming / Past), `/maintenance/new`,
//! `/maintenance/{id}/edit`, `/maintenance/{id}` DELETE,
//! `/maintenance/{id}/{start|end|pause|resume}` POST,
//! `/maintenance/monitor-picker` (htmx search for the monitor multi-select).

use crate::i18n::Tr as _;
use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::SignedCookieJar;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Deserialize;

use super::status_pages::{Picker, build_picker, form_pairs};
use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{CurrentUser, Role};
use crate::db::maintenance::{self as db_maint, MaintenanceWindow, NewWindow};
use crate::maintenance::{self, Schedule, Strategy, WindowState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/maintenance", get(list))
        .route("/maintenance/new", get(new_form).post(create))
        .route("/maintenance/monitor-picker", get(monitor_picker))
        .route("/maintenance/{id}", axum::routing::delete(delete_window))
        .route("/maintenance/{id}/edit", get(edit_form).post(update))
        .route("/maintenance/{id}/{action}", post(action))
        .layer(axum::middleware::from_fn(
            super::public_status::invalidate_after_writes,
        ))
}

/// ISO weekday numbers (Mon = 1); names come from `i18n::weekday_name`.
const WEEKDAYS: [i8; 7] = [1, 2, 3, 4, 5, 6, 7];

/// Human summary of a window's schedule, in the window's own time zone.
pub fn describe_schedule(w: &MaintenanceWindow) -> String {
    let tz_name = w.timezone.as_str();
    let tz = TimeZone::get(tz_name).unwrap_or(TimeZone::UTC);
    let range = || {
        format!(
            "{}–{}",
            w.time_from.as_deref().unwrap_or("?"),
            w.time_to.as_deref().unwrap_or("?")
        )
    };
    match Strategy::parse(&w.strategy).unwrap_or(Strategy::Manual) {
        Strategy::Manual => crate::i18n::t("mw.desc_manual").into(),
        Strategy::Once => {
            let fmt = |s: &Option<String>| {
                s.as_deref()
                    .and_then(maintenance::parse_db_ts)
                    .map(|t| crate::i18n::fmt_short_datetime(&t.to_zoned(tz.clone())))
                    .unwrap_or_else(|| "?".into())
            };
            format!("{} – {} ({tz_name})", fmt(&w.start_at), fmt(&w.end_at))
        }
        Strategy::Daily => {
            crate::i18n::tf("mw.desc_daily", &[("range", &range()), ("tz", tz_name)])
        }
        Strategy::Interval => {
            let first = w
                .start_at
                .as_deref()
                .and_then(maintenance::parse_db_ts)
                .map(|t| crate::i18n::fmt_date(t.to_zoned(tz.clone()).date()))
                .unwrap_or_else(|| "?".into());
            crate::i18n::tf(
                "mw.desc_interval",
                &[
                    ("n", &crate::i18n::fmt_int(w.interval_days.unwrap_or(1))),
                    ("from", &first),
                    ("range", &range()),
                    ("tz", tz_name),
                ],
            )
        }
        Strategy::Monthly => {
            let days = maintenance::parse_month_days(w.month_days.as_deref().unwrap_or(""));
            let names: Vec<String> = days
                .iter()
                .map(|d| {
                    if *d == -1 {
                        crate::i18n::t("mw.last_day_short").to_string()
                    } else {
                        d.to_string()
                    }
                })
                .collect();
            crate::i18n::tf(
                "mw.desc_monthly",
                &[
                    ("days", &names.join(", ")),
                    ("range", &range()),
                    ("tz", tz_name),
                ],
            )
        }
        Strategy::Cron => crate::i18n::tf(
            "mw.desc_cron",
            &[
                ("cron", w.cron.as_deref().unwrap_or("?")),
                (
                    "minutes",
                    &crate::i18n::fmt_int(w.duration_minutes.unwrap_or(0)),
                ),
                ("tz", tz_name),
            ],
        ),
        Strategy::Weekly => {
            let days = maintenance::parse_weekdays(w.weekdays.as_deref().unwrap_or(""));
            let names: Vec<&str> = WEEKDAYS
                .iter()
                .filter(|n| days.contains(n))
                .map(|n| crate::i18n::weekday_name(*n, false))
                .collect();
            format!("{}, {} ({tz_name})", names.join(", "), range())
        }
    }
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Row {
    id: i64,
    title: String,
    schedule: String,
    next: String,
    state: WindowState,
    monitors: usize,
    strategy: Strategy,
}

impl Row {
    fn can_start(&self) -> bool {
        matches!(
            (self.strategy, self.state),
            (Strategy::Manual, WindowState::Off) | (Strategy::Once, WindowState::Scheduled)
        )
    }
    fn can_end(&self) -> bool {
        self.state == WindowState::Active
    }
    fn can_pause(&self) -> bool {
        self.strategy != Strategy::Manual
            && matches!(self.state, WindowState::Active | WindowState::Scheduled)
    }
    fn can_resume(&self) -> bool {
        self.state == WindowState::Paused
    }
    fn monitors_label(&self) -> String {
        match self.monitors {
            0 => crate::i18n::t("mw.no_monitors").into(),
            n => crate::i18n::tn("count.monitors", n as i64),
        }
    }
}

#[derive(Template)]
#[template(path = "maintenance_list.html")]
struct ListTemplate {
    ctx: PageCtx,
    tab: &'static str,
    rows: Vec<Row>,
    active_count: usize,
    upcoming_count: usize,
    past_count: usize,
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    tab: String,
}

fn segment(state: WindowState) -> &'static str {
    match state {
        WindowState::Active => "active",
        WindowState::Scheduled | WindowState::Paused | WindowState::Off => "upcoming",
        WindowState::Ended => "past",
    }
}

async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Query(q): Query<ListQuery>,
) -> ApiResult<impl IntoResponse> {
    let (windows, links, settings) = state
        .with_conn(|c| {
            Ok((
                db_maint::list(c)?,
                db_maint::all_monitor_links(c)?,
                crate::db::settings::load(c)?,
            ))
        })
        .await?;
    let site_tz = super::public_status::site_tz(&settings.timezone);
    let now = Timestamp::now();
    let rows: Vec<Row> = windows
        .iter()
        .map(|w| {
            let sched = Schedule::from_window(w);
            let state = maintenance::window_state(&sched, now);
            let next = match (state, maintenance::next_occurrence(&sched, now)) {
                (WindowState::Active, Some((_, end))) => crate::i18n::tf(
                    "mw.ends_at",
                    &[("when", &super::public_status::fmt_ts(end, &site_tz))],
                ),
                (WindowState::Scheduled, Some((start, _))) => crate::i18n::tf(
                    "mw.starts_at",
                    &[("when", &super::public_status::fmt_ts(start, &site_tz))],
                ),
                _ => String::new(),
            };
            Row {
                id: w.id,
                title: w.title.clone(),
                schedule: describe_schedule(w),
                next,
                state,
                monitors: links.iter().filter(|(mid, _)| *mid == w.id).count(),
                strategy: sched.strategy,
            }
        })
        .collect();
    let count = |seg: &str| rows.iter().filter(|r| segment(r.state) == seg).count();
    let (active_count, upcoming_count, past_count) =
        (count("active"), count("upcoming"), count("past"));
    let tab: &'static str = match q.tab.as_str() {
        "upcoming" => "upcoming",
        "past" => "past",
        "active" => "active",
        // Default to the first segment with something in it.
        _ if active_count == 0 && upcoming_count > 0 => "upcoming",
        _ => "active",
    };
    let rows = rows
        .into_iter()
        .filter(|r| segment(r.state) == tab)
        .collect();
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("nav.maintenance"),
        "maintenance",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(ListTemplate {
            ctx,
            tab,
            rows,
            active_count,
            upcoming_count,
            past_count,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Form
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct WeekdayOpt {
    value: i8,
    label: &'static str,
    checked: bool,
}

#[derive(Debug, Clone)]
struct PageOpt {
    id: i64,
    title: String,
    checked: bool,
}

/// Form values (as typed) and per-field errors.
#[derive(Debug, Clone, Default)]
struct WindowForm {
    title: String,
    description: String,
    strategy: String,
    active: bool,
    timezone: String,
    start_at: String,
    end_at: String,
    time_from: String,
    time_to: String,
    weekdays: Vec<i8>,
    interval_days: String,
    month_days: String,
    cron: String,
    duration_minutes: String,
    valid_from: String,
    valid_until: String,
    monitor_ids: Vec<i64>,
    status_page_ids: Vec<i64>,
    /// Upcoming runs of a saved recurring window, as text.
    next_runs: Vec<String>,
    title_error: String,
    description_error: String,
    schedule_error: String,
    timezone_error: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WindowInput {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    strategy: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    timezone: String,
    #[serde(default)]
    start_at: String,
    #[serde(default)]
    end_at: String,
    #[serde(default)]
    time_from: String,
    #[serde(default)]
    time_to: String,
    #[serde(default)]
    weekday: Vec<i8>,
    #[serde(default)]
    interval_days: String,
    #[serde(default)]
    month_days: String,
    #[serde(default)]
    cron: String,
    #[serde(default)]
    duration_minutes: String,
    #[serde(default)]
    valid_from: String,
    #[serde(default)]
    valid_until: String,
    #[serde(default)]
    monitor_id: Vec<i64>,
    #[serde(default)]
    status_page_id: Vec<i64>,
}

/// Local calendar date (`YYYY-MM-DD`) at midnight in `tz`, plus `add_days`.
fn date_start(date: &str, tz: &TimeZone, add_days: i64) -> Option<Timestamp> {
    let d: jiff::civil::Date = date.trim().parse().ok()?;
    let d = d.checked_add(jiff::Span::new().days(add_days)).ok()?;
    d.to_zoned(tz.clone()).ok().map(|z| z.timestamp())
}

fn local_date(ts: Timestamp, tz: &TimeZone, add_days: i64) -> String {
    let d = ts.to_zoned(tz.clone()).date();
    d.checked_add(jiff::Span::new().days(add_days))
        .map(|d| d.to_string())
        .unwrap_or_default()
}

/// Validates the form and converts it to a DB row. Pure (no DB access).
fn validate(input: WindowInput) -> (WindowForm, Option<NewWindow>) {
    let mut f = WindowForm {
        title: input.title.trim().to_string(),
        description: input.description.trim().to_string(),
        strategy: Strategy::parse(&input.strategy)
            .map_or("manual", |s| s.as_str())
            .to_string(),
        active: input.active,
        timezone: input.timezone.trim().to_string(),
        start_at: input.start_at.trim().to_string(),
        end_at: input.end_at.trim().to_string(),
        time_from: input.time_from.trim().to_string(),
        time_to: input.time_to.trim().to_string(),
        weekdays: maintenance::parse_weekdays(
            &input
                .weekday
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
        interval_days: input.interval_days.trim().to_string(),
        month_days: input.month_days.trim().to_string(),
        cron: input.cron.split_whitespace().collect::<Vec<_>>().join(" "),
        duration_minutes: input.duration_minutes.trim().to_string(),
        valid_from: input.valid_from.trim().to_string(),
        valid_until: input.valid_until.trim().to_string(),
        monitor_ids: input.monitor_id,
        status_page_ids: input.status_page_id,
        ..WindowForm::default()
    };
    f.monitor_ids.sort_unstable();
    f.monitor_ids.dedup();
    f.status_page_ids.sort_unstable();
    f.status_page_ids.dedup();

    if f.title.is_empty() {
        f.title_error = crate::i18n::t("mw.err_title").into();
    } else if f.title.chars().count() > 200 {
        f.title_error = crate::i18n::t("inc.err_title_long").into();
    }
    if f.description.chars().count() > db_maint::DESCRIPTION_MAX {
        f.description_error = crate::i18n::tf(
            "mw.err_description_long",
            &[(
                "max",
                &crate::i18n::fmt_int(db_maint::DESCRIPTION_MAX as i64),
            )],
        );
    }
    if f.timezone.is_empty() {
        f.timezone = "UTC".into();
    }
    let tz = match TimeZone::get(&f.timezone) {
        Ok(tz) => tz,
        Err(_) => {
            f.timezone_error = crate::i18n::t("mw.err_tz").into();
            TimeZone::UTC
        }
    };
    let strategy = Strategy::parse(&f.strategy).unwrap_or(Strategy::Manual);
    let mut row = NewWindow {
        title: f.title.clone(),
        description: f.description.clone(),
        strategy: strategy.as_str().into(),
        active: f.active,
        timezone: f.timezone.clone(),
        monitor_ids: f.monitor_ids.clone(),
        status_page_ids: f.status_page_ids.clone(),
        ..NewWindow::default()
    };
    match strategy {
        Strategy::Manual => {}
        Strategy::Once => {
            let start = maintenance::parse_local_datetime(&f.start_at, &tz);
            let end = maintenance::parse_local_datetime(&f.end_at, &tz);
            match (start, end) {
                (Some(s), Some(e)) if e > s => {
                    row.start_at = Some(maintenance::fmt_db_ts(s));
                    row.end_at = Some(maintenance::fmt_db_ts(e));
                }
                (Some(_), Some(_)) => f.schedule_error = crate::i18n::t("mw.err_end_after").into(),
                _ => f.schedule_error = crate::i18n::t("mw.err_start_end").into(),
            }
        }
        Strategy::Daily
        | Strategy::Weekly
        | Strategy::Interval
        | Strategy::Monthly
        | Strategy::Cron => {
            if strategy.uses_times() {
                match (
                    maintenance::parse_hhmm(&f.time_from),
                    maintenance::parse_hhmm(&f.time_to),
                ) {
                    (Some(a), Some(b)) => {
                        row.time_from = Some(a.strftime("%H:%M").to_string());
                        row.time_to = Some(b.strftime("%H:%M").to_string());
                    }
                    _ => f.schedule_error = crate::i18n::t("mw.err_times").into(),
                }
            }
            match strategy {
                Strategy::Interval => match f.interval_days.parse::<i64>() {
                    Ok(n) if (1..=366).contains(&n) => row.interval_days = Some(n),
                    _ => f.schedule_error = crate::i18n::t("mw.err_interval").into(),
                },
                Strategy::Monthly => match maintenance::parse_month_days_strict(&f.month_days) {
                    Ok(days) if !days.is_empty() => {
                        row.month_days = Some(maintenance::fmt_month_days(&days))
                    }
                    _ => f.schedule_error = crate::i18n::t("mw.err_month_days").into(),
                },
                Strategy::Cron => {
                    if maintenance::CronSpec::parse(&f.cron).is_err() {
                        f.schedule_error = crate::i18n::t("mw.err_cron").into();
                    } else {
                        row.cron = Some(f.cron.clone());
                    }
                    match f.duration_minutes.parse::<i64>() {
                        Ok(n) if (1..=maintenance::MAX_DURATION_MINUTES).contains(&n) => {
                            row.duration_minutes = Some(n)
                        }
                        _ if f.schedule_error.is_empty() => {
                            f.schedule_error = crate::i18n::tf(
                                "mw.err_duration",
                                &[(
                                    "max",
                                    &crate::i18n::fmt_int(maintenance::MAX_DURATION_MINUTES),
                                )],
                            )
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            if strategy == Strategy::Weekly {
                if f.weekdays.is_empty() && f.schedule_error.is_empty() {
                    f.schedule_error = crate::i18n::t("mw.err_days").into();
                }
                row.weekdays = Some(
                    f.weekdays
                        .iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            if strategy == Strategy::Interval
                && f.valid_from.is_empty()
                && f.schedule_error.is_empty()
            {
                f.schedule_error = crate::i18n::t("mw.err_interval_start").into();
            }
            if !f.valid_from.is_empty() {
                match date_start(&f.valid_from, &tz, 0) {
                    Some(t) => row.start_at = Some(maintenance::fmt_db_ts(t)),
                    None => f.schedule_error = crate::i18n::t("mw.err_first_date").into(),
                }
            }
            if !f.valid_until.is_empty() {
                match date_start(&f.valid_until, &tz, 1) {
                    Some(t) => row.end_at = Some(maintenance::fmt_db_ts(t)),
                    None => f.schedule_error = crate::i18n::t("mw.err_last_date").into(),
                }
            }
            if let (Some(a), Some(b)) = (&row.start_at, &row.end_at)
                && b <= a
                && f.schedule_error.is_empty()
            {
                f.schedule_error = crate::i18n::t("mw.err_date_order").into();
            }
        }
    }
    let ok = f.title_error.is_empty()
        && f.description_error.is_empty()
        && f.schedule_error.is_empty()
        && f.timezone_error.is_empty();
    (f, ok.then_some(row))
}

/// The next few runs of a saved recurring window, formatted in its time zone.
fn next_runs(w: &MaintenanceWindow, tz: &TimeZone) -> Vec<String> {
    let schedule = Schedule::from_window(w);
    if !schedule.strategy.is_recurring() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut after = Timestamp::now();
    for _ in 0..3 {
        let Some((start, end)) = maintenance::next_occurrence(&schedule, after) else {
            break;
        };
        out.push(format!(
            "{} → {}",
            crate::i18n::fmt_short_datetime(&start.to_zoned(tz.clone())),
            crate::i18n::fmt_short_datetime(&end.to_zoned(tz.clone()))
        ));
        after = end;
    }
    out
}

impl WindowForm {
    fn from_window(
        w: &MaintenanceWindow,
        monitor_ids: Vec<i64>,
        status_page_ids: Vec<i64>,
    ) -> Self {
        let tz = TimeZone::get(&w.timezone).unwrap_or(TimeZone::UTC);
        let ts = |s: &Option<String>| s.as_deref().and_then(maintenance::parse_db_ts);
        let recurring = Strategy::parse(&w.strategy).is_some_and(Strategy::is_recurring);
        let (start_at, end_at, valid_from, valid_until) = if recurring {
            (
                String::new(),
                String::new(),
                ts(&w.start_at)
                    .map(|t| local_date(t, &tz, 0))
                    .unwrap_or_default(),
                ts(&w.end_at)
                    .map(|t| local_date(t, &tz, -1))
                    .unwrap_or_default(),
            )
        } else {
            (
                ts(&w.start_at)
                    .map(|t| maintenance::fmt_local_input(t, &tz))
                    .unwrap_or_default(),
                ts(&w.end_at)
                    .map(|t| maintenance::fmt_local_input(t, &tz))
                    .unwrap_or_default(),
                String::new(),
                String::new(),
            )
        };
        WindowForm {
            title: w.title.clone(),
            description: w.description.clone(),
            strategy: w.strategy.clone(),
            active: w.active,
            timezone: w.timezone.clone(),
            start_at,
            end_at,
            time_from: w.time_from.clone().unwrap_or_default(),
            time_to: w.time_to.clone().unwrap_or_default(),
            weekdays: maintenance::parse_weekdays(w.weekdays.as_deref().unwrap_or("")),
            interval_days: w.interval_days.map(|n| n.to_string()).unwrap_or_default(),
            month_days: w.month_days.clone().unwrap_or_default(),
            cron: w.cron.clone().unwrap_or_default(),
            duration_minutes: w
                .duration_minutes
                .map(|n| n.to_string())
                .unwrap_or_default(),
            next_runs: next_runs(w, &tz),
            valid_from,
            valid_until,
            monitor_ids,
            status_page_ids,
            ..WindowForm::default()
        }
    }
}

#[derive(Template)]
#[template(path = "maintenance_form.html")]
struct FormTemplate {
    ctx: PageCtx,
    window_id: i64,
    form: WindowForm,
    weekdays: Vec<WeekdayOpt>,
    timezones: Vec<String>,
    pages: Vec<PageOpt>,
    picker: Picker,
}

fn timezones() -> Vec<String> {
    let mut names: Vec<String> = jiff::tz::db()
        .available()
        .map(|n| n.as_str().to_string())
        .filter(|n| n.contains('/') || n == "UTC")
        .collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        names.push("UTC".into());
    }
    names
}

async fn render_form(
    state: &AppState,
    user: &CurrentUser,
    jar: SignedCookieJar,
    window_id: i64,
    mut form: WindowForm,
) -> ApiResult<(SignedCookieJar, HtmlTemplate<FormTemplate>)> {
    let checked = form.monitor_ids.clone();
    let selected_pages = form.status_page_ids.clone();
    let (picker, pages) = state
        .with_conn(move |c| {
            let picker = build_picker(c, "mw-monitors", "", &checked, &[])?;
            let pages = crate::db::status_pages::list(c)?
                .into_iter()
                .map(|s| PageOpt {
                    checked: selected_pages.contains(&s.page.id),
                    id: s.page.id,
                    title: s.page.title,
                })
                .collect();
            Ok((picker, pages))
        })
        .await?;
    let mut tzs = timezones();
    if !tzs.contains(&form.timezone) {
        if form.timezone_error.is_empty() && !form.timezone.is_empty() {
            tzs.insert(0, form.timezone.clone());
        } else {
            form.timezone = "UTC".into();
        }
    }
    let weekdays = WEEKDAYS
        .iter()
        .map(|value| WeekdayOpt {
            value: *value,
            label: crate::i18n::weekday_name(*value, false),
            checked: form.weekdays.contains(value),
        })
        .collect();
    let title = if window_id == 0 {
        crate::i18n::t("mw.new_title").to_string()
    } else {
        crate::i18n::tf("monitor_form.edit_heading", &[("name", &form.title)])
    };
    let (ctx, jar) = PageCtx::build(state, Some(user), &jar, title, "maintenance", "").await;
    Ok((
        jar,
        HtmlTemplate(FormTemplate {
            ctx,
            window_id,
            form,
            weekdays,
            timezones: tzs,
            pages,
            picker,
        }),
    ))
}

async fn new_form(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let settings = state.settings().await?;
    let tz = super::public_status::site_tz(&settings.timezone);
    let now = Timestamp::now();
    // Suggest tonight 22:00–23:00 local time.
    let today = now.to_zoned(tz.clone()).date();
    let start = today
        .at(22, 0, 0, 0)
        .to_zoned(tz.clone())
        .map(|z| z.timestamp())
        .unwrap_or(now);
    let form = WindowForm {
        strategy: "once".into(),
        active: true,
        timezone: tz.iana_name().unwrap_or("UTC").to_string(),
        start_at: maintenance::fmt_local_input(start, &tz),
        end_at: maintenance::fmt_local_input(start + jiff::SignedDuration::from_hours(1), &tz),
        time_from: "22:00".into(),
        time_to: "23:00".into(),
        ..WindowForm::default()
    };
    render_form(&state, &user, jar, 0, form).await
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    axum_extra::extract::Form(input): axum_extra::extract::Form<WindowInput>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let (form, row) = validate(input);
    let Some(row) = row else {
        let (jar, tpl) = render_form(&state, &user, jar, 0, form).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, tpl).into_response());
    };
    state.with_conn(move |c| db_maint::insert(c, &row)).await?;
    changed(&state);
    let jar = flash(jar, "success", crate::i18n::t("mw.saved"));
    Ok((jar, Redirect::to("/maintenance")).into_response())
}

async fn edit_form(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let (w, monitors, pages) = state
        .with_conn(move |c| {
            let w = db_maint::get(c, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            Ok((
                w,
                db_maint::monitor_ids(c, id)?,
                db_maint::status_page_ids(c, id)?,
            ))
        })
        .await?;
    let form = WindowForm::from_window(&w, monitors, pages);
    render_form(&state, &user, jar, id, form).await
}

async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    axum_extra::extract::Form(input): axum_extra::extract::Form<WindowInput>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let (form, row) = validate(input);
    let Some(row) = row else {
        let (jar, tpl) = render_form(&state, &user, jar, id, form).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, tpl).into_response());
    };
    if !state
        .with_conn(move |c| db_maint::update(c, id, &row))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    changed(&state);
    let jar = flash(jar, "success", crate::i18n::t("mw.saved"));
    Ok((jar, Redirect::to("/maintenance")).into_response())
}

async fn delete_window(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    if !state.with_conn(move |c| db_maint::delete(c, id)).await? {
        return Err(ApiError::NotFound);
    }
    changed(&state);
    let jar = flash(jar, "success", crate::i18n::t("mw.deleted"));
    Ok((jar, [("hx-redirect", "/maintenance")], StatusCode::OK).into_response())
}

/// Start / end / pause / resume.
async fn action(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, action)): Path<(i64, String)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let message = state
        .with_conn(move |c| {
            let w = db_maint::get(c, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let sched = Schedule::from_window(&w);
            let now = Timestamp::now();
            let now_text = maintenance::fmt_db_ts(now);
            let st = maintenance::window_state(&sched, now);
            Ok(match (action.as_str(), sched.strategy, st) {
                ("start", Strategy::Manual, _) => {
                    db_maint::set_active(c, id, true)?;
                    Ok(crate::i18n::t("mw.started"))
                }
                ("start", Strategy::Once, WindowState::Scheduled) => {
                    db_maint::set_times(c, id, Some(&now_text), w.end_at.as_deref())?;
                    Ok(crate::i18n::t("mw.started"))
                }
                ("end", Strategy::Manual, _) => {
                    db_maint::set_active(c, id, false)?;
                    Ok(crate::i18n::t("mw.ended"))
                }
                ("end", Strategy::Once, WindowState::Active) => {
                    db_maint::set_times(c, id, w.start_at.as_deref(), Some(&now_text))?;
                    Ok(crate::i18n::t("mw.ended"))
                }
                ("end", _, WindowState::Active) => {
                    db_maint::set_skip_until(c, id, Some(&now_text))?;
                    Ok(crate::i18n::t("mw.ended_recurring"))
                }
                ("pause", s, _) if s != Strategy::Manual => {
                    db_maint::set_active(c, id, false)?;
                    Ok(crate::i18n::t("mw.paused"))
                }
                ("resume", s, _) if s != Strategy::Manual => {
                    db_maint::set_active(c, id, true)?;
                    Ok(crate::i18n::t("mw.resumed"))
                }
                _ => Err(()),
            })
        })
        .await?
        .map_err(|()| ApiError::bad_request(crate::i18n::t("mw.err_action")))?;
    changed(&state);
    let jar = flash(jar, "success", message);
    Ok((jar, [("hx-refresh", "true")], StatusCode::OK).into_response())
}

fn changed(state: &AppState) {
    maintenance::invalidate();
    state.notify_update();
}

#[derive(Template)]
#[template(path = "partials/status_picker_list.html")]
struct PickerList {
    picker: Picker,
}

async fn monitor_picker(
    State(state): State<AppState>,
    user: CurrentUser,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let pairs = form_pairs(query.unwrap_or_default().as_bytes());
    let q = pairs
        .iter()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let checked: Vec<i64> = pairs
        .iter()
        .filter(|(k, _)| k == "monitor_id")
        .filter_map(|(_, v)| v.parse().ok())
        .collect();
    let picker = state
        .with_conn(move |c| build_picker(c, "mw-monitors", &q, &checked, &[]))
        .await?;
    Ok(HtmlTemplate(PickerList { picker }).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(strategy: &str) -> WindowInput {
        WindowInput {
            title: " DB upgrade ".into(),
            description: String::new(),
            strategy: strategy.into(),
            active: true,
            timezone: "Europe/Oslo".into(),
            start_at: "2026-07-01T10:00".into(),
            end_at: "2026-07-01T12:00".into(),
            time_from: "22:00".into(),
            time_to: "02:00".into(),
            weekday: vec![5, 1, 5],
            interval_days: String::new(),
            month_days: String::new(),
            cron: String::new(),
            duration_minutes: String::new(),
            valid_from: String::new(),
            valid_until: String::new(),
            monitor_id: vec![3, 1, 3],
            status_page_id: vec![],
        }
    }

    #[test]
    fn once_converts_local_times_to_utc() {
        let (form, row) = validate(input("once"));
        let row = row.expect("valid");
        assert_eq!(form.title, "DB upgrade");
        assert_eq!(row.start_at.as_deref(), Some("2026-07-01 08:00:00"));
        assert_eq!(row.end_at.as_deref(), Some("2026-07-01 10:00:00"));
        assert_eq!(row.monitor_ids, vec![1, 3]);
        assert_eq!(row.time_from, None);
    }

    #[test]
    fn once_requires_end_after_start() {
        let mut i = input("once");
        i.end_at = "2026-07-01T09:00".into();
        let (form, row) = validate(i);
        assert!(row.is_none());
        assert!(!form.schedule_error.is_empty());
    }

    #[test]
    fn weekly_needs_days_and_keeps_them_sorted() {
        let (_, row) = validate(input("weekly"));
        let row = row.unwrap();
        assert_eq!(row.weekdays.as_deref(), Some("1,5"));
        assert_eq!(row.time_from.as_deref(), Some("22:00"));
        let mut i = input("weekly");
        i.weekday.clear();
        assert!(validate(i).1.is_none());
    }

    #[test]
    fn recurring_date_range_round_trips() {
        let mut i = input("daily");
        i.valid_from = "2026-07-01".into();
        i.valid_until = "2026-07-31".into();
        let (_, row) = validate(i);
        let row = row.unwrap();
        // Local midnight in Oslo (UTC+2) and the day after the last date.
        assert_eq!(row.start_at.as_deref(), Some("2026-06-30 22:00:00"));
        assert_eq!(row.end_at.as_deref(), Some("2026-07-31 22:00:00"));
        let w = MaintenanceWindow {
            id: 1,
            title: row.title.clone(),
            description: String::new(),
            strategy: row.strategy.clone(),
            active: true,
            start_at: row.start_at.clone(),
            end_at: row.end_at.clone(),
            weekdays: None,
            time_from: row.time_from.clone(),
            time_to: row.time_to.clone(),
            interval_days: row.interval_days,
            month_days: row.month_days.clone(),
            cron: row.cron.clone(),
            duration_minutes: row.duration_minutes,
            timezone: row.timezone.clone(),
            skip_until: None,
            created_at: String::new(),
        };
        let back = WindowForm::from_window(&w, vec![], vec![]);
        assert_eq!(back.valid_from, "2026-07-01");
        assert_eq!(back.valid_until, "2026-07-31");
        assert_eq!(
            describe_schedule(&w),
            "Every day, 22:00–02:00 (Europe/Oslo)"
        );
    }

    #[test]
    fn bad_times_and_zone_are_reported() {
        let mut i = input("daily");
        i.time_to = "25:00".into();
        i.timezone = "Mars/Olympus".into();
        let (form, row) = validate(i);
        assert!(row.is_none());
        assert!(!form.schedule_error.is_empty());
        assert!(!form.timezone_error.is_empty());
    }

    #[test]
    fn manual_needs_only_a_title() {
        let mut i = input("manual");
        i.start_at.clear();
        let (_, row) = validate(i);
        let row = row.unwrap();
        assert_eq!(row.strategy, "manual");
        assert_eq!(row.start_at, None);
        let mut i = input("bogus");
        i.title = String::new();
        let (form, row) = validate(i);
        assert!(row.is_none());
        assert_eq!(form.strategy, "manual");
    }

    #[test]
    fn timezone_list_has_common_zones() {
        let tz = timezones();
        assert!(tz.contains(&"Europe/Oslo".to_string()));
        assert!(tz.contains(&"UTC".to_string()));
    }

    #[test]
    fn cron_monthly_and_interval_forms_validate() {
        let mut cron = input("cron");
        cron.cron = "  0  2 * * 0 ".into();
        cron.duration_minutes = "90".into();
        let (_, row) = validate(cron.clone());
        let row = row.expect("valid cron");
        assert_eq!(
            (
                row.cron.as_deref(),
                row.duration_minutes,
                row.time_from.clone()
            ),
            (Some("0 2 * * 0"), Some(90), None)
        );
        for (cron_text, minutes) in [
            ("nope", "90"),
            ("0 2 * * *", "0"),
            ("0 2 * * *", "x"),
            ("0 2 * * *", "99999999"),
        ] {
            let mut bad = cron.clone();
            bad.cron = cron_text.into();
            bad.duration_minutes = minutes.into();
            let (form, row) = validate(bad);
            assert!(
                row.is_none() && !form.schedule_error.is_empty(),
                "{cron_text} {minutes}"
            );
        }

        let mut monthly = input("monthly");
        monthly.month_days = "15, 1, last".into();
        let (_, row) = validate(monthly.clone());
        assert_eq!(row.unwrap().month_days.as_deref(), Some("1,15,-1"));
        monthly.month_days = "40".into();
        assert!(validate(monthly).1.is_none());

        let mut every = input("interval");
        every.interval_days = "3".into();
        every.valid_from = "2026-10-01".into();
        let (_, row) = validate(every.clone());
        let row = row.unwrap();
        // Oslo is UTC+2 on 1 October: local midnight is the evening before in UTC.
        assert_eq!(
            (row.interval_days, row.start_at.as_deref()),
            (Some(3), Some("2026-09-30 22:00:00"))
        );
        every.valid_from = String::new();
        assert!(
            validate(every.clone()).1.is_none(),
            "the first day is required"
        );
        every.valid_from = "2026-10-01".into();
        every.interval_days = "0".into();
        assert!(validate(every).1.is_none());
    }

    #[test]
    fn the_new_schedules_describe_themselves_and_preview_the_next_runs() {
        let mut cron = input("cron");
        cron.cron = "0 2 * * 0".into();
        cron.duration_minutes = "90".into();
        let row = validate(cron).1.unwrap();
        let w = MaintenanceWindow {
            id: 1,
            title: row.title.clone(),
            description: String::new(),
            strategy: row.strategy.clone(),
            active: true,
            start_at: None,
            end_at: None,
            weekdays: None,
            time_from: None,
            time_to: None,
            interval_days: row.interval_days,
            month_days: row.month_days.clone(),
            cron: row.cron.clone(),
            duration_minutes: row.duration_minutes,
            timezone: "Europe/Oslo".into(),
            skip_until: None,
            created_at: String::new(),
        };
        assert_eq!(
            describe_schedule(&w),
            "Cron 0 2 * * 0, 90 min each (Europe/Oslo)"
        );
        let form = WindowForm::from_window(&w, vec![], vec![]);
        assert_eq!(form.next_runs.len(), 3);
        assert_eq!(
            (form.cron.as_str(), form.duration_minutes.as_str()),
            ("0 2 * * 0", "90")
        );
        let monthly = MaintenanceWindow {
            strategy: "monthly".into(),
            cron: None,
            duration_minutes: None,
            month_days: Some("1,-1".into()),
            time_from: Some("22:00".into()),
            time_to: Some("23:00".into()),
            ..w.clone()
        };
        assert_eq!(
            describe_schedule(&monthly),
            "Days 1, last of the month, 22:00–23:00 (Europe/Oslo)"
        );
        let every = MaintenanceWindow {
            strategy: "interval".into(),
            cron: None,
            duration_minutes: None,
            interval_days: Some(3),
            start_at: Some("2026-09-30 22:00:00".into()),
            time_from: Some("01:00".into()),
            time_to: Some("02:00".into()),
            ..w
        };
        assert_eq!(
            describe_schedule(&every),
            "Every 3 days from Oct 1, 2026, 01:00–02:00 (Europe/Oslo)"
        );
    }
}
