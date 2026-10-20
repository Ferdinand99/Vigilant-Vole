//! Bulk edit: change a handful of settings on many monitors at once (the
//! dashboard's select mode and `POST /api/v1/monitors/bulk`).
//!
//! Every setting is a tri-state: left alone (the default) or set. Values go
//! through the monitor form's own rules ([`super::form`]). A setting that
//! doesn't apply to a monitor's type is skipped for that monitor and
//! reported as a [`Skip`]; everything is written in one transaction.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use super::form::{self, ChannelMode, Errors, Pairs};
use crate::db::models::{MonitorType, NewMonitor};
use crate::db::monitors as db_monitors;
use crate::db::notifications as db_notifications;
use crate::db::tags as db_tags;

// ---------------------------------------------------------------------------
// The edit
// ---------------------------------------------------------------------------

/// A validated bulk edit. `None` / empty parts leave monitors as they are.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BulkEdit {
    pub run_on_agents: Option<bool>,
    pub confirm_with_agents: Option<bool>,
    pub interval: Option<i64>,
    pub retry_interval: Option<i64>,
    pub timeout: Option<i64>,
    pub retries: Option<i64>,
    pub channels: ChannelEdit,
    pub tags: TagEdit,
}

/// What happens to the notification channels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ChannelEdit {
    #[default]
    Unchanged,
    /// Link `(channel_id, escalation)` (an existing link takes the new flag)
    /// and unlink channel ids. Removals go first.
    Change { add: Vec<(i64, bool)>, remove: Vec<i64> },
    /// Exactly these `(channel_id, escalation)` links (empty: none).
    Replace(Vec<(i64, bool)>),
}

/// What happens to the tags. Order: clear, remove, add, create.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagEdit {
    /// Remove every tag first.
    pub clear: bool,
    pub remove: Vec<i64>,
    /// `(tag_id, value)`; a tag the monitor has takes the new value.
    pub add: Vec<(i64, String)>,
    /// Tags to create (or reuse, matched by name) and add.
    pub create: Vec<NewTag>,
}

impl TagEdit {
    pub fn is_empty(&self) -> bool {
        !self.clear && self.remove.is_empty() && self.add.is_empty() && self.create.is_empty()
    }
}

/// A tag to create (palette colour) and attach with `value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTag {
    pub name: String,
    pub color: String,
    pub value: String,
}

impl BulkEdit {
    /// Nothing to change.
    pub fn is_empty(&self) -> bool {
        self.run_on_agents.is_none()
            && self.confirm_with_agents.is_none()
            && self.interval.is_none()
            && self.retry_interval.is_none()
            && self.timeout.is_none()
            && self.retries.is_none()
            && self.channels == ChannelEdit::Unchanged
            && self.tags.is_empty()
    }
}

