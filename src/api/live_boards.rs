//! Live status boards (Settings → Notifications → Live status boards, for
//! editors): a message in a Discord or Fluxer channel that is edited in place
//! to show a status page or some monitors. See [`crate::live_status`].
//!
//! Routes:
//! - `GET/POST /settings/notifications/boards`: list / add
//! - `GET /settings/notifications/boards/new`: the add form (`?page={id}` picks a status page)
//! - `GET /settings/notifications/boards/{id}/edit`, `POST /settings/notifications/boards/{id}`: change
//! - `DELETE /settings/notifications/boards/{id}`: delete (its messages too, as far as that works)
//! - `POST /settings/notifications/boards/{id}/refresh`, `…/retry`: refresh now / start a paused board again
//! - `POST /settings/notifications/boards/test`: "Test webhook" for the form
//!
//! The webhook address is never shown after it was saved: the edit form's
//! field is empty and an empty field keeps the saved address.

use std::collections::HashSet;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::SignedCookieJar;
use serde::Deserialize;

use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{CurrentUser, Role};
use crate::db::live_boards::{self as boards, Board};
use crate::db::status_pages as db_pages;
use crate::i18n::Tr as _;
use crate::live_status::view::pickable;
use crate::live_status::webhook::Platform;
use crate::live_status::{self, Errors, Input, runner};

const BASE: &str = "/settings/notifications/boards";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(BASE, get(page).post(create))
        .route(&format!("{BASE}/new"), get(new_page))
        .route(&format!("{BASE}/test"), post(test_form))
        .route(&format!("{BASE}/{{id}}"), post(update).delete(remove))
        .route(&format!("{BASE}/{{id}}/edit"), get(edit_page))
        .route(&format!("{BASE}/{{id}}/refresh"), post(refresh))
        .route(&format!("{BASE}/{{id}}/retry"), post(retry))
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

pub struct BoardRow {
    pub id: i64,
    pub name: String,
    pub platform: &'static str,
    pub source: String,
    /// Status class suffix: `up`, `pending`, `paused`.
    pub class: &'static str,
    pub state: String,
    pub last_ok: Option<String>,
    pub error: Option<String>,
    pub paused: bool,
}

pub struct PickItem {
    pub id: i64,
    pub name: String,
    pub sub: &'static str,
    pub checked: bool,
}

pub struct PageChoice {
    pub id: i64,
    pub title: String,
    pub selected: bool,
}

pub struct LangChoice {
    pub code: &'static str,
    pub label: String,
    pub selected: bool,
}

/// The add/edit form, as typed.
pub struct BoardForm {
    /// `None` = adding.
    pub id: Option<i64>,
    pub input: Input,
    /// An edit has a saved webhook ("leave empty to keep it").
    pub has_webhook: bool,
    pub errors: Errors,
    pub pages: Vec<PageChoice>,
    pub items: Vec<PickItem>,
    pub languages: Vec<LangChoice>,
}

impl BoardForm {
    pub fn err(&self, field: &str) -> &str {
        self.errors.get(field).map(String::as_str).unwrap_or("")
    }

    pub fn title(&self) -> &'static str {
        crate::i18n::t(if self.id.is_some() {
            "live.edit"
        } else {
            "live.add"
        })
    }

    pub fn submit_label(&self) -> &'static str {
        crate::i18n::t(if self.id.is_some() {
            "live.save"
        } else {
            "live.add"
        })
    }

    pub fn action(&self) -> String {
        match self.id {
            Some(id) => format!("{BASE}/{id}"),
            None => BASE.to_string(),
        }
    }

    pub fn is_page(&self) -> bool {
        self.input.source == "page"
    }

    pub fn is_fluxer(&self) -> bool {
        self.input.platform == "fluxer"
    }

    pub fn color_is(&self, value: &str) -> bool {
        self.input.color == value
    }
}

#[derive(Template)]
#[template(path = "settings_live_boards.html")]
struct BoardsTemplate {
    ctx: PageCtx,
    boards: Vec<BoardRow>,
    /// Shown above the list (an add, an edit, or one that didn't validate).
    form: Option<BoardForm>,
}

#[derive(Template)]
#[template(path = "partials/remote_browser_test.html")]
struct TestTemplate {
    ok: bool,
    title: String,
    text: String,
}

