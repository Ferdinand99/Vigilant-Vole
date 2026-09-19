use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt as _};

use super::{AppState, DashboardData, load_dashboard_data};

#[derive(Template)]
#[template(path = "monitor_list.html")]
struct MonitorListTemplate {
    data: DashboardData,
}

/// Streams a fresh `monitor_list.html` render every time the scheduler (or a
/// monitor CRUD action) broadcasts an update, so the dashboard refreshes
/// itself over SSE instead of requiring a manual page reload.
pub async fn stream(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.update_tx.subscribe();
    let db = state.db.clone();

    let events = BroadcastStream::new(rx)
        .then(move |_| {
            let db = db.clone();
            async move {
                let data = load_dashboard_data(&db).await;
                match (MonitorListTemplate { data }).render() {
                    Ok(html) => Event::default().event("monitors-updated").data(html),
                    Err(err) => {
                        tracing::warn!(%err, "failed to render monitor list for SSE update");
                        Event::default().comment("render-error")
                    }
                }
            }
        })
        .map(Ok::<_, Infallible>);

    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
