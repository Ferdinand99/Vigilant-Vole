//! MCP server: lets the owner's own AI assistant (Claude Code, Claude
//! Desktop, any MCP client) query and steer Vigilant Vole.
//!
//! Vigilant Vole never calls an AI model and never relays AI usage; it only
//! answers the client's requests. Transport: Streamable HTTP, `POST /mcp`
//! with JSON-RPC 2.0 (single messages; batches, which the 2025-06-18
//! revision dropped, are refused; the server is stateless and offers no
//! server-initiated stream, so `GET /mcp` is `405` as the specification
//! allows). Authentication is an API token (`Authorization: Bearer vv_…`):
//! a read token sees only the read tools, a write token also the editing
//! tools, and the user's role still applies. There are no session cookies.
//! A browser request must come from the configured public URL, or, without
//! one, from an IP address or `localhost` (DNS-rebinding protection: a name
//! that an attacker rebinds to this server would match the `Host` header,
//! so the `Host` header alone proves nothing).
//!
//! The tools are thin layers over the REST API: each call is sent to
//! `/api/v1/**` inside the process with the caller's token, so validation,
//! role checks and secret masking are exactly the REST API's. Secret values
//! are masked again on the way out. Nothing here deletes data or touches
//! users, settings, tokens or backups.
//!
//! The protocol itself (transport checks, rate limit, JSON-RPC) is
//! [`crate::kit::mcp`]; this module supplies Vigilant Vole's tools and
//! resources and the route.

mod tools;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use serde_json::{Map, Value};

use crate::api::AppState;
pub use crate::kit::mcp::PROTOCOL_VERSIONS;
pub(crate) use crate::kit::mcp::{Caller, Handler, MAX_BODY, RpcResult, not_offered};
#[cfg(test)]
pub(crate) use crate::kit::mcp::{RATE_LIMIT, RATE_WINDOW, origin_allowed, rate_limited};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mcp", post(handle).get(not_offered).delete(not_offered))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

async fn handle(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let instance = std::sync::Arc::as_ptr(&state.config) as usize;
    let caller = match crate::kit::mcp::authenticate(&state.core(), &headers, instance).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let ctx = tools::Ctx::new(state, caller, &headers);
    crate::kit::mcp::serve(&ctx, &body).await
}

impl Handler for tools::Ctx {
    fn can_write(&self) -> bool {
        tools::Ctx::can_write(self)
    }

    fn instructions(&self) -> String {
        let mode = if self.can_write() {
            "This token can also change things (pause monitors, post incidents, edit monitors); it acts with the rights of its owner."
        } else {
            "This token is read-only: you can look but not change anything."
        };
        format!(
            "Vigilant Vole is a self-hosted uptime monitor. Start with `instance_summary`, use `why_is_it_down` for a failing monitor and `monitor_history` for trends. {mode} Secrets are always shown as ••••••."
        )
    }

    fn tools(&self) -> Vec<Value> {
        tools::list(self.can_write())
    }

    async fn call_tool(&self, name: &str, args: Map<String, Value>) -> RpcResult {
        tools::call(self, name, args).await
    }

    async fn list_resources(&self) -> RpcResult {
        tools::list_resources(self).await
    }

    fn resource_templates(&self) -> Value {
        tools::resource_templates()
    }

    async fn read_resource(&self, uri: &str) -> RpcResult {
        tools::read_resource(self, uri).await
    }
}

#[cfg(test)]
mod tests;