fn rows(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<BoardRow>> {
    let pages: std::collections::HashMap<i64, String> = db_pages::list(conn)?
        .into_iter()
        .map(|s| (s.page.id, s.page.title))
        .collect();
    Ok(boards::list(conn)?
        .into_iter()
        .map(|b| {
            let source = match (b.source.as_str(), b.status_page_id) {
                ("page", Some(id)) => crate::i18n::tf(
                    "live.src_page",
                    &[("name", pages.get(&id).map_or("", String::as_str))],
                ),
                ("page", None) => crate::i18n::t("live.src_page_gone").to_string(),
                _ => crate::i18n::t("live.src_items").to_string(),
            };
            let (class, state) = if b.is_paused() {
                ("paused", crate::i18n::t("live.state_paused"))
            } else if b.failures > 0 {
                ("pending", crate::i18n::t("live.state_failing"))
            } else if b.last_ok_at.is_none() {
                ("pending", crate::i18n::t("live.state_new"))
            } else {
                ("up", crate::i18n::t("live.state_live"))
            };
            BoardRow {
                id: b.id,
                platform: Platform::parse(&b.platform).map_or("", Platform::label),
                source,
                class,
                state: state.to_string(),
                paused: b.is_paused(),
                last_ok: b.last_ok_at,
                error: b.last_error,
                name: b.name,
            }
        })
        .collect())
}

/// Everything the form's pickers need.
fn form_for(
    conn: &rusqlite::Connection,
    id: Option<i64>,
    input: Input,
    has_webhook: bool,
    errors: Errors,
) -> rusqlite::Result<BoardForm> {
    let selected_page = input.status_page_id;
    let pages = db_pages::list(conn)?
        .into_iter()
        .filter(|s| {
            (s.page.published && s.page.password_hash.is_none()) || Some(s.page.id) == selected_page
        })
        .map(|s| PageChoice {
            selected: Some(s.page.id) == selected_page,
            id: s.page.id,
            title: s.page.title,
        })
        .collect();
    // The saved choices first, in their order, so that sending the form
    // again keeps it; then the rest.
    let chosen: HashSet<i64> = input.items.iter().copied().collect();
    let all = pickable(conn)?;
    let mut items: Vec<PickItem> = input
        .items
        .iter()
        .filter_map(|id| all.iter().find(|(i, _, _)| i == id))
        .map(|(id, name, group)| PickItem {
            id: *id,
            name: name.clone(),
            sub: if *group {
                crate::i18n::t("monitor_type.group")
            } else {
                ""
            },
            checked: true,
        })
        .collect();
    items.extend(
        all.iter()
            .filter(|(id, _, _)| !chosen.contains(id))
            .map(|(id, name, group)| PickItem {
                id: *id,
                name: name.clone(),
                sub: if *group {
                    crate::i18n::t("monitor_type.group")
                } else {
                    ""
                },
                checked: false,
            }),
    );
    let languages = crate::i18n::language_choices()
        .into_iter()
        .map(|(code, label)| LangChoice {
            selected: code == input.lang,
            code,
            label,
        })
        .collect();
    Ok(BoardForm {
        id,
        input,
        has_webhook,
        errors,
        pages,
        items,
        languages,
    })
}

async fn render(
    state: &AppState,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    form: Option<BoardForm>,
) -> ApiResult<(SignedCookieJar, Response)> {
    let boards = state.with_conn(|c| rows(c)).await?;
    let (ctx, jar) = PageCtx::build(
        state,
        Some(user),
        jar,
        crate::i18n::t("live.title"),
        "settings",
        "notifications",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(BoardsTemplate { ctx, boards, form }).into_response(),
    ))
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct FormIn {
    #[serde(default)]
    name: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    webhook: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    status_page_id: String,
    #[serde(default)]
    items: Vec<i64>,
    #[serde(default)]
    show_uptime_24h: bool,
    #[serde(default)]
    show_uptime_30d: bool,
    #[serde(default)]
    show_response: bool,
    #[serde(default)]
    show_incidents: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    refresh_seconds: String,
    #[serde(default)]
    notify_down: bool,
    #[serde(default)]
    mention: String,
    #[serde(default)]
    lang: String,
}

impl FormIn {
    fn into_input(self) -> Input {
        Input {
            name: self.name,
            platform: self.platform,
            webhook: Some(self.webhook),
            source: self.source,
            status_page_id: self.status_page_id.trim().parse().ok(),
            items: self.items,
            show_uptime_24h: self.show_uptime_24h,
            show_uptime_30d: self.show_uptime_30d,
            show_response: self.show_response,
            show_incidents: self.show_incidents,
            title: self.title,
            description: self.description,
            color: self.color,
            refresh_seconds: self.refresh_seconds.trim().parse().unwrap_or(0),
            notify_down: self.notify_down,
            mention: self.mention,
            lang: self.lang,
        }
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

async fn page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Editor)?;
    render(&state, &user, &jar, None).await
}

#[derive(Deserialize)]
struct NewQuery {
    page: Option<i64>,
}

async fn new_page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Query(q): Query<NewQuery>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Editor)?;
    let mut input = Input::defaults();
    if let Some(page) = q.page {
        input.status_page_id = Some(page);
        if let Some(title) = state
            .with_conn(move |c| db_pages::get(c, page))
            .await?
            .map(|p| p.title)
        {
            input.name = title;
        }
    }
    let form = state
        .with_conn(move |c| form_for(c, None, input, false, Errors::new()))
        .await?;
    render(&state, &user, &jar, Some(form)).await
}

