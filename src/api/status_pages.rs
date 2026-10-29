//! Status page editor. Owned by the statuspages agent.
//!
//! Routes (all behind auth; viewers may only see the list):
//! - `/status-pages` list · `/status-pages/new` create
//! - `/status-pages/{id}/edit` content (sections, drag-to-reorder), `?tab=settings`
//!   (incl. password, logo/favicon via `/status-pages/{id}/assets/{logo|favicon}`)
//!   and `?tab=analytics` (views chart; settings saved by `POST /status-pages/{id}/analytics`)
//! - `/status-pages/{id}` DELETE · `/status-pages/{id}/order` POST (drag result)
//! - `/status-pages/{id}/sections[/{sid}[/monitors[/{mid}]|/picker]]` section editing
//! - `/status-pages/{id}/incidents[/{iid}[/edit|/resolve|/reopen]]` announcements

use crate::i18n::Tr as _;
use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{CurrentUser, Role};
use crate::db::incidents::{self as db_incidents, Incident, NewIncident};
use crate::db::models::MonitorType;
use crate::db::status_pages::extras::{self as page_extras, Analytics, Provider};
use crate::db::status_pages::{
    self as db_pages, NewStatusPage, Section, StatusPage, StatusPageSummary,
};
use crate::db::subscribers as db_subscribers;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status-pages", get(list))
        .route("/status-pages/new", get(new_form).post(create))
        .route("/status-pages/{id}", delete(delete_page))
        .route("/status-pages/{id}/edit", get(edit).post(save_settings))
        .route("/status-pages/{id}/order", post(save_order))
        .route("/status-pages/{id}/sections", post(add_section))
        .route(
            "/status-pages/{id}/sections/{sid}",
            post(rename_section).delete(delete_section),
        )
        .route("/status-pages/{id}/sections/{sid}/picker", get(picker))
        .route(
            "/status-pages/{id}/sections/{sid}/monitors",
            post(add_monitors),
        )
        .route(
            "/status-pages/{id}/sections/{sid}/monitors/{mid}",
            delete(remove_monitor),
        )
        .route(
            "/status-pages/{id}/incidents",
            get(incidents).post(create_incident),
        )
        .route(
            "/status-pages/{id}/incidents/{iid}",
            post(update_incident).delete(delete_incident),
        )
        .route(
            "/status-pages/{id}/incidents/{iid}/edit",
            get(incident_form),
        )
        .route(
            "/status-pages/{id}/incidents/{iid}/updates",
            post(post_incident_update),
        )
        .route(
            "/status-pages/{id}/incidents/{iid}/updates/{uid}",
            delete(delete_incident_update),
        )
        .route(
            "/status-pages/{id}/incidents/{iid}/resolve",
            post(resolve_incident),
        )
        .route(
            "/status-pages/{id}/incidents/{iid}/reopen",
            post(reopen_incident),
        )
        .route("/status-pages/{id}/analytics", post(save_analytics))
        .route(
            "/status-pages/{id}/subscribers",
            get(subscribers).post(save_subscribers),
        )
        .route(
            "/status-pages/{id}/subscribers/{sid}",
            delete(remove_subscriber),
        )
        .route(
            "/status-pages/{id}/subscribers/{sid}/approve",
            post(approve_subscriber),
        )
        .merge(super::public_status::assets::editor_routes())
        .layer(axum::middleware::from_fn(
            super::public_status::invalidate_after_writes,
        ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decodes an `application/x-www-form-urlencoded` body or query string into
/// ordered pairs (repeated keys preserved).
pub fn form_pairs(raw: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(raw).into_owned().collect()
}

pub use crate::kit::http::{ascii_json, toast_trigger};

fn with_trigger(mut resp: Response, message: &str, kind: &str, close_sheet: bool) -> Response {
    resp.headers_mut()
        .insert("hx-trigger", toast_trigger(message, kind, close_sheet));
    resp
}

/// Full reload of the current page (after the flash cookie is set).
fn hx_refresh(jar: SignedCookieJar) -> Response {
    (jar, [("hx-refresh", "true")], StatusCode::OK).into_response()
}

async fn load_page(state: &AppState, id: i64) -> ApiResult<StatusPage> {
    state
        .with_conn(move |c| db_pages::get(c, id))
        .await?
        .ok_or(ApiError::NotFound)
}

/// Picker entry (monitor multi-select, shared with the maintenance form).
#[derive(Debug, Clone)]
pub struct PickerItem {
    pub id: i64,
    pub name: String,
    pub sub: String,
    pub checked: bool,
    pub disabled: bool,
}

/// What `partials/status_picker_list.html` renders.
#[derive(Debug, Clone)]
pub struct Picker {
    pub list_id: &'static str,
    pub items: Vec<PickerItem>,
    pub query: String,
}

/// Builds picker rows: checked ones first (always shown), then those
/// matching `query` (case-insensitive substring on name), capped at 200.
pub fn build_picker(
    conn: &rusqlite::Connection,
    list_id: &'static str,
    query: &str,
    checked: &[i64],
    disabled: &[i64],
) -> rusqlite::Result<Picker> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.name, m.monitor_type, m.active,
                (SELECT COUNT(*) FROM monitors c WHERE c.parent_id = m.id) AS children
         FROM monitors m ORDER BY m.name COLLATE NOCASE, m.id",
    )?;
    let q = query.trim().to_lowercase();
    let mut first = Vec::new();
    let mut rest = Vec::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, bool>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    for row in rows {
        let (id, name, kind, active, children) = row?;
        let is_checked = checked.contains(&id);
        let is_disabled = disabled.contains(&id);
        if !is_checked && !q.is_empty() && !name.to_lowercase().contains(&q) {
            continue;
        }
        let mut sub = MonitorType::parse(&kind).map_or(kind.clone(), |t| t.label().to_string());
        if kind == "group" {
            sub = crate::i18n::tn("sp.group_covers", children);
        }
        if !active {
            sub.push_str(&format!(" · {}", crate::i18n::t("common.paused")));
        }
        if is_disabled {
            sub.push_str(&format!(" · {}", crate::i18n::t("sp.already_here")));
        }
        let item = PickerItem {
            id,
            name,
            sub,
            checked: is_checked || is_disabled,
            disabled: is_disabled,
        };
        if is_checked {
            first.push(item)
        } else {
            rest.push(item)
        }
    }
    first.extend(rest);
    first.truncate(200.max(checked.len()));
    Ok(Picker {
        list_id,
        items: first,
        query: query.to_string(),
    })
}

