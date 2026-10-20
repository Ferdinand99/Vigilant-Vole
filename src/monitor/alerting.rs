//! The status / alerting state machine. Owned by the engine agent.
//!
//! Everything here is pure: [`evaluate`] takes the previous [`MonitorState`],
//! one observation (raw check outcome + maintenance flag + agent votes), the
//! monitor's alerting config and the current time, and returns the status to
//! record, the next state, and the side effects to perform (notifications to
//! send, `monitor_events` to open/close). The scheduler performs the effects.
//!
//! Rules, in order:
//! 1. **Maintenance** → record `maintenance`, open a `maintenance` event
//!    (closing an ongoing outage), never notify; failure counters reset. The
//!    `notified` state is kept, so after maintenance an outage that users were
//!    told about still gets its "Up", and a fresh failure gets a "Down".
//! 2. **Upside-down** inverts raw up/down. A raw `pending` (e.g. Docker
//!    health "starting") is never inverted: it records `pending`, doesn't
//!    count toward retries, leaves the settled/notified state and flap
//!    history untouched, and never notifies.
//! 3. **Retries**: the first `retries` consecutive failures record `pending`.
//! 4. **Agent confirmation** (`confirm_with_agents`): a local failure past the
//!    retry budget only becomes `down` when there are no fresh agent results or
//!    a strict majority of them are down; otherwise `pending` with
//!    "Local check failed; N/M remote agents report up".
//! 5. **Flapping** (`flap_detection`): ≥ 4 up/not-up changes within the last 10
//!    checks → one `Flapping` notification + `flapping` event; Down/Up/Reminder
//!    notifications are muted until the result is stable for 5 checks, then
//!    any missed transition is announced.
//! 6. **Settled transitions** (up ↔ down; `pending` never notifies) go to the
//!    regular channels; `Up` carries the downtime. The first-ever `Up` is
//!    silent; the first-ever `Down` notifies.
//! 7. **Reminders** every `resend_interval` down checks after the last notice.
//! 8. **Escalation**: once per outage after `escalation_minutes` continuously
//!    down, to the escalation channels (which also get the recovery `Up`).

use std::time::Duration;

use time::OffsetDateTime;

use super::state::{AgentVotes, FLAP_WINDOW, MonitorState};
use crate::db::events::MonitorEventKind;
use crate::db::models::{EventKind, HeartbeatStatus, Monitor, MonitorType};

/// Up/not-up changes within [`FLAP_WINDOW`] checks that mean "flapping".
pub const FLAP_TRANSITIONS: usize = 4;
/// Consecutive identical results that end flapping.
pub const FLAP_STABLE_CHECKS: i64 = 5;
/// Days-left thresholds (besides the monitor's own `expiry_days`) at which an
/// expiry warning is repeated.
pub const EXPIRY_STEPS: [i64; 4] = [7, 3, 1, 0];

/// The part of a monitor's configuration the state machine reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AlertConfig {
    pub retries: i64,
    pub upside_down: bool,
    pub resend_interval: i64,
    pub escalation_minutes: i64,
    pub flap_detection: bool,
    pub confirm_with_agents: bool,
}

impl AlertConfig {
    /// Group monitors aggregate already-settled child statuses, so retries,
    /// upside-down and agent confirmation don't apply to them; push monitors
    /// have nothing for agents to confirm.
    pub fn for_monitor(m: &Monitor) -> Self {
        let is_group = m.monitor_type == MonitorType::Group;
        Self {
            retries: if is_group { 0 } else { m.retries.max(0) },
            upside_down: !is_group && m.upside_down,
            resend_interval: m.resend_interval.max(0),
            escalation_minutes: m.escalation_minutes.max(0),
            flap_detection: m.flap_detection,
            confirm_with_agents: m.confirm_with_agents && m.monitor_type.is_active_check(),
        }
    }
}

/// One raw observation fed into the state machine.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Observation {
    /// Raw status as produced by the checker / push / group aggregation
    /// (before upside-down). `Maintenance` is treated like `maintenance: true`.
    pub status: Option<HeartbeatStatus>,
    pub message: Option<String>,
    /// The monitor is inside an active maintenance window.
    pub maintenance: bool,
    /// Fresh remote-agent results, when agent confirmation applies.
    pub agents: Option<AgentVotes>,
}

impl Observation {
    pub fn new(status: HeartbeatStatus, message: Option<String>) -> Self {
        Self {
            status: Some(status),
            message,
            ..Self::default()
        }
    }

    fn raw(&self) -> HeartbeatStatus {
        self.status.unwrap_or(HeartbeatStatus::Pending)
    }
}

/// A notification to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: EventKind,
    /// `true` → the monitor's escalation channels; `false` → its regular channels.
    pub escalation: bool,
    pub message: Option<String>,
    /// On `Up`: how long the monitor was down.
    pub downtime: Option<Duration>,
    /// On `CertExpiring` / `DomainExpiring`.
    pub expires_in_days: Option<i64>,
}

