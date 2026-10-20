//! SVG badges. Owned by the statuspages agent.
//!
//! `GET /api/badge/{id}/{status|uptime|ping|cert-exp}` → shields.io-style
//! flat / flat-square SVG. Query: `style=flat|flat-square`, `label=`,
//! `duration=24h|7d|30d|90d` (uptime, ping), `upLabel=`, `downLabel=`,
//! `pendingLabel=`, `maintenanceLabel=`, `color=`, `labelColor=`,
//! `upColor=`, `downColor=` (hex without `#`, or a shields colour name).
//!
//! Only monitors shown on a published status page get badges, unless the
//! `badges_public` setting is on; everything else is a 404 (indistinguishable
//! from a missing monitor). Responses are cached for 60 s.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use super::error::escape_html;
use super::{ApiError, ApiResult, AppState};
use crate::db::models::HeartbeatStatus;
use crate::db::{rollups, settings as db_settings, status_pages as db_pages};
use crate::maintenance;

const CACHE_TTL: Duration = Duration::from_secs(60);
const CACHE_MAX: usize = 2000;

/// Unauthenticated routes.
pub fn public_router() -> Router<AppState> {
    Router::new().route("/api/badge/{id}/{kind}", get(badge))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeStyle {
    Flat,
    FlatSquare,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BadgeQuery {
    style: Option<String>,
    label: Option<String>,
    duration: Option<String>,
    up_label: Option<String>,
    down_label: Option<String>,
    pending_label: Option<String>,
    maintenance_label: Option<String>,
    color: Option<String>,
    label_color: Option<String>,
    up_color: Option<String>,
    down_color: Option<String>,
}

// ---------------------------------------------------------------------------
// SVG rendering (pure)
// ---------------------------------------------------------------------------

/// Approximate advance width of a character in 11px Verdana (what shields.io
/// measures with). Good enough to size the badge within a pixel or two.
pub fn char_width(c: char) -> f64 {
    match c {
        ' ' => 3.9,
        '!' | '|' | '\'' | 'i' | 'l' | 'j' | '.' | ',' | ':' | ';' => 3.5,
        'I' | 'f' | 't' | 'r' | '(' | ')' | '[' | ']' | '-' | '/' => 4.7,
        '0'..='9' | '$' | '#' | '_' | '+' | '=' | '<' | '>' | '~' | '^' | '*' => 7.0,
        'm' | 'w' => 9.6,
        'M' | 'W' => 10.6,
        '%' => 12.0,
        '@' => 11.0,
        'A'..='Z' => 7.6,
        'a'..='z' => 6.6,
        c if c.is_ascii() => 6.0,
        // Wide scripts (CJK etc.) are roughly square.
        c if (c as u32) >= 0x2E80 => 11.0,
        _ => 7.2,
    }
}

/// Rendered width of `text` in pixels (rounded up).
pub fn text_width(text: &str) -> u32 {
    text.chars().map(char_width).sum::<f64>().ceil() as u32
}

/// A shields colour name or 3/6-digit hex (without `#`) → `#rrggbb`-style
/// fill. `None` for anything else (so user input never reaches the SVG raw).
pub fn parse_color(input: &str) -> Option<String> {
    let s = input.trim().trim_start_matches('#').to_ascii_lowercase();
    let named = match s.as_str() {
        "brightgreen" | "success" => Some("#4c1"),
        "green" => Some("#97ca00"),
        "yellowgreen" => Some("#a4a61d"),
        "yellow" => Some("#dfb317"),
        "orange" | "important" => Some("#fe7d37"),
        "red" | "critical" => Some("#e05d44"),
        "blue" | "informational" => Some("#007ec6"),
        "lightgrey" | "lightgray" | "inactive" => Some("#9f9f9f"),
        "grey" | "gray" => Some("#555"),
        "blueviolet" => Some("#8a2be2"),
        _ => None,
    };
    if let Some(n) = named {
        return Some(n.to_string());
    }
    let hex = (s.len() == 3 || s.len() == 6) && s.bytes().all(|b| b.is_ascii_hexdigit());
    hex.then(|| format!("#{s}"))
}

/// Renders a two-part badge. `label` may be empty (message-only badge).
pub fn render_svg(label: &str, message: &str, label_color: &str, color: &str, style: BadgeStyle) -> String {
    let pad = 10;
    let label_w = if label.is_empty() { 0 } else { text_width(label) + pad };
    let msg_w = text_width(message) + pad;
    let total = label_w + msg_w;
    let title = if label.is_empty() {
        escape_html(message)
    } else {
        format!("{}: {}", escape_html(label), escape_html(message))
    };
    let (rx, gradient) = match style {
        BadgeStyle::Flat => (
            3,
            "<linearGradient id=\"s\" x2=\"0\" y2=\"100%\"><stop offset=\"0\" stop-color=\"#bbb\" stop-opacity=\".1\"/><stop offset=\"1\" stop-opacity=\".1\"/></linearGradient>",
        ),
        BadgeStyle::FlatSquare => (0, ""),
    };
    let mut svg = String::with_capacity(1024);
    let _ = write!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{total}\" height=\"20\" role=\"img\" aria-label=\"{title}\"><title>{title}</title>{gradient}<clipPath id=\"r\"><rect width=\"{total}\" height=\"20\" rx=\"{rx}\" fill=\"#fff\"/></clipPath><g clip-path=\"url(#r)\">"
    );
    if label_w > 0 {
        let _ = write!(svg, "<rect width=\"{label_w}\" height=\"20\" fill=\"{label_color}\"/>");
    }
    let _ = write!(svg, "<rect x=\"{label_w}\" width=\"{msg_w}\" height=\"20\" fill=\"{color}\"/>");
    if style == BadgeStyle::Flat {
        let _ = write!(svg, "<rect width=\"{total}\" height=\"20\" fill=\"url(#s)\"/>");
    }
    svg.push_str("</g><g fill=\"#fff\" text-anchor=\"middle\" font-family=\"Verdana,Geneva,DejaVu Sans,sans-serif\" text-rendering=\"geometricPrecision\" font-size=\"110\">");
    let mut text = |x_center: u32, width: u32, s: &str| {
        let len = (width - pad) * 10;
        let x = x_center * 10;
        let s = escape_html(s);
        if style == BadgeStyle::Flat {
            let _ = write!(
                svg,
                "<text aria-hidden=\"true\" x=\"{x}\" y=\"150\" fill=\"#010101\" fill-opacity=\".3\" transform=\"scale(.1)\" textLength=\"{len}\">{s}</text>"
            );
        }
        let _ = write!(svg, "<text x=\"{x}\" y=\"140\" transform=\"scale(.1)\" fill=\"#fff\" textLength=\"{len}\">{s}</text>");
    };
    if label_w > 0 {
        text(label_w / 2, label_w, label);
    }
    text(label_w + msg_w / 2, msg_w, message);
    svg.push_str("</g></svg>");
    svg
}

fn uptime_color(pct: f64) -> &'static str {
    match pct {
        p if p >= 99.9 => "#4c1",
        p if p >= 99.0 => "#97ca00",
        p if p >= 97.0 => "#a4a61d",
        p if p >= 95.0 => "#dfb317",
        p if p >= 90.0 => "#fe7d37",
        _ => "#e05d44",
    }
}