// ---------------------------------------------------------------------------
// List / create / delete
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "status_page_list.html")]
struct ListTemplate {
    ctx: PageCtx,
    pages: Vec<StatusPageSummary>,
}

async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    let pages = state.with_conn(|c| db_pages::list(c)).await?;
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("nav.status_pages"),
        "status-pages",
        "",
    )
    .await;
    Ok((jar, HtmlTemplate(ListTemplate { ctx, pages })))
}

#[derive(Debug, Default, Deserialize)]
struct NewPageForm {
    #[serde(default)]
    title: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    description: String,
}

#[derive(Template)]
#[template(path = "status_page_new.html")]
struct NewTemplate {
    ctx: PageCtx,
    title: String,
    slug: String,
    description: String,
    title_error: String,
    slug_error: String,
}

async fn new_form(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("sp.new"),
        "status-pages",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(NewTemplate {
            ctx,
            title: String::new(),
            slug: String::new(),
            description: String::new(),
            title_error: String::new(),
            slug_error: String::new(),
        }),
    ))
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<NewPageForm>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let title = form.title.trim().to_string();
    let slug = if form.slug.trim().is_empty() {
        db_pages::slugify(&title)
    } else {
        form.slug.trim().to_string()
    };
    let description = form.description.trim().to_string();
    let mut title_error = String::new();
    if title.is_empty() {
        title_error = crate::i18n::t("sp.err_title").into();
    } else if title.chars().count() > 120 {
        title_error = crate::i18n::t("sp.err_title_long").into();
    }
    let mut slug_error = db_pages::validate_slug(&slug)
        .err()
        .map(str::to_string)
        .unwrap_or_default();
    let candidate = NewStatusPage {
        slug: slug.clone(),
        title: title.clone(),
        description: description.clone(),
        ..NewStatusPage::default()
    };
    let created = if title_error.is_empty() && slug_error.is_empty() {
        state
            .with_conn(move |c| {
                if db_pages::slug_taken(c, &candidate.slug, None)? {
                    return Ok(None);
                }
                db_pages::insert(c, &candidate).map(Some)
            })
            .await?
    } else {
        None
    };
    match created {
        Some(id) => {
            super::public_status::invalidate_domains(&state);
            let jar = flash(jar, "success", crate::i18n::t("sp.created"));
            Ok((jar, Redirect::to(&format!("/status-pages/{id}/edit"))).into_response())
        }
        None => {
            if title_error.is_empty() && slug_error.is_empty() {
                slug_error = crate::i18n::t("sp.err_slug_taken").into();
            }
            let (ctx, jar) = PageCtx::build(
                &state,
                Some(&user),
                &jar,
                crate::i18n::t("sp.new"),
                "status-pages",
                "",
            )
            .await;
            let tpl = NewTemplate {
                ctx,
                title,
                slug,
                description,
                title_error,
                slug_error,
            };
            Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, HtmlTemplate(tpl)).into_response())
        }
    }
}

async fn delete_page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    if !state.with_conn(move |c| db_pages::delete(c, id)).await? {
        return Err(ApiError::NotFound);
    }
    super::public_status::invalidate_domains(&state);
    let jar = flash(jar, "success", crate::i18n::t("sp.deleted"));
    Ok((jar, [("hx-redirect", "/status-pages")], StatusCode::OK).into_response())
}

// ---------------------------------------------------------------------------
// Editor: content + settings
// ---------------------------------------------------------------------------

/// Settings form values and per-field errors (empty = valid).
#[derive(Debug, Clone, Default)]
struct SettingsForm {
    title: String,
    slug: String,
    description: String,
    published: bool,
    theme: String,
    show_tags: bool,
    show_uptime: bool,
    show_response: bool,
    show_powered_by: bool,
    footer_text: String,
    custom_css: String,
    refresh_seconds: String,
    custom_domain: String,
    custom_domain_only: bool,
    /// The page has a password now (the field is write-only).
    has_password: bool,
    /// New password ("" = keep), only carried for validation.
    new_password: String,
    remove_password: bool,
    title_error: String,
    slug_error: String,
    domain_error: String,
    css_error: String,
    refresh_error: String,
    footer_error: String,
    password_error: String,
}

impl SettingsForm {
    fn from_page(p: &StatusPage) -> Self {
        SettingsForm {
            title: p.title.clone(),
            slug: p.slug.clone(),
            description: p.description.clone(),
            published: p.published,
            theme: p.theme.clone(),
            show_tags: p.show_tags,
            show_uptime: p.show_uptime,
            show_response: p.show_response,
            show_powered_by: p.show_powered_by,
            footer_text: p.footer_text.clone().unwrap_or_default(),
            custom_css: p.custom_css.clone().unwrap_or_default(),
            refresh_seconds: p.refresh_seconds.to_string(),
            custom_domain: p.custom_domain.clone().unwrap_or_default(),
            custom_domain_only: p.custom_domain_only,
            has_password: p.is_protected(),
            ..SettingsForm::default()
        }
    }