/// Tri-state switch value: `on` / `off` (or `true` / `false`) sets it,
/// anything else (the "Unchanged" choice sends "") leaves it alone.
pub fn parse_tri(raw: &str) -> Option<bool> {
    match raw.trim() {
        "on" | "true" => Some(true),
        "off" | "false" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The sheet's raw values
// ---------------------------------------------------------------------------

/// Channel and tag rows: unchanged (""), add or remove.
pub const OP_ADD: &str = "add";
pub const OP_REMOVE: &str = "remove";
/// `channels_mode` value that replaces every link.
pub const MODE_REPLACE: &str = "replace";

/// The sheet as submitted (re-rendered as is when something is invalid).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BulkValues {
    /// "" (unchanged), "on" or "off".
    pub run_on_agents: String,
    pub confirm_with_agents: String,
    /// "" = unchanged.
    pub interval: String,
    pub retry_interval: String,
    pub timeout: String,
    pub retries: String,
    /// "change" (add/remove per channel) or "replace".
    pub channels_mode: String,
    /// `channel_{id}` ("", add, remove) and `channel_{id}_escalation`.
    pub channel_ops: Vec<(i64, String, bool)>,
    /// `replace_{id}`: off, alert or escalation.
    pub replace: Vec<(i64, ChannelMode)>,
    /// `tag_{id}` ("", add, remove) and `tag_value_{id}`.
    pub tag_ops: Vec<(i64, String, String)>,
    pub tags_clear: bool,
    pub new_tag_name: String,
    pub new_tag_value: String,
    pub new_tag_color: String,
}

fn op(raw: &str) -> String {
    match raw.trim() {
        OP_ADD => OP_ADD.into(),
        OP_REMOVE => OP_REMOVE.into(),
        _ => String::new(),
    }
}

fn tri_value(raw: &str) -> String {
    match parse_tri(raw) {
        Some(true) => "on".into(),
        Some(false) => "off".into(),
        None => String::new(),
    }
}

impl BulkValues {
    /// Reads the sheet's fields for the existing channels and tags.
    pub fn from_pairs(p: &Pairs, channel_ids: &[i64], tag_ids: &[i64]) -> Self {
        BulkValues {
            run_on_agents: tri_value(&p.text("run_on_agents")),
            confirm_with_agents: tri_value(&p.text("confirm_with_agents")),
            interval: p.text("interval_seconds"),
            retry_interval: p.text("retry_interval_seconds"),
            timeout: p.text("timeout_seconds"),
            retries: p.text("retries"),
            channels_mode: if p.text("channels_mode") == MODE_REPLACE { MODE_REPLACE.into() } else { "change".into() },
            channel_ops: channel_ids
                .iter()
                .map(|id| (*id, op(&p.text(&format!("channel_{id}"))), p.flag(&format!("channel_{id}_escalation"))))
                .collect(),
            replace: channel_ids
                .iter()
                .map(|id| (*id, ChannelMode::parse(&p.text(&format!("replace_{id}")))))
                .collect(),
            tag_ops: tag_ids
                .iter()
                .map(|id| (*id, op(&p.text(&format!("tag_{id}"))), p.text(&format!("tag_value_{id}"))))
                .collect(),
            tags_clear: p.flag("tags_clear"),
            new_tag_name: p.text("new_tag_name"),
            new_tag_value: p.text("new_tag_value"),
            new_tag_color: db_tags::normalize_color(&p.text("new_tag_color")).to_string(),
        }
    }

    pub fn replaces_channels(&self) -> bool {
        self.channels_mode == MODE_REPLACE
    }

    /// `run_on_agents` / `confirm_with_agents` choice ("", "on", "off").
    pub fn tri(&self, name: &str) -> &str {
        match name {
            "run_on_agents" => &self.run_on_agents,
            "confirm_with_agents" => &self.confirm_with_agents,
            _ => "",
        }
    }

    pub fn channel_op(&self, id: &i64) -> &str {
        self.channel_ops.iter().find(|(c, _, _)| c == id).map_or("", |(_, o, _)| o.as_str())
    }

    pub fn channel_escalation(&self, id: &i64) -> bool {
        self.channel_ops.iter().any(|(c, _, esc)| c == id && *esc)
    }

    pub fn replace_mode(&self, id: &i64) -> &'static str {
        self.replace.iter().find(|(c, _)| c == id).map_or(ChannelMode::Off, |(_, m)| *m).as_str()
    }

    pub fn tag_op(&self, id: &i64) -> &str {
        self.tag_ops.iter().find(|(t, _, _)| t == id).map_or("", |(_, o, _)| o.as_str())
    }

    pub fn tag_value(&self, id: &i64) -> &str {
        self.tag_ops.iter().find(|(t, _, _)| t == id).map_or("", |(_, _, v)| v.as_str())
    }

    /// How many settings the sheet changes as filled in (for the summary
    /// line; a value that is still invalid counts too).
    pub fn change_count(&self) -> usize {
        let set = |s: &str| usize::from(!s.trim().is_empty());
        let channels = if self.replaces_channels() {
            1
        } else {
            self.channel_ops.iter().filter(|(_, o, _)| !o.is_empty()).count()
        };
        set(&self.run_on_agents)
            + set(&self.confirm_with_agents)
            + set(&self.interval)
            + set(&self.retry_interval)
            + set(&self.timeout)
            + set(&self.retries)
            + channels
            + self.tag_ops.iter().filter(|(_, o, _)| !o.is_empty()).count()
            + usize::from(self.tags_clear)
            + set(&self.new_tag_name)
    }
}

