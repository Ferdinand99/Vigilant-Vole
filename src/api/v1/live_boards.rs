//! `/api/v1/live-boards/**`: Discord and Fluxer live status boards. Anyone
//! signed in can list them; changing them needs an editor.
//!
//! The webhook address is a secret: it is accepted on create and update,
//! and never returned (`webhook_set` says whether there is one). Leaving it
//! out of an update keeps the saved one.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Body, Id, V1Error, V1Result, created, json_ok, no_content};
use crate::api::AppState;
use crate::auth::{CurrentUser, Role};
use crate::db::live_boards::{self as boards, Board};
use crate::live_status::{self, Errors, Input, runner};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/live-boards", get(list).post(create))
        .route(
            "/live-boards/{id}",
            get(show).put(update).patch(update).delete(remove),
        )
        .route("/live-boards/{id}/test", post(test))
        .route("/live-boards/{id}/refresh", post(refresh))
        .route("/live-boards/{id}/retry", post(retry))
}

/// A board as the API shows it: everything but the webhook address.
pub fn board_json(b: &Board, items: &[i64]) -> Value {
    json!({
        "id": b.id,
        "name": b.name,
        "platform": b.platform,
        "webhook_set": !b.webhook_url.is_empty(),
        "source": b.source,
        "status_page_id": b.status_page_id,
        "items": items,
        "show_uptime_24h": b.show_uptime_24h,
        "show_uptime_30d": b.show_uptime_30d,
        "show_response": b.show_response,
        "show_incidents": b.show_incidents,
        "title": b.title,
        "description": b.description,
        "color": b.color,
        "refresh_seconds": b.refresh_seconds,
        "notify_down": b.notify_down,
        "mention": b.mention,
        "lang": b.lang,
        "state": b.state,
        "failures": b.failures,
        "last_error": b.last_error,
        "last_ok_at": b.last_ok_at,
        "created_at": b.created_at,
        "updated_at": b.updated_at,
    })
}

