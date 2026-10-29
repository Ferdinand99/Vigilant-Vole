//! Live updates over server-sent events: a page keeps an `EventSource` open
//! (htmx's `sse-connect`) and the server pushes freshly rendered HTML when
//! something it shows changed.
//!
//! The pieces, from the change to the browser:
//!
//! * a `tokio::sync::broadcast` channel the product sends on after each
//!   change (`()` when any change matters to every view, or an id: the
//!   project, the monitor);
//! * [`Changes`]: one open stream's view of that channel. It waits for a
//!   change it cares about, then for a quiet period, and swallows whatever
//!   else arrived meanwhile, so a burst of changes costs one render;
//! * [`SharedRenders`]: streams woken by the same change that show the same
//!   view share one render (single flight per view);
//! * [`StreamLimit`]: a cap on open streams;
//! * [`sse`]: the response, with a keep-alive comment every
//!   [`KEEP_ALIVE`].
//!
//! ```ignore
//! async fn live(State(app): State<App>, Path(id): Path<i64>) -> Response {
//!     let Some(slot) = STREAMS.acquire() else { return live::busy() };
//!     let changes = Changes::new(app.changes.subscribe(), move |p: &i64| *p == id)
//!         .quiet(Duration::from_secs(1));
//!     let lang = i18n::current();
//!     live::sse(futures_util::stream::unfold((changes, slot), move |(mut changes, slot)| {
//!         let app = app.clone();
//!         async move {
//!             changes.next().await?;
//!             let html = i18n::scope(lang, render(&app, id)).await.ok()?;
//!             Some((Event::default().event("list-updated").data(html), (changes, slot)))
//!         }
//!     }))
//! }
//! ```
//!
//! A stream is polled after its handler returned, outside the request's
//! language scope: carry the language along (`i18n::current()`, then
//! `i18n::scope`), as above.

use std::collections::HashMap;
use std::convert::Infallible;
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt as _};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use tokio::time::Instant;

/// How often an idle stream sends a comment, so proxies keep it open.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// How long a stream lives unless [`Changes::lifetime`] says otherwise. A
/// stream is authorised once, when it opens; ending it after an hour makes
/// the browser's `EventSource` reconnect, which authorises it again (a
/// signed-out or demoted user loses the stream).
pub const DEFAULT_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// The quiet period [`Changes`] waits by default.
pub const DEFAULT_QUIET: Duration = Duration::from_millis(500);

/// One open stream's view of a broadcast channel of changes: wakes for the
/// changes `wanted` accepts, coalesced.
pub struct Changes<T, F = fn(&T) -> bool> {
    rx: broadcast::Receiver<T>,
    wanted: F,
    quiet: Duration,
    deadline: Option<Instant>,
}

/// Accepts every change ([`Changes::all`]).
pub fn any_change<T>(_: &T) -> bool {
    true
}

impl<T: Clone> Changes<T> {
    /// Wakes for every change on `rx`.
    pub fn all(rx: broadcast::Receiver<T>) -> Self {
        Changes::new(rx, any_change::<T> as fn(&T) -> bool)
    }
}

impl<T: Clone, F: Fn(&T) -> bool> Changes<T, F> {
    /// Wakes for the changes on `rx` that `wanted` accepts. A receiver that
    /// fell behind (lagged) counts as a change: it may have missed one.
    pub fn new(rx: broadcast::Receiver<T>, wanted: F) -> Self {
        Changes {
            rx,
            wanted,
            quiet: DEFAULT_QUIET,
            deadline: Some(Instant::now() + DEFAULT_LIFETIME),
        }
    }

    /// The quiet period after a change, which merges a burst into one
    /// update ([`DEFAULT_QUIET`] unless set).
    pub fn quiet(mut self, quiet: Duration) -> Self {
        self.quiet = quiet;
        self
    }

    /// Changes the quiet period, say by how big the last render was.
    pub fn set_quiet(&mut self, quiet: Duration) {
        self.quiet = quiet;
    }

    /// Ends the stream after `lifetime` ([`DEFAULT_LIFETIME`] unless set):
    /// the browser's `EventSource` reconnects, which runs the route's
    /// authentication again.
    pub fn lifetime(mut self, lifetime: Duration) -> Self {
        self.deadline = Some(Instant::now() + lifetime);
        self
    }