fn given(raw: &str) -> Option<&str> {
    let t = raw.trim();
    (!t.is_empty()).then_some(t)
}

/// Validates the sheet with the monitor form's rules (same field names, so
/// errors land on the same inputs). Unknown channel and tag ids were already
/// dropped by [`BulkValues::from_pairs`].
pub fn validate(v: &BulkValues) -> Result<BulkEdit, Errors> {
    let mut e = Errors::new();
    let mut edit = BulkEdit {
        run_on_agents: parse_tri(&v.run_on_agents),
        confirm_with_agents: parse_tri(&v.confirm_with_agents),
        interval: given(&v.interval).and_then(|raw| form::check_interval(&mut e, raw)),
        retry_interval: given(&v.retry_interval).and_then(|raw| form::check_retry_interval(&mut e, raw)),
        timeout: given(&v.timeout).and_then(|raw| form::check_timeout(&mut e, raw)),
        retries: given(&v.retries).and_then(|raw| form::check_retries(&mut e, raw)),
        ..BulkEdit::default()
    };
    if let (Some(timeout), Some(interval)) = (edit.timeout, edit.interval) {
        form::check_timeout_fits(&mut e, timeout, interval);
    }

    edit.channels = if v.replaces_channels() {
        ChannelEdit::Replace(
            v.replace
                .iter()
                .filter_map(|(id, mode)| match mode {
                    ChannelMode::Off => None,
                    ChannelMode::Alert => Some((*id, false)),
                    ChannelMode::Escalation => Some((*id, true)),
                })
                .collect(),
        )
    } else {
        let add: Vec<(i64, bool)> = v.channel_ops.iter().filter(|(_, o, _)| o == OP_ADD).map(|(id, _, esc)| (*id, *esc)).collect();
        let remove: Vec<i64> = v.channel_ops.iter().filter(|(_, o, _)| o == OP_REMOVE).map(|(id, _, _)| *id).collect();
        if add.is_empty() && remove.is_empty() { ChannelEdit::Unchanged } else { ChannelEdit::Change { add, remove } }
    };

    edit.tags.clear = v.tags_clear;
    for (id, op, value) in &v.tag_ops {
        match op.as_str() {
            OP_ADD => {
                if let Some(msg) = form::tag_value_error(value) {
                    e.insert("tags", msg);
                }
                edit.tags.add.push((*id, value.trim().to_string()));
            }
            OP_REMOVE => edit.tags.remove.push(*id),
            _ => {}
        }
    }
    if let Some(name) = given(&v.new_tag_name) {
        if let Some(msg) = form::tag_name_error(name).or_else(|| form::tag_value_error(&v.new_tag_value)) {
            e.insert("new_tag_name", msg);
        }
        edit.tags.create.push(NewTag {
            name: name.to_string(),
            color: db_tags::normalize_color(&v.new_tag_color).to_string(),
            value: v.new_tag_value.trim().to_string(),
        });
    }

    if e.is_empty() { Ok(edit) } else { Err(e) }
}

// ---------------------------------------------------------------------------
// Applying it
// ---------------------------------------------------------------------------

/// Why a setting was left alone on a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Skip {
    /// Agent settings on a type agents can't run (push, group, real browser).
    Agents,
    /// Check timing on a group (none at all) or a push monitor (no timeout
    /// or retry interval).
    Timing,
    /// The timeout would end up longer than the interval.
    TimeoutOverInterval,
    /// An escalation channel on a monitor without an escalation delay.
    EscalationDelay,
}

impl Skip {
    pub const ALL: [Skip; 4] = [Skip::Agents, Skip::Timing, Skip::TimeoutOverInterval, Skip::EscalationDelay];

