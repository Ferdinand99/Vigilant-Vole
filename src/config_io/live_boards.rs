//! The `live_boards` section: Discord and Fluxer live status boards, keyed by
//! `name`. A board follows a status page (`status_page`: its slug) or lists
//! monitors and groups (`items`: monitor keys, in order).
//!
//! **The webhook address is never exported**, with or without secrets.
//! Importing a board that already exists keeps its saved webhook; a board that
//! is new needs `webhook` in the document (the one place a document may
//! carry it, for people who keep it in a secret store and fill it in before
//! applying). Boards the document doesn't list are never deleted, also in
//! `replace` mode: deleting a board also deletes its messages in the channel,
//! which a file import can't do. Delete them in Settings, or with the API.

use std::collections::{HashMap, HashSet};

use axum_extra::extract::cookie::Key;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{Action, KIND_BOARD, Mode, Plan, norm, resolve};
use crate::db::live_boards::{self as boards, Board, NewBoard};
use crate::live_status::{self, Input};

fn yes() -> bool {
    true
}

fn refresh_default() -> i64 {
    boards::REFRESH_DEFAULT
}

fn discord() -> String {
    "discord".into()
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveBoardDoc {
    pub name: String,
    /// `discord` (default) | `fluxer`.
    #[serde(default = "discord")]
    pub platform: String,
    /// Only read on import, never written by an export. Empty or masked
    /// keeps the saved one.
    #[serde(default, skip_serializing)]
    pub webhook: Option<String>,
    /// Slug of the status page the board follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_page: Option<String>,
    /// Keys of the monitors and groups of a board without a status page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    #[serde(default = "yes")]
    pub show_uptime_24h: bool,
    #[serde(default = "yes")]
    pub show_uptime_30d: bool,
    #[serde(default)]
    pub show_response: bool,
    #[serde(default = "yes")]
    pub show_incidents: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    #[serde(default = "refresh_default")]
    pub refresh_seconds: i64,
    #[serde(default)]
    pub notify_down: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mention: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lang: String,
}

/// The webhook address is never printed.
impl std::fmt::Debug for LiveBoardDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveBoardDoc")
            .field("name", &self.name)
            .field("platform", &self.platform)
            .field("webhook", &self.webhook.as_ref().map(|_| "••••••"))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub(super) enum BoardOp {
    /// Ids in `new` (page, items) may be placeholders.
    Create(NewBoard),
    Update(i64, NewBoard),
}

/// The section as exported, without any webhook address.
pub(super) fn export(
    conn: &Connection,
    monitor_keys: &HashMap<i64, String>,
    page_slugs: &HashMap<i64, String>,
) -> rusqlite::Result<Vec<LiveBoardDoc>> {
    let mut out = Vec::new();
    for b in boards::list(conn)? {
        let items = boards::items(conn, b.id)?;
        out.push(LiveBoardDoc {
            name: b.name,
            platform: b.platform,
            webhook: None,
            status_page: b.status_page_id.and_then(|id| page_slugs.get(&id).cloned()),
            items: items
                .iter()
                .filter_map(|id| monitor_keys.get(id).cloned())
                .collect(),
            show_uptime_24h: b.show_uptime_24h,
            show_uptime_30d: b.show_uptime_30d,
            show_response: b.show_response,
            show_incidents: b.show_incidents,
            title: b.title,
            description: b.description,
            color: b.color,
            refresh_seconds: b.refresh_seconds,
            notify_down: b.notify_down,
            mention: b.mention,
            lang: b.lang,
        });
    }
    Ok(out)
}

fn input_of(
    d: &LiveBoardDoc,
    page_ids: &HashMap<String, i64>,
    monitor_ids: &HashMap<String, i64>,
    unknown: &mut Vec<String>,
) -> Input {
    let mut status_page_id = None;
    if let Some(slug) = d
        .status_page
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match page_ids.get(slug) {
            Some(id) => status_page_id = Some(*id),
            None => unknown.push(format!("status page “{slug}”")),
        }
    }
    let mut items = Vec::new();
    for key in &d.items {
        match monitor_ids.get(&norm(key)) {
            Some(id) => items.push(*id),
            None => unknown.push(format!("monitor “{key}”")),
        }
    }
    Input {
        name: d.name.clone(),
        platform: d.platform.clone(),
        webhook: d.webhook.clone(),
        source: if d
            .status_page
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        {
            "page".into()
        } else {
            "items".into()
        },
        status_page_id,
        items,
        show_uptime_24h: d.show_uptime_24h,
        show_uptime_30d: d.show_uptime_30d,
        show_response: d.show_response,
        show_incidents: d.show_incidents,
        title: d.title.clone(),
        description: d.description.clone(),
        color: d.color.clone(),
        refresh_seconds: d.refresh_seconds,
        notify_down: d.notify_down,
        mention: d.mention.clone(),
        lang: d.lang.clone(),
    }
}