    /// Waits for the next wanted change, then for the quiet period, and
    /// drains everything that arrived meanwhile. Returns when the first
    /// change arrived (a render started after that shows it), or `None`
    /// once the channel is closed or the lifetime is over.
    pub async fn next(&mut self) -> Option<Instant> {
        match self.deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, self.next_change())
                .await
                .ok()
                .flatten(),
            None => self.next_change().await,
        }
    }

    async fn next_change(&mut self) -> Option<Instant> {
        loop {
            match self.rx.recv().await {
                Ok(change) if (self.wanted)(&change) => break,
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => break,
                Err(RecvError::Closed) => return None,
            }
        }
        let signalled = Instant::now();
        tokio::time::sleep(self.quiet).await;
        loop {
            match self.rx.try_recv() {
                Ok(_) | Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Empty) => return Some(signalled),
                Err(TryRecvError::Closed) => return None,
            }
        }
    }
}

/// The latest render of one view: when it started (it shows every change
/// signalled before that) and its HTML.
struct Render {
    started: Instant,
    html: Arc<str>,
}

type Slot = Arc<tokio::sync::Mutex<Option<Render>>>;

/// Renders shared by the streams that show the same view (`K`: whatever a
/// render depends on, like the query string, the viewer's rights and the
/// language). Streams that wake for the same change wait for one render and
/// reuse it; a render that started before the change is never reused.
pub struct SharedRenders<K> {
    slots: Mutex<HashMap<K, Slot>>,
    max_views: usize,
}

impl<K: Hash + Eq + Clone> SharedRenders<K> {
    /// Keeps up to `max_views` views; beyond that, views no stream is
    /// rendering are dropped.
    pub fn new(max_views: usize) -> Self {
        SharedRenders {
            slots: Mutex::new(HashMap::new()),
            max_views: max_views.max(1),
        }
    }

    /// `key`'s HTML, rendered after `since` (from [`Changes::next`]): the
    /// render another stream started after `since`, or a new one by
    /// `render`.
    pub async fn get<E, Fut>(
        &self,
        key: K,
        since: Instant,
        render: impl FnOnce() -> Fut,
    ) -> Result<Arc<str>, E>
    where
        Fut: Future<Output = Result<String, E>>,
    {
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            if slots.len() >= self.max_views && !slots.contains_key(&key) {
                slots.retain(|_, slot| Arc::strong_count(slot) > 1);
            }
            slots.entry(key).or_default().clone()
        };
        let mut latest = slot.lock().await;
        if let Some(shared) = latest.as_ref()
            && shared.started >= since
        {
            return Ok(shared.html.clone());
        }
        let started = Instant::now();
        let html: Arc<str> = render().await?.into();
        *latest = Some(Render {
            started,
            html: html.clone(),
        });
        Ok(html)
    }

    /// Views with a slot.
    pub fn len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// No view has a slot.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A cap on the streams open at once, across all clients. Keep it in a
/// `static`.
pub struct StreamLimit {
    max: usize,
    open: AtomicUsize,
}

/// One of a [`StreamLimit`]'s places, given back when it is dropped: keep it
/// in the stream's state.
pub struct StreamSlot {
    limit: &'static StreamLimit,
}

impl StreamLimit {
    pub const fn new(max: usize) -> Self {
        StreamLimit {
            max,
            open: AtomicUsize::new(0),
        }
    }

    /// A place for one more stream, or `None` when all are taken (answer
    /// with [`busy`]).
    pub fn acquire(&'static self) -> Option<StreamSlot> {
        self.open
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max).then_some(n + 1)
            })
            .ok()
            .map(|_| StreamSlot { limit: self })
    }

    /// Streams open now.
    pub fn open(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.limit.open.fetch_sub(1, Ordering::AcqRel);
    }
}

/// `503 Service Unavailable` with `Retry-After: 30`: too many streams are
/// open. The browser's `EventSource` tries again.
pub fn busy() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "30")],
    )
        .into_response()
}