    /// Stable machine code (REST API).
    pub fn code(self) -> &'static str {
        match self {
            Skip::Agents => "agents_unsupported",
            Skip::Timing => "timing_unsupported",
            Skip::TimeoutOverInterval => "timeout_exceeds_interval",
            Skip::EscalationDelay => "escalation_needs_delay",
        }
    }

    /// Why, for people (lowercase; it follows "skipped 5 (").
    pub fn reason(self) -> &'static str {
        match self {
            Skip::Agents => crate::i18n::t("bulk.reason_agents"),
            Skip::Timing => crate::i18n::t("bulk.reason_timing"),
            Skip::TimeoutOverInterval => crate::i18n::t("bulk.reason_timeout"),
            Skip::EscalationDelay => crate::i18n::t("bulk.reason_escalation"),
        }
    }
}

/// Applies the agent and timing settings to one monitor; returns what didn't apply.
pub fn apply_settings(m: &mut NewMonitor, edit: &BulkEdit) -> Vec<Skip> {
    let mut skips = Vec::new();
    let kind = m.monitor_type;
    if edit.run_on_agents.is_some() || edit.confirm_with_agents.is_some() {
        if kind.runs_on_agents() {
            if let Some(on) = edit.run_on_agents {
                m.run_on_agents = on;
            }
            if let Some(on) = edit.confirm_with_agents {
                m.confirm_with_agents = on;
            }
        } else {
            skips.push(Skip::Agents);
        }
    }

    // The monitor form's rules: groups have no timing at all; push monitors
    // only an interval and retries.
    let before = (m.interval_seconds, m.retries, m.retry_interval_seconds, m.timeout_seconds);
    let mut unsupported = false;
    if kind == MonitorType::Group {
        unsupported |= edit.interval.is_some() || edit.retries.is_some();
    } else {
        m.interval_seconds = edit.interval.unwrap_or(m.interval_seconds);
        m.retries = edit.retries.unwrap_or(m.retries);
    }
    if kind.is_active_check() {
        if let Some(r) = edit.retry_interval {
            m.retry_interval_seconds = Some(r);
        }
        m.timeout_seconds = edit.timeout.unwrap_or(m.timeout_seconds);
    } else {
        unsupported |= edit.retry_interval.is_some() || edit.timeout.is_some();
    }
    if unsupported {
        skips.push(Skip::Timing);
    }
    let moved = (m.interval_seconds, m.timeout_seconds) != (before.0, before.3);
    if kind.is_active_check() && moved && !form::timeout_fits(m.timeout_seconds, m.interval_seconds) {
        (m.interval_seconds, m.retries, m.retry_interval_seconds, m.timeout_seconds) = before;
        skips.push(Skip::TimeoutOverInterval);
    }
    skips
}

/// The channel links after `edit` (sorted by channel id). Escalation links
/// need `escalation_ok` (the monitor has an escalation delay); otherwise they
/// are skipped.
pub fn apply_channels(current: &[(i64, bool)], edit: &ChannelEdit, escalation_ok: bool, skips: &mut Vec<Skip>) -> Vec<(i64, bool)> {
    let mut links: BTreeMap<i64, bool> = current.iter().copied().collect();
    let mut add = |links: &mut BTreeMap<i64, bool>, list: &[(i64, bool)]| {
        for (id, escalation) in list {
            if *escalation && !escalation_ok {
                if !skips.contains(&Skip::EscalationDelay) {
                    skips.push(Skip::EscalationDelay);
                }
                continue;
            }
            links.insert(*id, *escalation);
        }
    };
    match edit {
        ChannelEdit::Unchanged => {}
        ChannelEdit::Change { add: list, remove } => {
            for id in remove {
                links.remove(id);
            }
            add(&mut links, list);
        }
        ChannelEdit::Replace(list) => {
            links.clear();
            add(&mut links, list);
        }
    }
    links.into_iter().collect()
}

/// The tags after `edit` (sorted by tag id); `created` are the resolved
/// `(tag_id, value)` of [`TagEdit::create`].
pub fn apply_tags(current: &[(i64, String)], edit: &TagEdit, created: &[(i64, String)]) -> Vec<(i64, String)> {
    let mut tags: BTreeMap<i64, String> = if edit.clear { BTreeMap::new() } else { current.iter().cloned().collect() };
    for id in &edit.remove {
        tags.remove(id);
    }
    for (id, value) in edit.add.iter().chain(created) {
        tags.insert(*id, value.clone());
    }
    tags.into_iter().collect()
}

