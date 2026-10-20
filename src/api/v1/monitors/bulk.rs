//! `POST /api/v1/monitors/bulk`: the dashboard's bulk edit for scripts.
//! Parsing only; validation and the transaction are
//! [`crate::api::monitors::bulk_edit`]'s (and through it the monitor form's).

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{Map, Value, json};

use super::MonitorFilter;
use super::super::{Body, V1Error, V1Result, json_ok};
use crate::api::AppState;
use crate::api::monitors::bulk_edit::{self, BulkEdit, ChannelEdit, NewTag};
use crate::api::monitors::form::{self, Errors};
use crate::auth::{CurrentUser, Role};
use crate::db::models::NotificationChannel;
use crate::db::tags::{self as db_tags, Tag};
use crate::db::{monitors as db_monitors, notifications as db_notifications};

/// Keys `set` understands.
const SET_KEYS: &[&str] = &[
    "run_on_agents",
    "confirm_with_agents",
    "interval_seconds",
    "retry_interval_seconds",
    "timeout_seconds",
    "retries",
    "channels",
    "tags",
];

fn unknown_keys(obj: &Map<String, Value>, known: &[&str], prefix: &str) -> V1Result<()> {
    let unknown: BTreeMap<String, String> = obj
        .keys()
        .filter(|k| !known.contains(&k.as_str()))
        .map(|k| (format!("{prefix}{k}"), format!("Unknown field “{k}”.")))
        .collect();
    if unknown.is_empty() { Ok(()) } else { Err(V1Error::validation("", unknown)) }
}

fn object<'a>(v: &'a Value, field: &str) -> V1Result<&'a Map<String, Value>> {
    v.as_object().ok_or_else(|| V1Error::field(field, "Send a JSON object."))
}

fn list<'a>(v: Option<&'a Value>, field: &str) -> V1Result<&'a [Value]> {
    match v {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(V1Error::field(field, "Send a list.")),
    }
}

fn flag(v: Option<&Value>, field: &str) -> V1Result<Option<bool>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(V1Error::field(field, "Use true or false.")),
    }
}

/// A number setting through the form's own rule (`null` / missing = unchanged).
fn number(
    v: Option<&Value>,
    e: &mut Errors,
    rule: fn(&mut Errors, &str) -> Option<i64>,
) -> Option<i64> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => rule(e, &n.to_string()),
        // Anything else reads as "not a whole number" in the rule's words.
        Some(_) => rule(e, "x"),
    }
}

/// `{"id": 3}`, `{"name": "Ops"}`, `3` or `"Ops"` → a channel id.
fn channel_ref(v: &Value, channels: &[NotificationChannel], field: &str) -> V1Result<(i64, bool)> {
    let escalation = v.get("escalation").and_then(Value::as_bool).unwrap_or(false);
    let found = match v {
        Value::Number(n) => n.as_i64().and_then(|id| channels.iter().find(|c| c.id == id)),
        Value::String(name) => channels.iter().find(|c| c.name.eq_ignore_ascii_case(name.trim())),
        Value::Object(o) => match (o.get("id").and_then(Value::as_i64), o.get("name").and_then(Value::as_str)) {
            (Some(id), _) => channels.iter().find(|c| c.id == id),
            (None, Some(name)) => channels.iter().find(|c| c.name.eq_ignore_ascii_case(name.trim())),
            _ => return Err(V1Error::field(field, "Each channel needs an id or a name.")),
        },
        _ => return Err(V1Error::field(field, "Each channel is an id, a name or an object.")),
    };
    found
        .map(|c| (c.id, escalation))
        .ok_or_else(|| V1Error::field(field, format!("No notification channel {v}.")))
}

