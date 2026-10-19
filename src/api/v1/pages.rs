//! `/api/v1/status-pages/**` (pages, section layout, incidents).

use std::collections::HashMap;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, put};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Body, Id, V1Error, V1Result, created, json_ok, no_content};
use crate::api::AppState;
use crate::auth::{CurrentUser, Role};
use crate::config_io::{self, StatusPageDoc};
use crate::db::incidents::{self as db_incidents, NewIncident};
use crate::db::status_pages::{self as db_pages, StatusPage};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status-pages", get(list).post(create))
        .route("/status-pages/{id}", get(show).put(update).patch(update).delete(remove))
        .route("/status-pages/{id}/sections", put(set_sections))
        .route("/status-pages/{id}/incidents", get(list_incidents).post(create_incident))
        .route(
            "/status-pages/{id}/incidents/{iid}",
            get(show_incident).put(update_incident).patch(update_incident).delete(delete_incident),
        )
        .layer(axum::middleware::from_fn(crate::api::public_status::invalidate_after_writes))
}

fn page_json(conn: &Connection, p: &StatusPage) -> rusqlite::Result<Value> {
    let sections: Vec<Value> = db_pages::sections(conn, p.id)?
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "monitors": s.monitors.iter().map(|m| json!({"id": m.monitor_id, "name": m.name})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut v = serde_json::to_value(p).unwrap_or_else(|_| json!({}));
    v["sections"] = json!(sections);
    v["public_url"] = json!(format!("/status/{}", p.slug));
    Ok(v)
}

async fn load(state: &AppState, id: i64) -> V1Result<Value> {
    state
        .with_conn(move |c| match db_pages::get(c, id)? {
            Some(p) => page_json(c, &p).map(Some),
            None => Ok(None),
        })
        .await?
        .ok_or_else(V1Error::not_found)
}

async fn list(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap) -> V1Result<Response> {
    let data = state
        .with_conn(|c| {
            let mut out = Vec::new();
            for s in db_pages::list(c)? {
                let mut v = serde_json::to_value(&s.page).unwrap_or_else(|_| json!({}));
                v["monitor_count"] = json!(s.monitor_count);
                v["public_url"] = json!(format!("/status/{}", s.page.slug));
                out.push(v);
            }
            Ok(out)
        })
        .await?;
    Ok(json_ok(&headers, &json!({ "data": data, "next_cursor": null })))
}

async fn show(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap, Id(id): Id<i64>) -> V1Result<Response> {
    Ok(json_ok(&headers, &load(&state, id).await?))
}

/// Settings in a request; missing fields keep the current value (defaults on create).
#[derive(Debug, Default, Deserialize)]
struct PageIn {
    slug: Option<String>,
    title: Option<String>,
    description: Option<String>,
    custom_domain: Option<String>,
    published: Option<bool>,
    theme: Option<String>,
    show_tags: Option<bool>,
    show_uptime: Option<bool>,
    show_response: Option<bool>,
    show_powered_by: Option<bool>,
    footer_text: Option<String>,
    custom_css: Option<String>,
    refresh_seconds: Option<i64>,
    /// Optional layout: `[{"name": "API", "monitor_ids": [1, 2]}]`.
    sections: Option<Vec<SectionIn>>,
}

#[derive(Debug, Clone, Deserialize)]
struct SectionIn {
    name: String,
    #[serde(default)]
    monitor_ids: Vec<i64>,
}

fn merge(input: &PageIn, base: StatusPageDoc) -> StatusPageDoc {
    StatusPageDoc {
        slug: input.slug.clone().unwrap_or(base.slug),
        title: input.title.clone().unwrap_or(base.title),
        description: input.description.clone().unwrap_or(base.description),
        custom_domain: match &input.custom_domain {
            Some(d) => Some(d.clone()),
            None => base.custom_domain,
        },
        published: input.published.unwrap_or(base.published),
        theme: input.theme.clone().unwrap_or(base.theme),
        show_tags: input.show_tags.unwrap_or(base.show_tags),
        show_uptime: input.show_uptime.unwrap_or(base.show_uptime),
        show_response: input.show_response.unwrap_or(base.show_response),
        show_powered_by: input.show_powered_by.unwrap_or(base.show_powered_by),
        footer_text: input.footer_text.clone().or(base.footer_text),
        custom_css: input.custom_css.clone().or(base.custom_css),
        refresh_seconds: input.refresh_seconds.unwrap_or(base.refresh_seconds),
        sections: None,
    }
}

fn doc_of(p: &StatusPage) -> StatusPageDoc {
    StatusPageDoc {
        slug: p.slug.clone(),
        title: p.title.clone(),
        description: p.description.clone(),
        custom_domain: p.custom_domain.clone(),
        published: p.published,
        theme: p.theme.clone(),
        show_tags: p.show_tags,
        show_uptime: p.show_uptime,
        show_response: p.show_response,
        show_powered_by: p.show_powered_by,
        footer_text: p.footer_text.clone(),
        custom_css: p.custom_css.clone(),
        refresh_seconds: p.refresh_seconds,
        sections: None,
    }
}

fn sections_of(list: &[SectionIn]) -> V1Result<Vec<(String, Vec<i64>)>> {
    list.iter()
        .map(|s| {
            let name = s.name.trim().to_string();
            if name.chars().count() > 80 {
                return Err(V1Error::field("sections", "Keep section names to 80 characters or fewer."));
            }
            let mut ids = s.monitor_ids.clone();
            let mut seen = std::collections::HashSet::new();
            ids.retain(|id| seen.insert(*id));
            Ok((name, ids))
        })
        .collect()
}

/// Checks uniqueness and monitor ids, then writes. `id = None` creates.
fn save(
    conn: &mut Connection,
    id: Option<i64>,
    page: &db_pages::NewStatusPage,
    sections: Option<&[(String, Vec<i64>)]>,
    hosts: &[String],
) -> rusqlite::Result<Result<i64, V1Error>> {
    if db_pages::slug_taken(conn, &page.slug, id)? {
        return Ok(Err(V1Error::conflict("Another page already uses this slug.")));
    }
    if let Some(d) = &page.custom_domain {
        let base_url = crate::db::settings::load(conn)?.base_url;
        let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
        if db_pages::is_own_host(d, base_url.as_deref(), &hosts) {
            return Ok(Err(V1Error::field("custom_domain", crate::i18n::t("sp.err_domain_self"))));
        }
    }
    if let Some(d) = &page.custom_domain
        && db_pages::domain_taken(conn, d, id)?
    {
        return Ok(Err(V1Error::conflict("Another page already uses this domain.")));
    }
    if let Some(list) = sections {
        let all: Vec<i64> = list.iter().flat_map(|(_, ids)| ids.iter().copied()).collect();
        let existing = crate::db::monitors::existing_ids(conn, &all)?;
        if let Some(bad) = all.iter().find(|id| !existing.contains(id)) {
            return Ok(Err(V1Error::field("sections", format!("No monitor with id {bad}."))));
        }
    }
    let tx = conn.transaction()?;
    let id = match id {
        Some(id) => {
            db_pages::update(&tx, id, page)?;
            id
        }
        None => {
            tx.execute(
                "INSERT INTO status_pages (slug, title, description, custom_domain, published, theme, show_tags, show_uptime,
                     show_response, show_powered_by, footer_text, custom_css, refresh_seconds)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    page.slug,
                    page.title,
                    page.description,
                    page.custom_domain,
                    page.published,
                    page.theme,
                    page.show_tags,
                    page.show_uptime,
                    page.show_response,
                    page.show_powered_by,
                    page.footer_text,
                    page.custom_css,
                    page.refresh_seconds
                ],
            )?;
            let id = tx.last_insert_rowid();
            if sections.is_none() {
                config_io::write_sections(&tx, id, &[("Services".into(), Vec::new())], &HashMap::new())?;
            }
            id
        }
    };
    if let Some(list) = sections {
        config_io::write_sections(&tx, id, list, &HashMap::new())?;
    }
    tx.commit()?;
    Ok(Ok(id))
}

