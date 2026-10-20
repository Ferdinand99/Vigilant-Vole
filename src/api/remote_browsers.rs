//! Remote browsers for real-browser monitors (Settings → Remote browsers,
//! admin) and the failure screenshot of a real-browser monitor.
//!
//! Routes:
//! - `GET/POST /settings/browsers`: list / add
//! - `GET /settings/browsers/{id}/edit`, `POST /settings/browsers/{id}`: change
//! - `DELETE /settings/browsers/{id}`: remove (htmx row)
//! - `POST /settings/browsers/test`: "Test connection" for the form (htmx fragment)
//! - `POST /settings/browsers/{id}/test`: the same for a saved browser
//! - `GET /monitors/{id}/screenshot`: the latest failure screenshot (any
//!   signed-in user; never on public pages)
//!
//! A token or password in a browser's URL is shown masked; a masked URL
//! keeps the stored secret only while it still points at the same scheme,
//! host and port (see `real_browser::endpoint::unmask`).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::i18n::Tr as _;
use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::SignedCookieJar;
use rusqlite::Connection;
use serde::Deserialize;

use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{CurrentUser, Role};
use crate::db::models::{Monitor, MonitorType};
use crate::db::remote_browsers::{self as db_rb, MAX_NAME_LEN};
use crate::monitor::checkers::real_browser::{self, endpoint};

/// How long "Test connection" waits.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/browsers", get(page).post(create))
        .route("/settings/browsers/test", post(test_form))
        .route("/settings/browsers/{id}", post(update).delete(remove))
        .route("/settings/browsers/{id}/edit", get(edit_page))
        .route("/settings/browsers/{id}/test", post(test_saved))
        .route("/monitors/{id}/screenshot", get(screenshot))
}

// ---------------------------------------------------------------------------
// Validation (shared with the JSON API)
// ---------------------------------------------------------------------------

/// Field → message.
pub type Errors = BTreeMap<&'static str, String>;

/// Checks a submitted name and URL (`editing`: the stored browser, whose
/// masked secret may be kept). Returns the name and the full URL to store.
pub fn validate(conn: &Connection, editing: Option<&db_rb::RemoteBrowser>, name: &str, url: &str) -> rusqlite::Result<Result<(String, String), Errors>> {
    let mut e = Errors::new();
    let name = name.trim();
    if name.is_empty() {
        e.insert("name", crate::i18n::t("rb.err_name").into());
    } else if name.chars().count() > MAX_NAME_LEN {
        e.insert("name", crate::i18n::tf("rb.err_name_long", &[("max", &MAX_NAME_LEN.to_string())]));
    } else if db_rb::name_taken(conn, name, editing.map(|b| b.id))? {
        e.insert("name", crate::i18n::t("rb.err_name_taken").into());
    }
    let full = match endpoint::unmask(url, editing.map(|b| b.url.as_str())) {
        Ok(u) => u,
        Err(endpoint::Unmask::Reenter) => {
            e.insert("url", crate::i18n::t("rb.err_reenter").into());
            String::new()
        }
    };
    if !e.contains_key("url") {
        if full.trim().is_empty() {
            e.insert("url", crate::i18n::t("rb.err_url").into());
        } else {
            match url::Url::parse(full.trim()) {
                Ok(u) if !matches!(u.scheme(), "ws" | "wss" | "http" | "https") => {
                    e.insert("url", crate::i18n::t("rb.err_scheme").into());
                }
                Ok(_) if endpoint::parse(&full).is_ok() => {}
                _ => {
                    e.insert("url", crate::i18n::t("rb.err_url_invalid").into());
                }
            }
        }
    }
    Ok(if e.is_empty() { Ok((name.to_string(), full.trim().to_string())) } else { Err(e) })
}

// ---------------------------------------------------------------------------
// Settings page
// ---------------------------------------------------------------------------