async fn edit_page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Editor)?;
    let form = state
        .with_conn(move |c| {
            let Some(b) = boards::get(c, id)? else {
                return Ok(None);
            };
            let items = boards::items(c, id)?;
            Ok(Some(form_for(
                c,
                Some(id),
                Input::from_board(&b, items),
                true,
                Errors::new(),
            )?))
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    render(&state, &user, &jar, Some(form)).await
}

// ---------------------------------------------------------------------------
// Save
// ---------------------------------------------------------------------------

async fn redisplay(
    state: &AppState,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    id: Option<i64>,
    input: Input,
    has_webhook: bool,
    errors: Errors,
) -> ApiResult<(SignedCookieJar, Response)> {
    let form = state
        .with_conn(move |c| form_for(c, id, input, has_webhook, errors))
        .await?;
    render(state, user, jar, Some(form)).await
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    axum_extra::extract::Form(form): axum_extra::extract::Form<FormIn>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Editor)?;
    let input = form.into_input();
    let (key, i) = (state.session_key.clone(), input.clone());
    let checked = state
        .with_conn(move |c| live_status::validate(c, &key, None, &i))
        .await?;
    let new = match checked {
        Ok(n) => n,
        Err(errors) => return redisplay(&state, &user, &jar, None, input, false, errors).await,
    };
    if let Err(message) = confirm(&input).await {
        let mut errors = Errors::new();
        errors.insert("webhook", message);
        return redisplay(&state, &user, &jar, None, input, false, errors).await;
    }
    let id = state.with_conn(move |c| boards::insert(c, &new)).await?;
    tracing::info!(board_id = id, user = %user.username, "live status board added");
    runner::kick(&state, id);
    let jar = flash(jar, "success", crate::i18n::t("live.added"));
    Ok((jar, Redirect::to(BASE).into_response()))
}

/// For a new Fluxer webhook: is the host a Fluxer instance?
async fn confirm(input: &Input) -> Result<(), String> {
    match input.new_webhook() {
        Some(url) => live_status::confirm_webhook(&input.platform, url).await,
        None => Ok(()),
    }
}