fn changed(state: &AppState) {
    crate::api::public_status::invalidate_domains();
    crate::maintenance::invalidate();
    state.notify_update();
}

/// Every `Host` the request arrived under (`Host`, `X-Forwarded-Host`).
fn request_hosts(headers: &HeaderMap) -> Vec<String> {
    ["host", "x-forwarded-host"]
        .iter()
        .filter_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()))
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

async fn create(State(state): State<AppState>, user: CurrentUser, headers: HeaderMap, Body(input): Body<PageIn>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let mut base = doc_of(&StatusPage {
        id: 0,
        slug: String::new(),
        title: String::new(),
        description: String::new(),
        custom_domain: None,
        published: true,
        theme: "auto".into(),
        show_tags: false,
        show_uptime: true,
        show_response: true,
        show_powered_by: true,
        footer_text: None,
        custom_css: None,
        refresh_seconds: 60,
        created_at: String::new(),
        updated_at: String::new(),
    });
    if input.slug.as_deref().is_none_or(|s| s.trim().is_empty()) {
        base.slug = db_pages::slugify(input.title.as_deref().unwrap_or_default());
    }
    let mut doc = merge(&input, base);
    if doc.slug.trim().is_empty() {
        doc.slug = db_pages::slugify(&doc.title);
    }
    let page = config_io::validate_page(&doc).map_err(V1Error::fields)?;
    let sections = input.sections.as_deref().map(sections_of).transpose()?;
    let hosts = request_hosts(&headers);
    let id = state.with_conn(move |c| save(c, None, &page, sections.as_deref(), &hosts)).await??;
    changed(&state);
    tracing::info!(user = %user.username, page_id = id, "status page created via API");
    Ok(created(format!("/api/v1/status-pages/{id}"), &load(&state, id).await?))
}