    fn has_errors(&self) -> bool {
        [
            &self.title_error,
            &self.slug_error,
            &self.domain_error,
            &self.css_error,
            &self.refresh_error,
            &self.footer_error,
            &self.password_error,
        ]
        .iter()
        .any(|e| !e.is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct SettingsInput {
    #[serde(default)]
    title: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    published: bool,
    #[serde(default)]
    theme: String,
    #[serde(default)]
    show_tags: bool,
    #[serde(default)]
    show_uptime: bool,
    #[serde(default)]
    show_response: bool,
    #[serde(default)]
    show_powered_by: bool,
    #[serde(default)]
    footer_text: String,
    #[serde(default)]
    custom_css: String,
    #[serde(default)]
    refresh_seconds: String,
    #[serde(default)]
    custom_domain: String,
    #[serde(default)]
    custom_domain_only: bool,
    /// Write-only: a new page password ("" keeps the current one).
    #[serde(default)]
    page_password: String,
    #[serde(default)]
    remove_password: bool,
}

fn footer_too_long() -> String {
    crate::i18n::tf(
        "sp.err_footer_long",
        &[("max", &crate::i18n::fmt_int(db_pages::FOOTER_MAX as i64))],
    )
}

/// Every `Host` the request arrived under (`Host`, `X-Forwarded-Host`).
fn request_hosts(headers: &HeaderMap) -> Vec<String> {
    [header::HOST.as_str(), "x-forwarded-host"]
        .iter()
        .filter_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()))
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// Pure validation (uniqueness is checked against the DB afterwards).
fn validate_settings(input: SettingsInput) -> (SettingsForm, Option<NewStatusPage>) {
    let mut f = SettingsForm {
        title: input.title.trim().to_string(),
        slug: input.slug.trim().to_string(),
        description: input.description.trim().to_string(),
        published: input.published,
        theme: if db_pages::THEMES.contains(&input.theme.as_str()) {
            input.theme
        } else {
            "auto".into()
        },
        show_tags: input.show_tags,
        show_uptime: input.show_uptime,
        show_response: input.show_response,
        show_powered_by: input.show_powered_by,
        footer_text: input.footer_text.trim().to_string(),
        custom_css: input.custom_css.trim().to_string(),
        refresh_seconds: input.refresh_seconds.trim().to_string(),
        custom_domain: input.custom_domain.trim().to_string(),
        custom_domain_only: input.custom_domain_only,
        remove_password: input.remove_password,
        ..SettingsForm::default()
    };
    if !input.remove_password && !input.page_password.is_empty() {
        match page_extras::password_problem(&input.page_password) {
            Some(e) => f.password_error = e.into(),
            None => f.new_password = input.page_password,
        }
    }
    if f.title.is_empty() {
        f.title_error = crate::i18n::t("sp.err_title").into();
    } else if f.title.chars().count() > 120 {
        f.title_error = crate::i18n::t("sp.err_title_long").into();
    }
    if let Err(e) = db_pages::validate_slug(&f.slug) {
        f.slug_error = e.into();
    }
    let domain = match db_pages::normalize_domain(&f.custom_domain) {
        Ok(d) => {
            f.custom_domain = d.clone().unwrap_or_default();
            d
        }
        Err(e) => {
            f.domain_error = e.into();
            None
        }
    };
    if let Err(e) = db_pages::validate_css(&f.custom_css) {
        f.css_error = e.into();
    }
    if f.footer_text.chars().count() > db_pages::FOOTER_MAX {
        f.footer_error = footer_too_long();
    }
    let refresh = match f.refresh_seconds.parse::<i64>() {
        Ok(n) if (db_pages::REFRESH_MIN..=db_pages::REFRESH_MAX).contains(&n) => n,
        _ => {
            f.refresh_error = crate::i18n::tf(
                "sp.err_refresh",
                &[
                    ("min", &db_pages::REFRESH_MIN.to_string()),
                    ("max", &db_pages::REFRESH_MAX.to_string()),
                ],
            );
            60
        }
    };
    if f.has_errors() {
        return (f, None);
    }
    let page = NewStatusPage {
        slug: f.slug.clone(),
        title: f.title.clone(),
        description: f.description.clone(),
        custom_domain: domain,
        custom_domain_only: f.custom_domain_only,
        published: f.published,
        theme: f.theme.clone(),
        show_tags: f.show_tags,
        show_uptime: f.show_uptime,
        show_response: f.show_response,
        show_powered_by: f.show_powered_by,
        footer_text: Some(f.footer_text.clone()).filter(|s| !s.is_empty()),
        custom_css: Some(f.custom_css.clone()).filter(|s| !s.is_empty()),
        refresh_seconds: refresh,
    };
    (f, Some(page))
}

#[derive(Template)]
#[template(path = "status_page_edit.html")]
struct EditTemplate {
    ctx: PageCtx,
    page: StatusPage,
    tab: &'static str,
    sections: Vec<Section>,
    form: SettingsForm,
    badges: Vec<BadgeRow>,
    badges_live: bool,
    /// "Logo and icon" section (settings tab), pre-rendered.
    assets_html: String,
    /// Analytics tab.
    stats: super::public_status::analytics::Stats,
    aform: AnalyticsForm,
    providers: Vec<(&'static str, &'static str)>,
    /// Only administrators choose the analytics script URL.
    is_admin: bool,
}

/// Analytics tab form values and per-field errors.
#[derive(Debug, Clone, Default)]
struct AnalyticsForm {
    enabled: bool,
    unique: bool,
    provider: String,
    site_id: String,
    url: String,
    site_id_error: String,
    url_error: String,
}

impl AnalyticsForm {
    fn from_settings(a: &Analytics) -> Self {
        AnalyticsForm {
            enabled: a.enabled,
            unique: a.unique,
            provider: a.provider.as_str().into(),
            site_id: a.site_id.clone(),
            url: a.url.clone(),
            ..AnalyticsForm::default()
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnalyticsInput {
    #[serde(default)]
    analytics_enabled: bool,
    #[serde(default)]
    analytics_unique: bool,
    #[serde(default)]
    analytics_provider: String,
    #[serde(default)]
    analytics_site_id: String,
    #[serde(default)]
    analytics_url: String,
}

/// Pure validation of the analytics tab.
fn validate_analytics(input: AnalyticsInput) -> (AnalyticsForm, Option<Analytics>) {
    let mut form = AnalyticsForm {
        enabled: input.analytics_enabled,
        unique: input.analytics_unique,
        provider: input.analytics_provider.trim().to_string(),
        site_id: input.analytics_site_id.trim().to_string(),
        url: input.analytics_url.trim().to_string(),
        ..AnalyticsForm::default()
    };
    let Some(provider) = Provider::parse(&form.provider) else {
        form.provider = "none".into();
        form.site_id_error = crate::i18n::t("spx.err_analytics_provider").into();
        return (form, None);
    };
    let candidate = Analytics {
        enabled: form.enabled,
        unique: form.unique,
        provider,
        site_id: form.site_id.clone(),
        url: form.url.clone(),
    };
    match candidate.validated() {
        Ok(a) => (form, Some(a)),
        Err(errors) => {
            for (field, msg) in errors {
                match field {
                    "analytics_url" => form.url_error = msg.into(),
                    _ => form.site_id_error = msg.into(),
                }
            }
            (form, None)
        }
    }
}

#[derive(Template)]
#[template(path = "partials/status_sections.html")]
struct SectionsPartial {
    page: StatusPage,
    sections: Vec<Section>,
}

/// One monitor in the "Badges" sheet.
#[derive(Debug, Clone)]
struct BadgeRow {
    name: String,
    /// Same-origin preview paths (`img-src 'self'`); absolute URLs (with the
    /// base URL) only appear in the copyable Markdown.
    status_src: String,
    uptime_src: String,
    markdown: String,
    uptime_markdown: String,
    status_copy_id: String,
    uptime_copy_id: String,
}

fn base_url(configured: Option<String>, headers: &HeaderMap) -> String {
    if let Some(b) = configured.filter(|b| !b.trim().is_empty()) {
        return b.trim_end_matches('/').to_string();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{proto}://{host}")
}

#[derive(Debug, Default, Deserialize)]
struct EditQuery {
    #[serde(default)]
    tab: String,
}

#[allow(clippy::too_many_arguments)]
async fn render_edit(
    state: &AppState,
    user: &CurrentUser,
    jar: SignedCookieJar,
    headers: &HeaderMap,
    page: StatusPage,
    tab: &'static str,
    form: Option<SettingsForm>,
    aform: Option<AnalyticsForm>,
) -> ApiResult<(SignedCookieJar, HtmlTemplate<EditTemplate>)> {
    let id = page.id;
    let with_stats = tab == "analytics";
    let (sections, settings, stats) = state
        .with_conn(move |c| {
            let stats = if with_stats {
                super::public_status::analytics::load_stats(c, id)?
            } else {
                Default::default()
            };
            Ok((
                db_pages::sections(c, id)?,
                crate::db::settings::load(c)?,
                stats,
            ))
        })
        .await?;
    let base = base_url(settings.base_url.clone(), headers);
    let mut seen = std::collections::HashSet::new();
    let badges = sections
        .iter()
        .flat_map(|s| s.monitors.iter())
        .filter(|m| seen.insert(m.monitor_id))
        .map(|m| {
            let status_src = format!("/api/badge/{}/status", m.monitor_id);
            let uptime_src = format!("/api/badge/{}/uptime?duration=30d", m.monitor_id);
            let status_url = format!("{base}{status_src}");
            let uptime_url = format!("{base}{uptime_src}");
            let alt = m.name.replace(['[', ']'], "");
            BadgeRow {
                name: m.name.clone(),
                markdown: format!("![{alt} status]({status_url})"),
                uptime_markdown: format!("![{alt} uptime]({uptime_url})"),
                status_copy_id: format!("badge-status-{}", m.monitor_id),
                uptime_copy_id: format!("badge-uptime-{}", m.monitor_id),
                status_src,
                uptime_src,
            }
        })
        .collect();
    let (ctx, jar) = PageCtx::build(
        state,
        Some(user),
        &jar,
        crate::i18n::tf("monitor_form.edit_heading", &[("name", &page.title)]),
        "status-pages",
        "",
    )
    .await;
    let form = form.unwrap_or_else(|| SettingsForm::from_page(&page));
    let aform = aform.unwrap_or_else(|| AnalyticsForm::from_settings(&page.analytics));
    let assets_html = if tab == "settings" {
        super::public_status::assets::AssetsPartial::new(page.clone(), String::new()).html()?
    } else {
        String::new()
    };
    Ok((
        jar,
        HtmlTemplate(EditTemplate {
            ctx,
            badges_live: (page.published && !page.is_protected()) || settings.badges_public,
            page,
            tab,
            sections,
            form,
            badges,
            assets_html,
            stats,
            aform,
            providers: Provider::ALL
                .iter()
                .map(|p| (p.as_str(), p.label()))
                .collect(),
            is_admin: user.is_admin(),
        }),
    ))
}

async fn edit(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    axum::extract::Query(q): axum::extract::Query<EditQuery>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let page = load_page(&state, id).await?;
    let tab = match q.tab.as_str() {
        "settings" => "settings",
        "analytics" => "analytics",
        _ => "content",
    };
    render_edit(&state, &user, jar, &headers, page, tab, None, None).await
}

async fn save_analytics(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(input): Form<AnalyticsInput>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let page = load_page(&state, id).await?;
    let (mut form, mut valid) = validate_analytics(input);
    // The script URL ends up on the page and in its CSP: administrators only.
    if !user.is_admin()
        && valid
            .as_ref()
            .is_some_and(|a| page_extras::changes_script_url(Some(&page.analytics), a))
    {
        form.url_error = crate::i18n::t("spx.script_url_admin").into();
        valid = None;
    }
    if let Some(a) = valid {
        state
            .with_conn(move |c| page_extras::set_analytics(c, id, &a))
            .await?;
        let jar = flash(jar, "success", crate::i18n::t("spx.analytics_saved"));
        return Ok((
            jar,
            Redirect::to(&format!("/status-pages/{id}/edit?tab=analytics")),
        )
            .into_response());
    }
    let (jar, tpl) = render_edit(
        &state,
        &user,
        jar,
        &headers,
        page,
        "analytics",
        None,
        Some(form),
    )
    .await?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, tpl).into_response())
}

async fn save_settings(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(input): Form<SettingsInput>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let page = load_page(&state, id).await?;
    let (mut form, valid) = validate_settings(input);
    let hosts = request_hosts(&headers);
    if let Some(new) = valid {
        // Write-only password: hash a new one (slow, off the async threads).
        let password = if form.remove_password {
            page_extras::PasswordChange::Remove
        } else if form.new_password.is_empty() {
            page_extras::PasswordChange::Keep
        } else {
            page_extras::PasswordChange::Set(
                crate::auth::hash_password_async(std::mem::take(&mut form.new_password)).await?,
            )
        };
        let saved = state
            .with_conn(move |c| {
                if db_pages::slug_taken(c, &new.slug, Some(id))? {
                    return Ok(Err("slug"));
                }
                if let Some(d) = &new.custom_domain {
                    let base_url = crate::db::settings::load(c)?.base_url;
                    let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
                    if db_pages::is_own_host(d, base_url.as_deref(), &hosts) {
                        return Ok(Err("self"));
                    }
                }
                if let Some(d) = &new.custom_domain
                    && db_pages::domain_taken(c, d, Some(id))?
                {
                    return Ok(Err("domain"));
                }
                db_pages::update(c, id, &new)?;
                page_extras::apply_extras(
                    c,
                    id,
                    &page_extras::ExtrasChange {
                        password,
                        ..Default::default()
                    },
                )?;
                Ok(Ok(()))
            })
            .await?;
        match saved {
            Ok(()) => {
                super::public_status::invalidate_domains(&state);
                let jar = flash(jar, "success", crate::i18n::t("sp.saved"));
                return Ok((
                    jar,
                    Redirect::to(&format!("/status-pages/{id}/edit?tab=settings")),
                )
                    .into_response());
            }
            Err("slug") => form.slug_error = crate::i18n::t("sp.err_slug_taken").into(),
            Err("self") => form.domain_error = crate::i18n::t("sp.err_domain_self").into(),
            Err(_) => form.domain_error = crate::i18n::t("sp.err_domain_taken").into(),
        }
    }
    form.new_password.clear();
    form.has_password = page.is_protected();
    let (jar, tpl) = render_edit(
        &state,
        &user,
        jar,
        &headers,
        page,
        "settings",
        Some(form),
        None,
    )
    .await?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, tpl).into_response())
}

async fn sections_response(state: &AppState, page_id: i64) -> ApiResult<Response> {
    let (page, sections) = state
        .with_conn(move |c| {
            let page = db_pages::get(c, page_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            Ok((page, db_pages::sections(c, page_id)?))
        })
        .await?;
    Ok(HtmlTemplate(SectionsPartial { page, sections }).into_response())
}

/// Parses the drag-and-drop form: `sections[]` (section order) and
/// `section_{id}[]` (monitor order per section).
pub fn parse_order(pairs: &[(String, String)]) -> (Vec<i64>, Vec<(i64, Vec<i64>)>) {
    let sections: Vec<i64> = pairs
        .iter()
        .filter(|(k, _)| k == "sections[]")
        .filter_map(|(_, v)| v.parse().ok())
        .collect();
    let monitors = sections
        .iter()
        .map(|sid| {
            let key = format!("section_{sid}[]");
            let ids = pairs
                .iter()
                .filter(|(k, _)| *k == key)
                .filter_map(|(_, v)| v.parse().ok())
                .collect();
            (*sid, ids)
        })
        .collect();
    (sections, monitors)
}

async fn save_order(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let (sections, monitors) = parse_order(&form_pairs(&body));
    if sections.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    load_page(&state, id).await?;
    state
        .with_conn(move |c| db_pages::apply_order(c, id, &sections, &monitors))
        .await?;
    state.notify_update();
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn clean_section_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.chars().count() > 80 {
        return Err(ApiError::bad_request(crate::i18n::t("sp.err_section_long")));
    }
    Ok(name.to_string())
}

async fn add_section(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let pairs = form_pairs(&body);
    let raw = pairs
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let name = clean_section_name(raw)?;
    let name = if name.is_empty() {
        crate::i18n::t("sp.new_section").to_string()
    } else {
        name
    };
    load_page(&state, id).await?;
    state
        .with_conn(move |c| db_pages::add_section(c, id, &name))
        .await?;
    Ok(with_trigger(
        sections_response(&state, id).await?,
        crate::i18n::t("sp.section_added"),
        "success",
        false,
    ))
}

async fn rename_section(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, sid)): Path<(i64, i64)>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    // The input is named `name_{sid}` because it lives inside the order form.
    let key = format!("name_{sid}");
    let pairs = form_pairs(&body);
    let raw = pairs
        .iter()
        .find(|(k, _)| *k == key || k == "name")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let name = clean_section_name(raw)?;
    if !state
        .with_conn(move |c| db_pages::rename_section(c, id, sid, &name))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(with_trigger(
        StatusCode::OK.into_response(),
        crate::i18n::t("sp.section_renamed"),
        "success",
        false,
    ))
}

async fn delete_section(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, sid)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    if !state
        .with_conn(move |c| db_pages::delete_section(c, id, sid))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(with_trigger(
        sections_response(&state, id).await?,
        crate::i18n::t("sp.section_deleted"),
        "success",
        false,
    ))
}

