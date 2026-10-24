//! Remote probe agents. Owned by the integrations agent.
//!
//! Protected (admin): `GET/POST /settings/agents`, `POST /settings/agents/{id}/rotate`,
//! `POST /settings/agents/{id}/active`, `DELETE /settings/agents/{id}`.
//!
//! Public, agent-token auth (`Authorization: Bearer vva_…`):
//! `GET /api/agent/v1/monitors`, `POST /api/agent/v1/results` — see
//! [`crate::agent::protocol`].
//!
//! Trust model: an agent token can read the full configuration of every
//! `run_on_agents` monitor (including that monitor's own headers and basic
//! auth, which the agent needs to run the check) and report results for those
//! monitors only. It can't read anything else or change configuration.

use crate::i18n::Tr as _;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use askama::Template;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use axum_extra::extract::SignedCookieJar;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::agent::protocol::{
    AgentInfo, MAX_BATCH, MAX_MESSAGE_CHARS, MAX_RESULT_AGE_SECS, MONITORS_PATH, MonitorsResponse,
    NAME_HEADER, POLL_SECONDS, RESULTS_PATH, Rejected, ResultItem, ResultsResponse, VERSION_HEADER,
    parse_db,
};
use crate::auth::{CurrentUser, Role};
use crate::db::agents::{self as db_agents, Agent};
use crate::db::models::{CheckOutcome, HeartbeatStatus};
use crate::db::settings::Settings;

/// Image used in the copy-paste snippets.
pub const DOCKER_IMAGE: &str = "vigilant-vole:latest";
/// Agent API requests allowed per agent per [`RATE_WINDOW`]. A healthy agent
/// makes ~14 a minute (a poll every 30 s, a flush every 5 s).
const RATE_LIMIT: u32 = 120;
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Fixed-window request counters per agent id: `(window start, requests)`.
static AGENT_RATE: LazyLock<Mutex<HashMap<i64, (Instant, u32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Counts a request for `agent_id`; `Some(wait)` once it is over budget.
fn rate_limited(agent_id: i64, now: Instant) -> Option<Duration> {
    let mut map = AGENT_RATE.lock().unwrap_or_else(PoisonError::into_inner);
    let entry = map.entry(agent_id).or_insert((now, 0));
    if now.saturating_duration_since(entry.0) >= RATE_WINDOW {
        *entry = (now, 0);
    }
    entry.1 = entry.1.saturating_add(1);
    (entry.1 > RATE_LIMIT)
        .then(|| RATE_WINDOW.saturating_sub(now.saturating_duration_since(entry.0)))
}
const MAX_NAME_CHARS: usize = 80;
const MAX_LOCATION_CHARS: usize = 80;

/// Protected routes (behind `require_auth`; `CurrentUser` is available).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/agents", get(agents_page).post(create_agent))
        .route("/settings/agents/{id}/rotate", post(rotate_agent))
        .route("/settings/agents/{id}/active", post(set_agent_active))
        .route("/settings/agents/{id}", axum::routing::delete(delete_agent))
}

/// Unauthenticated routes (agent-token auth inside).
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route(MONITORS_PATH, get(agent_monitors))
        .route(RESULTS_PATH, post(agent_results))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Public URL of this server: the `base_url` setting, else derived from the
/// request (`Host`, and `X-Forwarded-Host`/`-Proto` only behind a trusted
/// proxy, `VV_TRUST_PROXY`).
pub fn public_base_url(settings: &Settings, headers: &HeaderMap) -> String {
    base_url_from(settings, headers, crate::auth::session::trust_proxy())
}

fn base_url_from(settings: &Settings, headers: &HeaderMap, trust_proxy: bool) -> String {
    if let Some(base) = settings
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        return base.trim_end_matches('/').to_string();
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    // Anybody can send X-Forwarded-*; only a proxy we trust sets them for real.
    let forwarded = |name: &str| trust_proxy.then(|| header(name)).flatten();
    let host = forwarded("x-forwarded-host")
        .and_then(|h| h.split(',').next())
        .map(str::trim)
        .or_else(|| header("host"))
        .filter(|h| !h.is_empty())
        .unwrap_or("localhost:3001");
    let proto = forwarded("x-forwarded-proto")
        .and_then(|p| p.split(',').next())
        .map(str::trim)
        .filter(|p| *p == "https" || *p == "http")
        .unwrap_or("http");
    format!("{proto}://{host}")
}