/// A side effect produced by [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Notify(Notice),
    OpenEvent {
        kind: MonitorEventKind,
        message: Option<String>,
        started_at: OffsetDateTime,
    },
    CloseEvent {
        kind: MonitorEventKind,
        ended_at: OffsetDateTime,
    },
}

/// Result of one evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Status to record in `heartbeats`.
    pub status: HeartbeatStatus,
    /// Message to record (may differ from the raw one, e.g. retry/agent notes).
    pub message: Option<String>,
    pub state: MonitorState,
    pub actions: Vec<Action>,
}

impl Decision {
    pub fn notices(&self) -> impl Iterator<Item = &Notice> {
        self.actions.iter().filter_map(|a| match a {
            Action::Notify(n) => Some(n),
            _ => None,
        })
    }
}

fn elapsed(from: OffsetDateTime, to: OffsetDateTime) -> Duration {
    (to - from).max(time::Duration::ZERO).unsigned_abs()
}

fn notify(
    actions: &mut Vec<Action>,
    kind: EventKind,
    escalation: bool,
    message: Option<String>,
    downtime: Option<Duration>,
) {
    actions.push(Action::Notify(Notice {
        kind,
        escalation,
        message,
        downtime,
        expires_in_days: None,
    }));
}

/// Applies one observation to `prev`. See the module docs for the rules.
pub fn evaluate(prev: &MonitorState, obs: &Observation, cfg: &AlertConfig, now: OffsetDateTime) -> Decision {
    let mut s = prev.clone();
    let mut actions = Vec::new();

    // 1. Maintenance.
    if obs.maintenance || obs.raw() == HeartbeatStatus::Maintenance {
        if s.settled != Some(HeartbeatStatus::Maintenance) {
            if s.settled == Some(HeartbeatStatus::Down) {
                actions.push(Action::CloseEvent {
                    kind: MonitorEventKind::Down,
                    ended_at: now,
                });
            }
            if s.flapping {
                s.flapping = false;
                actions.push(Action::CloseEvent {
                    kind: MonitorEventKind::Flapping,
                    ended_at: now,
                });
            }
            actions.push(Action::OpenEvent {
                kind: MonitorEventKind::Maintenance,
                message: None,
                started_at: now,
            });
            s.settled = Some(HeartbeatStatus::Maintenance);
        }
        s.consecutive_failures = 0;
        s.failing_since = None;
        s.down_since = None;
        s.checks_since_notice = 0;
        s.escalated = false;
        s.history.clear();
        s.stable_checks = 0;
        let message = obs.message.clone().or_else(|| Some("Under maintenance".into()));
        return finish(s, HeartbeatStatus::Maintenance, message, actions, now);
    }
    if s.settled == Some(HeartbeatStatus::Maintenance) {
        actions.push(Action::CloseEvent {
            kind: MonitorEventKind::Maintenance,
            ended_at: now,
        });
        s.settled = None;
    }

    // 2. Upside-down.
    let raw = match (obs.raw(), cfg.upside_down) {
        (HeartbeatStatus::Up, true) => HeartbeatStatus::Down,
        (HeartbeatStatus::Down, true) => HeartbeatStatus::Up,
        (other, _) => other,
    };
    let mut message = match (obs.raw(), cfg.upside_down, &obs.message) {
        (HeartbeatStatus::Up, true, Some(m)) => Some(format!("Upside-down mode: check succeeded ({m})")),
        (HeartbeatStatus::Up, true, None) => Some("Upside-down mode: check succeeded".into()),
        (_, _, m) => m.clone(),
    };

    // 3./4. Retries and agent confirmation.
    let status = match raw {
        HeartbeatStatus::Up => {
            s.consecutive_failures = 0;
            s.failing_since = None;
            HeartbeatStatus::Up
        }
        HeartbeatStatus::Down => {
            s.consecutive_failures += 1;
            s.failing_since.get_or_insert(now);
            if s.consecutive_failures <= cfg.retries {
                let note = format!("retry {}/{}", s.consecutive_failures, cfg.retries);
                message = Some(match message {
                    Some(m) => format!("{m} ({note})"),
                    None => note,
                });
                HeartbeatStatus::Pending
            } else if let Some(text) = unconfirmed_by_agents(cfg, obs.agents) {
                message = Some(text);
                HeartbeatStatus::Pending
            } else {
                HeartbeatStatus::Down
            }
        }
        _ => HeartbeatStatus::Pending,
    };

    // 5. Flap detection (before notifications so a flap that ends now can
    // announce the transition it muted).
    // A raw `pending` (e.g. a container still starting) is "no verdict": it
    // doesn't enter the flap history, so up → starting → up isn't a flap.
    let is_up = status == HeartbeatStatus::Up;
    if raw != HeartbeatStatus::Pending {
        s.stable_checks = if s.history.back() == Some(&is_up) {
            s.stable_checks + 1
        } else {
            1
        };
        s.history.push_back(is_up);
        while s.history.len() > FLAP_WINDOW {
            s.history.pop_front();
        }
    }
    if !cfg.flap_detection {
        if s.flapping {
            s.flapping = false;
            actions.push(Action::CloseEvent {
                kind: MonitorEventKind::Flapping,
                ended_at: now,
            });
        }
    } else if s.flapping {
        if s.stable_checks >= FLAP_STABLE_CHECKS {
            s.flapping = false;
            s.history.clear();
            s.history.push_back(is_up);
            actions.push(Action::CloseEvent {
                kind: MonitorEventKind::Flapping,
                ended_at: now,
            });
        }
    } else if s.transitions() >= FLAP_TRANSITIONS {
        s.flapping = true;
        let text = format!(
            "Changed state {} times in the last {} checks",
            s.transitions(),
            s.history.len()
        );
        actions.push(Action::OpenEvent {
            kind: MonitorEventKind::Flapping,
            message: Some(text.clone()),
            started_at: now,
        });
        notify(&mut actions, EventKind::Flapping, false, Some(text), None);
    }

    // 6a. Settled state → outage events.
    let was_escalated = s.escalated;
    match status {
        HeartbeatStatus::Down if s.settled != Some(HeartbeatStatus::Down) => {
            let since = s.failing_since.unwrap_or(now);
            s.settled = Some(HeartbeatStatus::Down);
            s.down_since = Some(since);
            s.escalated = false;
            actions.push(Action::OpenEvent {
                kind: MonitorEventKind::Down,
                message: message.clone(),
                started_at: since,
            });
        }
        HeartbeatStatus::Up => {
            if s.settled == Some(HeartbeatStatus::Down) {
                actions.push(Action::CloseEvent {
                    kind: MonitorEventKind::Down,
                    ended_at: now,
                });
            }
            s.settled = Some(HeartbeatStatus::Up);
            s.down_since = None;
            s.escalated = false;
        }
        _ => {}
    }

    // 6b./7./8. Notifications.
    let muted = s.flapping;
    match status {
        HeartbeatStatus::Down => {
            if s.notified != Some(HeartbeatStatus::Down) {
                if !muted {
                    notify(&mut actions, EventKind::Down, false, message.clone(), None);
                    s.notified = Some(HeartbeatStatus::Down);
                    s.notified_down_since = s.down_since;
                    s.checks_since_notice = 0;
                }
            } else if !muted && cfg.resend_interval > 0 {
                s.checks_since_notice += 1;
                if s.checks_since_notice >= cfg.resend_interval {
                    s.checks_since_notice = 0;
                    notify(&mut actions, EventKind::Reminder, false, message.clone(), None);
                }
            }
            if cfg.escalation_minutes > 0
                && !s.escalated
                && s.down_since.is_some_and(|since| {
                    elapsed(since, now) >= Duration::from_secs(cfg.escalation_minutes.unsigned_abs() * 60)
                })
            {
                s.escalated = true;
                notify(&mut actions, EventKind::Escalation, true, message.clone(), None);
            }
        }
        HeartbeatStatus::Up => {
            if s.notified == Some(HeartbeatStatus::Down) {
                if !muted {
                    let downtime = s.notified_down_since.map(|since| elapsed(since, now));
                    notify(&mut actions, EventKind::Up, false, message.clone(), downtime);
                    if was_escalated {
                        notify(&mut actions, EventKind::Up, true, message.clone(), downtime);
                    }
                    s.notified = Some(HeartbeatStatus::Up);
                    s.notified_down_since = None;
                    s.checks_since_notice = 0;
                }
            } else if !muted {
                s.notified = Some(HeartbeatStatus::Up);
            }
        }
        _ => {}
    }

    finish(s, status, message, actions, now)
}