#[derive(Template)]
#[template(path = "partials/status_section_picker.html")]
struct PickerTemplate {
    page_id: i64,
    section_id: i64,
    section_name: String,
    list_only: bool,
    picker: Picker,
}

async fn picker(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, sid)): Path<(i64, i64)>,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let pairs = form_pairs(query.unwrap_or_default().as_bytes());
    let q = pairs
        .iter()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let list_only = pairs.iter().any(|(k, _)| k == "list");
    let checked: Vec<i64> = pairs
        .iter()
        .filter(|(k, _)| k == "monitor_id")
        .filter_map(|(_, v)| v.parse().ok())
        .collect();
    let (section_name, picker) = state
        .with_conn(move |c| {
            let sections = db_pages::sections(c, id)?;
            let section = sections
                .into_iter()
                .find(|s| s.id == sid)
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let existing: Vec<i64> = section.monitors.iter().map(|m| m.monitor_id).collect();
            Ok((
                section.name,
                build_picker(c, "sp-picker-list", &q, &checked, &existing)?,
            ))
        })
        .await?;
    Ok(HtmlTemplate(PickerTemplate {
        page_id: id,
        section_id: sid,
        section_name,
        list_only,
        picker,
    })
    .into_response())
}

#[derive(Debug, Default, Deserialize)]
struct MonitorIdsForm {
    #[serde(default)]
    monitor_id: Vec<i64>,
}