/// What a bulk edit did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Monitors whose settings, channels or tags changed (in request order).
    pub updated: Vec<i64>,
    /// `(monitor_id, reason)`: at most one entry per monitor and reason.
    pub skipped: Vec<(i64, Skip)>,
}

impl Outcome {
    /// How many monitors were skipped for each reason (reasons that occurred).
    pub fn skip_counts(&self) -> Vec<(Skip, usize)> {
        Skip::ALL
            .iter()
            .map(|s| (*s, self.skipped.iter().filter(|(_, r)| r == s).count()))
            .filter(|(_, n)| *n > 0)
            .collect()
    }

    /// The toast: "Updated 38 monitors · skipped 5 (…)".
    pub fn message(&self) -> String {
        let mut out = if self.updated.is_empty() {
            crate::i18n::t("bulk.nothing_changed").to_string()
        } else {
            crate::i18n::tf(
                "bulk.updated",
                &[("monitors", &crate::i18n::tn("count.monitors", self.updated.len() as i64))],
            )
        };
        for (skip, n) in self.skip_counts() {
            out.push_str(" · ");
            out.push_str(&crate::i18n::tf(
                "bulk.skipped",
                &[("n", &crate::i18n::fmt_int(n as i64)), ("reason", skip.reason())],
            ));
        }
        out
    }
}

/// The summary line under the sheet: "Changes 3 settings on 43 monitors".
pub fn summary(settings: usize, monitors: usize) -> String {
    match (settings, monitors) {
        (0, _) => crate::i18n::t("bulk.summary_none").to_string(),
        (_, 0) => crate::i18n::t("bulk.summary_no_monitors").to_string(),
        (s, n) => crate::i18n::tf(
            "bulk.summary",
            &[
                ("settings", &crate::i18n::tn("bulk.settings", s as i64)),
                ("monitors", &crate::i18n::tn("count.monitors", n as i64)),
            ],
        ),
    }
}

/// Applies `edit` to the monitors in `ids` (missing ones are ignored) in one
/// transaction: any error rolls everything back.
pub fn apply(conn: &mut Connection, ids: &[i64], edit: &BulkEdit) -> rusqlite::Result<Outcome> {
    let tx = conn.transaction()?;
    let created: Vec<(i64, String)> = edit
        .tags
        .create
        .iter()
        .map(|t| Ok((db_tags::get_or_create(&tx, &t.name, &t.color)?, t.value.clone())))
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Outcome::default();
    let mut seen = BTreeSet::new();
    for &id in ids {
        if !seen.insert(id) {
            continue;
        }
        let Some(existing) = db_monitors::get_monitor(&tx, id)? else {
            continue;
        };
        let before = existing.to_new();
        let mut m = before.clone();
        let mut skips = apply_settings(&mut m, edit);
        let mut changed = false;
        if m != before {
            db_monitors::update_monitor(&tx, id, &m)?;
            changed = true;
        }
        if edit.channels != ChannelEdit::Unchanged {
            let current = db_notifications::monitor_channel_links(&tx, id)?;
            let next = apply_channels(&current, &edit.channels, m.escalation_minutes > 0, &mut skips);
            if next != current {
                db_notifications::set_monitor_channels(&tx, id, &next)?;
                changed = true;
            }
        }
        if !edit.tags.is_empty() {
            let mut current: Vec<(i64, String)> =
                db_tags::tags_for_monitor(&tx, id)?.into_iter().map(|t| (t.tag_id, t.value)).collect();
            current.sort_by_key(|(tag, _)| *tag);
            let next = apply_tags(&current, &edit.tags, &created);
            if next != current {
                db_tags::set_monitor_tags(&tx, id, &next)?;
                changed = true;
            }
        }
        if changed {
            out.updated.push(id);
        }
        out.skipped.extend(skips.into_iter().map(|s| (id, s)));
    }
    tx.commit()?;
    Ok(out)
}

#[cfg(test)]
mod tests;