/// `HX-Trigger` header value for a toast.
pub fn toast_trigger(message: &str, kind: &str) -> HeaderValue {
    super::status_pages::toast_trigger(message, kind, false)
}

// ---------------------------------------------------------------------------
// Agent-token extractor
// ---------------------------------------------------------------------------

/// The agent behind an `Authorization: Bearer vva_…` header. Rejects with 401
/// for a missing/unknown token, 403 for a disabled agent and 429 when the
/// agent exceeds [`RATE_LIMIT`]. Records `last_seen_at` and the reported
/// version on success.
pub struct AuthedAgent(pub Agent);

impl FromRequestParts<AppState> for AuthedAgent {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let agent = authenticate_agent(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        if let Some(wait) = rate_limited(agent.id, Instant::now()) {
            tracing::warn!(agent_id = agent.id, "agent over its request budget");
            let retry = wait.as_secs().max(1).to_string();
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry)],
                Json(serde_json::json!({ "error": "too many requests" })),
            )
                .into_response());
        }
        Ok(AuthedAgent(agent))
    }
}

async fn authenticate_agent(parts: &Parts, state: &AppState) -> Result<Agent, ApiError> {
    let token = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim().to_string())
        .ok_or(ApiError::Unauthorized)?;
    let version = parts
        .headers
        .get(VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().chars().take(40).collect::<String>())
        .filter(|v| !v.is_empty());
    let agent = state
        .with_conn(move |c| db_agents::find_by_token(c, &token))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if !agent.active {
        return Err(ApiError::Forbidden);
    }
    let id = agent.id;
    state
        .with_conn(move |c| db_agents::touch(c, id, version.as_deref()))
        .await?;
    if let Some(name) = parts.headers.get(NAME_HEADER).and_then(|v| v.to_str().ok()) {
        tracing::trace!(agent_id = id, reported_name = name, "agent request");
    }
    Ok(agent)
}

// ---------------------------------------------------------------------------
// Agent API
// ---------------------------------------------------------------------------

async fn agent_monitors(
    State(state): State<AppState>,
    AuthedAgent(agent): AuthedAgent,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let mut monitors = state
        .with_conn(|c| db_agents::list_agent_monitors(c))
        .await?;
    for m in &mut monitors {
        m.push_token = None;
    }
    // Old agents can't read monitor types they don't know (and would skip
    // OAuth2/mTLS): only send what this agent's version can run.
    let version = headers
        .get(VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    let total = monitors.len();
    monitors.retain(|m| crate::agent::protocol::agent_can_run(m, version));
    if monitors.len() < total {
        warn_held_back(agent.id, version, total - monitors.len());
    }
    let body = MonitorsResponse {
        agent: AgentInfo {
            id: agent.id,
            name: agent.name,
            location: agent.location,
        },
        poll_seconds: POLL_SECONDS,
        monitors,
    };
    let json = serde_json::to_vec(&body).map_err(ApiError::internal)?;
    let etag = format!("\"{}\"", hex::encode(&Sha256::digest(&json)[..16]));
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/json")], json).into_response()
    };
    if let Ok(v) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, v);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Logs once per agent and version that monitors were held back.
fn warn_held_back(agent_id: i64, version: Option<&str>, count: usize) {
    static WARNED: Mutex<Option<HashSet<(i64, String)>>> = Mutex::new(None);
    let version = version.unwrap_or_default().to_string();
    let first = WARNED
        .lock()
        .map(|mut w| {
            w.get_or_insert_with(HashSet::new)
                .insert((agent_id, version.clone()))
        })
        .unwrap_or(true);
    if first {
        tracing::warn!(
            agent_id,
            agent_version = %version,
            held_back = count,
            needs = crate::agent::protocol::SERVICE_MONITORS_SINCE,
            "agent is too old for some of its monitors; upgrade it to run them there"
        );
    }
}

