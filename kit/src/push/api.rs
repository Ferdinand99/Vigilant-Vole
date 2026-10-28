//! The three routes the Hedgerow app calls:
//!
//! * `POST /api/v1/push/devices` `{"handle": "hrh_…"}`: `201 {"id"}`; the same
//!   handle again `200` with the same id; `409 push_disabled` while the
//!   server has no active `hedgerow` channel; `409 too_many_devices` (16 a
//!   user); `409 too_many_pending` (4 devices of the user, or 20 on the server, that no push has reached yet);
//!   `429 rate_limited` (4 new handles an hour).
//! * `DELETE /api/v1/push/devices/{id}`: `204`; `404` for a device that is
//!   not yours.
//! * `GET /api/v1/push/events/{ref}`: the text of a push, or `404` when it is
//!   unknown or older than 24 hours.
//!
//! **Who may call them.** Any signed-in user and any API token, including a
//! read-only one: the app's tokens often are. That is safe because the
//! routes change nothing but the caller's *own* list of devices (the user
//! comes from the session or token, never from the request), and only read
//! the push texts, which every role can see in the product anyway. They
//! check no role; everything else a read-only token reaches is still
//! limited by the role checks of the routes it calls.
//!
//! Mount [`router`] behind [`require_auth`](crate::auth::guard::require_auth)
//! (the paths are under `/api/v1`, which products exempt from the CSRF guard)
//! and list [`ROUTE_DOCS`] in the API reference.

use axum::extract::{FromRef, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{devices, events};
use crate::auth::{AuthMethod, CurrentUser};
use crate::crypto::Sealer;
use crate::docs::{RouteDoc, r};
use crate::http::rest::{Body, Id, V1Error, V1Result, created, no_content};
use crate::state::Core;

/// The routes, for [`DocsSpec::routes`](crate::docs::DocsSpec::routes).
pub const ROUTE_DOCS: [RouteDoc; 3] = [
    r(
        "POST",
        "/api/v1/push/devices",
        "viewer",
        "Push",
        "Register a Hedgerow app install to receive this server's push notifications. The handle is the one the app made at the push relay for this connection. Registering the same handle again returns the same id (200). Read-only tokens may call this: it only changes your own list of devices, and the device is removed when the token is. 409 push_disabled while no active Hedgerow app notification channel exists; 409 too_many_devices at 16 devices per user; 409 too_many_pending while 4 of your devices, or 20 on the server, have not received a push yet; 429 rate_limited after 4 new handles in an hour.",
    )
    .req(r#"{"handle": "hrh_…"}"#)
    .res(r#"{"id": 3}"#),
    r(
        "DELETE",
        "/api/v1/push/devices/{id}",
        "viewer",
        "Push",
        "Remove one of your own devices. Returns 204; 404 for a device that is not yours. Read-only tokens may call this.",
    ),
    r(
        "GET",
        "/api/v1/push/events/{ref}",
        "viewer",
        "Push",
        "The text of a push notification, by the ref the push carried. 404 when it is unknown or older than 24 hours. Read-only tokens may call this.",
    )
    .params(&[("ref", "string")])
    .res(r#"{"title": "Payments API is down", "body": "HTTP 503", "kind": "problem", "level": "critical", "occurred_at": "2026-10-28T09:41:07Z"}"#),
];

/// The push routes, for a product whose state `S` has a [`Core`].
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    Core: FromRef<S>,
{
    Router::new()
        .route("/api/v1/push/devices", post(register))
        .route("/api/v1/push/devices/{id}", delete(remove))
        .route("/api/v1/push/events/{ref}", get(event))
}

#[derive(Deserialize)]
struct RegisterBody {
    handle: String,
}

fn push_disabled() -> V1Error {
    V1Error::new(
        StatusCode::CONFLICT,
        "push_disabled",
        "Push is off: add a Hedgerow app notification channel first.",
    )
}

async fn register(
    State(core): State<Core>,
    user: CurrentUser,
    method: Option<Extension<AuthMethod>>,
    Body(body): Body<RegisterBody>,
) -> V1Result<Response> {
    let handle = body.handle.trim().to_string();
    if !devices::valid_handle(&handle) {
        return Err(V1Error::field("handle", "Not a Hedgerow handle (hrh_…)."));
    }
    if !core.with_conn(|c| devices::has_active_channel(c)).await? {
        return Err(push_disabled());
    }
    let token_id = match method.map(|Extension(m)| m) {
        Some(AuthMethod::Token { token_id, .. }) => Some(token_id),
        _ => None,
    };
    let sealer = Sealer::new(&core.session_key, devices::PURPOSE);
    let user_id = user.id;
    let outcome = core
        .with_conn(move |c| devices::register(c, &sealer, user_id, token_id, &handle))
        .await?;
    match outcome {
        Ok(devices::Registered::Created(id)) => Ok(created(
            format!("/api/v1/push/devices/{id}"),
            &json!({ "id": id }),
        )),
        Ok(devices::Registered::Existing(id)) => Ok(Json(json!({ "id": id })).into_response()),
        Err(devices::LimitReached::User) => Err(V1Error::new(
            StatusCode::CONFLICT,
            "too_many_devices",
            "You have registered the most devices allowed (16). Remove one first.",
        )),
        Err(devices::LimitReached::Server) => Err(V1Error::new(
            StatusCode::CONFLICT,
            "too_many_devices",
            "This server has registered the most devices allowed (256).",
        )),
        Err(devices::LimitReached::Pending) => Err(V1Error::new(
            StatusCode::CONFLICT,
            "too_many_pending",
            "Four of your devices have not received a push yet. Wait for an alert, or remove one.",
        )),
        Err(devices::LimitReached::PendingServer) => Err(V1Error::new(
            StatusCode::CONFLICT,
            "too_many_pending",
            "Too many devices on this server have not received a push yet. Try again after the next alert.",
        )),
        Err(devices::LimitReached::Rate) => Err(V1Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "You have registered four new devices in the last hour. Try again later.",
        )),
    }
}

async fn remove(
    State(core): State<Core>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    let user_id = user.id;
    if core
        .with_conn(move |c| devices::delete_own(c, user_id, id))
        .await?
    {
        Ok(no_content())
    } else {
        Err(V1Error::not_found())
    }
}

async fn event(
    State(core): State<Core>,
    _user: CurrentUser,
    Path(reference): Path<String>,
) -> V1Result<Response> {
    if !events::valid_ref(&reference) {
        return Err(V1Error::not_found());
    }
    let found = core
        .with_conn(move |c| events::get(c, &reference))
        .await?
        .ok_or_else(V1Error::not_found)?;
    let mut response = Json(found).into_response();
    // A push text is not for caches.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