fn load_one(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Option<Value>> {
    Ok(match boards::get(conn, id)? {
        Some(b) => Some(board_json(&b, &boards::items(conn, id)?)),
        None => None,
    })
}

async fn list(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> V1Result<Response> {
    let data = state
        .with_conn(|c| {
            boards::list(c)?
                .iter()
                .map(|b| Ok(board_json(b, &boards::items(c, b.id)?)))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    Ok(json_ok(
        &headers,
        &json!({ "data": data, "next_cursor": null }),
    ))
}

async fn show(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    let v = state
        .with_conn(move |c| load_one(c, id))
        .await?
        .ok_or_else(V1Error::not_found)?;
    Ok(json_ok(&headers, &v))
}

/// What a client may send; every field optional so that `PATCH` changes only
/// what it names.
#[derive(Default, Deserialize)]
struct BoardIn {
    name: Option<String>,
    platform: Option<String>,
    webhook: Option<String>,
    source: Option<String>,
    status_page_id: Option<i64>,
    items: Option<Vec<i64>>,
    show_uptime_24h: Option<bool>,
    show_uptime_30d: Option<bool>,
    show_response: Option<bool>,
    show_incidents: Option<bool>,
    title: Option<String>,
    description: Option<String>,
    color: Option<String>,
    refresh_seconds: Option<i64>,
    notify_down: Option<bool>,
    mention: Option<String>,
    lang: Option<String>,
}

/// The webhook address is never printed.
impl std::fmt::Debug for BoardIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoardIn")
            .field("name", &self.name)
            .field("webhook", &self.webhook.as_ref().map(|_| "••••••"))
            .finish_non_exhaustive()
    }
}

impl BoardIn {
    /// `base` with the fields this request names replaced.
    fn apply(self, mut base: Input) -> Input {
        let source_given = self.source.is_some();
        macro_rules! set {
            ($($f:ident),* $(,)?) => { $(if let Some(v) = self.$f { base.$f = v; })* };
        }
        set!(
            name,
            platform,
            source,
            items,
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
        if let Some(w) = self.webhook {
            base.webhook = Some(w);
        }
        if let Some(p) = self.status_page_id {
            base.status_page_id = Some(p);
            // Naming a page without a source means a page board.
            if !source_given {
                base.source = "page".into();
            }
        }
        base
    }
}

fn invalid(errors: Errors) -> V1Error {
    V1Error::validation(
        "",
        errors
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Body(input): Body<BoardIn>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let input = input.apply(Input::defaults());
    let (key, i) = (state.session_key.clone(), input.clone());
    let new = state
        .with_conn(move |c| live_status::validate(c, &key, None, &i))
        .await?
        .map_err(invalid)?;
    if let Some(url) = input.new_webhook()
        && let Err(message) = live_status::confirm_webhook(&input.platform, url).await
    {
        return Err(invalid(Errors::from([("webhook", message)])));
    }
    let id = state.with_conn(move |c| boards::insert(c, &new)).await?;
    runner::kick(&state, id);
    let saved = state
        .with_conn(move |c| load_one(c, id))
        .await?
        .unwrap_or(Value::Null);
    Ok(created(format!("/api/v1/live-boards/{id}"), &saved))
}

async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
    Body(input): Body<BoardIn>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let key = state.session_key.clone();
    let (current, merged, checked) = state
        .with_conn(move |c| {
            let Some(current) = boards::get(c, id)? else {
                return Ok(None);
            };
            let items = boards::items(c, id)?;
            let merged = input.apply(Input::from_board(&current, items));
            let checked = live_status::validate(c, &key, Some(&current), &merged)?;
            Ok(Some((current, merged, checked)))
        })
        .await?
        .ok_or_else(V1Error::not_found)?;
    let new = checked.map_err(invalid)?;
    if let Some(url) = merged.new_webhook()
        && let Err(message) = live_status::confirm_webhook(&merged.platform, url).await
    {
        return Err(invalid(Errors::from([("webhook", message)])));
    }
    // Another webhook is another channel: remove the old messages first.
    let lock = runner::lock(&state, id).await;
    let moved = new.webhook_url != current.webhook_url;
    if moved {
        runner::delete_messages(&state, crate::live_status::webhook::client(), &current).await;
    }
    let saved = state
        .with_conn(move |c| {
            if moved {
                boards::forget_messages(c, id)?;
            }
            boards::update(c, id, &new)?;
            load_one(c, id)
        })
        .await?
        .ok_or_else(V1Error::not_found)?;
    drop(lock);
    runner::kick(&state, id);
    Ok(json_ok(&HeaderMap::new(), &saved))
}

async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let board = state
        .with_conn(move |c| boards::get(c, id))
        .await?
        .ok_or_else(V1Error::not_found)?;
    // Its messages go with it, as far as that works.
    let _lock = runner::lock(&state, id).await;
    runner::delete_messages(&state, crate::live_status::webhook::client(), &board).await;
    state.with_conn(move |c| boards::delete(c, id)).await?;
    Ok(no_content())
}

fn failed(what: &str, message: String) -> Response {
    V1Error::new(
        StatusCode::BAD_GATEWAY,
        "delivery_failed",
        format!("{what}: {message}"),
    )
    .into_response()
}

async fn test(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let board = state
        .with_conn(move |c| boards::get(c, id))
        .await?
        .ok_or_else(V1Error::not_found)?;
    let Some(platform) = live_status::webhook::Platform::parse(&board.platform) else {
        return Ok(failed("Couldn’t send the test", "unknown platform".into()));
    };
    let Ok(url) = crate::kit::notify::sealed::open_value(&state.session_key, &board.webhook_url)
    else {
        return Ok(failed(
            "Couldn’t send the test",
            crate::kit::notify::sealed::unreadable_message().into(),
        ));
    };
    let lang = crate::i18n::Lang::from_code(&board.lang).unwrap_or_default();
    Ok(
        match runner::test_webhook(
            live_status::webhook::client(),
            platform,
            &url,
            lang,
            &board.name,
        )
        .await
        {
            Ok(()) => json_ok(&HeaderMap::new(), &json!({ "ok": true })),
            Err(message) => failed("Couldn’t send the test", message),
        },
    )
}

fn outcome_response(outcome: runner::Outcome) -> Response {
    match outcome {
        runner::Outcome::Ok | runner::Outcome::Skipped => {
            json_ok(&HeaderMap::new(), &json!({ "ok": true }))
        }
        runner::Outcome::Deferred(wait) => V1Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!(
                "The channel is rate limited. The board tries again in {} s.",
                wait.as_secs().max(1)
            ),
        )
        .into_response(),
        runner::Outcome::Failed { message, paused } => failed(
            if paused {
                "The refresh failed and the board is paused"
            } else {
                "The refresh failed"
            },
            message,
        ),
    }
}

async fn refresh(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    if state
        .with_conn(move |c| boards::get(c, id))
        .await?
        .is_none()
    {
        return Err(V1Error::not_found());
    }
    Ok(outcome_response(runner::refresh_now(&state, id).await))
}

async fn retry(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    if !state.with_conn(move |c| boards::retry(c, id)).await? {
        return Err(V1Error::not_found());
    }
    Ok(outcome_response(runner::refresh_now(&state, id).await))
}