async fn agent_results(
    State(state): State<AppState>,
    AuthedAgent(agent): AuthedAgent,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let items: Vec<ResultItem> = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("expected a JSON array of results: {e}")))?;
    if items.len() > MAX_BATCH {
        return Ok((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(
                serde_json::json!({ "error": format!("at most {MAX_BATCH} results per request") }),
            ),
        )
            .into_response());
    }
    let monitors = state
        .with_conn(|c| db_agents::list_agent_monitors(c))
        .await?;
    let intervals: HashMap<i64, i64> = monitors
        .iter()
        .map(|m| (m.id, m.interval_seconds))
        .collect();
    let now = time::OffsetDateTime::now_utc();

    // At most one accepted result per monitor per batch: the newest valid
    // one (agents send oldest first, so walk the batch backwards).
    let mut seen: HashSet<i64> = HashSet::new();
    let mut accepted = 0;
    let mut rejected = Vec::new();
    for item in items.into_iter().rev() {
        if seen.contains(&item.monitor_id) {
            rejected.push(Rejected {
                monitor_id: item.monitor_id,
                reason: "duplicate monitor_id in batch".into(),
            });
            continue;
        }
        match validate_result(&item, &intervals, now) {
            Ok(outcome) => {
                seen.insert(item.monitor_id);
                state
                    .scheduler
                    .ingest_external(item.monitor_id, outcome, Some(agent.id))
                    .await;
                accepted += 1;
            }
            Err(reason) => rejected.push(Rejected {
                monitor_id: item.monitor_id,
                reason: reason.into(),
            }),
        }
    }
    rejected.reverse();
    if !rejected.is_empty() {
        tracing::debug!(
            agent_id = agent.id,
            accepted,
            rejected = rejected.len(),
            "agent results partly rejected"
        );
    }
    Ok(Json(ResultsResponse { accepted, rejected }).into_response())
}

/// Turn a reported result into a RAW outcome, or say why it was rejected.
fn validate_result(
    item: &ResultItem,
    intervals: &HashMap<i64, i64>,
    now: time::OffsetDateTime,
) -> Result<CheckOutcome, &'static str> {
    let Some(interval) = intervals.get(&item.monitor_id) else {
        return Err("not a remote-agent monitor");
    };
    if !matches!(
        item.status,
        HeartbeatStatus::Up | HeartbeatStatus::Down | HeartbeatStatus::Pending
    ) {
        return Err("status must be up, down or pending");
    }
    if let Some(at) = item.checked_at.as_deref() {
        let at = parse_db(at).ok_or("checked_at is not a UTC timestamp")?;
        let max_age = MAX_RESULT_AGE_SECS.max(interval.saturating_mul(2));
        let age = (now - at).whole_seconds();
        if age > max_age {
            return Err("stale result");
        }
        if age < -MAX_RESULT_AGE_SECS {
            return Err("checked_at is in the future");
        }
    }
    let message = item
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| m.chars().take(MAX_MESSAGE_CHARS).collect::<String>());
    Ok(CheckOutcome {
        status: item.status,
        response_time_ms: item.response_time_ms.map(|ms| ms.clamp(0, 3_600_000)),
        message,
        expiry: None,
    })
}

// ---------------------------------------------------------------------------
// Settings page
// ---------------------------------------------------------------------------

/// One row of the agents table.
pub struct AgentRow {
    pub id: i64,
    pub name: String,
    pub location: String,
    pub prefix: String,
    pub version: String,
    pub last_seen_at: Option<String>,
    pub online: bool,
    pub active: bool,
    pub reporting: i64,
}

/// A token to show once, with ready-to-paste snippets.
pub struct ShownToken {
    pub agent_name: String,
    pub token: String,
    pub docker_run: String,
    pub compose: String,
    pub binary: String,
    pub rotated: bool,
}

#[derive(Template)]
#[template(path = "settings_agents.html")]
struct AgentsTemplate {
    ctx: PageCtx,
    agents: Vec<AgentRow>,
    shown: Option<ShownToken>,
    agent_monitors: i64,
    name_value: String,
    location_value: String,
    name_error: String,
}

#[derive(Debug, Default, Deserialize)]
struct CreateForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    location: String,
}

