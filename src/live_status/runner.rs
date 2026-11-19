//! Keeps boards up to date: [`run_board`] does one refresh of one board,
//! [`spawn`] runs every active board on its interval and, a few seconds
//! after something changed, again.
//!
//! A refresh builds the board's messages ([`super::view`], [`super::layout`]),
//! then makes the channel match: it edits the message it remembers (the
//! content hash says whether anything changed), posts a new one when there is
//! none or the old one was deleted, and deletes the messages a shrunken
//! board no longer needs. When a message is posted anew, the ones below it are
//! posted anew too, so the channel keeps the order of the board.
//!
//! A board that fails five times in a row pauses with its error, as does one
//! whose webhook is refused for good (deleted, no access). "Retry" starts it
//! again. A rate limit is not a failure: the board waits and goes on.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::layout;
use super::view::{self, LoadError};
use super::webhook::{Client, Failure, Platform, Target};
use crate::api::AppState;
use crate::db::live_boards::{self as boards, Board};
use crate::i18n::Lang;

/// How long after a change the boards are refreshed.
pub const DEBOUNCE: Duration = Duration::from_secs(10);
/// Fewest seconds between two refreshes of one board.
pub const MIN_GAP: Duration = Duration::from_secs(10);
/// How often the scheduler looks at the boards.
const TICK: Duration = Duration::from_secs(5);
/// Most "went down" messages one refresh posts.
pub const MAX_DOWN_POSTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The board's interval: edit every message, which also notices a deleted one.
    Interval,
    /// Something changed: edit only what differs.
    Change,
    /// Somebody asked: like the interval, and also for a paused board.
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The channel matches the board.
    Ok,
    /// Nothing to do (paused, or nothing changed and nothing needed sending).
    Skipped,
    /// It failed; `paused` says the board stopped itself.
    Failed { message: String, paused: bool },
    /// The platform's rate limit: try again after this long.
    Deferred(Duration),
}

/// Boards being refreshed right now.
/// (data directory, board id): the data directory tells instances apart when
/// one process holds several (tests).
type RunKey = (std::path::PathBuf, i64);

static RUNNING: LazyLock<Mutex<HashSet<RunKey>>> = LazyLock::new(Mutex::default);

struct Guard(RunKey);

impl Guard {
    fn take(state: &AppState, id: i64) -> Option<Guard> {
        let key = (state.config.data_dir.clone(), id);
        RUNNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone())
            .then_some(Guard(key))
    }
}

