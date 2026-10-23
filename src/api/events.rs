//! `/events` SSE stream for the live dashboard. Owned by the monitors agent.
//!
//! The dashboard connects with its own query string (`/events?status=down…`),
//! so every update renders `monitor_list.html` for exactly the view the user
//! is looking at. Bursts of updates are coalesced: after the first signal we
//! wait [`COALESCE`] and swallow everything that arrived meanwhile, so a busy
//! fleet re-renders at most a couple of times per second per client.
//!
//! Streams showing the same view share one render ([`shared_render`]): every
//! open dashboard wakes on the same signal, so without sharing, N dashboards
//! cost N identical renders per update.
//!
//! The coalescing, the shared renders, the stream cap and the SSE response
//! are vole-kit's (`kit::http::live`).

use crate::i18n::Tr as _;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use tokio::time::Instant;

use super::dashboard::{DashQuery, DashboardData, load_dashboard_data};
use super::{ApiError, AppState};
use crate::auth::CurrentUser;
use crate::db::users as db_users;
use crate::kit::http::live::{self, Changes, SharedRenders, StreamLimit, StreamSlot};

/// Quiet period that merges bursts of updates into one render.
const COALESCE: Duration = Duration::from_millis(500);
/// Upper bound for the adaptive quiet period (huge fleets).
const MAX_COALESCE: Duration = Duration::from_secs(5);

/// Bigger renders (hundreds of monitors) wait longer between updates so a
/// busy fleet can't turn every open dashboard into a firehose: ~1 s per
/// 200 KB of HTML, between [`COALESCE`] and [`MAX_COALESCE`].
fn quiet_period(rendered_bytes: usize) -> Duration {
    let ms = (rendered_bytes as u64).saturating_mul(1000) / (200 * 1024);
    Duration::from_millis(ms).clamp(COALESCE, MAX_COALESCE)
}

pub fn router() -> Router<AppState> {
    Router::new().route("/events", get(stream_handler))
}

/// The live region's content (stats + lists); swapped into the SSE element.
#[derive(Template)]
#[template(path = "monitor_list.html")]
pub struct MonitorListTemplate {
    pub data: DashboardData,
}

/// Renders the list's HTML.
async fn render_html(
    state: &AppState,
    query: &DashQuery,
    can_edit: bool,
) -> Result<String, String> {
    let data = load_dashboard_data(state, query, can_edit)
        .await
        .map_err(|e| e.to_string())?;
    MonitorListTemplate { data }
        .render()
        .map_err(|e| e.to_string())
}

/// (data dir, raw query, can edit, language): what a render depends on. The
/// data dir keeps app instances apart (tests run several at once).
type RenderKey = (PathBuf, String, bool, crate::i18n::Lang);

/// More distinct views than this and slots no stream is using are dropped.
const RENDER_SLOTS_MAX: usize = 64;

static RENDERS: LazyLock<SharedRenders<RenderKey>> =
    LazyLock::new(|| SharedRenders::new(RENDER_SLOTS_MAX));

/// The list's HTML for this view, rendered after `since`. Streams that wake
/// for the same update wait for one render and reuse it (single flight per
/// view); a render that started before `since` is never reused.
async fn shared_render(
    state: &AppState,
    query: &DashQuery,
    raw_query: &str,
    can_edit: bool,
    since: Instant,
) -> Result<Arc<str>, String> {
    let key: RenderKey = (
        state.config.data_dir.clone(),
        raw_query.to_owned(),
        can_edit,
        crate::i18n::current(),
    );
    RENDERS
        .get(key, since, || render_html(state, query, can_edit))
        .await
}

/// Renders the list as an SSE event; also returns its size in bytes.
async fn render(
    state: &AppState,
    query: &DashQuery,
    raw_query: &str,
    can_edit: bool,
    since: Instant,
) -> (Event, usize) {
    match shared_render(state, query, raw_query, can_edit, since).await {
        Ok(html) => (live::html_event("monitors-updated", &html), html.len()),
        Err(err) => {
            tracing::warn!(%err, "failed to render monitor list for SSE update");
            (Event::default().comment("render-error"), 0)
        }
    }
}

/// Open `/events` streams across all clients; beyond this new ones get 503.
const MAX_STREAMS: usize = 256;
/// A stream ends after this long; the browser's `EventSource` reconnects,
/// which re-runs authentication.
const MAX_STREAM_LIFETIME: Duration = Duration::from_secs(60 * 60);

static STREAMS: StreamLimit = StreamLimit::new(MAX_STREAMS);

/// Who the stream was opened for: re-checked before every render so a
/// logout elsewhere, a disabled account or a role change takes effect.
#[derive(Debug, Clone, Copy)]
struct Viewer {
    user_id: i64,
    session_version: i64,
}

/// The user's current edit permission, or `None` when the stream must end
/// (account gone or disabled, or its sessions were revoked).
async fn revalidate(state: &AppState, viewer: Viewer) -> Option<bool> {
    let id = viewer.user_id;
    match state.with_conn(move |c| db_users::get_user(c, id)).await {
        Ok(Some(u)) if u.active && u.session_version == viewer.session_version => {
            Some(u.role.can_edit())
        }
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(%err, "SSE: could not re-check the user; closing stream");
            None
        }
    }
}

