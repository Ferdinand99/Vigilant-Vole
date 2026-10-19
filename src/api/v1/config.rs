//! `GET/POST /api/v1/config` — configuration as code (see [`crate::config_io`]).

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::json;

use super::{Params, V1Error, V1Result};
use crate::api::AppState;
use crate::auth::{CurrentUser, Role};
use crate::config_io::{self, Format, ImportError, Mode, Plan};

pub fn router() -> Router<AppState> {
    Router::new().route("/config", get(export).post(import))
}

/// Largest document accepted (the global body limit is 2 MB anyway).
pub const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;

fn truthy(v: Option<&str>) -> bool {
    matches!(v.map(str::trim), Some("1" | "true" | "yes" | "on"))
}

#[derive(Debug, Default, Deserialize)]
pub struct ExportQuery {
    pub format: Option<String>,
    pub secrets: Option<String>,
    /// `1` adds `Content-Disposition: attachment`.
    pub download: Option<String>,
}

/// Renders the export (shared with the backup page).
pub async fn export_response(state: &AppState, user: &CurrentUser, q: &ExportQuery) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let format = match q.format.as_deref() {
        None => Format::Yaml,
        Some(f) => Format::parse(f).ok_or_else(|| V1Error::field("format", "Use yaml or json."))?,
    };
    let secrets = truthy(q.secrets.as_deref());
    if secrets {
        user.require(Role::Admin).map_err(|_| V1Error::forbidden_msg("Only admins can export secrets."))?;
    }
    let doc = state.with_conn(move |c| config_io::export(c, secrets)).await?;
    let text = config_io::render(&doc, format).map_err(V1Error::internal)?;
    tracing::info!(user = %user.username, secrets, "configuration exported");
    let mut res = ([(header::CONTENT_TYPE, HeaderValue::from_static(format.content_type()))], text).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if truthy(q.download.as_deref()) {
        let stamp = time::OffsetDateTime::now_utc();
        let name = format!(
            "vigilant-vole-config-{:04}{:02}{:02}.{}",
            stamp.year(),
            u8::from(stamp.month()),
            stamp.day(),
            format.extension()
        );
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
            res.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
    Ok(res)
}

async fn export(State(state): State<AppState>, user: CurrentUser, Params(q): Params<ExportQuery>) -> V1Result<Response> {
    export_response(&state, &user, &q).await
}

#[derive(Debug, Default, Deserialize)]
struct ImportQuery {
    mode: Option<String>,
    dry_run: Option<String>,
    /// Required (`1`) to apply a `replace` import.
    confirm: Option<String>,
}

/// The plan as JSON, with a summary.
pub fn plan_json(plan: &Plan, applied: bool) -> serde_json::Value {
    json!({
        "applied": applied,
        "mode": plan.mode,
        "summary": plan.summary(),
        "items": plan.items,
        "errors": plan.errors,
        "warnings": plan.warnings,
    })
}

/// 422 with the error envelope plus the plan, so clients see every problem.
fn invalid(plan: &Plan) -> Response {
    let fields: serde_json::Map<String, serde_json::Value> = plan
        .errors
        .iter()
        .map(|e| {
            let mut key = format!("{}.{}", e.kind, e.key);
            if let Some(f) = &e.field {
                key = format!("{key}.{f}");
            }
            (key, json!(e.message))
        })
        .collect();
    let n = plan.errors.len();
    let body = json!({
        "error": {
            "code": "validation_failed",
            "message": format!("The document has {n} problem{}. Nothing was changed.", if n == 1 { "" } else { "s" }),
            "fields": fields,
        },
        "plan": plan_json(plan, false),
    });
    (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response()
}

async fn import(State(state): State<AppState>, user: CurrentUser, Params(q): Params<ImportQuery>, body: Bytes) -> V1Result<Response> {
    user.require_admin()?;
    let mode = Mode::parse(q.mode.as_deref().unwrap_or("merge")).ok_or_else(|| V1Error::field("mode", "Use merge or replace."))?;
    let dry_run = truthy(q.dry_run.as_deref());
    if mode == Mode::Replace && !dry_run && !truthy(q.confirm.as_deref()) {
        return Err(V1Error::new(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "Replace deletes everything the document doesn’t list. Run with dry_run=1 first, then add confirm=1.",
        ));
    }
    if body.len() > MAX_DOCUMENT_BYTES {
        return Err(V1Error::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "The document is larger than 2 MB."));
    }
    let text = std::str::from_utf8(&body).map_err(|_| V1Error::bad_request("The document must be UTF-8 text (YAML or JSON)."))?;
    let doc = config_io::parse_document(text).map_err(|msg| V1Error::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_document", msg))?;
    match config_io::import(&state, doc, mode, dry_run).await {
        Ok(plan) => {
            if !dry_run {
                tracing::info!(user = %user.username, mode = mode.as_str(), "configuration imported via API");
            }
            if plan.is_ok() {
                Ok(axum::Json(plan_json(&plan, !dry_run)).into_response())
            } else {
                Ok(invalid(&plan))
            }
        }
        Err(ImportError::Invalid(plan)) => Ok(invalid(&plan)),
        Err(ImportError::Api(e)) => Err(e.into()),
    }
}
