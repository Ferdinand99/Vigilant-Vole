//! General settings (admin). The personal Security page is the kit's
//! ([`crate::kit::auth::pages`]).
//!
//! Routes:
//! - `GET  /settings` → `/settings/general` (admins) or `/settings/security`
//! - `GET|POST /settings/general`

use crate::i18n::Tr as _;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use super::{ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::CurrentUser;
use crate::db::settings as db_settings;
use crate::db::users as db_users;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings", get(settings_home))
        .route("/settings/general", get(general_page).post(general_save))
}

async fn settings_home(user: CurrentUser) -> Redirect {
    if user.is_admin() {
        Redirect::to("/settings/general")
    } else {
        Redirect::to("/settings/security")
    }
}

// Time zones (also used by the setup wizard) are the kit's.
pub use crate::kit::tz::{timezones, valid_timezone};

/// Validates and normalises a public base URL (the kit's).
pub use crate::kit::settings::normalize_base_url;

// ---------------------------------------------------------------------------
// General (admin)
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings_general.html")]
struct GeneralTemplate {
    ctx: PageCtx,
    site_name: String,
    base_url: String,
    timezone: String,
    language: String,
    retention_days: String,
    badges_public: bool,
    agent_confirmation_default: bool,
    timezones: Vec<String>,
    languages: Vec<(&'static str, String)>,
    db_size: String,
    heartbeat_count: String,
    site_name_error: String,
    base_url_error: String,
    timezone_error: String,
    retention_error: String,
}

impl GeneralTemplate {
    fn is_language(&self, code: &str) -> bool {
        self.language == code
    }

    fn has_errors(&self) -> bool {
        [
            &self.site_name_error,
            &self.base_url_error,
            &self.timezone_error,
            &self.retention_error,
        ]
        .iter()
        .any(|e| !e.is_empty())
    }
}

#[derive(Deserialize)]
pub struct GeneralForm {
    #[serde(default)]
    site_name: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    timezone: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    retention_days: String,
    #[serde(default)]
    badges_public: bool,
    #[serde(default)]
    agent_confirmation_default: bool,
}

/// Database file size and heartbeat count for the retention help text.
async fn db_stats(state: &AppState) -> ApiResult<(String, String)> {
    let (bytes, beats) = state
        .with_conn(|c| {
            let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let beats: i64 = c.query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get(0))?;
            Ok((pages.saturating_mul(size), beats))
        })
        .await?;
    Ok((format_bytes(bytes), format_count(beats)))
}

pub fn format_bytes(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} {}", crate::i18n::t("general.bytes"));
    }
    let mut value = bytes as f64;
    let mut unit = crate::i18n::t("general.bytes");
    for u in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = u;
    }
    let decimals = if value < 10.0 { 1 } else { 0 };
    format!("{} {unit}", crate::i18n::fmt_dec(value, decimals))
}

/// 1234567 → "1,234,567" ("1 234 567" in Bokmål).
pub fn format_count(n: i64) -> String {
    crate::i18n::fmt_int(n)
}

async fn general_template(
    state: &AppState,
    ctx: PageCtx,
    s: &db_settings::Settings,
) -> ApiResult<GeneralTemplate> {
    let (db_size, heartbeat_count) = db_stats(state).await?;
    Ok(GeneralTemplate {
        ctx,
        site_name: s.site_name.clone(),
        base_url: s.base_url.clone().unwrap_or_default(),
        timezone: s.timezone.clone(),
        language: s.language.clone(),
        retention_days: s.retention_days.to_string(),
        badges_public: s.badges_public,
        agent_confirmation_default: s.agent_confirmation_default,
        timezones: timezones(),
        languages: crate::i18n::language_choices(),
        db_size,
        heartbeat_count,
        site_name_error: String::new(),
        base_url_error: String::new(),
        timezone_error: String::new(),
        retention_error: String::new(),
    })
}

async fn general_page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    user.require_admin()?;
    let settings = state.settings().await?;
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("nav.general"),
        "settings",
        "general",
    )
    .await;
    let page = general_template(&state, ctx, &settings).await?;
    Ok((jar, HtmlTemplate(page)).into_response())
}