/// Snippets for running an agent with `token` against `server`.
pub fn snippets(server: &str, token: &str, name: &str) -> (String, String, String) {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let container = if slug.is_empty() {
        "vigilant-vole-agent".to_string()
    } else {
        format!("vigilant-vole-agent-{slug}")
    };
    // The token goes in the environment, not argv, so `ps` can't show it.
    let docker_run = format!(
        "docker run -d --name {container} --restart unless-stopped --cap-add NET_RAW \\\n  -e VV_AGENT_SERVER={server} \\\n  -e VV_AGENT_TOKEN={token} \\\n  {DOCKER_IMAGE} agent"
    );
    let compose = format!(
        "services:\n  {container}:\n    image: {DOCKER_IMAGE}\n    container_name: {container}\n    restart: unless-stopped\n    command: agent\n    cap_add:\n      - NET_RAW\n    environment:\n      - VV_AGENT_SERVER={server}\n      - VV_AGENT_TOKEN={token}\n"
    );
    let binary = format!("VV_AGENT_TOKEN={token} vigilant-vole agent --server {server}");
    (docker_run, compose, binary)
}

async fn render_page(
    state: &AppState,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    shown: Option<ShownToken>,
    form: CreateForm,
    name_error: String,
) -> ApiResult<(SignedCookieJar, Response)> {
    let (list, agent_monitors) = state
        .with_conn(|c| {
            Ok((
                db_agents::list_agents(c)?,
                db_agents::count_agent_monitors(c)?,
            ))
        })
        .await?;
    let agents = list
        .into_iter()
        .map(|s| AgentRow {
            id: s.agent.id,
            name: s.agent.name,
            location: s.agent.location,
            prefix: s.agent.prefix,
            version: s.agent.version.unwrap_or_default(),
            last_seen_at: s.agent.last_seen_at,
            online: s.online,
            active: s.agent.active,
            reporting: s.reporting_monitors,
        })
        .collect();
    let (ctx, jar) = PageCtx::build(
        state,
        Some(user),
        jar,
        crate::i18n::t("nav.agents"),
        "settings",
        "agents",
    )
    .await;
    let page = AgentsTemplate {
        ctx,
        agents,
        shown,
        agent_monitors,
        name_value: form.name,
        location_value: form.location,
        name_error,
    };
    Ok((jar, HtmlTemplate(page).into_response()))
}

async fn agents_page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Admin)?;
    render_page(
        &state,
        &user,
        &jar,
        None,
        CreateForm::default(),
        String::new(),
    )
    .await
}

async fn create_agent(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Admin)?;
    let name = form.name.trim().to_string();
    let location = form.location.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        let error = if name.is_empty() {
            crate::i18n::t("agents.err_name").to_string()
        } else {
            crate::i18n::tf(
                "agents.err_name_long",
                &[("max", &MAX_NAME_CHARS.to_string())],
            )
        };
        return render_page(&state, &user, &jar, None, form, error).await;
    }
    let location: String = location.chars().take(MAX_LOCATION_CHARS).collect();
    let token = db_agents::generate_token();
    let plain = token.token.clone();
    let (n, l) = (name.clone(), location.clone());
    let id = state
        .with_conn(move |c| db_agents::insert_agent(c, &n, &l, &token))
        .await?;
    tracing::info!(agent_id = id, user = %user.username, "remote agent created");
    let settings = state.settings().await?;
    let shown = shown_token(&settings, &headers, &name, plain, false);
    render_page(
        &state,
        &user,
        &jar,
        Some(shown),
        CreateForm::default(),
        String::new(),
    )
    .await
}

fn shown_token(
    settings: &Settings,
    headers: &HeaderMap,
    name: &str,
    token: String,
    rotated: bool,
) -> ShownToken {
    let server = public_base_url(settings, headers);
    let (docker_run, compose, binary) = snippets(&server, &token, name);
    ShownToken {
        agent_name: name.to_string(),
        token,
        docker_run,
        compose,
        binary,
        rotated,
    }
}