/// `24h` → 1 day, `7d`, `30d`, `90d` (anything else: `None`).
pub fn parse_duration_days(s: &str) -> Option<(i64, &'static str)> {
    Some(match s {
        "24h" | "1d" => (1, "24h"),
        "7d" => (7, "7d"),
        "30d" => (30, "30d"),
        "90d" => (90, "90d"),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

struct BadgeData {
    label: String,
    message: String,
    color: String,
}

fn label_or(q: &BadgeQuery, default: String) -> String {
    match &q.label {
        Some(l) => l.chars().take(64).collect(),
        None => default,
    }
}

fn custom(s: &Option<String>) -> Option<String> {
    s.as_ref().map(|v| v.chars().take(64).collect())
}

fn avg_response_ms(conn: &Connection, id: i64, days: i64) -> rusqlite::Result<Option<f64>> {
    if days <= 1 {
        return conn.query_row(
            "SELECT AVG(response_time_ms) FROM heartbeats
             WHERE monitor_id = ?1 AND agent_id IS NULL AND status = 'up' AND checked_at >= datetime('now', '-1 day')",
            params![id],
            |r| r.get(0),
        );
    }
    let stats = rollups::daily_uptime(conn, id, days)?;
    let (mut sum, mut n) = (0.0, 0i64);
    for d in stats {
        if let Some(avg) = d.avg_ms {
            let w = d.total().max(1);
            sum += avg * w as f64;
            n += w;
        }
    }
    Ok((n > 0).then(|| sum / n as f64))
}

fn load(conn: &Connection, id: i64, kind: &str, q: &BadgeQuery) -> rusqlite::Result<Option<BadgeData>> {
    let row: Option<(String, bool)> = conn
        .query_row("SELECT name, active FROM monitors WHERE id = ?1", params![id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((_name, active)) = row else {
        return Ok(None);
    };
    if !db_settings::load(conn)?.badges_public && !db_pages::monitor_is_public(conn, id)? {
        return Ok(None);
    }
    let (days, dur_label) = q.duration.as_deref().and_then(parse_duration_days).unwrap_or((1, "24h"));
    let color_override = q.color.as_deref().and_then(parse_color);

    let data = match kind {
        "status" => {
            let last: Option<String> = conn
                .query_row(
                    "SELECT status FROM heartbeats WHERE monitor_id = ?1 AND agent_id IS NULL
                     ORDER BY checked_at DESC, id DESC LIMIT 1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            let status = last.as_deref().and_then(HeartbeatStatus::parse);
            let in_maint = maintenance::is_under_maintenance(conn, id)?;
            let (message, color) = if in_maint || status == Some(HeartbeatStatus::Maintenance) {
                (custom(&q.maintenance_label).unwrap_or_else(|| "maintenance".into()), "#007ec6".to_string())
            } else if !active {
                ("paused".to_string(), "#9f9f9f".to_string())
            } else {
                match status {
                    Some(HeartbeatStatus::Up) => (
                        custom(&q.up_label).unwrap_or_else(|| "up".into()),
                        q.up_color.as_deref().and_then(parse_color).unwrap_or_else(|| "#4c1".into()),
                    ),
                    Some(HeartbeatStatus::Down) => (
                        custom(&q.down_label).unwrap_or_else(|| "down".into()),
                        q.down_color.as_deref().and_then(parse_color).unwrap_or_else(|| "#e05d44".into()),
                    ),
                    Some(HeartbeatStatus::Pending) => {
                        (custom(&q.pending_label).unwrap_or_else(|| "pending".into()), "#dfb317".to_string())
                    }
                    _ => ("unknown".to_string(), "#9f9f9f".to_string()),
                }
            };
            BadgeData {
                label: label_or(q, "status".into()),
                message,
                color,
            }
        }
        "uptime" => {
            let pct = rollups::uptime_pct(conn, id, days)?;
            let (message, color) = match pct {
                Some(p) => (super::public_status::fmt_pct(p), uptime_color(p).to_string()),
                None => ("no data".to_string(), "#9f9f9f".to_string()),
            };
            BadgeData {
                label: label_or(q, format!("uptime ({dur_label})")),
                message,
                color,
            }
        }
        "ping" => {
            let avg = avg_response_ms(conn, id, days)?;
            let (message, color) = match avg {
                Some(ms) => (format!("{} ms", ms.round() as i64), "#007ec6".to_string()),
                None => ("no data".to_string(), "#9f9f9f".to_string()),
            };
            BadgeData {
                label: label_or(q, format!("response ({dur_label})")),
                message,
                color,
            }
        }
        "cert-exp" => {
            let expires: Option<String> = conn
                .query_row(
                    "SELECT expires_at FROM monitor_expiry WHERE monitor_id = ?1 AND kind = 'cert'",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            let days_left = expires
                .as_deref()
                .and_then(maintenance::parse_db_ts)
                .map(|t| jiff::Timestamp::now().duration_until(t).as_secs().div_euclid(86_400));
            let (message, color) = match days_left {
                None => ("no data".to_string(), "#9f9f9f".to_string()),
                Some(d) if d < 0 => ("expired".to_string(), "#e05d44".to_string()),
                Some(d) => (
                    if d == 1 { "1 day".to_string() } else { format!("{d} days") },
                    match d {
                        0..=6 => "#e05d44",
                        7..=13 => "#fe7d37",
                        14..=29 => "#dfb317",
                        _ => "#4c1",
                    }
                    .to_string(),
                ),
            };
            BadgeData {
                label: label_or(q, "cert expires".into()),
                message,
                color,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(BadgeData {
        color: color_override.unwrap_or(data.color),
        ..data
    }))
}

// ---------------------------------------------------------------------------
// Handler + cache
// ---------------------------------------------------------------------------

/// Longer query strings are served but never cached.
const CACHEABLE_QUERY_MAX: usize = 512;

/// The query reduced to what affects the badge: labels cut to 64 characters,
/// colours and durations parsed (unknown ones dropped), so junk parameters or
/// padding can't create new cache entries.
fn normalized(q: &BadgeQuery) -> BadgeQuery {
    let color = |c: &Option<String>| c.as_deref().and_then(parse_color);
    BadgeQuery {
        style: Some(if q.style.as_deref() == Some("flat-square") { "flat-square" } else { "flat" }.into()),
        label: custom(&q.label),
        duration: q.duration.as_deref().and_then(parse_duration_days).map(|(_, l)| l.to_string()),
        up_label: custom(&q.up_label),
        down_label: custom(&q.down_label),
        pending_label: custom(&q.pending_label),
        maintenance_label: custom(&q.maintenance_label),
        color: color(&q.color),
        label_color: color(&q.label_color),
        up_color: color(&q.up_color),
        down_color: color(&q.down_color),
    }
}

/// (app data dir, monitor id, kind, normalized query) → (rendered at, SVG).
type CacheKey = (std::path::PathBuf, i64, String, BadgeQuery);
type BadgeCache = HashMap<CacheKey, (Instant, String)>;

static CACHE: LazyLock<Mutex<BadgeCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Forget rendered badges (a page was unpublished or got a password, so a
/// monitor may no longer be public).
pub fn invalidate_cache() {
    if let Ok(mut c) = CACHE.lock() {
        c.clear();
    }
}

fn svg_response(svg: String) -> Response {
    let mut resp = ([(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")], svg).into_response();
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    resp
}

async fn badge(
    State(state): State<AppState>,
    Path((id, kind)): Path<(i64, String)>,
    Query(q): Query<BadgeQuery>,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    if !matches!(kind.as_str(), "status" | "uptime" | "ping" | "cert-exp") {
        return Err(ApiError::NotFound);
    }
    let q = normalized(&q);
    let cacheable = uri.query().is_none_or(|s| s.len() <= CACHEABLE_QUERY_MAX);
    let key: Option<CacheKey> = cacheable.then(|| (state.config.data_dir.clone(), id, kind.clone(), q.clone()));
    if let Some(key) = &key
        && let Ok(cache) = CACHE.lock()
        && let Some((at, svg)) = cache.get(key)
        && at.elapsed() < CACHE_TTL
    {
        return Ok(svg_response(svg.clone()));
    }
    let style = match q.style.as_deref() {
        Some("flat-square") => BadgeStyle::FlatSquare,
        _ => BadgeStyle::Flat,
    };
    let label_color = q
        .label_color
        .as_deref()
        .and_then(parse_color)
        .unwrap_or_else(|| "#555".into());
    let data = state
        .with_conn(move |c| load(c, id, &kind, &q))
        .await?
        .ok_or(ApiError::NotFound)?;
    let svg = render_svg(&data.label, &data.message, &label_color, &data.color, style);
    if let Some(key) = key
        && let Ok(mut cache) = CACHE.lock()
    {
        if cache.len() >= CACHE_MAX {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            if cache.len() >= CACHE_MAX {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), svg.clone()));
    }
    Ok(svg_response(svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_width_grows_with_text_and_respects_narrow_glyphs() {
        assert_eq!(text_width(""), 0);
        assert!(text_width("status") < text_width("statuses"));
        assert!(text_width("iiii") < text_width("mmmm"));
        // "up" = u(6.6) + p(6.6) = 13.2 → 14
        assert_eq!(text_width("up"), 14);
        // "99.95%" = 4 digits * 7 + '.' 3.5 + '%' 12 = 43.5 → 44
        assert_eq!(text_width("99.95%"), 44);
    }

    #[test]
    fn badge_width_is_label_plus_message_plus_padding() {
        let svg = render_svg("status", "up", "#555", "#4c1", BadgeStyle::Flat);
        let label_w = text_width("status") + 10;
        let msg_w = text_width("up") + 10;
        assert!(svg.starts_with(&format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\"", label_w + msg_w)));
        assert!(svg.contains(&format!("<rect x=\"{label_w}\" width=\"{msg_w}\"")));
        assert!(svg.contains("rx=\"3\""));
        assert!(svg.contains("linearGradient"));
    }

    #[test]
    fn flat_square_has_no_rounding_or_gradient() {
        let svg = render_svg("a", "b", "#555", "#4c1", BadgeStyle::FlatSquare);
        assert!(svg.contains("rx=\"0\""));
        assert!(!svg.contains("linearGradient"));
        assert!(!svg.contains("fill-opacity"));
    }

    #[test]
    fn message_only_badge() {
        let svg = render_svg("", "up", "#555", "#4c1", BadgeStyle::Flat);
        assert!(svg.contains(&format!("width=\"{}\"", text_width("up") + 10)));
        assert!(svg.contains("<title>up</title>"));
    }

    #[test]
    fn text_is_escaped() {
        let svg = render_svg("<script>", "a\"b&c", "#555", "#4c1", BadgeStyle::Flat);
        assert!(!svg.contains("<script>"));
        assert!(svg.contains("&lt;script&gt;"));
        assert!(svg.contains("a&quot;b&amp;c"));
    }

    #[test]
    fn colors_are_whitelisted() {
        assert_eq!(parse_color("brightgreen").as_deref(), Some("#4c1"));
        assert_eq!(parse_color("FF0000").as_deref(), Some("#ff0000"));
        assert_eq!(parse_color("#abc").as_deref(), Some("#abc"));
        assert_eq!(parse_color("abcd"), None);
        assert_eq!(parse_color("red\" onload=\"x"), None);
        assert_eq!(parse_color("url(#x)"), None);
    }

    #[test]
    fn cache_key_ignores_junk_and_long_labels() {
        let a = BadgeQuery { label: Some("x".repeat(64)), color: Some("FF0000".into()), ..Default::default() };
        let b = BadgeQuery {
            label: Some("x".repeat(10_000)),
            color: Some("ff0000".into()),
            label_color: Some("not-a-colour".into()),
            duration: Some("1y".into()),
            style: Some("bogus".into()),
            ..Default::default()
        };
        assert_eq!(normalized(&a), normalized(&b));
        assert_eq!(normalized(&b).label.unwrap().chars().count(), 64);
        let c = BadgeQuery { duration: Some("7d".into()), ..Default::default() };
        assert_ne!(normalized(&a), normalized(&c));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_days("24h"), Some((1, "24h")));
        assert_eq!(parse_duration_days("90d"), Some((90, "90d")));
        assert_eq!(parse_duration_days("1y"), None);
        assert_eq!(uptime_color(100.0), "#4c1");
        assert_eq!(uptime_color(50.0), "#e05d44");
    }
}