pub struct BrowserRow {
    pub id: i64,
    pub name: String,
    /// URL with its secret masked.
    pub url: String,
    pub monitors: i64,
    pub is_default: bool,
}

/// The add/edit form, as typed.
#[derive(Default)]
pub struct BrowserForm {
    /// `None` = adding.
    pub id: Option<i64>,
    pub name: String,
    pub url: String,
    pub errors: Errors,
}

impl BrowserForm {
    pub fn err(&self, field: &str) -> &str {
        self.errors.get(field).map(String::as_str).unwrap_or("")
    }

    pub fn title(&self) -> &'static str {
        crate::i18n::t(if self.id.is_some() { "rb.edit" } else { "rb.add" })
    }

    pub fn submit_label(&self) -> &'static str {
        crate::i18n::t(if self.id.is_some() { "rb.save" } else { "rb.add" })
    }
}

#[derive(Template)]
#[template(path = "settings_remote_browsers.html")]
struct BrowsersTemplate {
    ctx: PageCtx,
    browsers: Vec<BrowserRow>,
    /// Shown above the list (an edit, or an add that didn't validate).
    form: Option<BrowserForm>,
}

#[derive(Template)]
#[template(path = "partials/remote_browser_test.html")]
struct TestTemplate {
    ok: bool,
    title: String,
    text: String,
}

#[derive(Deserialize, Default)]
struct FormIn {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    /// The browser being edited (test only).
    #[serde(default)]
    id: Option<String>,
}

fn rows(conn: &Connection) -> rusqlite::Result<Vec<BrowserRow>> {
    let used = db_rb::usage(conn)?;
    Ok(db_rb::list(conn)?
        .into_iter()
        .enumerate()
        .map(|(i, b)| BrowserRow { id: b.id, url: endpoint::masked(&b.url), monitors: used.get(&b.id).copied().unwrap_or(0), name: b.name, is_default: i == 0 })
        .collect())
}

async fn render(state: &AppState, user: &CurrentUser, jar: &SignedCookieJar, form: Option<BrowserForm>) -> ApiResult<(SignedCookieJar, Response)> {
    let browsers = state.with_conn(|c| rows(c)).await?;
    let (ctx, jar) = PageCtx::build(state, Some(user), jar, crate::i18n::t("nav.browsers"), "settings", "browsers").await;
    Ok((jar, HtmlTemplate(BrowsersTemplate { ctx, browsers, form }).into_response()))
}

async fn page(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Admin)?;
    render(&state, &user, &jar, None).await
}

async fn edit_page(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar, Path(id): Path<i64>) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Admin)?;
    let b = state.with_conn(move |c| db_rb::get(c, id)).await?.ok_or(ApiError::NotFound)?;
    let form = BrowserForm { id: Some(b.id), name: b.name, url: endpoint::masked(&b.url), errors: Errors::new() };
    render(&state, &user, &jar, Some(form)).await
}

async fn create(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar, Form(input): Form<FormIn>) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Admin)?;
    let (name, url) = (input.name.clone(), input.url.clone());
    let saved = state
        .with_conn(move |c| {
            Ok(match validate(c, None, &name, &url)? {
                Ok((name, url)) => Ok(db_rb::insert(c, &name, &url)?),
                Err(e) => Err(e),
            })
        })
        .await?;
    match saved {
        Ok(id) => {
            tracing::info!(remote_browser_id = id, user = %user.username, "remote browser added");
            let jar = flash(jar, "success", crate::i18n::t("rb.added"));
            Ok((jar, Redirect::to("/settings/browsers").into_response()))
        }
        Err(errors) => {
            let form = BrowserForm { id: None, name: input.name, url: input.url, errors };
            render(&state, &user, &jar, Some(form)).await
        }
    }
}