async fn rotate_agent(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Admin)?;
    let token = db_agents::generate_token();
    let plain = token.token.clone();
    let agent = state
        .with_conn(move |c| {
            if db_agents::rotate_token(c, id, &token)? {
                db_agents::get_agent(c, id)
            } else {
                Ok(None)
            }
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    tracing::info!(agent_id = id, user = %user.username, "remote agent token rotated");
    let settings = state.settings().await?;
    let shown = shown_token(&settings, &headers, &agent.name, plain, true);
    render_page(
        &state,
        &user,
        &jar,
        Some(shown),
        CreateForm::default(),
        String::new(),
    )
    .await
}

async fn set_agent_active(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: String,
) -> ApiResult<(SignedCookieJar, Redirect)> {
    user.require(Role::Admin)?;
    // Lenient body parsing: the "Disable" form posts no fields at all.
    let active =
        url::form_urlencoded::parse(body.as_bytes()).any(|(k, v)| k == "active" && v == "true");
    let found = state
        .with_conn(move |c| db_agents::set_active(c, id, active))
        .await?;
    if !found {
        return Err(ApiError::NotFound);
    }
    tracing::info!(agent_id = id, active, user = %user.username, "remote agent toggled");
    let msg = if active {
        crate::i18n::t("agents.enabled")
    } else {
        crate::i18n::t("agents.disabled")
    };
    Ok((flash(jar, "success", msg), Redirect::to("/settings/agents")))
}

async fn delete_agent(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Admin)?;
    let found = state
        .with_conn(move |c| db_agents::delete_agent(c, id))
        .await?;
    if !found {
        return Err(ApiError::NotFound);
    }
    tracing::info!(agent_id = id, user = %user.username, "remote agent deleted");
    state.notify_update();
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert(
        "hx-trigger",
        toast_trigger(crate::i18n::t("agents.deleted"), "success"),
    );
    Ok(response)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use axum_extra::extract::cookie::Key;

    use crate::api::AppState;
    use crate::config::Config;
    use crate::monitor::Scheduler;

    /// A full `AppState` on a fresh on-disk database in the temp dir.
    pub(crate) async fn test_state() -> AppState {
        let dir = std::env::temp_dir().join(format!(
            "vv-integrations-test-{}-{}",
            std::process::id(),
            crate::db::agents::generate_token().hash
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let pool = crate::db::build_pool(&dir.join("test.db"));
        crate::db::run_migrations(&pool, &dir.join("backups")).await;
        let (update_tx, _) = tokio::sync::broadcast::channel(16);
        let http = reqwest::Client::new();
        let key = Key::generate();
        let scheduler = Scheduler::new(pool.clone(), update_tx.clone(), http.clone(), key.clone());
        AppState {
            db: pool,
            scheduler,
            session_key: key,
            update_tx,
            http,
            sign_in_limits: Default::default(),
            config: Arc::new(Config {
                data_dir: dir,
                bind_addr: "127.0.0.1:0".into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    use super::test_support::test_state;
    use super::*;
    use crate::agent::{self, AgentArgs};
    use crate::db::models::{MonitorType, NewMonitor};
    use crate::db::monitors as db_monitors;

    struct Fixture {
        state: AppState,
        token: String,
        agent_id: i64,
        agent_monitor: i64,
        local_monitor: i64,
    }

    async fn fixture(target: &str) -> Fixture {
        let state = test_state().await;
        let target = target.to_string();
        let (token, agent_id, agent_monitor, local_monitor) = state
            .with_conn(move |c| {
                let t = db_agents::generate_token();
                let agent_id = db_agents::insert_agent(c, "Probe", "eu", &t)?;
                let base = NewMonitor {
                    name: "tcp".into(),
                    monitor_type: MonitorType::Tcp,
                    target,
                    interval_seconds: 1,
                    timeout_seconds: 2,
                    basic_auth_pass: Some("monitor-secret".into()),
                    run_on_agents: true,
                    ..NewMonitor::default()
                };
                let on = db_monitors::insert_monitor(c, &base)?;
                let off = db_monitors::insert_monitor(
                    c,
                    &NewMonitor {
                        run_on_agents: false,
                        ..base
                    },
                )?;
                Ok((t.token, agent_id, on, off))
            })
            .await
            .unwrap();
        Fixture {
            state,
            token,
            agent_id,
            agent_monitor,
            local_monitor,
        }
    }

    fn app(state: &AppState) -> Router {
        public_router()
            .layer(axum::middleware::from_fn(crate::api::error::negotiate))
            .with_state(state.clone())
    }

    async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let resp = app(state).oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    fn get_monitors(token: Option<&str>) -> axum::http::request::Builder {
        let mut b = Request::builder().uri(MONITORS_PATH);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b
    }

    fn post_results(token: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(RESULTS_PATH)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn agent_heartbeats(state: &AppState, agent_id: i64) -> Vec<(i64, String)> {
        state
            .with_conn(move |c| {
                let mut stmt = c.prepare(
                    "SELECT monitor_id, status FROM heartbeats WHERE agent_id = ?1 ORDER BY id",
                )?;
                let rows = stmt.query_map([agent_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect()
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn agent_endpoints_require_a_valid_active_token() {
        let f = fixture("127.0.0.1:1").await;
        let (status, _, _) = send(&f.state, get_monitors(None).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = send(
            &f.state,
            get_monitors(Some("vva_nope")).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = send(&f.state, post_results("vva_nope", &serde_json::json!([]))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let id = f.agent_id;
        f.state
            .with_conn(move |c| db_agents::set_active(c, id, false))
            .await
            .unwrap();
        let (status, _, _) = send(
            &f.state,
            get_monitors(Some(&f.token)).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn monitors_endpoint_lists_agent_monitors_with_etag() {
        let f = fixture("127.0.0.1:1").await;
        let req = get_monitors(Some(&f.token))
            .header(VERSION_HEADER, "9.9.9")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = send(&f.state, req).await;
        assert_eq!(status, StatusCode::OK);
        let list: MonitorsResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(list.agent.id, f.agent_id);
        let ids: Vec<i64> = list.monitors.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![f.agent_monitor]);
        assert_eq!(
            list.monitors[0].basic_auth_pass.as_deref(),
            Some("monitor-secret")
        );
        let etag = headers
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let req = get_monitors(Some(&f.token))
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&f.state, req).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty());

        let id = f.agent_id;
        let agent = f
            .state
            .with_conn(move |c| db_agents::get_agent(c, id))
            .await
            .unwrap()
            .unwrap();
        assert!(agent.last_seen_at.is_some());
        assert_eq!(agent.version.as_deref(), Some("9.9.9"));
    }

    #[tokio::test]
    async fn old_agents_get_only_what_they_can_run_and_new_ones_get_auth_secrets() {
        let f = fixture("127.0.0.1:1").await;
        let (pg, oauth, mtls) = f
            .state
            .with_conn(|c| {
                let on = |m: NewMonitor| NewMonitor {
                    run_on_agents: true,
                    interval_seconds: 60,
                    ..m
                };
                let pg = db_monitors::insert_monitor(
                    c,
                    &on(NewMonitor {
                        name: "pg".into(),
                        monitor_type: MonitorType::Postgres,
                        target: "postgres://db/app".into(),
                        ..NewMonitor::default()
                    }),
                )?;
                let oauth = db_monitors::insert_monitor(
                    c,
                    &on(NewMonitor {
                        name: "oauth".into(),
                        target: "https://api.example.com/".into(),
                        auth_method: "oauth2".into(),
                        oauth_token_url: Some("https://login.example.com/token".into()),
                        oauth_client_id: Some("vole".into()),
                        oauth_client_secret: Some("client-secret-value".into()),
                        ..NewMonitor::default()
                    }),
                )?;
                let mtls = db_monitors::insert_monitor(
                    c,
                    &on(NewMonitor {
                        name: "mtls".into(),
                        target: "https://mtls.example.com/".into(),
                        auth_method: "mtls".into(),
                        tls_client_cert: Some("CERT".into()),
                        tls_client_key: Some("KEY".into()),
                        ..NewMonitor::default()
                    }),
                )?;
                Ok((pg, oauth, mtls))
            })
            .await
            .unwrap();
        let fetch = |version: Option<&'static str>| {
            let mut req = get_monitors(Some(&f.token));
            if let Some(v) = version {
                req = req.header(VERSION_HEADER, v);
            }
            let state = f.state.clone();
            async move {
                let (status, _, body) = send(&state, req.body(Body::empty()).unwrap()).await;
                assert_eq!(status, StatusCode::OK);
                serde_json::from_str::<MonitorsResponse>(&body)
                    .unwrap()
                    .monitors
            }
        };
        for old in [None, Some("0.0.9")] {
            let ids: Vec<i64> = fetch(old).await.iter().map(|m| m.id).collect();
            assert_eq!(ids, vec![f.agent_monitor], "{old:?}");
        }
        let list = fetch(Some("9.9.9")).await;
        let mut ids: Vec<i64> = list.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![f.agent_monitor, pg, oauth, mtls]);
        // The agent runs the check itself, so it gets the settings it needs.
        let by_id = |id: i64| list.iter().find(|m| m.id == id).unwrap();
        assert_eq!(
            by_id(oauth).oauth_client_secret.as_deref(),
            Some("client-secret-value")
        );
        assert_eq!(
            (
                by_id(mtls).tls_client_cert.as_deref(),
                by_id(mtls).tls_client_key.as_deref()
            ),
            (Some("CERT"), Some("KEY"))
        );
    }

    #[tokio::test]
    async fn results_are_ingested_for_agent_monitors_only() {
        let f = fixture("127.0.0.1:1").await;
        let body = serde_json::json!([
            {"monitor_id": f.agent_monitor, "status": "down", "response_time_ms": 12, "message": "refused", "checked_at": crate::agent::protocol::now_db()},
            {"monitor_id": f.local_monitor, "status": "up"},
            {"monitor_id": 999, "status": "up"},
            {"monitor_id": f.agent_monitor, "status": "maintenance"},
            {"monitor_id": f.agent_monitor, "status": "up", "checked_at": "2001-01-01 00:00:00"},
        ]);
        let (status, _, text) = send(&f.state, post_results(&f.token, &body)).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let resp: ResultsResponse = serde_json::from_str(&text).unwrap();
        assert_eq!(resp.accepted, 1);
        assert_eq!(resp.rejected.len(), 4);
        assert!(resp.rejected.iter().any(|r| r.reason == "stale result"));
        assert_eq!(
            agent_heartbeats(&f.state, f.agent_id).await,
            vec![(f.agent_monitor, "down".to_string())]
        );
    }

    #[tokio::test]
    async fn duplicate_monitor_ids_in_a_batch_keep_only_the_last() {
        let f = fixture("127.0.0.1:1").await;
        let body = serde_json::json!([
            {"monitor_id": f.agent_monitor, "status": "up"},
            {"monitor_id": f.agent_monitor, "status": "up"},
            {"monitor_id": f.agent_monitor, "status": "down"},
        ]);
        let (status, _, text) = send(&f.state, post_results(&f.token, &body)).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let resp: ResultsResponse = serde_json::from_str(&text).unwrap();
        assert_eq!(resp.accepted, 1);
        assert_eq!(resp.rejected.len(), 2);
        assert!(resp.rejected.iter().all(|r| r.reason.contains("duplicate")));
        assert_eq!(
            agent_heartbeats(&f.state, f.agent_id).await,
            vec![(f.agent_monitor, "down".to_string())]
        );
    }

    #[test]
    fn agent_requests_are_rate_limited_per_window() {
        // Negative ids never collide with real agents in other tests.
        let t = Instant::now();
        for _ in 0..RATE_LIMIT {
            assert_eq!(rate_limited(-7, t), None);
        }
        let wait = rate_limited(-7, t + Duration::from_secs(10)).unwrap();
        assert_eq!(wait, Duration::from_secs(50));
        assert_eq!(rate_limited(-8, t), None, "other agents are unaffected");
        assert_eq!(
            rate_limited(-7, t + RATE_WINDOW),
            None,
            "a new window starts over"
        );
    }

    #[tokio::test]
    async fn oversized_and_malformed_batches_are_rejected() {
        let f = fixture("127.0.0.1:1").await;
        let many: Vec<_> = (0..=MAX_BATCH)
            .map(|_| serde_json::json!({"monitor_id": f.agent_monitor, "status": "up"}))
            .collect();
        let (status, _, _) = send(&f.state, post_results(&f.token, &serde_json::json!(many))).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let (status, _, _) = send(
            &f.state,
            post_results(&f.token, &serde_json::json!({"x": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(agent_heartbeats(&f.state, f.agent_id).await.is_empty());
    }

    async fn serve(state: &AppState) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = app(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn args(server: &str, token: &str) -> AgentArgs {
        AgentArgs {
            server: server.into(),
            token: token.into(),
            name: Some("test-agent".into()),
            once: true,
            concurrency: 4,
        }
    }

    #[tokio::test]
    async fn agent_client_checks_and_reports_once() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let f = fixture(&target.local_addr().unwrap().to_string()).await;
        let server = serve(&f.state).await;
        let client = agent::Client::new(&args(&server, &f.token)).unwrap();
        let summary = agent::run_once(&client, 4).await.unwrap();
        assert_eq!(
            (
                summary.checked,
                summary.up,
                summary.accepted,
                summary.rejected
            ),
            (1, 1, 1, 0)
        );
        assert_eq!(summary.agent, "Probe");
        assert_eq!(
            agent_heartbeats(&f.state, f.agent_id).await,
            vec![(f.agent_monitor, "up".to_string())]
        );
        drop(target);
    }

    #[tokio::test]
    async fn agent_loop_polls_checks_and_flushes() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let f = fixture(&target.local_addr().unwrap().to_string()).await;
        let server = serve(&f.state).await;
        let client = agent::Client::new(&args(&server, &f.token)).unwrap();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(agent::run_loop(client, 2, stop_rx));
        let mut seen = Vec::new();
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            seen = agent_heartbeats(&f.state, f.agent_id).await;
            if !seen.is_empty() {
                break;
            }
        }
        stop_tx.send(true).unwrap();
        handle.await.unwrap().unwrap();
        assert!(!seen.is_empty(), "agent results never arrived");
        assert!(
            seen.iter()
                .all(|(id, s)| *id == f.agent_monitor && s == "up"),
            "{seen:?}"
        );
        drop(target);
    }

    #[tokio::test]
    async fn agent_loop_fails_fast_on_a_bad_token() {
        let f = fixture("127.0.0.1:1").await;
        let server = serve(&f.state).await;
        let client = agent::Client::new(&args(&server, "vva_wrongtoken")).unwrap();
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let err = agent::run_loop(client, 1, stop_rx).await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[test]
    fn base_url_prefers_setting_then_forwarded_headers() {
        let mut s = Settings::default();
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("vv.local:3001"));
        assert_eq!(base_url_from(&s, &h, true), "http://vv.local:3001");
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        h.insert(
            "x-forwarded-host",
            HeaderValue::from_static("status.proxy.example, inner"),
        );
        assert_eq!(base_url_from(&s, &h, true), "https://status.proxy.example");
        s.base_url = Some("https://status.example.com/".into());
        assert_eq!(base_url_from(&s, &h, true), "https://status.example.com");
    }

    #[test]
    fn forwarded_headers_are_ignored_without_a_trusted_proxy() {
        let s = Settings::default();
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("vv.local:3001"));
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        h.insert("x-forwarded-host", HeaderValue::from_static("evil.example"));
        assert_eq!(base_url_from(&s, &h, false), "http://vv.local:3001");
    }

    #[test]
    fn snippets_include_server_and_token() {
        let (run, compose, bin) = snippets("https://s.example", "vva_x", "Frankfurt probe");
        assert!(run.contains("--name vigilant-vole-agent-frankfurt-probe"));
        assert!(
            run.contains("-e VV_AGENT_SERVER=https://s.example")
                && run.contains("-e VV_AGENT_TOKEN=vva_x")
        );
        // Never on the command line, where `ps` would show it.
        assert!(!run.contains("--token") && !bin.contains("--token"));
        assert!(compose.contains("VV_AGENT_TOKEN=vva_x") && compose.contains("command: agent"));
        assert_eq!(
            bin,
            "VV_AGENT_TOKEN=vva_x vigilant-vole agent --server https://s.example"
        );
    }
}