fn finish(
    mut s: MonitorState,
    status: HeartbeatStatus,
    message: Option<String>,
    actions: Vec<Action>,
    now: OffsetDateTime,
) -> Decision {
    if s.last_status != Some(status) {
        s.status_since = Some(now);
    }
    s.last_status = Some(status);
    s.last_recorded_at = Some(now);
    Decision {
        status,
        message,
        state: s,
        actions,
    }
}

/// `Some(message)` when agent confirmation is on, fresh agent results exist,
/// and no strict majority of them is down — i.e. the local failure is not confirmed.
fn unconfirmed_by_agents(cfg: &AlertConfig, votes: Option<AgentVotes>) -> Option<String> {
    let votes = votes.filter(|_| cfg.confirm_with_agents)?;
    let total = votes.total();
    if total == 0 || votes.down * 2 > total {
        return None;
    }
    Some(format!(
        "Local check failed; {}/{} remote agents report up",
        votes.up, total
    ))
}

// ---------------------------------------------------------------------------
// Group monitors
// ---------------------------------------------------------------------------

/// Status of a group from its active children's latest statuses: down >
/// pending > up; only-maintenance → maintenance. Children without any
/// heartbeat yet are ignored; `None` when no child has a status.
pub fn aggregate_group(children: &[(String, Option<HeartbeatStatus>)]) -> Option<(HeartbeatStatus, String)> {
    let known: Vec<(&str, HeartbeatStatus)> = children
        .iter()
        .filter_map(|(name, status)| status.map(|s| (name.as_str(), s)))
        .collect();
    if known.is_empty() {
        return None;
    }
    let names_with = |wanted: HeartbeatStatus| -> Vec<&str> {
        known.iter().filter(|(_, s)| *s == wanted).map(|(n, _)| *n).collect()
    };
    let total = known.len();
    let list = |names: &[&str]| {
        const SHOWN: usize = 5;
        let mut text = names.iter().take(SHOWN).copied().collect::<Vec<_>>().join(", ");
        if names.len() > SHOWN {
            text.push_str(&format!(" and {} more", names.len() - SHOWN));
        }
        text
    };
    let down = names_with(HeartbeatStatus::Down);
    if !down.is_empty() {
        return Some((
            HeartbeatStatus::Down,
            format!("{} of {total} down: {}", down.len(), list(&down)),
        ));
    }
    let pending = names_with(HeartbeatStatus::Pending);
    if !pending.is_empty() {
        return Some((
            HeartbeatStatus::Pending,
            format!("{} of {total} pending: {}", pending.len(), list(&pending)),
        ));
    }
    let up = names_with(HeartbeatStatus::Up).len();
    if up > 0 {
        let maint = total - up;
        let text = if maint == 0 {
            format!("All {up} monitors up")
        } else {
            format!("{up} up, {maint} in maintenance")
        };
        return Some((HeartbeatStatus::Up, text));
    }
    Some((HeartbeatStatus::Maintenance, "All monitors in maintenance".into()))
}