async fn add_monitors(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, sid)): Path<(i64, i64)>,
    axum_extra::extract::Form(form): axum_extra::extract::Form<MonitorIdsForm>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let ids = form.monitor_id;
    let added = state
        .with_conn(move |c| db_pages::add_monitors(c, id, sid, &ids))
        .await?;
    let msg = match added {
        0 => crate::i18n::t("sp.nothing_to_add").to_string(),
        n => crate::i18n::tn("sp.added_monitors", n as i64),
    };
    state.notify_update();
    Ok(with_trigger(
        sections_response(&state, id).await?,
        &msg,
        "success",
        true,
    ))
}

async fn remove_monitor(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, sid, mid)): Path<(i64, i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    if !state
        .with_conn(move |c| db_pages::remove_monitor(c, id, sid, mid))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(with_trigger(
        sections_response(&state, id).await?,
        crate::i18n::t("sp.monitor_removed"),
        "success",
        false,
    ))
}

// ---------------------------------------------------------------------------
// Incidents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct IncidentForm {
    title: String,
    body: String,
    severity: String,
    pinned: bool,
}

#[derive(Template)]
#[template(path = "incident_list.html")]
struct IncidentsTemplate {
    ctx: PageCtx,
    page: StatusPage,
    tab: &'static str,
    incidents: Vec<IncidentRow>,
    /// For the embedded "new incident" form.
    page_id: i64,
    incident_id: i64,
    form: IncidentForm,
}