async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    axum_extra::extract::Form(form): axum_extra::extract::Form<FormIn>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Editor)?;
    let input = form.into_input();
    let (key, i) = (state.session_key.clone(), input.clone());
    let checked = state
        .with_conn(move |c| {
            let Some(current) = boards::get(c, id)? else {
                return Ok(None);
            };
            let r = live_status::validate(c, &key, Some(&current), &i)?;
            Ok(Some((current, r)))
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    let (current, result) = checked;
    let new = match result {
        Ok(n) => n,
        Err(errors) => return redisplay(&state, &user, &jar, Some(id), input, true, errors).await,
    };
    if let Err(message) = confirm(&input).await {
        let mut errors = Errors::new();
        errors.insert("webhook", message);
        return redisplay(&state, &user, &jar, Some(id), input, true, errors).await;
    }
    // A new webhook is another channel: the old messages are removed (as far
    // as that works) and the board starts over there.
    let moved = new.webhook_url != current.webhook_url;
    if moved {
        runner::delete_messages(&state, crate::live_status::webhook::client(), &current).await;
    }
    state
        .with_conn(move |c| {
            if moved {
                boards::forget_messages(c, id)?;
            }
            boards::update(c, id, &new)
        })
        .await?;
    tracing::info!(board_id = id, user = %user.username, "live status board changed");
    runner::kick(&state, id);
    let jar = flash(jar, "success", crate::i18n::t("live.saved"));
    Ok((jar, Redirect::to(BASE).into_response()))
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

fn toast(message: &str, kind: &str) -> HeaderValue {
    super::agents::toast_trigger(message, kind)
}

async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let board = state
        .with_conn(move |c| boards::get(c, id))
        .await?
        .ok_or(ApiError::NotFound)?;
    runner::delete_messages(&state, crate::live_status::webhook::client(), &board).await;
    state.with_conn(move |c| boards::delete(c, id)).await?;
    tracing::info!(board_id = id, user = %user.username, "live status board removed");
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert(
        "hx-trigger",
        toast(crate::i18n::t("live.removed"), "success"),
    );
    Ok(response)
}

/// Runs a refresh and answers the page's htmx call with a reload and a toast.
async fn run_and_reload(
    state: &AppState,
    jar: SignedCookieJar,
    id: i64,
    ok_message: &'static str,
) -> (SignedCookieJar, Response) {
    let outcome = runner::refresh_now(state, id).await;
    let jar = match outcome {
        runner::Outcome::Failed { .. } => {
            flash(jar, "error", crate::i18n::t("live.refresh_failed"))
        }
        runner::Outcome::Deferred(_) => flash(jar, "info", crate::i18n::t("live.rate_limited")),
        _ => flash(jar, "success", crate::i18n::t(ok_message)),
    };
    let mut response = StatusCode::OK.into_response();
    response
        .headers_mut()
        .insert("hx-redirect", HeaderValue::from_static(BASE));
    (jar, response)
}

async fn refresh(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Editor)?;
    if state
        .with_conn(move |c| boards::get(c, id))
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    Ok(run_and_reload(&state, jar, id, "live.refreshed").await)
}

async fn retry(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Editor)?;
    if !state.with_conn(move |c| boards::retry(c, id)).await? {
        return Err(ApiError::NotFound);
    }
    Ok(run_and_reload(&state, jar, id, "live.refreshed").await)
}

#[derive(Deserialize, Default)]
struct TestIn {
    #[serde(default)]
    platform: String,
    #[serde(default)]
    webhook: String,
    #[serde(default)]
    name: String,
    /// The board being edited (its saved webhook is used when the field is empty).
    #[serde(default)]
    id: Option<String>,
}

fn test_page(ok: bool, text: String) -> Response {
    HtmlTemplate(TestTemplate {
        ok,
        title: crate::i18n::t(if ok {
            "live.test_ok"
        } else {
            "live.test_failed"
        })
        .into(),
        text,
    })
    .into_response()
}

async fn test_form(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::Form(input): axum::Form<TestIn>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let Some(platform) = Platform::parse(&input.platform) else {
        return Ok(test_page(false, crate::i18n::t("live.err_choice").into()));
    };
    let mut url = input.webhook.trim().to_string();
    if url.is_empty() || url == crate::kit::notify::SECRET_MASK {
        // The saved address of the board being edited.
        let id = input
            .id
            .as_deref()
            .and_then(|s| s.trim().parse::<i64>().ok());
        let stored = match id {
            Some(id) => state
                .with_conn(move |c| boards::get(c, id))
                .await?
                .filter(|b| b.platform == platform.as_str())
                .map(|b| b.webhook_url),
            None => None,
        };
        let opened = stored
            .and_then(|s| crate::kit::notify::sealed::open_value(&state.session_key, &s).ok());
        match opened {
            Some(u) => url = u,
            None => {
                return Ok(test_page(
                    false,
                    crate::i18n::t("live.err_webhook_required").into(),
                ));
            }
        }
    }
    if let Err(message) = live_status::confirm_webhook(platform.as_str(), &url).await {
        return Ok(test_page(false, message));
    }
    let name = if input.name.trim().is_empty() {
        crate::product::NAME.to_string()
    } else {
        input.name.trim().to_string()
    };
    let result = runner::test_webhook(
        live_status::webhook::client(),
        platform,
        &url,
        crate::i18n::current(),
        &name,
    )
    .await;
    Ok(match result {
        Ok(()) => test_page(true, crate::i18n::t("live.test_ok_text").into()),
        Err(message) => test_page(false, message),
    })
}