async fn update(State(state): State<AppState>, user: CurrentUser, headers: HeaderMap, Id(id): Id<i64>, Body(input): Body<PageIn>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let current = state.with_conn(move |c| db_pages::get(c, id)).await?.ok_or_else(V1Error::not_found)?;
    let doc = merge(&input, doc_of(&current));
    let page = config_io::validate_page(&doc).map_err(V1Error::fields)?;
    let sections = input.sections.as_deref().map(sections_of).transpose()?;
    let hosts = request_hosts(&headers);
    state.with_conn(move |c| save(c, Some(id), &page, sections.as_deref(), &hosts)).await??;
    changed(&state);
    Ok(json_ok(&HeaderMap::new(), &load(&state, id).await?))
}

async fn remove(State(state): State<AppState>, user: CurrentUser, Id(id): Id<i64>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    if !state.with_conn(move |c| db_pages::delete(c, id)).await? {
        return Err(V1Error::not_found());
    }
    changed(&state);
    Ok(no_content())
}

/// `PUT /status-pages/{id}/sections` — replace the layout.
async fn set_sections(State(state): State<AppState>, user: CurrentUser, Id(id): Id<i64>, Body(list): Body<Vec<SectionIn>>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let current = state.with_conn(move |c| db_pages::get(c, id)).await?.ok_or_else(V1Error::not_found)?;
    let page = current.to_new();
    let sections = sections_of(&list)?;
    state.with_conn(move |c| save(c, Some(id), &page, Some(&sections), &[])).await??;
    changed(&state);
    Ok(json_ok(&HeaderMap::new(), &load(&state, id).await?))
}

// ---------------------------------------------------------------------------
// Incidents
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct IncidentIn {
    title: Option<String>,
    body: Option<String>,
    /// info | warning | danger | success
    severity: Option<String>,
    pinned: Option<bool>,
    /// `true` resolves, `false` reopens.
    resolved: Option<bool>,
}