fn parse_channels(v: &Value, channels: &[NotificationChannel]) -> V1Result<ChannelEdit> {
    let obj = object(v, "set.channels")?;
    unknown_keys(obj, &["add", "remove", "replace"], "set.channels.")?;
    let refs = |key: &str| -> V1Result<Vec<(i64, bool)>> {
        let field = format!("set.channels.{key}");
        list(obj.get(key), &field)?.iter().map(|c| channel_ref(c, channels, &field)).collect()
    };
    let given = |key: &str| obj.get(key).is_some_and(|v| !v.is_null());
    if given("replace") {
        if given("add") || given("remove") {
            return Err(V1Error::field("set.channels", "Use replace on its own, or add and remove."));
        }
        return Ok(ChannelEdit::Replace(refs("replace")?));
    }
    let (add, remove) = (refs("add")?, refs("remove")?);
    Ok(if add.is_empty() && remove.is_empty() {
        ChannelEdit::Unchanged
    } else {
        ChannelEdit::Change { add, remove: remove.into_iter().map(|(id, _)| id).collect() }
    })
}

/// A tag by id or (case-insensitive) name.
fn find_tag<'a>(v: &Value, tags: &'a [Tag]) -> Option<&'a Tag> {
    match v {
        Value::Number(n) => n.as_i64().and_then(|id| tags.iter().find(|t| t.id == id)),
        Value::String(name) => tags.iter().find(|t| t.name.eq_ignore_ascii_case(name.trim())),
        Value::Object(o) => match (o.get("id").and_then(Value::as_i64), o.get("name").and_then(Value::as_str)) {
            (Some(id), _) => tags.iter().find(|t| t.id == id),
            (None, Some(name)) => tags.iter().find(|t| t.name.eq_ignore_ascii_case(name.trim())),
            _ => None,
        },
        _ => None,
    }
}

fn parse_tags(v: &Value, tags: &[Tag], edit: &mut BulkEdit, e: &mut Errors) -> V1Result<()> {
    let obj = object(v, "set.tags")?;
    unknown_keys(obj, &["add", "remove", "clear"], "set.tags.")?;
    edit.tags.clear = flag(obj.get("clear"), "set.tags.clear")?.unwrap_or(false);
    for t in list(obj.get("remove"), "set.tags.remove")? {
        let tag = find_tag(t, tags).ok_or_else(|| V1Error::field("set.tags.remove", format!("No tag {t}.")))?;
        edit.tags.remove.push(tag.id);
    }
    for t in list(obj.get("add"), "set.tags.add")? {
        let value = t.get("value").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if let Some(msg) = form::tag_value_error(&value) {
            e.insert("set.tags.add", msg);
        }
        if let Some(tag) = find_tag(t, tags) {
            edit.tags.add.push((tag.id, value));
            continue;
        }
        let name = match t {
            Value::String(name) => name.trim(),
            Value::Object(o) if o.get("id").is_none() => o.get("name").and_then(Value::as_str).unwrap_or_default().trim(),
            _ => return Err(V1Error::field("set.tags.add", format!("No tag {t}."))),
        };
        if name.is_empty() {
            return Err(V1Error::field("set.tags.add", "Each tag needs a name or an id."));
        }
        if let Some(msg) = form::tag_name_error(name) {
            e.insert("set.tags.add", msg);
        }
        let color = t.get("color").and_then(Value::as_str).unwrap_or(db_tags::DEFAULT_COLOR);
        edit.tags.create.push(NewTag {
            name: name.to_string(),
            color: db_tags::normalize_color(color).to_string(),
            value,
        });
    }
    Ok(())
}

