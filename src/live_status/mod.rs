//! Live status boards: one message (or a few, when the platforms' limits
//! require) in a Discord or Fluxer channel that is edited in place to show
//! a status page, or a hand-picked set of monitors and groups.
//!
//! * [`webhook`]: webhook addresses (the only hosts a board may talk to) and
//!   the client: create, edit, delete, rate limits, SSRF rules for Fluxer.
//! * [`layout`]: board content to messages within the platforms' limits.
//! * [`view`]: what a board shows, from the database, and nothing more than
//!   its status page or its picked monitors show.
//! * [`runner`]: one refresh of a board, and the background job.
//!
//! The webhook address is a secret: it is stored sealed, shown nowhere after
//! it was saved, and never in logs, the API, exports or MCP output.
//! [`validate`] is the one place a board is checked, for the settings page,
//! the API and imports alike.

pub mod layout;
pub mod runner;
pub mod view;
pub mod webhook;

use std::collections::BTreeMap;

use axum_extra::extract::cookie::Key;
use rusqlite::Connection;

use crate::db::live_boards::{self as boards, Board, NewBoard};
use crate::db::{monitors as db_monitors, status_pages as db_pages};
use crate::kit::notify::SECRET_MASK;
use webhook::Platform;

/// Field → message.
pub type Errors = BTreeMap<&'static str, String>;

/// What a form, the API or an import submits.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub name: String,
    /// `discord` | `fluxer`.
    pub platform: String,
    /// The webhook address. `None`, empty or [`SECRET_MASK`] keeps the saved
    /// one (an edit) or is an error (a new board).
    pub webhook: Option<String>,
    /// `page` | `items`.
    pub source: String,
    pub status_page_id: Option<i64>,
    pub items: Vec<i64>,
    pub show_uptime_24h: bool,
    pub show_uptime_30d: bool,
    pub show_response: bool,
    pub show_incidents: bool,
    pub title: String,
    pub description: String,
    pub color: String,
    pub refresh_seconds: i64,
    pub notify_down: bool,
    pub mention: String,
    pub lang: String,
}

impl Input {
    /// The values a new board starts with.
    pub fn defaults() -> Input {
        Input {
            platform: "discord".into(),
            source: "page".into(),
            show_uptime_24h: true,
            show_uptime_30d: true,
            show_incidents: true,
            refresh_seconds: boards::REFRESH_DEFAULT,
            lang: crate::i18n::site_default().code().to_string(),
            ..Input::default()
        }
    }

    /// The saved board's settings, without its webhook.
    pub fn from_board(b: &Board, items: Vec<i64>) -> Input {
        Input {
            name: b.name.clone(),
            platform: b.platform.clone(),
            webhook: None,
            source: b.source.clone(),
            status_page_id: b.status_page_id,
            items,
            show_uptime_24h: b.show_uptime_24h,
            show_uptime_30d: b.show_uptime_30d,
            show_response: b.show_response,
            show_incidents: b.show_incidents,
            title: b.title.clone(),
            description: b.description.clone(),
            color: b.color.clone(),
            refresh_seconds: b.refresh_seconds,
            notify_down: b.notify_down,
            mention: b.mention.clone(),
            lang: b.lang.clone(),
        }
    }

    /// Whether the input carries a webhook address to check and save.
    pub fn new_webhook(&self) -> Option<&str> {
        self.webhook
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty() && *w != SECRET_MASK)
    }
}

fn err(e: &mut Errors, field: &'static str, key: &'static str) {
    e.insert(field, crate::i18n::t(key).to_string());
}