fn validate_incident(input: &IncidentIn, base: NewIncident) -> V1Result<NewIncident> {
    let title = input.title.clone().unwrap_or(base.title).trim().to_string();
    if title.is_empty() {
        return Err(V1Error::field("title", "Give the incident a title."));
    }
    if title.chars().count() > 200 {
        return Err(V1Error::field("title", "Keep the title to 200 characters or fewer."));
    }
    let body = input.body.clone().unwrap_or(base.body);
    if body.len() > 20_000 {
        return Err(V1Error::field("body", "Keep the update under 20,000 characters."));
    }
    let severity = input.severity.clone().unwrap_or(base.severity);
    if !db_incidents::SEVERITIES.contains(&severity.as_str()) {
        return Err(V1Error::field("severity", "Use info, warning, danger or success."));
    }
    Ok(NewIncident { title, body: body.trim().to_string(), severity, pinned: input.pinned.unwrap_or(base.pinned) })
}

async fn page_exists(state: &AppState, id: i64) -> V1Result<()> {
    state.with_conn(move |c| db_pages::get(c, id)).await?.map(|_| ()).ok_or_else(V1Error::not_found)
}

async fn list_incidents(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap, Id(id): Id<i64>) -> V1Result<Response> {
    page_exists(&state, id).await?;
    let data = state.with_conn(move |c| db_incidents::list_for_page(c, id)).await?;
    Ok(json_ok(&headers, &json!({ "data": data, "next_cursor": null })))
}

async fn show_incident(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap, Id((id, iid)): Id<(i64, i64)>) -> V1Result<Response> {
    let incident = state.with_conn(move |c| db_incidents::get(c, id, iid)).await?.ok_or_else(V1Error::not_found)?;
    Ok(json_ok(&headers, &incident))
}

async fn create_incident(State(state): State<AppState>, user: CurrentUser, Id(id): Id<i64>, Body(input): Body<IncidentIn>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    page_exists(&state, id).await?;
    let base = NewIncident { severity: "info".into(), pinned: true, ..NewIncident::default() };
    let new = validate_incident(&input, base)?;
    let resolved = input.resolved.unwrap_or(false);
    let incident = state
        .with_conn(move |c| {
            let iid = db_incidents::insert(c, id, &new)?;
            if resolved {
                db_incidents::set_resolved(c, id, iid, true)?;
            }
            db_incidents::get(c, id, iid)
        })
        .await?
        .ok_or_else(|| V1Error::internal("incident vanished after insert"))?;
    state.notify_update();
    Ok(created(format!("/api/v1/status-pages/{id}/incidents/{}", incident.id), &incident))
}

async fn update_incident(State(state): State<AppState>, user: CurrentUser, Id((id, iid)): Id<(i64, i64)>, Body(input): Body<IncidentIn>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let current = state.with_conn(move |c| db_incidents::get(c, id, iid)).await?.ok_or_else(V1Error::not_found)?;
    let base = NewIncident { title: current.title, body: current.body, severity: current.severity, pinned: current.pinned };
    let new = validate_incident(&input, base)?;
    let resolved = input.resolved;
    let incident = state
        .with_conn(move |c| {
            db_incidents::update(c, id, iid, &new)?;
            if let Some(r) = resolved {
                db_incidents::set_resolved(c, id, iid, r)?;
            }
            db_incidents::get(c, id, iid)
        })
        .await?
        .ok_or_else(V1Error::not_found)?;
    state.notify_update();
    Ok(json_ok(&HeaderMap::new(), &incident))
}

async fn delete_incident(State(state): State<AppState>, user: CurrentUser, Id((id, iid)): Id<(i64, i64)>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    if !state.with_conn(move |c| db_incidents::delete(c, id, iid)).await? {
        return Err(V1Error::not_found());
    }
    state.notify_update();
    Ok(no_content())
}