#[derive(Debug, Clone)]
struct IncidentRow {
    incident: Incident,
    body_html: String,
    meta: String,
    updates: Vec<super::public_status::UpdateView>,
    /// Base of the update endpoints for this incident.
    updates_url: String,
}

#[derive(Template)]
#[template(path = "partials/status_incident_form.html")]
struct IncidentFormPartial {
    page_id: i64,
    incident_id: i64,
    form: IncidentForm,
}

#[derive(Debug, Deserialize)]
struct IncidentInput {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    pinned: bool,
}

fn validate_incident(input: IncidentInput) -> Result<NewIncident, ApiError> {
    let title = input.title.trim().to_string();
    if title.is_empty() {
        return Err(ApiError::bad_request(crate::i18n::t("inc.err_title")));
    }
    if title.chars().count() > 200 {
        return Err(ApiError::bad_request(crate::i18n::t("inc.err_title_long")));
    }
    if input.body.len() > 20_000 {
        return Err(ApiError::bad_request(crate::i18n::t("inc.err_body_long")));
    }
    let severity = if db_incidents::SEVERITIES.contains(&input.severity.as_str()) {
        input.severity
    } else {
        "info".into()
    };
    Ok(NewIncident {
        title,
        body: input.body.trim().to_string(),
        severity,
        pinned: input.pinned,
    })
}

async fn incidents(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let (page, list, mut timelines, settings) = state
        .with_conn(move |c| {
            let page = db_pages::get(c, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let list = db_incidents::list_for_page(c, id)?;
            let ids: Vec<i64> = list.iter().map(|i| i.id).collect();
            let timelines = db_incidents::updates_for(c, &ids)?;
            Ok((page, list, timelines, crate::db::settings::load(c)?))
        })
        .await?;
    let tz = super::public_status::site_tz(&settings.timezone);
    let incidents = list
        .into_iter()
        .map(|i| {
            let mut meta = crate::i18n::tf(
                "inc.posted",
                &[("when", &super::public_status::fmt_local(&i.created_at, &tz))],
            );
            if let Some(r) = &i.resolved_at {
                meta.push_str(" · ");
                meta.push_str(&crate::i18n::tf(
                    "inc.resolved_at",
                    &[("when", &super::public_status::fmt_local(r, &tz))],
                ));
            }
            let updates = timelines
                .remove(&i.id)
                .unwrap_or_default()
                .into_iter()
                .map(|u| super::public_status::UpdateView {
                    id: u.id,
                    label: u.status_label(),
                    time: super::public_status::fmt_local(&u.created_at, &tz),
                    body_html: super::public_status::markdown::render(&u.body),
                    status: u.status,
                    posted_at: u.created_at,
                    body: u.body,
                })
                .collect();
            IncidentRow {
                body_html: super::public_status::markdown::render(&i.body),
                meta,
                updates,
                updates_url: format!("/status-pages/{id}/incidents/{}/updates", i.id),
                incident: i,
            }
        })
        .collect();
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        format!("{} · {}", crate::i18n::t("sp.incidents"), page.title),
        "status-pages",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(IncidentsTemplate {
            ctx,
            page_id: page.id,
            incident_id: 0,
            page,
            tab: "incidents",
            incidents,
            form: IncidentForm {
                severity: "warning".into(),
                pinned: true,
                ..IncidentForm::default()
            },
        }),
    ))
}

async fn create_incident(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(input): Form<IncidentInput>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let back = format!("/status-pages/{id}/incidents");
    let new = match validate_incident(input) {
        Ok(n) => n,
        Err(e) => {
            let jar = flash(jar, "error", e.public_message());
            return Ok((jar, Redirect::to(&back)).into_response());
        }
    };
    load_page(&state, id).await?;
    let iid = state
        .with_conn(move |c| db_incidents::insert(c, id, &new))
        .await?;
    state.notify_update();
    crate::subscribers::announce_incident(&state, id, iid, crate::subscribers::Kind::Incident);
    let jar = flash(jar, "success", crate::i18n::t("inc.posted_flash"));
    Ok((jar, Redirect::to(&back)).into_response())
}

async fn incident_form(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, iid)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let i = state
        .with_conn(move |c| db_incidents::get(c, id, iid))
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(HtmlTemplate(IncidentFormPartial {
        page_id: id,
        incident_id: iid,
        form: IncidentForm {
            title: i.title,
            body: i.body,
            severity: i.severity,
            pinned: i.pinned,
        },
    })
    .into_response())
}

async fn update_incident(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, iid)): Path<(i64, i64)>,
    Form(input): Form<IncidentInput>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let back = format!("/status-pages/{id}/incidents");
    let new = match validate_incident(input) {
        Ok(n) => n,
        Err(e) => {
            let jar = flash(jar, "error", e.public_message());
            return Ok((jar, Redirect::to(&back)).into_response());
        }
    };
    if !state
        .with_conn(move |c| db_incidents::update(c, id, iid, &new))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    state.notify_update();
    let jar = flash(jar, "success", crate::i18n::t("inc.updated"));
    Ok((jar, Redirect::to(&back)).into_response())
}