/// Checks `input` and returns what to store (the webhook sealed), or the
/// errors per field. `editing` is the saved board of an edit.
///
/// A Fluxer address is only checked for its shape here. Whether the host is
/// a Fluxer instance takes a request: see [`confirm_webhook`], which the
/// caller runs when [`Input::new_webhook`] is set.
pub fn validate(
    conn: &Connection,
    key: &Key,
    editing: Option<&Board>,
    input: &Input,
) -> rusqlite::Result<Result<NewBoard, Errors>> {
    let mut e = Errors::new();

    let name = input.name.trim();
    if name.is_empty() {
        err(&mut e, "name", "live.err_name");
    } else if name.chars().count() > boards::MAX_NAME_LEN {
        err(&mut e, "name", "live.err_too_long");
    } else if boards::name_taken(conn, name, editing.map(|b| b.id))? {
        err(&mut e, "name", "live.err_name_taken");
    }
    if editing.is_none() && boards::count(conn)? >= boards::MAX_BOARDS {
        err(&mut e, "name", "live.err_limit");
    }

    let platform = Platform::parse(&input.platform);
    if platform.is_none() {
        err(&mut e, "platform", "live.err_choice");
    }

    // The webhook: new, or the saved one kept.
    let mut sealed_webhook = String::new();
    match (input.new_webhook(), editing) {
        (Some(url), _) => {
            if let Some(platform) = platform {
                match webhook::client().parse(platform, url) {
                    Ok(_) => sealed_webhook = crate::kit::notify::sealed::seal_value(key, url),
                    Err(why) => err(&mut e, "webhook", why.message_key()),
                }
            }
        }
        (None, Some(b)) => {
            // Keeping it only works while it can still be opened.
            if crate::kit::notify::sealed::open_value(key, &b.webhook_url).is_err() {
                err(&mut e, "webhook", "notif.err_secret_unreadable");
            } else if platform.is_some_and(|p| p.as_str() != b.platform) {
                // Another platform needs its own address.
                err(&mut e, "webhook", "live.err_webhook_platform");
            } else {
                sealed_webhook = b.webhook_url.clone();
            }
        }
        (None, None) => err(&mut e, "webhook", "live.err_webhook_required"),
    }

    // The source.
    let mut status_page_id = None;
    let mut items = Vec::new();
    match input.source.as_str() {
        "page" => match input.status_page_id {
            None => err(&mut e, "status_page_id", "live.err_page"),
            // A negative id stands for a status page an import is creating.
            Some(id) if id < 0 => status_page_id = Some(id),
            Some(id) => match db_pages::get(conn, id)? {
                None => err(&mut e, "status_page_id", "live.err_page"),
                Some(p) if !p.published || p.password_hash.is_some() => {
                    err(&mut e, "status_page_id", "live.err_page");
                }
                Some(_) => status_page_id = Some(id),
            },
        },
        "items" => {
            let mut seen = std::collections::HashSet::new();
            let wanted: Vec<i64> = input
                .items
                .iter()
                .copied()
                .filter(|i| seen.insert(*i))
                .collect();
            if wanted.is_empty() || wanted.len() > boards::MAX_ITEMS {
                e.insert(
                    "items",
                    crate::i18n::tf("live.err_items", &[("max", &boards::MAX_ITEMS.to_string())]),
                );
            } else {
                // Negative ids stand for monitors an import is creating.
                let real: Vec<i64> = wanted.iter().copied().filter(|i| *i > 0).collect();
                let existing: std::collections::HashSet<i64> =
                    db_monitors::existing_ids(conn, &real)?
                        .into_iter()
                        .collect();
                if real.iter().any(|i| !existing.contains(i)) {
                    e.insert(
                        "items",
                        crate::i18n::tf(
                            "live.err_items",
                            &[("max", &boards::MAX_ITEMS.to_string())],
                        ),
                    );
                } else {
                    items = wanted;
                }
            }
        }
        _ => err(&mut e, "source", "live.err_choice"),
    }

    let title = input.title.trim();
    if title.chars().count() > boards::MAX_TITLE_LEN {
        err(&mut e, "title", "live.err_too_long");
    }
    let description = input.description.trim();
    if description.chars().count() > boards::MAX_DESCRIPTION_LEN {
        err(&mut e, "description", "live.err_too_long");
    }
    let color = input.color.trim().to_ascii_lowercase();
    if !color.is_empty() && layout::parse_colour(&color).is_none() {
        err(&mut e, "color", "live.err_choice");
    }
    if !(boards::REFRESH_MIN..=boards::REFRESH_MAX).contains(&input.refresh_seconds) {
        e.insert(
            "refresh_seconds",
            crate::i18n::tf(
                "live.err_refresh",
                &[
                    ("min", &boards::REFRESH_MIN.to_string()),
                    ("max", &boards::REFRESH_MAX.to_string()),
                ],
            ),
        );
    }
    let mention = input.mention.trim();
    if !mention.is_empty() && !(mention.len() <= 30 && mention.bytes().all(|b| b.is_ascii_digit()))
    {
        err(&mut e, "mention", "live.err_mention");
    }
    let lang = input.lang.trim();
    let lang = if lang.is_empty() {
        crate::i18n::site_default().code().to_string()
    } else {
        match crate::i18n::Lang::from_exact_code(lang) {
            Some(l) => l.code().to_string(),
            None => {
                err(&mut e, "lang", "live.err_choice");
                String::new()
            }
        }
    };

    if !e.is_empty() {
        return Ok(Err(e));
    }
    Ok(Ok(NewBoard {
        name: name.to_string(),
        webhook_url: sealed_webhook,
        platform: input.platform.clone(),
        source: input.source.clone(),
        status_page_id,
        show_uptime_24h: input.show_uptime_24h,
        show_uptime_30d: input.show_uptime_30d,
        show_response: input.show_response,
        show_incidents: input.show_incidents,
        title: title.to_string(),
        description: description.to_string(),
        color,
        refresh_seconds: input.refresh_seconds,
        notify_down: input.notify_down,
        mention: mention.to_string(),
        lang,
        items,
    }))
}

/// For a Fluxer webhook: asks the host's discovery document whether it is
/// a Fluxer instance with this API base. Discord addresses pass. The error
/// is a message for the form field.
pub async fn confirm_webhook(platform: &str, url: &str) -> Result<(), String> {
    let Some(platform) = Platform::parse(platform) else {
        return Ok(());
    };
    if platform != Platform::Fluxer {
        return Ok(());
    }
    let client = webhook::client();
    let target = client
        .parse(platform, url)
        .map_err(|e| crate::i18n::t(e.message_key()).to_string())?;
    client.confirm_instance(&target).await.map_err(|f| match f {
        webhook::Failure::Permanent(_) => crate::i18n::t("live.err_not_fluxer").to_string(),
        other => crate::i18n::tf(
            "live.err_fluxer_unreachable",
            &[("error", &other.message())],
        ),
    })
}

#[cfg(test)]
mod tests;