// ---------------------------------------------------------------------------
// Expiry warnings
// ---------------------------------------------------------------------------

/// Whole days until `expires_at` (negative once expired; floor division).
pub fn days_left(expires_at: OffsetDateTime, now: OffsetDateTime) -> i64 {
    (expires_at - now).whole_seconds().div_euclid(86_400)
}

/// The warning step `days_left` falls into: the smallest threshold of
/// `{expiry_days, 7, 3, 1, 0}` (those ≤ `expiry_days`) that is ≥ `days_left`.
/// `None` outside the warning window.
pub fn expiry_step(days_left: i64, expiry_days: i64) -> Option<i64> {
    if expiry_days < 0 || days_left > expiry_days {
        return None;
    }
    std::iter::once(expiry_days)
        .chain(EXPIRY_STEPS.into_iter().filter(|t| *t < expiry_days))
        .filter(|t| days_left <= *t)
        .min()
}

/// Days left to report when a warning is due now: inside the window and in
/// a lower step than the one last notified (`notified_at`). `None` otherwise.
pub fn expiry_due(
    expires_at: OffsetDateTime,
    notified_at: Option<OffsetDateTime>,
    expiry_days: i64,
    now: OffsetDateTime,
) -> Option<i64> {
    let left = days_left(expires_at, now);
    let step = expiry_step(left, expiry_days)?;
    let previous = notified_at.and_then(|at| expiry_step(days_left(expires_at, at), expiry_days));
    match previous {
        Some(prev) if step >= prev => None,
        _ => Some(left),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::state::parse_db_time;
    use HeartbeatStatus::{Down, Maintenance, Pending, Up};

    fn t0() -> OffsetDateTime {
        parse_db_time("2026-03-10 12:00:00").unwrap()
    }

    fn at(minutes: i64) -> OffsetDateTime {
        t0() + time::Duration::minutes(minutes)
    }

    fn cfg() -> AlertConfig {
        AlertConfig {
            flap_detection: true,
            ..AlertConfig::default()
        }
    }

    fn obs(status: HeartbeatStatus) -> Observation {
        Observation::new(status, None)
    }

    /// Runs a sequence of raw statuses one minute apart; returns the final
    /// state and every decision.
    fn run(start: MonitorState, cfg: &AlertConfig, seq: &[HeartbeatStatus]) -> (MonitorState, Vec<Decision>) {
        let mut state = start;
        let mut out = Vec::new();
        for (i, status) in seq.iter().enumerate() {
            let d = evaluate(&state, &obs(*status), cfg, at(i as i64));
            state = d.state.clone();
            out.push(d);
        }
        (state, out)
    }

    fn kinds(decisions: &[Decision]) -> Vec<(EventKind, bool)> {
        decisions
            .iter()
            .flat_map(|d| d.notices().map(|n| (n.kind, n.escalation)).collect::<Vec<_>>())
            .collect()
    }

    fn up_state() -> MonitorState {
        MonitorState {
            settled: Some(Up),
            notified: Some(Up),
            last_status: Some(Up),
            history: [true].into_iter().collect(),
            stable_checks: 1,
            ..MonitorState::default()
        }
    }

    #[test]
    fn first_up_is_recorded_silently() {
        let (state, ds) = run(MonitorState::default(), &cfg(), &[Up]);
        assert_eq!(ds[0].status, Up);
        assert!(kinds(&ds).is_empty());
        assert_eq!(state.notified, Some(Up));
    }

    #[test]
    fn first_down_notifies_and_opens_outage() {
        let (_, ds) = run(MonitorState::default(), &cfg(), &[Down]);
        assert_eq!(kinds(&ds), [(EventKind::Down, false)]);
        assert!(ds[0].actions.iter().any(|a| matches!(
            a,
            Action::OpenEvent {
                kind: MonitorEventKind::Down,
                ..
            }
        )));
    }

    #[test]
    fn restart_seeded_as_up_does_not_send_spurious_up() {
        let (_, ds) = run(up_state(), &cfg(), &[Up, Up]);
        assert!(kinds(&ds).is_empty());
    }

    #[test]
    fn retries_record_pending_before_down() {
        let c = AlertConfig { retries: 2, ..cfg() };
        let (_, ds) = run(up_state(), &c, &[Down, Down, Down]);
        let statuses: Vec<_> = ds.iter().map(|d| d.status).collect();
        assert_eq!(statuses, [Pending, Pending, Down]);
        assert_eq!(ds[0].message.as_deref(), Some("retry 1/2"));
        assert_eq!(kinds(&ds), [(EventKind::Down, false)]);
    }

    #[test]
    fn recovery_within_retries_is_silent() {
        let c = AlertConfig { retries: 2, ..cfg() };
        let (_, ds) = run(up_state(), &c, &[Down, Up]);
        assert!(kinds(&ds).is_empty());
        assert!(ds.iter().all(|d| d.actions.is_empty()));
    }

    #[test]
    fn outage_starts_at_first_failed_check() {
        let c = AlertConfig { retries: 2, ..cfg() };
        let (state, _) = run(up_state(), &c, &[Down, Down, Down]);
        assert_eq!(state.down_since, Some(at(0)));
    }

    #[test]
    fn up_after_down_carries_downtime_and_closes_outage() {
        let (state, ds) = run(up_state(), &cfg(), &[Down, Down, Down, Up]);
        let up = ds[3].notices().next().unwrap();
        assert_eq!(up.kind, EventKind::Up);
        assert_eq!(up.downtime, Some(Duration::from_secs(180)));
        assert!(ds[3].actions.iter().any(|a| matches!(
            a,
            Action::CloseEvent {
                kind: MonitorEventKind::Down,
                ..
            }
        )));
        assert_eq!(state.notified, Some(Up));
    }

    #[test]
    fn upside_down_inverts_status() {
        let c = AlertConfig {
            upside_down: true,
            ..cfg()
        };
        let (_, ds) = run(MonitorState::default(), &c, &[Up, Down]);
        assert_eq!(ds[0].status, Down);
        assert_eq!(ds[0].message.as_deref(), Some("Upside-down mode: check succeeded"));
        assert_eq!(ds[1].status, Up);
    }

    #[test]
    fn pending_raw_status_never_notifies() {
        let (state, ds) = run(up_state(), &cfg(), &[Pending, Pending]);
        assert!(kinds(&ds).is_empty());
        assert_eq!(state.consecutive_failures, 0);
    }

    #[test]
    fn raw_pending_is_recorded_as_is_and_leaves_state_alone() {
        let c = AlertConfig { retries: 1, ..cfg() };
        let (before, _) = run(up_state(), &c, &[Down]); // one retry used
        let d = evaluate(&before, &obs(Pending), &c, at(5));
        assert_eq!(d.status, Pending);
        assert!(d.actions.is_empty());
        assert_eq!(d.state.consecutive_failures, before.consecutive_failures);
        assert_eq!((d.state.settled, d.state.notified), (before.settled, before.notified));
        assert_eq!(d.state.history, before.history);
    }

    #[test]
    fn raw_pending_is_not_inverted_by_upside_down() {
        let c = AlertConfig {
            upside_down: true,
            ..cfg()
        };
        let d = evaluate(&up_state(), &obs(Pending), &c, t0());
        assert_eq!(d.status, Pending);
        assert!(d.notices().next().is_none());
    }

    #[test]
    fn pending_does_not_interrupt_a_down_streak_or_resend_down() {
        let (down, _) = run(up_state(), &cfg(), &[Down]);
        let (_, ds) = run(down, &cfg(), &[Pending, Down]);
        assert!(kinds(&ds).is_empty(), "still the same outage: no second Down");
    }

    #[test]
    fn starting_containers_do_not_count_as_flapping() {
        let (state, ds) = run(
            up_state(),
            &cfg(),
            &[Pending, Up, Pending, Up, Pending, Up, Pending, Up],
        );
        assert!(!state.flapping);
        assert!(kinds(&ds).is_empty());
    }

    #[test]
    fn reminders_every_resend_interval_down_checks() {
        let c = AlertConfig {
            resend_interval: 2,
            ..cfg()
        };
        let (_, ds) = run(up_state(), &c, &[Down, Down, Down, Down, Down]);
        assert_eq!(
            kinds(&ds),
            [
                (EventKind::Down, false),
                (EventKind::Reminder, false),
                (EventKind::Reminder, false)
            ]
        );
        assert_eq!(ds[2].notices().next().map(|n| n.kind), Some(EventKind::Reminder));
    }

    #[test]
    fn no_reminders_when_resend_interval_is_zero() {
        let (_, ds) = run(up_state(), &cfg(), &[Down; 6]);
        assert_eq!(kinds(&ds), [(EventKind::Down, false)]);
    }

    #[test]
    fn escalation_fires_once_after_continuous_downtime() {
        let c = AlertConfig {
            escalation_minutes: 3,
            ..cfg()
        };
        let (_, ds) = run(up_state(), &c, &[Down; 7]);
        assert_eq!(kinds(&ds), [(EventKind::Down, false), (EventKind::Escalation, true)]);
        assert_eq!(ds[3].notices().next().map(|n| n.kind), Some(EventKind::Escalation));
    }

    #[test]
    fn escalated_outage_recovery_also_notifies_escalation_channels() {
        let c = AlertConfig {
            escalation_minutes: 1,
            ..cfg()
        };
        let (_, ds) = run(up_state(), &c, &[Down, Down, Up]);
        assert_eq!(kinds(&ds[2..]), [(EventKind::Up, false), (EventKind::Up, true)]);
    }

    #[test]
    fn escalation_timer_resets_after_recovery() {
        let c = AlertConfig {
            escalation_minutes: 2,
            flap_detection: false,
            ..cfg()
        };
        let (_, ds) = run(up_state(), &c, &[Down, Down, Up, Down, Down]);
        assert!(!kinds(&ds).contains(&(EventKind::Escalation, true)));
    }

    #[test]
    fn maintenance_records_maintenance_without_notifying() {
        let mut o = obs(Down);
        o.maintenance = true;
        let d = evaluate(&up_state(), &o, &cfg(), t0());
        assert_eq!(d.status, Maintenance);
        assert_eq!(d.notices().count(), 0);
        assert!(d.actions.iter().any(|a| matches!(
            a,
            Action::OpenEvent {
                kind: MonitorEventKind::Maintenance,
                ..
            }
        )));
    }

    #[test]
    fn maintenance_closes_ongoing_outage_and_reopens_nothing_twice() {
        let (down, _) = run(up_state(), &cfg(), &[Down]);
        let mut o = obs(Down);
        o.maintenance = true;
        let d1 = evaluate(&down, &o, &cfg(), at(5));
        assert!(d1.actions.iter().any(|a| matches!(
            a,
            Action::CloseEvent {
                kind: MonitorEventKind::Down,
                ..
            }
        )));
        let d2 = evaluate(&d1.state, &o, &cfg(), at(6));
        assert!(d2.actions.is_empty());
    }

    #[test]
    fn leaving_maintenance_closes_event_and_up_is_silent() {
        let mut o = obs(Up);
        o.maintenance = true;
        let d1 = evaluate(&up_state(), &o, &cfg(), at(0));
        let d2 = evaluate(&d1.state, &obs(Up), &cfg(), at(1));
        assert!(d2.actions.iter().any(|a| matches!(
            a,
            Action::CloseEvent {
                kind: MonitorEventKind::Maintenance,
                ..
            }
        )));
        assert_eq!(d2.notices().count(), 0);
    }

    #[test]
    fn down_after_maintenance_notifies() {
        let mut o = obs(Up);
        o.maintenance = true;
        let d1 = evaluate(&up_state(), &o, &cfg(), at(0));
        let d2 = evaluate(&d1.state, &obs(Down), &cfg(), at(1));
        assert_eq!(d2.notices().map(|n| n.kind).collect::<Vec<_>>(), [EventKind::Down]);
    }

    #[test]
    fn maintenance_raw_status_counts_as_maintenance() {
        let d = evaluate(&up_state(), &obs(Maintenance), &cfg(), t0());
        assert_eq!(d.status, Maintenance);
    }

    #[test]
    fn agents_reporting_up_keep_local_failure_pending() {
        let c = AlertConfig {
            confirm_with_agents: true,
            ..cfg()
        };
        let mut o = obs(Down);
        o.agents = Some(AgentVotes { up: 2, down: 1 });
        let d = evaluate(&up_state(), &o, &c, t0());
        assert_eq!(d.status, Pending);
        assert_eq!(
            d.message.as_deref(),
            Some("Local check failed; 2/3 remote agents report up")
        );
        assert_eq!(d.notices().count(), 0);
    }

    #[test]
    fn agent_majority_down_confirms_outage() {
        let c = AlertConfig {
            confirm_with_agents: true,
            ..cfg()
        };
        let mut o = obs(Down);
        o.agents = Some(AgentVotes { up: 1, down: 2 });
        assert_eq!(evaluate(&up_state(), &o, &c, t0()).status, Down);
    }

    #[test]
    fn agent_tie_is_not_a_majority() {
        let c = AlertConfig {
            confirm_with_agents: true,
            ..cfg()
        };
        let mut o = obs(Down);
        o.agents = Some(AgentVotes { up: 1, down: 1 });
        assert_eq!(evaluate(&up_state(), &o, &c, t0()).status, Pending);
    }

    #[test]
    fn no_fresh_agent_results_means_local_decides() {
        let c = AlertConfig {
            confirm_with_agents: true,
            ..cfg()
        };
        let mut o = obs(Down);
        o.agents = Some(AgentVotes::default());
        assert_eq!(evaluate(&up_state(), &o, &c, t0()).status, Down);
    }

    #[test]
    fn agent_votes_ignored_when_confirmation_is_off() {
        let mut o = obs(Down);
        o.agents = Some(AgentVotes { up: 3, down: 0 });
        assert_eq!(evaluate(&up_state(), &o, &cfg(), t0()).status, Down);
    }

    #[test]
    fn flapping_sends_one_notification_and_mutes_transitions() {
        let (state, ds) = run(up_state(), &cfg(), &[Down, Up, Down, Up, Down, Up, Down]);
        let k = kinds(&ds);
        // Down, Up, Down (3 transitions so far) then the 4th change trips flapping.
        assert_eq!(
            k,
            [
                (EventKind::Down, false),
                (EventKind::Up, false),
                (EventKind::Down, false),
                (EventKind::Flapping, false)
            ]
        );
        assert!(state.flapping);
    }

    #[test]
    fn flapping_ends_after_five_stable_checks_and_announces_missed_transition() {
        let (flapping, _) = run(up_state(), &cfg(), &[Down, Up, Down, Up]);
        assert!(flapping.flapping);
        assert_eq!(flapping.notified, Some(Down));
        let (state, ds) = run(flapping, &cfg(), &[Up, Up, Up, Up]);
        assert!(!state.flapping);
        // Stable for 5 checks (the flap-triggering Up + 4 more) → Up announced.
        assert_eq!(kinds(&ds), [(EventKind::Up, false)]);
        assert!(ds[3].actions.iter().any(|a| matches!(
            a,
            Action::CloseEvent {
                kind: MonitorEventKind::Flapping,
                ..
            }
        )));
    }

    #[test]
    fn flapping_still_records_outage_events() {
        let (flapping, _) = run(up_state(), &cfg(), &[Down, Up, Down, Up]);
        let d = evaluate(&flapping, &obs(Down), &cfg(), at(10));
        assert_eq!(d.notices().count(), 0);
        assert!(d.actions.iter().any(|a| matches!(
            a,
            Action::OpenEvent {
                kind: MonitorEventKind::Down,
                ..
            }
        )));
    }

    #[test]
    fn flap_detection_off_never_flaps() {
        let c = AlertConfig {
            flap_detection: false,
            ..cfg()
        };
        let (state, ds) = run(up_state(), &c, &[Down, Up, Down, Up, Down, Up]);
        assert!(!state.flapping);
        assert_eq!(kinds(&ds).len(), 6);
    }

    #[test]
    fn disabling_flap_detection_ends_flapping() {
        let (flapping, _) = run(up_state(), &cfg(), &[Down, Up, Down, Up]);
        let c = AlertConfig {
            flap_detection: false,
            ..cfg()
        };
        let d = evaluate(&flapping, &obs(Up), &c, at(10));
        assert!(!d.state.flapping);
        assert!(d.actions.iter().any(|a| matches!(
            a,
            Action::CloseEvent {
                kind: MonitorEventKind::Flapping,
                ..
            }
        )));
    }

    #[test]
    fn status_since_tracks_start_of_current_status() {
        let (state, _) = run(up_state(), &cfg(), &[Up, Down, Down]);
        assert_eq!(state.status_since, Some(at(1)));
        assert_eq!(state.last_recorded_at, Some(at(2)));
    }

    #[test]
    fn group_config_ignores_retries_and_upside_down() {
        let m = Monitor {
            monitor_type: MonitorType::Group,
            retries: 3,
            upside_down: true,
            confirm_with_agents: true,
            ..test_monitor()
        };
        let c = AlertConfig::for_monitor(&m);
        assert_eq!((c.retries, c.upside_down, c.confirm_with_agents), (0, false, false));
    }

    fn test_monitor() -> Monitor {
        let new = crate::db::models::NewMonitor::default();
        Monitor {
            id: 1,
            name: new.name,
            description: new.description,
            monitor_type: new.monitor_type,
            target: new.target,
            parent_id: None,
            interval_seconds: new.interval_seconds,
            retry_interval_seconds: None,
            timeout_seconds: new.timeout_seconds,
            retries: 0,
            active: true,
            upside_down: false,
            http_method: new.http_method,
            http_headers: None,
            http_body: None,
            http_body_encoding: new.http_body_encoding,
            accepted_status_codes: new.accepted_status_codes,
            max_redirects: new.max_redirects,
            ignore_tls: false,
            basic_auth_user: None,
            basic_auth_pass: None,
            secret: None,
            proxy_url: None,
            keyword: None,
            keyword_invert: false,
            json_path: None,
            json_operator: new.json_operator,
            json_expected: None,
            dns_record_type: new.dns_record_type,
            dns_resolver: None,
            dns_expected: None,
            docker_host: None,
            push_token: None,
            expiry_notify: true,
            expiry_days: 14,
            resend_interval: 0,
            escalation_minutes: 0,
            flap_detection: true,
            run_on_agents: false,
            confirm_with_agents: false,
            source: new.source,
            external_id: None,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
            auth_method: new.auth_method,
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
            oauth_audience: None,
            oauth_client_auth: new.oauth_client_auth,
            tls_client_cert: None,
            tls_client_key: None,
            tls_ca_cert: None,
            remote_browser_id: None,
            browser_selector: None,
            browser_wait: new.browser_wait,
            browser_viewport: new.browser_viewport,
            browser_js_errors: false,
            grpc_proto: None,
        }
    }

    // -- groups --------------------------------------------------------------

    fn children(list: &[(&str, Option<HeartbeatStatus>)]) -> Vec<(String, Option<HeartbeatStatus>)> {
        list.iter().map(|(n, s)| (n.to_string(), *s)).collect()
    }

    #[test]
    fn group_is_down_when_any_child_is_down() {
        let c = children(&[("a", Some(Up)), ("b", Some(Down)), ("c", Some(Pending))]);
        assert_eq!(aggregate_group(&c), Some((Down, "1 of 3 down: b".into())));
    }

    #[test]
    fn group_pending_beats_up() {
        let c = children(&[("a", Some(Up)), ("b", Some(Pending))]);
        assert_eq!(aggregate_group(&c).map(|g| g.0), Some(Pending));
    }

    #[test]
    fn group_up_ignores_maintenance_and_unknown_children() {
        let c = children(&[("a", Some(Up)), ("b", Some(Maintenance)), ("c", None)]);
        assert_eq!(aggregate_group(&c), Some((Up, "1 up, 1 in maintenance".into())));
    }

    #[test]
    fn group_all_maintenance_is_maintenance() {
        let c = children(&[("a", Some(Maintenance)), ("b", Some(Maintenance))]);
        assert_eq!(aggregate_group(&c).map(|g| g.0), Some(Maintenance));
    }

    #[test]
    fn group_without_child_statuses_is_unknown() {
        assert_eq!(aggregate_group(&children(&[("a", None)])), None);
        assert_eq!(aggregate_group(&[]), None);
    }

    #[test]
    fn group_message_truncates_long_lists() {
        let names: Vec<String> = (0..7).map(|i| format!("m{i}")).collect();
        let c: Vec<_> = names.iter().map(|n| (n.clone(), Some(Down))).collect();
        let (_, msg) = aggregate_group(&c).unwrap();
        assert_eq!(msg, "7 of 7 down: m0, m1, m2, m3, m4 and 2 more");
    }

    // -- expiry --------------------------------------------------------------

    #[test]
    fn days_left_floors_and_goes_negative() {
        assert_eq!(days_left(t0() + time::Duration::hours(36), t0()), 1);
        assert_eq!(days_left(t0() - time::Duration::hours(1), t0()), -1);
    }

    #[test]
    fn expiry_steps_follow_configured_window() {
        let steps: Vec<_> = [20, 14, 10, 7, 5, 3, 2, 1, 0, -3]
            .iter()
            .map(|d| expiry_step(*d, 14))
            .collect();
        assert_eq!(
            steps,
            [
                None,
                Some(14),
                Some(14),
                Some(7),
                Some(7),
                Some(3),
                Some(3),
                Some(1),
                Some(0),
                Some(0)
            ]
        );
    }

    #[test]
    fn expiry_window_smaller_than_seven_days() {
        assert_eq!(expiry_step(5, 5), Some(5));
        assert_eq!(expiry_step(3, 5), Some(3));
        assert_eq!(expiry_step(6, 5), None);
    }

    #[test]
    fn expiry_notifies_once_per_step() {
        let expires = t0() + time::Duration::days(10);
        assert_eq!(expiry_due(expires, None, 14, t0()), Some(10));
        // Already notified in the 14-day step: quiet until the 7-day step.
        assert_eq!(
            expiry_due(expires, Some(t0()), 14, t0() + time::Duration::days(1)),
            None
        );
        assert_eq!(
            expiry_due(expires, Some(t0()), 14, t0() + time::Duration::days(3)),
            Some(7)
        );
    }

    #[test]
    fn expired_certificate_notifies_once_at_zero() {
        let expires = t0();
        // Last warned two days before expiry, i.e. in the 3-day step.
        let notified = t0() - time::Duration::days(2);
        assert_eq!(
            expiry_due(expires, Some(notified), 14, t0() + time::Duration::hours(1)),
            Some(-1)
        );
        let notified_at_zero = t0() + time::Duration::hours(1);
        assert_eq!(
            expiry_due(expires, Some(notified_at_zero), 14, t0() + time::Duration::days(5)),
            None
        );
    }

    #[test]
    fn expiry_outside_window_is_quiet() {
        assert_eq!(expiry_due(t0() + time::Duration::days(60), None, 14, t0()), None);
    }
}