/// Plans the section. `page_ids` and `monitor_ids` are the final state of
/// the document (ids may be placeholders of things this import creates).
#[allow(clippy::too_many_arguments)]
pub(super) fn plan(
    conn: &Connection,
    key: &Key,
    cur: &[Board],
    docs: Option<&[LiveBoardDoc]>,
    mode: Mode,
    page_ids: &HashMap<String, i64>,
    monitor_ids: &HashMap<String, i64>,
    plan: &mut Plan,
    ops: &mut Vec<BoardOp>,
) -> rusqlite::Result<()> {
    let Some(docs) = docs else {
        return Ok(());
    };
    let mut seen = HashSet::new();
    for d in docs {
        let name = d.name.trim();
        if !seen.insert(norm(name)) {
            plan.error(KIND_BOARD, name, None, "This board is listed twice.");
            continue;
        }
        let existing = cur.iter().find(|b| norm(&b.name) == norm(name));
        let mut unknown = Vec::new();
        let input = input_of(d, page_ids, monitor_ids, &mut unknown);
        for what in unknown {
            plan.error(
                KIND_BOARD,
                name,
                None,
                format!("The {what} is not in the document or in this instance."),
            );
        }
        // Checked like everywhere else. Ids that are placeholders of things
        // this import creates are accepted: they exist once it is applied.
        let new = match live_status::validate(conn, key, existing, &input)? {
            Ok(n) => n,
            Err(errors) => {
                for (field, message) in errors {
                    plan.error(KIND_BOARD, name, Some(field), message);
                }
                continue;
            }
        };
        match existing {
            Some(b) => {
                let items = boards::items(conn, b.id)?;
                let mut changes = Vec::new();
                macro_rules! diff {
                    ($($f:ident),*) => { $(if b.$f != new.$f { changes.push(stringify!($f).to_string()); })* };
                }
                diff!(
                    name,
                    platform,
                    source,
                    status_page_id,
                    show_uptime_24h,
                    show_uptime_30d,
                    show_response,
                    show_incidents,
                    title,
                    description,
                    color,
                    refresh_seconds,
                    notify_down,
                    mention,
                    lang
                );
                if b.webhook_url != new.webhook_url {
                    changes.push("webhook".into());
                }
                if items != new.items {
                    changes.push("items".into());
                }
                if changes.is_empty() {
                    plan.item(KIND_BOARD, name, Action::Unchanged, changes);
                } else {
                    plan.item(KIND_BOARD, name, Action::Update, changes);
                    ops.push(BoardOp::Update(b.id, new));
                }
            }
            None => {
                if d.webhook.as_deref().is_none_or(|w| w.trim().is_empty()) {
                    plan.error(
                        KIND_BOARD,
                        name,
                        Some("webhook"),
                        "A new board needs its webhook address: a document never carries it in an export, so add it to the board or create the board in Settings.",
                    );
                    continue;
                }
                plan.item(KIND_BOARD, name, Action::Create, Vec::new());
                ops.push(BoardOp::Create(new));
            }
        }
    }
    if mode == Mode::Replace {
        for b in cur {
            if !seen.contains(&norm(&b.name)) {
                plan.skip(
                    KIND_BOARD,
                    &b.name,
                    "kept: delete a board in Settings so that its messages in the channel are removed too",
                );
            }
        }
    }
    Ok(())
}

pub(super) fn execute(
    conn: &Connection,
    ops: &[BoardOp],
    ids: &HashMap<i64, i64>,
) -> rusqlite::Result<()> {
    let resolved = |n: &NewBoard| -> rusqlite::Result<NewBoard> {
        let mut n = n.clone();
        n.status_page_id = n.status_page_id.map(|p| resolve(ids, p)).transpose()?;
        n.items = n
            .items
            .iter()
            .map(|i| resolve(ids, *i))
            .collect::<rusqlite::Result<_>>()?;
        Ok(n)
    };
    for op in ops {
        match op {
            BoardOp::Create(n) => {
                boards::insert(conn, &resolved(n)?)?;
            }
            BoardOp::Update(id, n) => {
                let moved = boards::get(conn, *id)?.is_some_and(|b| b.webhook_url != n.webhook_url);
                if moved {
                    boards::forget_messages(conn, *id)?;
                }
                boards::update(conn, *id, &resolved(n)?)?;
            }
        }
    }
    Ok(())
}