/// Holds a board's refresh slot, so that nothing refreshes it meanwhile.
pub struct Exclusive(#[allow(dead_code)] Option<Guard>);

/// Waits (up to half a minute) for a running refresh of the board to end
/// and keeps others from starting until the result is dropped. Take it
/// around "delete its messages, then change or delete the board": a refresh
/// that ran in between could post into the old channel again.
pub async fn lock(state: &AppState, id: i64) -> Exclusive {
    for _ in 0..60 {
        if let Some(g) = Guard::take(state, id) {
            return Exclusive(Some(g));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Exclusive(None)
}

/// What the board looked like when it was last refreshed ([`view::fingerprint`]).
static FINGERPRINTS: LazyLock<Mutex<HashMap<RunKey, String>>> = LazyLock::new(Mutex::default);

fn fingerprint_of(key: &RunKey) -> Option<String> {
    FINGERPRINTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .cloned()
}

fn remember_fingerprint(key: RunKey, fp: Option<String>) {
    let mut map = FINGERPRINTS.lock().unwrap_or_else(PoisonError::into_inner);
    match fp {
        Some(fp) => {
            map.insert(key, fp);
        }
        None => {
            map.remove(&key);
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        RUNNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// The hash of what a message says, without its time stamp.
pub fn hash_payload(payload: &Value) -> String {
    let mut p = payload.clone();
    for e in p["embeds"].as_array_mut().into_iter().flatten() {
        if let Some(o) = e.as_object_mut() {
            o.remove("timestamp");
        }
    }
    hex::encode(Sha256::digest(p.to_string().as_bytes()))
}

fn mention_payload(board: &Board, lang: Lang, name: &str) -> Value {
    let title = crate::i18n::sync_scope(lang, || {
        crate::i18n::tf("msg.down", &[("name", &layout::clip(name, 200))])
    });
    let role = board.mention.trim();
    let role = (!role.is_empty() && role.bytes().all(|b| b.is_ascii_digit())).then_some(role);
    let mut payload = json!({
        "embeds": [{
            "title": layout::clip(&format!("\u{1f534} {title}"), layout::MAX_TITLE),
            "color": layout::RED,
            "timestamp": view_now(),
        }],
        "allowed_mentions": { "parse": [], "roles": role.into_iter().collect::<Vec<_>>() },
    });
    if let Some(role) = role {
        payload["content"] = json!(format!("<@&{role}>"));
    }
    payload
}

fn view_now() -> String {
    crate::maintenance::fmt_db_ts(Timestamp::now()).replacen(' ', "T", 1) + "Z"
}

async fn db<T, F>(state: &AppState, f: F) -> Result<T, String>
where
    F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    T: Send + 'static,
{
    crate::db::interact(&state.db, f)
        .await
        .map_err(|_| "could not read the database".to_string())
}

/// Records a failed refresh and says what came of it.
async fn fail(state: &AppState, id: i64, message: String, permanent: bool) -> Outcome {
    let text = message.clone();
    let paused = db(state, move |c| {
        boards::record_failure(c, id, &text, permanent)
    })
    .await
    .unwrap_or(false);
    tracing::warn!(board_id = id, error = %message, paused, "live status board refresh failed");
    Outcome::Failed { message, paused }
}

/// One refresh of one board.
pub async fn run_board(state: &AppState, client: &Client, id: i64, mode: Mode) -> Outcome {
    let Some(_guard) = Guard::take(state, id) else {
        return Outcome::Skipped;
    };
    let loaded = db(state, move |c| {
        Ok(match boards::get(c, id)? {
            Some(b) => {
                let m = boards::messages(c, id)?;
                Some((b, m))
            }
            None => None,
        })
    })
    .await;
    let (board, stored) = match loaded {
        Ok(Some(x)) => x,
        Ok(None) => return Outcome::Skipped,
        Err(message) => {
            return Outcome::Failed {
                message,
                paused: false,
            };
        }
    };
    if board.is_paused() && mode != Mode::Manual {
        return Outcome::Skipped;
    }

    // A change-driven refresh starts with a cheap look: when nothing that
    // decides the messages has changed, the view is not even built.
    let key = (state.config.data_dir.clone(), id);
    let (b, now) = (board.clone(), Timestamp::now());
    let fp = db(state, move |c| Ok(view::fingerprint(c, &b, now).ok()))
        .await
        .unwrap_or(None);
    if mode == Mode::Change && fp.is_some() && fp == fingerprint_of(&key) {
        return Outcome::Skipped;
    }

    let outcome = refresh(state, client, &board, stored, mode).await;
    match &outcome {
        Ok(()) => {
            remember_fingerprint(key, fp);
            let _ = db(state, move |c| boards::record_ok(c, id)).await;
            Outcome::Ok
        }
        Err(Failure::RateLimited(wait)) => Outcome::Deferred(*wait),
        Err(f) => {
            remember_fingerprint(key, None);
            fail(state, id, f.message(), f.is_permanent()).await
        }
    }
}

async fn refresh(
    state: &AppState,
    client: &Client,
    board: &Board,
    stored: Vec<boards::Message>,
    mode: Mode,
) -> Result<(), Failure> {
    let id = board.id;
    let url = crate::kit::notify::sealed::open_value(&state.session_key, &board.webhook_url)
        .map_err(|_| {
            Failure::Permanent(
                "the webhook address can't be read with this instance's key: enter it again".into(),
            )
        })?;
    let platform = Platform::parse(&board.platform)
        .ok_or_else(|| Failure::Permanent("unknown platform".into()))?;
    let target = client.parse(platform, &url).map_err(|_| {
        Failure::Permanent("the webhook address is not one this platform accepts".into())
    })?;
    let lang = Lang::from_code(&board.lang).unwrap_or_default();

    // What the board shows now.
    let b = board.clone();
    let loaded = db(state, move |c| {
        Ok(crate::i18n::sync_scope(lang, || {
            view::load(c, &b, Timestamp::now())
        }))
    })
    .await
    .map_err(Failure::Transient)?;
    let loaded = match loaded {
        Ok(l) => l,
        Err(e @ (LoadError::PageGone | LoadError::PageNotPublic)) => {
            return Err(Failure::Permanent(e.message()));
        }
        Err(e) => return Err(Failure::Transient(e.message())),
    };
    let payloads = layout::build(&loaded.view);

    // Make the channel match.
    let mut existing: BTreeMap<i64, boards::Message> =
        stored.into_iter().map(|m| (m.slot, m)).collect();
    let first_run = existing.is_empty();
    let mut recreate_rest = false;
    for (i, payload) in payloads.iter().enumerate() {
        let slot = i as i64;
        let hash = hash_payload(payload);
        let mut message_id = None;
        if let Some(m) = existing.remove(&slot) {
            if recreate_rest {
                // An earlier message is new: this one has to follow it.
                client.delete(&target, &m.message_id).await?;
            } else if mode == Mode::Change && m.content_hash == hash {
                continue;
            } else {
                match client.edit(&target, &m.message_id, payload).await {
                    Ok(()) => message_id = Some(m.message_id),
                    Err(Failure::MessageGone) => recreate_rest = true,
                    Err(f) => return Err(f),
                }
            }
        } else {
            recreate_rest = true;
        }
        let message_id = match message_id {
            Some(m) => m,
            None => client.create(&target, payload).await?,
        };
        let h = hash.clone();
        db(state, move |c| {
            boards::put_message(c, id, slot, &message_id, &h)
        })
        .await
        .map_err(Failure::Transient)?;
    }
    // Messages a smaller board no longer needs.
    for (slot, m) in existing {
        match client.delete(&target, &m.message_id).await {
            Ok(()) | Err(Failure::MessageGone | Failure::Permanent(_)) => {
                db(state, move |c| boards::delete_message(c, id, slot))
                    .await
                    .map_err(Failure::Transient)?;
            }
            Err(f) => return Err(f),
        }
    }

    announce_down(state, client, board, &target, lang, &loaded.down, first_run).await;
    Ok(())
}

/// Posts a message for each monitor that went down since the last refresh
/// (when the board does that) and remembers who is down.
async fn announce_down(
    state: &AppState,
    client: &Client,
    board: &Board,
    target: &Target,
    lang: Lang,
    down: &[(i64, String)],
    first_run: bool,
) {
    let mut known: Vec<i64> = down.iter().map(|(id, _)| *id).collect();
    if board.notify_down && !first_run {
        let mut posted = 0;
        for (mid, name) in down {
            if board.known_down.contains(mid) {
                continue;
            }
            if posted >= MAX_DOWN_POSTS {
                break;
            }
            posted += 1;
            if let Err(f) = client
                .post(target, &mention_payload(board, lang, name))
                .await
            {
                // Tell about it again at the next refresh.
                known.retain(|k| k != mid);
                tracing::warn!(board_id = board.id, error = %f.message(), "could not post a down message");
                if matches!(f, Failure::RateLimited(_)) {
                    break;
                }
            }
        }
    }
    known.sort_unstable();
    if known != board.known_down {
        let id = board.id;
        let _ = db(state, move |c| boards::set_known_down(c, id, &known)).await;
    }
}

// ---------------------------------------------------------------------------
// What the pages and the API call
// ---------------------------------------------------------------------------

/// "Refresh now" (also the first refresh after a save, and "Retry" after
/// [`boards::retry`]). Waits for the refresh.
pub async fn refresh_now(state: &AppState, id: i64) -> Outcome {
    run_board(state, super::webhook::client(), id, Mode::Manual).await
}

/// Refreshes a board in the background (after it was saved).
pub fn kick(state: &AppState, id: i64) {
    // Tests don't talk to the platforms.
    if cfg!(test) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let _ = refresh_now(&state, id).await;
    });
}

/// Deletes the messages a board keeps in its channel, as far as that works
/// (a webhook that is gone or refused is skipped).
pub async fn delete_messages(state: &AppState, client: &Client, board: &Board) {
    let id = board.id;
    let Ok(stored) = db(state, move |c| boards::messages(c, id)).await else {
        return;
    };
    if stored.is_empty() {
        return;
    }
    let Ok(url) = crate::kit::notify::sealed::open_value(&state.session_key, &board.webhook_url)
    else {
        return;
    };
    let Some(platform) = Platform::parse(&board.platform) else {
        return;
    };
    let Ok(target) = client.parse(platform, &url) else {
        return;
    };
    for m in stored {
        if let Err(f) = client.delete(&target, &m.message_id).await {
            tracing::info!(board_id = id, error = %f.message(), "could not delete a board message");
            if f.is_permanent() || matches!(f, Failure::RateLimited(_)) {
                break;
            }
        }
    }
}

/// "Test": posts a short message to the webhook and deletes it again.
/// The error is safe to show.
pub async fn test_webhook(
    client: &Client,
    platform: Platform,
    url: &str,
    lang: Lang,
    name: &str,
) -> Result<(), String> {
    let target = client
        .parse(platform, url)
        .map_err(|e| crate::i18n::t(e.message_key()).to_string())?;
    let text = crate::i18n::sync_scope(lang, || {
        crate::i18n::tf("live.test_text", &[("name", &layout::clip(name, 100))])
    });
    let payload = json!({
        "embeds": [{
            "title": layout::clip(&text, layout::MAX_TITLE),
            "color": layout::PURPLE,
            "footer": { "text": crate::product::NAME },
        }],
        "allowed_mentions": { "parse": [] },
    });
    let message_id = client
        .create(&target, &payload)
        .await
        .map_err(|f| f.message())?;
    // The test message is not needed any more; if it can't be deleted the
    // webhook still works.
    let _ = client.delete(&target, &message_id).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Times {
    last_run: Option<Instant>,
    not_before: Option<Instant>,
}

/// Refreshes the background job runs at once.
static PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

static TIMES: LazyLock<Mutex<HashMap<i64, Times>>> = LazyLock::new(Mutex::default);

fn times<R>(f: impl FnOnce(&mut HashMap<i64, Times>) -> R) -> R {
    f(&mut TIMES.lock().unwrap_or_else(PoisonError::into_inner))
}

/// What to do with a board now: its interval is up, something changed and
/// the board has not run in the last [`MIN_GAP`], or nothing.
fn due(board: &Board, changed: bool, now: Instant) -> Option<Mode> {
    times(|t| {
        let t = t.entry(board.id).or_default();
        if t.not_before.is_some_and(|nb| nb > now) {
            return None;
        }
        let since = t.last_run.map(|l| now.saturating_duration_since(l));
        match since {
            None => Some(Mode::Interval),
            Some(s) if s >= Duration::from_secs(board.refresh_seconds.max(60) as u64) => {
                Some(Mode::Interval)
            }
            Some(s) if changed && s >= MIN_GAP => Some(Mode::Change),
            _ => None,
        }
    })
}

/// The background job: runs every active board on its interval and, after a
/// change, again within [`DEBOUNCE`] (plus the scheduler's tick).
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut changes = state.update_tx.subscribe();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty: Option<Instant> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                r = changes.recv() => {
                    match r {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            dirty.get_or_insert_with(Instant::now);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                    continue;
                }
            }
            let Ok(active) = db(&state, |c| boards::list_active(c)).await else {
                continue;
            };
            let now = Instant::now();
            let changed = dirty.is_some_and(|d| now.saturating_duration_since(d) >= DEBOUNCE);
            let mut waiting = false;
            for board in &active {
                let Some(mode) = due(board, changed, now) else {
                    // A change waits for a board that ran a moment ago.
                    waiting |= changed
                        && times(|t| t.get(&board.id).and_then(|t| t.last_run))
                            .is_some_and(|l| now.saturating_duration_since(l) < MIN_GAP);
                    continue;
                };
                times(|t| t.entry(board.id).or_default().last_run = Some(now));
                let state = state.clone();
                let id = board.id;
                tokio::spawn(async move {
                    // A few refreshes at a time, so many boards can't flood the database.
                    let _permit = PERMITS.acquire().await;
                    let outcome = run_board(&state, super::webhook::client(), id, mode).await;
                    if let Outcome::Deferred(wait) = outcome {
                        times(|t| {
                            t.entry(id).or_default().not_before = Some(Instant::now() + wait);
                        });
                    }
                });
            }
            if changed && !waiting {
                dirty = None;
            }
            // Forget boards that no longer exist.
            let ids: HashSet<i64> = active.iter().map(|b| b.id).collect();
            times(|t| t.retain(|id, _| ids.contains(id)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(id: i64, refresh: i64) -> Board {
        Board {
            id,
            name: "b".into(),
            webhook_url: String::new(),
            platform: "discord".into(),
            source: "items".into(),
            status_page_id: None,
            show_uptime_24h: true,
            show_uptime_30d: true,
            show_response: false,
            show_incidents: true,
            title: String::new(),
            description: String::new(),
            color: String::new(),
            refresh_seconds: refresh,
            notify_down: false,
            mention: String::new(),
            lang: "en".into(),
            state: "active".into(),
            failures: 0,
            last_error: None,
            last_ok_at: None,
            known_down: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn the_time_stamp_is_not_part_of_what_changed() {
        let a = json!({"embeds": [{"title": "x", "timestamp": "2026-11-19T10:00:00Z"}]});
        let b = json!({"embeds": [{"title": "x", "timestamp": "2026-11-19T10:05:00Z"}]});
        let c = json!({"embeds": [{"title": "y", "timestamp": "2026-11-19T10:00:00Z"}]});
        assert_eq!(hash_payload(&a), hash_payload(&b));
        assert_ne!(hash_payload(&a), hash_payload(&c));
    }

    #[test]
    fn a_board_is_due_on_its_interval_and_after_a_change_but_not_more_often_than_the_gap() {
        let id = 9_000_001;
        let b = board(id, 300);
        let t0 = Instant::now();
        // Never ran: due.
        assert_eq!(due(&b, false, t0), Some(Mode::Interval));
        times(|t| t.entry(id).or_default().last_run = Some(t0));
        // A moment later: not due, and a change has to wait for the gap.
        assert_eq!(due(&b, false, t0 + Duration::from_secs(5)), None);
        assert_eq!(due(&b, true, t0 + Duration::from_secs(5)), None);
        assert_eq!(
            due(&b, true, t0 + Duration::from_secs(11)),
            Some(Mode::Change)
        );
        assert_eq!(due(&b, false, t0 + Duration::from_secs(299)), None);
        assert_eq!(
            due(&b, false, t0 + Duration::from_secs(300)),
            Some(Mode::Interval)
        );
        // A rate limit holds the board back.
        times(|t| t.entry(id).or_default().not_before = Some(t0 + Duration::from_secs(400)));
        assert_eq!(due(&b, false, t0 + Duration::from_secs(350)), None);
        assert_eq!(
            due(&b, false, t0 + Duration::from_secs(401)),
            Some(Mode::Interval)
        );
        times(|t| t.remove(&id));
    }
}