async fn general_save(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<GeneralForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let current = state.settings().await?;
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("nav.general"),
        "settings",
        "general",
    )
    .await;
    let mut page = general_template(&state, ctx, &current).await?;

    let site_name = form.site_name.trim().to_string();
    page.site_name = site_name.clone();
    if site_name.chars().count() > 80 {
        page.site_name_error = crate::i18n::t("general.err_site_name").into();
    }
    page.base_url = form.base_url.trim().to_string();
    let base_url = match normalize_base_url(&form.base_url) {
        Ok(url) => url,
        Err(msg) => {
            page.base_url_error = msg.into();
            None
        }
    };
    let timezone = form.timezone.trim().to_string();
    page.timezone = timezone.clone();
    if !valid_timezone(&timezone) {
        page.timezone_error = crate::i18n::t("general.err_timezone").into();
    }
    let language = if crate::i18n::Lang::from_exact_code(&form.language).is_some() {
        form.language.clone()
    } else {
        "en".to_string()
    };
    page.language = language.clone();
    page.retention_days = form.retention_days.trim().to_string();
    let retention = form
        .retention_days
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|d| (1..=3650).contains(d));
    if retention.is_none() {
        page.retention_error = crate::i18n::t("general.err_retention").into();
    }
    page.badges_public = form.badges_public;
    page.agent_confirmation_default = form.agent_confirmation_default;
    if page.has_errors() {
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, HtmlTemplate(page)).into_response());
    }

    let site_name = if site_name.is_empty() {
        db_settings::Settings::default().site_name
    } else {
        site_name
    };
    let retention = retention.unwrap_or(current.retention_days);
    let badges = form.badges_public;
    let confirm_default = form.agent_confirmation_default;
    let new_lang = crate::i18n::Lang::from_code(&language).unwrap_or_default();
    let uid = user.id;
    let own_lang = state
        .with_conn(move |c| {
            let tx = c.transaction()?;
            db_settings::set(&tx, db_settings::SITE_NAME, &site_name)?;
            db_settings::set_opt(&tx, db_settings::BASE_URL, base_url.as_deref())?;
            db_settings::set(&tx, db_settings::TIMEZONE, &timezone)?;
            db_settings::set(&tx, db_settings::LANGUAGE, &language)?;
            db_settings::set(&tx, db_settings::RETENTION_DAYS, &retention.to_string())?;
            db_settings::set_bool(&tx, db_settings::BADGES_PUBLIC, badges)?;
            db_settings::set_bool(
                &tx,
                db_settings::AGENT_CONFIRMATION_DEFAULT,
                confirm_default,
            )?;
            tx.commit()?;
            Ok(db_users::get_user(c, uid)?.and_then(|u| u.language))
        })
        .await?;
    tracing::info!(user = %user.username, "general settings saved");
    // Public pages show a subscribe link only with a public URL.
    super::public_status::invalidate_views(&state);
    // Confirm in the language the next page will use.
    let lang = own_lang
        .as_deref()
        .and_then(crate::i18n::Lang::from_code)
        .unwrap_or(new_lang);
    let jar = flash(jar, "success", crate::i18n::t_in(lang, "general.saved"));
    Ok((jar, Redirect::to("/settings/general")).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_normalised() {
        assert_eq!(
            normalize_base_url(" https://up.example.com/ "),
            Ok(Some("https://up.example.com".into()))
        );
        assert_eq!(
            normalize_base_url("http://10.0.0.5:3001/vole/"),
            Ok(Some("http://10.0.0.5:3001/vole".into()))
        );
        assert_eq!(normalize_base_url(""), Ok(None));
    }

    #[test]
    fn base_url_rejects_bad_input() {
        for bad in [
            "up.example.com",
            "ftp://x.example",
            "https://u:p@x.example",
            "https://x.example/?a=1",
            "javascript:alert(1)",
        ] {
            assert!(normalize_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bytes_are_human_readable() {
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(23_400_000), "23 MB");
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_count(999), "999");
    }
}