async fn set_resolved(
    state: &AppState,
    jar: SignedCookieJar,
    id: i64,
    iid: i64,
    resolved: bool,
) -> ApiResult<Response> {
    if !state
        .with_conn(move |c| db_incidents::set_resolved(c, id, iid, resolved))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    state.notify_update();
    if resolved {
        crate::subscribers::announce_incident(state, id, iid, crate::subscribers::Kind::Resolved);
    }
    let jar = flash(
        jar,
        "success",
        if resolved {
            crate::i18n::t("inc.resolved_flash")
        } else {
            crate::i18n::t("inc.reopened")
        },
    );
    Ok(hx_refresh(jar))
}

async fn resolve_incident(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, iid)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    set_resolved(&state, jar, id, iid, true).await
}

async fn reopen_incident(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, iid)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    set_resolved(&state, jar, id, iid, false).await
}

#[derive(Debug, Deserialize)]
struct UpdateInput {
    #[serde(default)]
    status: String,
    #[serde(default)]
    body: String,
}

/// Longest update text, in characters.
const UPDATE_MAX: usize = 5_000;

/// Validates an update: a known status and some text.
fn validate_update(input: UpdateInput) -> Result<(String, String), ApiError> {
    let status = input.status.trim().to_string();
    if !db_incidents::UPDATE_STATUSES.contains(&status.as_str()) {
        return Err(ApiError::bad_request(crate::i18n::t(
            "inc.err_update_status",
        )));
    }
    let body = input.body.trim().to_string();
    if body.is_empty() {
        return Err(ApiError::bad_request(crate::i18n::t("inc.err_update_body")));
    }
    if body.chars().count() > UPDATE_MAX {
        return Err(ApiError::bad_request(crate::i18n::tf(
            "inc.err_update_long",
            &[("max", &crate::i18n::fmt_int(UPDATE_MAX as i64))],
        )));
    }
    Ok((status, body))
}

async fn post_incident_update(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, iid)): Path<(i64, i64)>,
    Form(input): Form<UpdateInput>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let back = format!("/status-pages/{id}/incidents#incident-{iid}");
    let (status, body) = match validate_update(input) {
        Ok(v) => v,
        Err(e) => {
            return Ok(
                (flash(jar, "error", e.public_message()), Redirect::to(&back)).into_response(),
            );
        }
    };
    let kind = if status == "resolved" {
        crate::subscribers::Kind::Resolved
    } else {
        crate::subscribers::Kind::Update
    };
    if state
        .with_conn(move |c| db_incidents::add_update(c, id, iid, &status, &body))
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    state.notify_update();
    crate::subscribers::announce_incident(&state, id, iid, kind);
    Ok((
        flash(jar, "success", crate::i18n::t("inc.update_posted")),
        Redirect::to(&back),
    )
        .into_response())
}

async fn delete_incident_update(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, iid, uid)): Path<(i64, i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    if !state
        .with_conn(move |c| db_incidents::delete_update(c, id, iid, uid))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    state.notify_update();
    Ok(hx_refresh(flash(
        jar,
        "success",
        crate::i18n::t("inc.update_deleted"),
    )))
}

async fn delete_incident(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, iid)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    if !state
        .with_conn(move |c| db_incidents::delete(c, id, iid))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    state.notify_update();
    let jar = flash(jar, "success", crate::i18n::t("inc.deleted"));
    Ok(hx_refresh(jar))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> SettingsInput {
        SettingsInput {
            title: " Acme ".into(),
            slug: "acme".into(),
            description: String::new(),
            published: true,
            theme: "dark".into(),
            show_tags: false,
            show_uptime: true,
            show_response: false,
            show_powered_by: true,
            footer_text: String::new(),
            custom_css: String::new(),
            refresh_seconds: "60".into(),
            custom_domain: "HTTPS://Status.Acme.test/".into(),
            custom_domain_only: false,
            page_password: String::new(),
            remove_password: false,
        }
    }

    #[test]
    fn settings_carry_the_domain_only_flag() {
        let (form, page) = validate_settings(input());
        assert!(!form.custom_domain_only);
        assert!(!page.unwrap().custom_domain_only);
        let mut on = input();
        on.custom_domain_only = true;
        let (form, page) = validate_settings(on);
        assert!(form.custom_domain_only);
        assert!(page.unwrap().custom_domain_only);
    }

    #[test]
    fn settings_validation_checks_the_page_password() {
        let mut short = input();
        short.page_password = "short".into();
        let (form, page) = validate_settings(short);
        assert!(page.is_none());
        assert!(!form.password_error.is_empty());
        let mut ok = input();
        ok.page_password = "correct horse".into();
        let (form, page) = validate_settings(ok);
        assert!(page.is_some());
        assert_eq!(form.new_password, "correct horse");
        // Removing wins over a typed password.
        let mut remove = input();
        remove.page_password = "x".into();
        remove.remove_password = true;
        let (form, page) = validate_settings(remove);
        assert!(page.is_some() && form.remove_password && form.new_password.is_empty());
    }

    #[test]
    fn analytics_tab_validation() {
        let input = |provider: &str, id: &str, url: &str| AnalyticsInput {
            analytics_enabled: true,
            analytics_unique: false,
            analytics_provider: provider.into(),
            analytics_site_id: id.into(),
            analytics_url: url.into(),
        };
        let (_, a) = validate_analytics(input(
            "plausible",
            "example.com",
            "https://plausible.io/js/script.js",
        ));
        let a = a.unwrap();
        assert_eq!((a.provider, a.unique), (Provider::Plausible, false));
        let (form, a) = validate_analytics(input("umami", "nope", "http://x.test/s.js"));
        assert!(a.is_none());
        assert!(!form.site_id_error.is_empty() && !form.url_error.is_empty());
        let (form, a) = validate_analytics(input("evil", "", ""));
        assert!(a.is_none());
        assert_eq!(form.provider, "none");
    }

    #[test]
    fn settings_validation_normalises_and_accepts() {
        let (form, page) = validate_settings(input());
        let page = page.expect("valid");
        assert!(!form.has_errors());
        assert_eq!(page.title, "Acme");
        assert_eq!(page.custom_domain.as_deref(), Some("status.acme.test"));
        assert_eq!(page.theme, "dark");
        assert_eq!(page.footer_text, None);
    }

    #[test]
    fn settings_validation_reports_each_field() {
        let mut bad = input();
        bad.title = " ".into();
        bad.slug = "New".into();
        bad.custom_domain = "localhost:3000".into();
        bad.custom_css = "a{}</style><script>".into();
        bad.refresh_seconds = "2".into();
        bad.theme = "neon".into();
        let (form, page) = validate_settings(bad);
        assert!(page.is_none());
        assert!(!form.title_error.is_empty());
        assert!(!form.slug_error.is_empty());
        assert!(!form.domain_error.is_empty());
        assert!(!form.css_error.is_empty());
        assert!(!form.refresh_error.is_empty());
        assert_eq!(form.theme, "auto");
    }

    #[test]
    fn order_form_parsing() {
        let pairs = form_pairs(b"sections%5B%5D=3&sections%5B%5D=1&section_1%5B%5D=9&name_3=x&section_3%5B%5D=7&section_3%5B%5D=8&section_5%5B%5D=4&sections%5B%5D=bad");
        let (sections, monitors) = parse_order(&pairs);
        assert_eq!(sections, vec![3, 1]);
        assert_eq!(monitors, vec![(3, vec![7, 8]), (1, vec![9])]);
    }

    #[test]
    fn incident_validation() {
        let ok = validate_incident(IncidentInput {
            title: " Slow ".into(),
            body: "x".into(),
            severity: "bogus".into(),
            pinned: true,
        })
        .unwrap();
        assert_eq!(ok.title, "Slow");
        assert_eq!(ok.severity, "info");
        assert!(
            validate_incident(IncidentInput {
                title: "".into(),
                body: String::new(),
                severity: "info".into(),
                pinned: false
            })
            .is_err()
        );
    }

    #[test]
    fn toast_trigger_is_json() {
        let v = toast_trigger("Saved “x” ✓ 😀", "success", true);
        let parsed: serde_json::Value = serde_json::from_str(v.to_str().unwrap()).unwrap();
        assert_eq!(parsed["vv:toast"]["message"], "Saved “x” ✓ 😀");
        assert_eq!(parsed["vv:close-sheet"], true);
    }
}