/// The SSE response for `events`, with a keep-alive comment every
/// [`KEEP_ALIVE`]. The stream ends the response when it ends.
pub fn sse<S>(events: S) -> Response
where
    S: Stream<Item = Event> + Send + 'static,
{
    Sse::new(events.map(Ok::<_, Infallible>))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// A rendered fragment as an event named `name` (htmx's `sse-swap`).
pub fn html_event(name: &str, html: &str) -> Event {
    Event::default().event(name).data(html)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;

    #[tokio::test]
    async fn bursts_coalesce_into_one_update() {
        let (tx, rx) = broadcast::channel(4);
        let mut changes = Changes::all(rx).quiet(Duration::from_millis(20));
        for _ in 0..10 {
            let _ = tx.send(());
        }
        assert!(changes.next().await.is_some());
        // Everything from the burst (including lagged messages) was drained.
        assert!(matches!(changes.rx.try_recv(), Err(TryRecvError::Empty)));
        drop(tx);
        assert!(changes.next().await.is_none());
    }

    #[tokio::test]
    async fn only_wanted_changes_wake_a_stream() {
        let (tx, rx) = broadcast::channel(16);
        let mut changes = Changes::new(rx, |project: &i64| *project == 7).quiet(Duration::ZERO);
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        let woke = tokio::time::timeout(Duration::from_millis(50), changes.next()).await;
        assert!(woke.is_err(), "other projects don't wake it");
        tx.send(7).unwrap();
        assert!(changes.next().await.is_some());
    }

    #[tokio::test]
    async fn a_stream_has_a_lifetime_without_asking_for_one() {
        let (_tx, rx) = broadcast::channel::<()>(4);
        let changes = Changes::all(rx);
        let deadline = changes.deadline.expect("a default lifetime");
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            left > DEFAULT_LIFETIME - Duration::from_secs(60),
            "{left:?}"
        );
        assert!(left <= DEFAULT_LIFETIME);
        let (_tx, rx) = broadcast::channel::<()>(4);
        let short =
            Changes::new(rx, any_change::<()> as fn(&()) -> bool).lifetime(Duration::from_secs(5));
        let left = short
            .deadline
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(left <= Duration::from_secs(5), "{left:?}");
    }

    #[tokio::test]
    async fn a_stream_ends_after_its_lifetime() {
        let (_tx, rx) = broadcast::channel::<()>(4);
        let mut changes = Changes::all(rx).lifetime(Duration::from_millis(20));
        assert!(changes.next().await.is_none());
    }

    #[tokio::test]
    async fn streams_woken_by_one_change_share_a_render() {
        let renders = SharedRenders::new(8);
        let count = AtomicU32::new(0);
        let count = &count;
        let render = move || async move {
            let n = count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok::<_, Infallible>(format!("render {n}"))
        };
        let since = Instant::now();
        let (a, b) = tokio::join!(
            renders.get("view", since, render),
            renders.get("view", since, render),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(Arc::ptr_eq(&a, &b), "one render for both streams");
        let other = renders.get("other", since, render).await.unwrap();
        assert!(!Arc::ptr_eq(&a, &other));

        // A later change needs a render that started after it...
        tokio::time::sleep(Duration::from_millis(5)).await;
        let later = renders.get("view", Instant::now(), render).await.unwrap();
        assert!(!Arc::ptr_eq(&a, &later));
        // ...and a newer render also serves streams woken by older changes.
        let again = renders.get("view", since, render).await.unwrap();
        assert!(Arc::ptr_eq(&later, &again));
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn idle_views_make_room() {
        let renders = SharedRenders::new(2);
        let now = Instant::now();
        let ok = || async { Ok::<_, Infallible>(String::new()) };
        for key in 0..5 {
            renders.get(key, now, ok).await.unwrap();
        }
        assert!(renders.len() <= 2);
    }

    #[test]
    fn stream_slots_are_capped_and_released() {
        static LIMIT: StreamLimit = StreamLimit::new(3);
        let a = LIMIT.acquire().unwrap();
        let _b = LIMIT.acquire().unwrap();
        let _c = LIMIT.acquire().unwrap();
        assert!(LIMIT.acquire().is_none());
        assert_eq!(LIMIT.open(), 3);
        drop(a);
        assert!(LIMIT.acquire().is_some());
    }

    #[tokio::test]
    async fn sse_responses_stream_events() {
        let res = sse(futures_util::stream::iter([html_event(
            "updated", "<p>1</p>",
        )]));
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"event: updated\ndata: <p>1</p>\n\n");
        assert_eq!(busy().status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