struct StreamState {
    changes: Changes<()>,
    state: AppState,
    query: DashQuery,
    /// The query string as sent: streams with the same one share renders.
    raw_query: String,
    viewer: Viewer,
    _slot: StreamSlot,
}

async fn stream_handler(
    State(state): State<AppState>,
    user: CurrentUser,
    RawQuery(raw): RawQuery,
) -> Response {
    let Some(slot) = STREAMS.acquire() else {
        tracing::warn!(
            max = MAX_STREAMS,
            "too many live-update streams; refusing a new one"
        );
        return live::busy();
    };
    let id = user.id;
    let session_version = match state.with_conn(move |c| db_users::get_user(c, id)).await {
        Ok(Some(u)) if u.active => u.session_version,
        Ok(_) => return ApiError::Unauthorized.into_response(),
        Err(err) => return err.into_response(),
    };
    let mut query = DashQuery::parse(raw.as_deref());
    // Select mode is static (checkboxes would be wiped by live swaps).
    query.select = false;
    let raw_query = raw.unwrap_or_default();
    // The stream is polled after this handler returns, outside the request's
    // language scope, so carry the language along.
    let lang = crate::i18n::current();
    let init = StreamState {
        changes: Changes::all(state.update_tx.subscribe())
            .quiet(COALESCE)
            .lifetime(MAX_STREAM_LIFETIME),
        state,
        query,
        raw_query,
        viewer: Viewer {
            user_id: user.id,
            session_version,
        },
        _slot: slot,
    };
    let events = stream::unfold(init, move |mut st| async move {
        let since = st.changes.next().await?;
        let can_edit = revalidate(&st.state, st.viewer).await?;
        let render = render(&st.state, &st.query, &st.raw_query, can_edit, since);
        let (event, bytes) = crate::i18n::scope(lang, render).await;
        st.changes.set_quiet(quiet_period(bytes));
        Some((event, st))
    });
    live::sse(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bursts_coalesce_into_one_update() {
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        let mut changes = Changes::all(rx).quiet(COALESCE);
        for _ in 0..10 {
            let _ = tx.send(());
        }
        assert!(changes.next().await.is_some());
        drop(tx);
        // Everything from the burst (including lagged messages) was drained.
        assert!(changes.next().await.is_none());
    }

    #[tokio::test]
    async fn streams_woken_by_one_update_share_a_render() {
        let state = crate::api::agents::test_support::test_state().await;
        let query = DashQuery::parse(None);
        let since = Instant::now();
        let (a, b) = tokio::join!(
            shared_render(&state, &query, "", true, since),
            shared_render(&state, &query, "", true, since),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(Arc::ptr_eq(&a, &b), "one render for both streams");

        // Another view (here: without edit rights) renders on its own.
        let viewer = shared_render(&state, &query, "", false, since)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &viewer));

        // A later update needs a render that started after it...
        tokio::time::sleep(Duration::from_millis(5)).await;
        let later = shared_render(&state, &query, "", true, Instant::now())
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &later));
        // ...and a newer render also serves streams woken by older updates.
        let again = shared_render(&state, &query, "", true, since)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&later, &again));
    }

    #[test]
    fn stream_slots_are_capped_and_released() {
        // Other tests may hold slots; only count the ones taken here.
        let mut held = Vec::new();
        while let Some(slot) = STREAMS.acquire() {
            held.push(slot);
            assert!(held.len() <= MAX_STREAMS);
        }
        assert!(STREAMS.acquire().is_none());
        held.pop();
        assert!(STREAMS.acquire().is_some());
    }

    #[tokio::test]
    async fn revalidation_ends_streams_for_revoked_or_disabled_users() {
        use crate::db::models::Role;
        let state = crate::api::agents::test_support::test_state().await;
        let id = state
            .with_conn(|c| db_users::insert_user(c, "sse-viewer", "x", Role::Editor))
            .await
            .unwrap();
        let viewer = Viewer {
            user_id: id,
            session_version: state
                .with_conn(move |c| db_users::get_user(c, id))
                .await
                .unwrap()
                .unwrap()
                .session_version,
        };
        assert_eq!(revalidate(&state, viewer).await, Some(true));

        // Role change: the next render drops the edit controls.
        state
            .with_conn(move |c| c.execute("UPDATE users SET role = 'viewer' WHERE id = ?1", [id]))
            .await
            .unwrap();
        assert_eq!(revalidate(&state, viewer).await, Some(false));

        // Logout everywhere / password change bumps the version.
        state
            .with_conn(move |c| db_users::bump_session_version(c, id))
            .await
            .unwrap();
        assert_eq!(revalidate(&state, viewer).await, None);

        // Unknown users end the stream too.
        let gone = Viewer {
            user_id: id + 1000,
            ..viewer
        };
        assert_eq!(revalidate(&state, gone).await, None);
    }

    #[test]
    fn quiet_period_grows_with_render_size() {
        assert_eq!(quiet_period(0), COALESCE);
        assert_eq!(quiet_period(400 * 1024), Duration::from_secs(2));
        assert_eq!(quiet_period(50 * 1024 * 1024), MAX_COALESCE);
    }
}
