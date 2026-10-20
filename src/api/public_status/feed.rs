//! `GET /status/{slug}/feed.xml`: an Atom feed of a status page's incidents.
//!
//! One entry per incident (newest activity first); its content holds the
//! announcement and the whole update timeline, and `<updated>` moves whenever
//! an update is posted, so feed readers show the incident again. Published
//! pages only; a password-protected page answers 401 (feed readers can't
//! unlock it) and nothing about it leaks.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use jiff::Timestamp;

use super::{ApiError, ApiResult, AppState, markdown, resolve};
use crate::db::incidents::{self as db_incidents, Incident, IncidentUpdate};
use crate::db::status_pages::StatusPage;
use crate::maintenance::parse_db_ts;

/// Entries in the feed.
const ENTRIES: usize = 30;

pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Characters XML 1.0 can't carry at all.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// `2026-10-20 14:05:00` (UTC) → `2026-10-20T14:05:00Z`.
fn rfc3339(db: &str) -> String {
    parse_db_ts(db)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

/// The HTML of one entry: the announcement, then its updates, newest first.
fn entry_html(incident: &Incident, updates: &[IncidentUpdate]) -> String {
    let mut html = markdown::render(&incident.body);
    for u in updates {
        html.push_str(&format!(
            "<h3>{}</h3><p><small>{}</small></p>{}",
            xml_escape(u.status_label()),
            xml_escape(&format!("{} UTC", u.created_at)),
            markdown::render(&u.body)
        ));
    }
    html
}

/// The feed document. `base` is the public URL of this server.
pub fn build_feed(
    page: &StatusPage,
    base: &str,
    lang: &str,
    items: &[(Incident, Vec<IncidentUpdate>)],
    now: Timestamp,
) -> String {
    let page_url = format!("{base}/status/{}", page.slug);
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!("<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:lang=\"{}\">\n", xml_escape(lang)));
    out.push_str(&format!(
        "  <title>{}</title>\n",
        xml_escape(&crate::i18n::tf("pub.feed_title", &[("title", &page.title)]))
    ));
    out.push_str(&format!("  <id>{}/feed.xml</id>\n", xml_escape(&page_url)));
    out.push_str(&format!("  <link rel=\"self\" type=\"application/atom+xml\" href=\"{}/feed.xml\"/>\n", xml_escape(&page_url)));
    out.push_str(&format!("  <link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>\n", xml_escape(&page_url)));
    let latest = items
        .iter()
        .map(|(i, u)| u.iter().map(|u| u.created_at.as_str()).chain([i.updated_at.as_str(), i.created_at.as_str()]).max().unwrap_or(""))
        .max()
        .map(rfc3339)
        .unwrap_or_else(|| now.strftime("%Y-%m-%dT%H:%M:%SZ").to_string());
    out.push_str(&format!("  <updated>{latest}</updated>\n"));
    out.push_str("  <generator>Vigilant Vole</generator>\n");
    for (incident, updates) in items {
        let url = format!("{page_url}#incident-{}", incident.id);
        let newest = updates
            .iter()
            .map(|u| u.created_at.as_str())
            .chain([incident.updated_at.as_str(), incident.created_at.as_str()])
            .max()
            .unwrap_or(&incident.created_at);
        out.push_str("  <entry>\n");
        out.push_str(&format!("    <id>{}</id>\n", xml_escape(&url)));
        out.push_str(&format!("    <title>{}</title>\n", xml_escape(&incident.title)));
        out.push_str(&format!("    <link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>\n", xml_escape(&url)));
        out.push_str(&format!("    <published>{}</published>\n", rfc3339(&incident.created_at)));
        out.push_str(&format!("    <updated>{}</updated>\n", rfc3339(newest)));
        out.push_str(&format!("    <category term=\"{}\"/>\n", xml_escape(&incident.severity)));
        let state = updates.first().map(|u| u.status.as_str()).unwrap_or(if incident.is_resolved() { "resolved" } else { "investigating" });
        out.push_str(&format!("    <category term=\"{}\"/>\n", xml_escape(state)));
        out.push_str(&format!("    <content type=\"html\">{}</content>\n", xml_escape(&entry_html(incident, updates))));
        out.push_str("  </entry>\n");
    }
    out.push_str("</feed>\n");
    out
}

pub async fn feed(State(state): State<AppState>, jar: SignedCookieJar, headers: HeaderMap, Path(slug): Path<String>) -> ApiResult<Response> {
    let (view, access) = resolve(&state, slug, &jar).await?;
    // Only published pages have a feed; a preview of a draft stays in the browser.
    if !view.page.published {
        return Err(ApiError::NotFound);
    }
    if !access.allows(&view.page) {
        return Ok((StatusCode::UNAUTHORIZED, [(header::CACHE_CONTROL, "no-store")], "This status page is password protected.\n").into_response());
    }
    let page_id = view.page.id;
    let (items, settings) = state
        .with_conn(move |c| {
            let incidents = db_incidents::list_recent(c, page_id, ENTRIES)?;
            let ids: Vec<i64> = incidents.iter().map(|i| i.id).collect();
            let mut timelines = db_incidents::updates_for(c, &ids)?;
            let items = incidents.into_iter().map(|i| { let u = timelines.remove(&i.id).unwrap_or_default(); (i, u) }).collect::<Vec<_>>();
            Ok((items, crate::db::settings::load(c)?))
        })
        .await?;
    let base = crate::api::agents::public_base_url(&settings, &headers);
    let body = build_feed(&view.page, &base, crate::i18n::current().code(), &items, Timestamp::now());
    let mut resp = body.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/atom+xml; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escaping_covers_markup_and_control_characters() {
        assert_eq!(xml_escape("a<b>&\"'c\u{1}\u{7}d\te"), "a&lt;b&gt;&amp;&quot;&apos;cd\te");
    }

    #[test]
    fn db_times_become_rfc3339() {
        assert_eq!(rfc3339("2026-10-20 14:05:09"), "2026-10-20T14:05:09Z");
        assert_eq!(rfc3339("garbage"), "1970-01-01T00:00:00Z");
    }
}