/// Whether a board's settings page link is shown for a status page: the
/// boards that follow it (for the status page editor).
pub fn boards_of_page(conn: &rusqlite::Connection, page_id: i64) -> rusqlite::Result<Vec<Board>> {
    boards::for_page(conn, page_id)
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use axum_extra::extract::cookie::Cookie;
    use tower::ServiceExt;

    use super::*;

    async fn session(state: &AppState, name: &str, role: Role) -> String {
        let name = name.to_string();
        let user = state
            .with_conn(move |c| {
                let id = crate::db::users::insert_user(c, &name, "x", role)?;
                crate::db::users::get_user(c, id)
            })
            .await
            .unwrap()
            .unwrap();
        let claims = crate::auth::session::SessionClaims::new(
            user.id,
            user.session_version,
            crate::auth::session::now_unix(),
        );
        let jar = SignedCookieJar::new(state.session_key.clone())
            .add(Cookie::new(crate::auth::SESSION_COOKIE, claims.encode()));
        let resp = (jar, ()).into_response();
        resp.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost")
            .header("hx-request", "true")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = to_bytes(resp.into_body(), 4 << 20).await.unwrap();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    const TOKEN: &str = "SECRETwebhookToken";

    fn form(webhook: &str, items: &str) -> String {
        format!(
            "name=Support&platform=discord&webhook={webhook}&source=items&{items}&show_uptime_24h=true&show_incidents=true&refresh_seconds=300&lang=en&color=&title=&description=&mention="
        )
    }

    #[tokio::test]
    async fn the_page_adds_edits_and_deletes_boards_and_never_shows_the_webhook() {
        let state = crate::api::agents::test_support::test_state().await;
        let monitor = state
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO monitors (name, monitor_type, target) VALUES ('API', 'http', 'https://secret-host.internal')",
                    [],
                )?;
                Ok(c.last_insert_rowid())
            })
            .await
            .unwrap();
        let app = crate::api::build_router(state.clone());
        let viewer = session(&state, "viewer", Role::Viewer).await;
        let editor = session(&state, "editor", Role::Editor).await;

        let (status, _, _) = send(&app, "GET", BASE, &viewer, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, body) = send(&app, "GET", BASE, &editor, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("/settings/notifications/boards/new"),
            "{body}"
        );
        let (status, _, body) = send(&app, "GET", &format!("{BASE}/new"), &editor, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("name=\"webhook\"") && body.contains("name=\"items\""),
            "{body}"
        );

        // A bad address is shown again with its error and nothing is saved.
        let bad = form(
            "https%3A%2F%2Fevil.example%2Fapi%2Fwebhooks%2F1%2Fx",
            &format!("items={monitor}"),
        );
        let (status, _, body) = send(&app, "POST", BASE, &editor, &bad).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("field-error"), "{body}");
        assert!(
            !body.contains("evil.example/api"),
            "the bad address was echoed"
        );
        assert_eq!(state.with_conn(|c| boards::count(c)).await.unwrap(), 0);

        // A good one is saved and the page redirects to the list.
        let good = form(
            &format!("https%3A%2F%2Fdiscord.com%2Fapi%2Fwebhooks%2F123456789%2F{TOKEN}"),
            &format!("items={monitor}"),
        );
        let (status, headers, body) = send(&app, "POST", BASE, &editor, &good).await;
        assert!(status.is_redirection(), "{status} {body}");
        assert_eq!(headers[header::LOCATION], BASE);
        let board = state
            .with_conn(|c| Ok(boards::list(c)?.remove(0)))
            .await
            .unwrap();
        assert!(!board.webhook_url.contains(TOKEN));

        // Neither the list nor the edit form carries the address or the token.
        for uri in [BASE.to_string(), format!("{BASE}/{}/edit", board.id)] {
            let (status, _, body) = send(&app, "GET", &uri, &editor, "").await;
            assert_eq!(status, StatusCode::OK);
            for secret in [TOKEN, "123456789/", &board.webhook_url, "secret-host"] {
                assert!(!body.contains(secret), "{uri} shows {secret}");
            }
            assert!(body.contains("Support"));
        }

        // Saving with the webhook field empty keeps the saved address.
        let again = form("", &format!("items={monitor}")).replace("name=Support", "name=Renamed");
        let (status, _, body) = send(
            &app,
            "POST",
            &format!("{BASE}/{}", board.id),
            &editor,
            &again,
        )
        .await;
        assert!(status.is_redirection(), "{status} {body}");
        let renamed = state
            .with_conn(move |c| boards::get(c, board.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.name, "Renamed");
        assert_eq!(renamed.webhook_url, board.webhook_url);

        // A paused board is retried from the list; deleting removes it.
        let (status, _, _) =
            send(&app, "DELETE", &format!("{BASE}/{}", board.id), &editor, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state.with_conn(|c| boards::count(c)).await.unwrap(), 0);
    }
}
