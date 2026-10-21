//! `/events` SSE stream for the live dashboard. Owned by the monitors agent.
//!
//! The dashboard connects with its own query string (`/events?status=down…`),
//! so every update renders `monitor_list.html` for exactly the view the user
//! is looking at. Bursts of updates are coalesced: after the first signal we
//! wait [`COALESCE`] and swallow everything that arrived meanwhile, so a busy
//! fleet re-renders at most a couple of times per second per client.

use crate::i18n::Tr as _;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use tokio::sync::broadcast::{self, error::RecvError};

use super::dashboard::{DashQuery, DashboardData, load_dashboard_data};
use super::{ApiError, AppState};
use crate::auth::CurrentUser;
use crate::db::users as db_users;

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

/// Waits for the next update signal, then drains whatever else arrives
/// within the coalescing window. `false` once the channel is closed.
async fn next_update(rx: &mut broadcast::Receiver<()>, quiet: Duration) -> bool {
    match rx.recv().await {
        Ok(()) | Err(RecvError::Lagged(_)) => {}
        Err(RecvError::Closed) => return false,
    }
    tokio::time::sleep(quiet).await;
    loop {
        match rx.try_recv() {
            Ok(()) | Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(broadcast::error::TryRecvError::Empty) => return true,
            Err(broadcast::error::TryRecvError::Closed) => return false,
        }
    }
}

/// Renders the list as an SSE event; also returns its size in bytes.
async fn render(state: &AppState, query: &DashQuery, can_edit: bool) -> (Event, usize) {
    let rendered = match load_dashboard_data(state, query, can_edit).await {
        Ok(data) => MonitorListTemplate { data }
            .render()
            .map_err(|e| e.to_string()),
        Err(err) => Err(err.to_string()),
    };
    match rendered {
        Ok(html) => {
            let len = html.len();
            (Event::default().event("monitors-updated").data(html), len)
        }
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

static OPEN_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// One slot of [`MAX_STREAMS`]; released when the stream is dropped.
struct StreamSlot;

impl StreamSlot {
    fn acquire() -> Option<Self> {
        OPEN_STREAMS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_STREAMS).then_some(n + 1)
            })
            .ok()
            .map(|_| StreamSlot)
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        OPEN_STREAMS.fetch_sub(1, Ordering::AcqRel);
    }
}

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
    rx: broadcast::Receiver<()>,
    state: AppState,
    query: DashQuery,
    quiet: Duration,
    viewer: Viewer,
    deadline: tokio::time::Instant,
    _slot: StreamSlot,
}

async fn stream_handler(
    State(state): State<AppState>,
    user: CurrentUser,
    RawQuery(raw): RawQuery,
) -> Response {
    let Some(slot) = StreamSlot::acquire() else {
        tracing::warn!(
            max = MAX_STREAMS,
            "too many live-update streams; refusing a new one"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "30")],
        )
            .into_response();
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
    // The stream is polled after this handler returns, outside the request's
    // language scope, so carry the language along.
    let lang = crate::i18n::current();
    let init = StreamState {
        rx: state.update_tx.subscribe(),
        state,
        query,
        quiet: COALESCE,
        viewer: Viewer {
            user_id: user.id,
            session_version,
        },
        deadline: tokio::time::Instant::now() + MAX_STREAM_LIFETIME,
        _slot: slot,
    };
    let events = stream::unfold(init, move |mut st| async move {
        match tokio::time::timeout_at(st.deadline, next_update(&mut st.rx, st.quiet)).await {
            Ok(true) => {}
            Ok(false) | Err(_) => return None,
        }
        let can_edit = revalidate(&st.state, st.viewer).await?;
        let (event, bytes) = crate::i18n::scope(lang, render(&st.state, &st.query, can_edit)).await;
        st.quiet = quiet_period(bytes);
        Some((Ok::<_, Infallible>(event), st))
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bursts_coalesce_into_one_update() {
        let (tx, mut rx) = broadcast::channel(4);
        for _ in 0..10 {
            let _ = tx.send(());
        }
        assert!(next_update(&mut rx, COALESCE).await);
        // Everything from the burst (including lagged messages) was drained.
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        drop(tx);
        assert!(!next_update(&mut rx, COALESCE).await);
    }

    #[test]
    fn stream_slots_are_capped_and_released() {
        // Other tests may hold slots; only count the ones taken here.
        let mut held = Vec::new();
        while let Some(slot) = StreamSlot::acquire() {
            held.push(slot);
            assert!(held.len() <= MAX_STREAMS);
        }
        assert!(StreamSlot::acquire().is_none());
        held.pop();
        assert!(StreamSlot::acquire().is_some());
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