/// Parses and validates `set` (the form's rules, errors under `set.<field>`).
fn parse_set(v: &Value, channels: &[NotificationChannel], tags: &[Tag]) -> V1Result<BulkEdit> {
    let obj = object(v, "set")?;
    unknown_keys(obj, SET_KEYS, "set.")?;
    let mut e = Errors::new();
    let mut edit = BulkEdit {
        run_on_agents: flag(obj.get("run_on_agents"), "set.run_on_agents")?,
        confirm_with_agents: flag(obj.get("confirm_with_agents"), "set.confirm_with_agents")?,
        interval: number(obj.get("interval_seconds"), &mut e, form::check_interval),
        retry_interval: number(obj.get("retry_interval_seconds"), &mut e, form::check_retry_interval),
        timeout: number(obj.get("timeout_seconds"), &mut e, form::check_timeout),
        retries: number(obj.get("retries"), &mut e, form::check_retries),
        ..BulkEdit::default()
    };
    if let (Some(timeout), Some(interval)) = (edit.timeout, edit.interval) {
        form::check_timeout_fits(&mut e, timeout, interval);
    }
    if let Some(c) = obj.get("channels").filter(|c| !c.is_null()) {
        edit.channels = parse_channels(c, channels)?;
    }
    if let Some(t) = obj.get("tags").filter(|t| !t.is_null()) {
        parse_tags(t, tags, &mut edit, &mut e)?;
    }
    if !e.is_empty() {
        let fields = e
            .into_iter()
            .map(|(k, v)| (if k.starts_with("set.") { k.to_string() } else { format!("set.{k}") }, v))
            .collect();
        return Err(V1Error::validation("", fields));
    }
    if edit.is_empty() {
        return Err(V1Error::field("set", "Choose at least one setting to change."));
    }
    Ok(edit)
}

/// Which monitors: `ids`, or every monitor matching `filter` (`{}` = all).
enum Selection {
    Ids(Vec<i64>),
    Filter(MonitorFilter),
}

fn parse_selection(ids: Option<Value>, filter: Option<Value>) -> V1Result<Selection> {
    match (ids, filter) {
        (Some(_), Some(_)) => Err(V1Error::field("ids", "Send ids or filter, not both.")),
        (None, None) => Err(V1Error::field("ids", "Send the monitor ids, or a filter ({} selects every monitor).")),
        (Some(ids), None) => serde_json::from_value::<Vec<i64>>(ids)
            .map(Selection::Ids)
            .map_err(|_| V1Error::field("ids", "Send a list of monitor ids.")),
        (None, Some(filter)) => {
            let filter: MonitorFilter = serde_json::from_value(filter)
                .map_err(|e| V1Error::field("filter", format!("Use status, type, tag and q ({e}).")))?;
            filter.check("filter.")?;
            Ok(Selection::Filter(filter))
        }
    }
}

/// `POST /api/v1/monitors/bulk`.
pub(super) async fn bulk(state: axum::extract::State<AppState>, user: CurrentUser, Body(body): Body<Value>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let Value::Object(mut body) = body else {
        return Err(V1Error::bad_request("Send a JSON object."));
    };
    let (ids, filter, set) = (body.remove("ids"), body.remove("filter"), body.remove("set"));
    unknown_keys(&body, &[], "")?;
    let selection = parse_selection(ids, filter)?;
    let set = set.ok_or_else(|| V1Error::field("set", "Say what to change in set."))?;
    let (channels, tags) = state
        .with_conn(|c| Ok((db_notifications::list_channels(c)?, db_tags::list_tags(c)?)))
        .await?;
    let edit = parse_set(&set, &channels, &tags)?;

    let outcome = state
        .with_conn(move |c| {
            let ids = match selection {
                Selection::Ids(ids) => ids,
                Selection::Filter(filter) => {
                    let tags = db_tags::tags_by_monitor(c)?;
                    db_monitors::list_monitors_with_status(c)?
                        .into_iter()
                        .filter(|row| filter.matches(&row.monitor, row.status, tags.get(&row.monitor.id).map(Vec::as_slice).unwrap_or_default()))
                        .map(|row| row.monitor.id)
                        .collect()
                }
            };
            bulk_edit::apply(c, &ids, &edit)
        })
        .await?;
    crate::maintenance::invalidate();
    for id in &outcome.updated {
        state.scheduler.reload(*id).await;
    }
    state.notify_update();
    tracing::info!(user = %user.username, updated = outcome.updated.len(), skipped = outcome.skipped.len(), "bulk edit via API");
    let skipped: Vec<Value> = outcome.skipped.iter().map(|(id, s)| json!({ "id": id, "reason": s.code() })).collect();
    Ok(json_ok(&HeaderMap::new(), &json!({ "updated": outcome.updated.len(), "skipped": skipped })))
}