async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(input): Form<FormIn>,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Admin)?;
    let (name, url) = (input.name.clone(), input.url.clone());
    let saved = state
        .with_conn(move |c| {
            let Some(current) = db_rb::get(c, id)? else { return Ok(None) };
            Ok(Some(match validate(c, Some(&current), &name, &url)? {
                Ok((name, url)) => {
                    db_rb::update(c, id, &name, &url)?;
                    Ok(())
                }
                Err(e) => Err(e),
            }))
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    match saved {
        Ok(()) => {
            tracing::info!(remote_browser_id = id, user = %user.username, "remote browser changed");
            let jar = flash(jar, "success", crate::i18n::t("rb.saved"));
            Ok((jar, Redirect::to("/settings/browsers").into_response()))
        }
        Err(errors) => {
            let form = BrowserForm { id: Some(id), name: input.name, url: input.url, errors };
            render(&state, &user, &jar, Some(form)).await
        }
    }
}

async fn remove(State(state): State<AppState>, user: CurrentUser, Path(id): Path<i64>) -> ApiResult<Response> {
    user.require(Role::Admin)?;
    if !state.with_conn(move |c| db_rb::delete(c, id)).await? {
        return Err(ApiError::NotFound);
    }
    tracing::info!(remote_browser_id = id, user = %user.username, "remote browser removed");
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert("hx-trigger", super::agents::toast_trigger(crate::i18n::t("rb.removed"), "success"));
    Ok(response)
}

/// The test result fragment.
async fn run_test(url: String) -> Response {
    let page = match real_browser::test_connection(&url, TEST_TIMEOUT).await {
        Ok(v) => TestTemplate {
            ok: true,
            title: crate::i18n::t("rb.test_ok").into(),
            text: if v.product.is_empty() { v.user_agent } else { crate::i18n::tf("rb.test_ok_text", &[("product", &v.product), ("protocol", &v.protocol)]) },
        },
        Err(err) => TestTemplate { ok: false, title: crate::i18n::t("rb.test_failed").into(), text: err },
    };
    HtmlTemplate(page).into_response()
}

async fn test_form(State(state): State<AppState>, user: CurrentUser, Form(input): Form<FormIn>) -> ApiResult<Response> {
    user.require(Role::Admin)?;
    let id = input.id.as_deref().and_then(|s| s.trim().parse::<i64>().ok());
    let stored = match id {
        Some(id) => state.with_conn(move |c| db_rb::get(c, id)).await?.map(|b| b.url),
        None => None,
    };
    match endpoint::unmask(&input.url, stored.as_deref()) {
        Ok(url) => Ok(run_test(url).await),
        Err(endpoint::Unmask::Reenter) => Ok(HtmlTemplate(TestTemplate {
            ok: false,
            title: crate::i18n::t("rb.test_failed").into(),
            text: crate::i18n::t("rb.err_reenter").into(),
        })
        .into_response()),
    }
}

async fn test_saved(State(state): State<AppState>, user: CurrentUser, Path(id): Path<i64>) -> ApiResult<Response> {
    user.require(Role::Admin)?;
    let b = state.with_conn(move |c| db_rb::get(c, id)).await?.ok_or(ApiError::NotFound)?;
    Ok(run_test(b.url).await)
}

// ---------------------------------------------------------------------------
// Monitor detail
// ---------------------------------------------------------------------------

/// The real-browser card on the monitor detail page.
pub struct BrowserPanel {
    pub monitor_id: i64,
    /// The remote browser in use (`None`: none configured).
    pub browser: Option<String>,
    pub wait: &'static str,
    pub viewport: &'static str,
    /// "Selector #app" / "Keyword “OK”" / "".
    pub expect: String,
    pub js_errors: bool,
    pub shot: Option<ShotView>,
}

pub struct ShotView {
    pub taken_at: String,
    pub message: String,
    /// Cache-busting version for the image URL.
    pub version: String,
}

pub fn detail_panel(conn: &Connection, m: &Monitor) -> rusqlite::Result<Option<BrowserPanel>> {
    if m.monitor_type != MonitorType::RealBrowser {
        return Ok(None);
    }
    let browser = db_rb::for_monitor(conn, m.remote_browser_id)?.map(|b| b.name);
    let shot = db_rb::screenshot_info(conn, m.id)?.map(|s| ShotView {
        version: s.taken_at.chars().filter(char::is_ascii_digit).collect(),
        taken_at: s.taken_at,
        message: s.message.unwrap_or_default(),
    });
    let expect = match (&m.browser_selector, &m.keyword) {
        (Some(s), _) => crate::i18n::tf(if m.keyword_invert { "rb.expect_selector_absent" } else { "rb.expect_selector" }, &[("selector", s)]),
        (None, Some(k)) if !k.is_empty() => crate::i18n::tf(if m.keyword_invert { "rb.expect_keyword_absent" } else { "rb.expect_keyword" }, &[("keyword", k)]),
        _ => String::new(),
    };
    Ok(Some(BrowserPanel {
        monitor_id: m.id,
        browser,
        wait: crate::i18n::t(if m.browser_wait == "network_idle" { "browser.wait_network_idle" } else { "browser.wait_load" }),
        viewport: crate::i18n::t(if m.browser_viewport == "mobile" { "browser.viewport_mobile" } else { "browser.viewport_desktop" }),
        expect,
        js_errors: m.browser_js_errors,
        shot,
    }))
}

async fn screenshot(State(state): State<AppState>, _user: CurrentUser, Path(id): Path<i64>) -> ApiResult<Response> {
    let shot = state.with_conn(move |c| db_rb::get_screenshot(c, id)).await?.ok_or(ApiError::NotFound)?;
    let mime = if shot.mime == "image/png" { "image/png" } else { "image/jpeg" };
    let mut response = shot.image.into_response();
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::db::models::NewMonitor;

    #[test]
    fn names_and_urls_are_validated_and_masked_tokens_kept() {
        let conn = crate::db::test_conn();
        let errs = validate(&conn, None, "", "ftp://x").unwrap().unwrap_err();
        assert!(errs.contains_key("name") && errs.contains_key("url"), "{errs:?}");
        assert!(validate(&conn, None, "B", "").unwrap().unwrap_err().contains_key("url"));
        assert!(validate(&conn, None, "B", "chrome:9222").unwrap().unwrap_err().contains_key("url"));
        let (name, url) = validate(&conn, None, " Chrome ", " wss://b.example?token=s3cret ").unwrap().unwrap();
        assert_eq!((name.as_str(), url.as_str()), ("Chrome", "wss://b.example?token=s3cret"));
        let id = db_rb::insert(&conn, &name, &url).unwrap();
        assert!(validate(&conn, None, "chrome", "ws://x:1").unwrap().unwrap_err().contains_key("name"), "names are unique");
        let stored = db_rb::get(&conn, id).unwrap().unwrap();
        let masked = endpoint::masked(&stored.url);
        let (_, kept) = validate(&conn, Some(&stored), "Chrome", &masked).unwrap().unwrap();
        assert!(kept.contains("token=s3cret"), "{kept}");
        let errs = validate(&conn, Some(&stored), "Chrome", &masked.replace("b.example", "evil.example")).unwrap().unwrap_err();
        assert!(errs.contains_key("url"), "a moved URL needs its token again");
    }

    #[test]
    fn the_detail_panel_describes_the_check_and_its_screenshot() {
        let conn = crate::db::test_conn();
        let new = NewMonitor {
            name: "Shop".into(),
            monitor_type: MonitorType::RealBrowser,
            target: "https://shop.example.com".into(),
            browser_selector: Some("#app".into()),
            browser_viewport: "mobile".into(),
            ..NewMonitor::default()
        };
        let id = crate::db::monitors::insert_monitor(&conn, &new).unwrap();
        let m = crate::db::monitors::get_monitor(&conn, id).unwrap().unwrap();
        let p = detail_panel(&conn, &m).unwrap().unwrap();
        assert!(p.browser.is_none() && p.shot.is_none());
        assert!(p.expect.contains("#app"));
        db_rb::insert(&conn, "Chrome", "ws://chrome:9222").unwrap();
        db_rb::save_screenshot(&conn, id, b"jpeg", "image/jpeg", Some("Selector #app not found")).unwrap();
        let p = detail_panel(&conn, &m).unwrap().unwrap();
        assert_eq!(p.browser.as_deref(), Some("Chrome"));
        assert_eq!(p.shot.unwrap().message, "Selector #app not found");
        let http = crate::db::monitors::insert_monitor(&conn, &NewMonitor { name: "x".into(), ..NewMonitor::default() }).unwrap();
        let http = crate::db::monitors::get_monitor(&conn, http).unwrap().unwrap();
        assert!(detail_panel(&conn, &http).unwrap().is_none());
    }

    async fn session(state: &AppState, name: &str, role: Role) -> String {
        use axum_extra::extract::cookie::Cookie;
        let name = name.to_string();
        let user = state
            .with_conn(move |c| {
                let id = crate::db::users::insert_user(c, &name, "x", role)?;
                crate::db::users::get_user(c, id)
            })
            .await
            .unwrap()
            .unwrap();
        let claims = crate::auth::session::SessionClaims::new(user.id, user.session_version, crate::auth::session::now_unix());
        let jar = SignedCookieJar::new(state.session_key.clone()).add(Cookie::new(crate::auth::SESSION_COOKIE, claims.encode()));
        let resp = (jar, ()).into_response();
        resp.headers()[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string()
    }

    async fn send(app: &axum::Router, method: &str, uri: &str, cookie: &str, body: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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
        (status, headers, to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec())
    }

    #[tokio::test]
    async fn screenshots_need_a_session_and_the_settings_an_admin() {
        let state = crate::api::agents::test_support::test_state().await;
        let id = state
            .with_conn(|c| {
                let id = crate::db::monitors::insert_monitor(
                    c,
                    &NewMonitor { name: "Shop".into(), monitor_type: MonitorType::RealBrowser, target: "https://s.example".into(), ..NewMonitor::default() },
                )?;
                db_rb::save_screenshot(c, id, b"\xff\xd8jpeg", "image/jpeg", Some("HTTP 500"))?;
                Ok(id)
            })
            .await
            .unwrap();
        let app = crate::api::build_router(state.clone());
        let shot = format!("/monitors/{id}/screenshot");
        let (status, _, _) = send(&app, "GET", &shot, "", "").await;
        assert!(status.is_redirection() || status == StatusCode::UNAUTHORIZED, "{status}");

        let viewer = session(&state, "viewer", Role::Viewer).await;
        let (status, headers, body) = send(&app, "GET", &shot, &viewer, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(headers[header::CACHE_CONTROL], "private, no-store");
        assert_eq!(body, b"\xff\xd8jpeg");
        let (status, _, _) = send(&app, "GET", "/settings/browsers", &viewer, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let admin = session(&state, "admin", Role::Admin).await;
        let (status, _, body) = send(&app, "POST", "/settings/browsers", &admin, "name=Chrome&url=wss%3A%2F%2Fb.example%3Ftoken%3Ds3cret").await;
        assert!(status.is_redirection(), "{status} {}", String::from_utf8_lossy(&body));
        let (status, _, body) = send(&app, "GET", "/settings/browsers", &admin, "").await;
        let html = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Chrome") && html.contains("b.example") && !html.contains("s3cret"), "the token is masked");
        let (status, _, body) = send(&app, "POST", "/settings/browsers", &admin, "name=chrome&url=nope").await;
        let html = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("field-error"), "errors are shown on the form");
        let bid = state.with_conn(|c| db_rb::ids(c)).await.unwrap()[0];
        let (status, _, _) = send(&app, "DELETE", &format!("/settings/browsers/{bid}"), &admin, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(state.with_conn(|c| db_rb::list(c)).await.unwrap().is_empty());
    }
}