// ---------------------------------------------------------------------------
// Subscribers
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "status_page_subscribers.html")]
struct SubscribersTemplate {
    ctx: PageCtx,
    page: StatusPage,
    tab: &'static str,
    settings: db_subscribers::Settings,
    /// Active email channels: (id, name, selected).
    channels: Vec<(i64, String, bool)>,
    rows: Vec<SubscriberRow>,
    /// Links in messages have no address to point at (no public URL and no
    /// custom domain), so the page offers no subscriptions.
    base_missing: bool,
    /// The viewer can open Settings → General.
    is_admin: bool,
}

struct SubscriberRow {
    id: i64,
    target: String,
    kind_label: &'static str,
    confirmed: bool,
    /// A webhook waiting for an editor's approval.
    awaiting: bool,
    created: String,
}

async fn subscribers(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let (page, settings, channels, list, site) = state
        .with_conn(move |c| {
            let page = db_pages::get(c, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let channels: Vec<(i64, String)> = crate::db::notifications::list_channels(c)?
                .into_iter()
                .filter(|ch| ch.active && ch.kind == "email")
                .map(|ch| (ch.id, ch.name))
                .collect();
            Ok((
                page,
                db_subscribers::settings(c, id)?,
                channels,
                db_subscribers::list(c, id)?,
                crate::db::settings::load(c)?,
            ))
        })
        .await?;
    let base_missing = crate::subscribers::link_base(site.base_url.as_deref(), &page).is_none();
    let channels = channels
        .into_iter()
        .map(|(cid, name)| (cid, name, settings.channel_id == Some(cid)))
        .collect();
    let tz = super::public_status::site_tz(&site.timezone);
    let rows = list
        .into_iter()
        .map(|s| SubscriberRow {
            kind_label: if s.kind == "email" {
                crate::i18n::t("sub.type_email")
            } else {
                crate::i18n::t("sub.type_webhook")
            },
            confirmed: s.is_confirmed(),
            awaiting: s.awaits_approval(),
            created: super::public_status::fmt_local(&s.created_at, &tz),
            id: s.id,
            target: s.target,
        })
        .collect();
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        format!("{} · {}", crate::i18n::t("sub.tab"), page.title),
        "status-pages",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(SubscribersTemplate {
            ctx,
            page,
            tab: "subscribers",
            settings,
            channels,
            rows,
            base_missing,
            is_admin: user.is_admin(),
        }),
    ))
}

#[derive(Debug, Deserialize)]
struct SubscribersInput {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    webhooks: bool,
}

async fn save_subscribers(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(input): Form<SubscribersInput>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    load_page(&state, id).await?;
    let settings = db_subscribers::Settings {
        enabled: input.enabled,
        channel_id: input.channel_id.trim().parse().ok(),
        webhooks: input.webhooks,
    };
    // Only an email channel can send the mails.
    let valid = match settings.channel_id {
        Some(cid) => state
            .with_conn(move |c| crate::db::notifications::get_channel(c, cid))
            .await?
            .is_some_and(|ch| ch.kind == "email"),
        None => true,
    };
    let back = format!("/status-pages/{id}/subscribers");
    if !valid {
        let jar = flash(jar, "error", crate::i18n::t("sub.err_channel"));
        return Ok((jar, Redirect::to(&back)).into_response());
    }
    state
        .with_conn(move |c| db_subscribers::set_settings(c, id, &settings))
        .await?;
    state.notify_update();
    Ok((
        flash(jar, "success", crate::i18n::t("sub.saved")),
        Redirect::to(&back),
    )
        .into_response())
}

/// Approves a webhook sign-up; the webhook then gets a "subscribed" message
/// with its unsubscribe link.
async fn approve_subscriber(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, sid)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    if state
        .with_conn(move |c| db_subscribers::approve(c, id, sid))
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    let bg = state.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::subscribers::send_welcome(&bg, id, sid).await {
            tracing::warn!(page = id, subscriber = sid, error = %e, "webhook welcome failed");
        }
    });
    Ok(hx_refresh(flash(
        jar,
        "success",
        crate::i18n::t("sub.approved"),
    )))
}

async fn remove_subscriber(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path((id, sid)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    if !state
        .with_conn(move |c| db_subscribers::delete(c, id, sid))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(hx_refresh(flash(
        jar,
        "success",
        crate::i18n::t("sub.removed"),
    )))
}
