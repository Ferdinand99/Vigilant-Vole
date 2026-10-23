//! The General settings page (admins), for a product that turns it on
//! ([`KitConfig::general`](crate::config::KitConfig::general)):
//! - `GET  /settings/general`: site name, public URL, time zone, language
//!   and the product's own fields
//! - `POST /settings/general`: validate and save; errors re-render the page
//!   with 422

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;

use crate::auth::CurrentUser;
use crate::db::{settings as db_settings, users as db_users};
use crate::http::{ApiResult, HtmlTemplate};
use crate::i18n::{Lang, Tr as _};
use crate::page::{PageCtx, flash};
use crate::settings::{FieldKind, GeneralSettings, SettingField, SiteSettings, normalize_base_url};
use crate::state::Core;

/// One product field as the template shows it.
struct FieldView {
    key: &'static str,
    label: &'static str,
    help: &'static str,
    /// "text", "number", "switch" or "select".
    kind: &'static str,
    value: String,
    error: String,
    placeholder: &'static str,
    min: String,
    max: String,
    unit: &'static str,
    /// `(value, label)` of a select.
    options: Vec<(&'static str, &'static str)>,
}

impl FieldView {
    fn new(field: &'static SettingField, value: String) -> Self {
        let mut view = FieldView {
            key: field.key,
            label: crate::i18n::t(field.label),
            help: if field.help.is_empty() {
                ""
            } else {
                crate::i18n::t(field.help)
            },
            kind: "text",
            value,
            error: String::new(),
            placeholder: "",
            min: String::new(),
            max: String::new(),
            unit: "",
            options: Vec::new(),
        };
        match field.kind {
            FieldKind::Text { placeholder, .. } => view.placeholder = placeholder,
            FieldKind::Number { min, max, unit } => {
                view.kind = "number";
                view.min = min.to_string();
                view.max = max.to_string();
                if !unit.is_empty() {
                    view.unit = crate::i18n::t(unit);
                }
            }
            FieldKind::Switch => view.kind = "switch",
            FieldKind::Select { options } => {
                view.kind = "select";
                view.options = options
                    .iter()
                    .map(|(v, label)| (*v, crate::i18n::t(label)))
                    .collect();
            }
        }
        view
    }

    fn checked(&self) -> bool {
        db_settings::parse_bool(&self.value)
    }

    fn selected(&self, value: &str) -> bool {
        self.value == value
    }

    /// `aria-describedby` of a number or select field.
    fn described_by(&self) -> String {
        let mut ids = Vec::new();
        if !self.help.is_empty() {
            ids.push(format!("f-{}-help", self.key));
        }
        if !self.error.is_empty() {
            ids.push(format!("f-{}-error", self.key));
        }
        ids.join(" ")
    }
}

struct SectionView {
    /// For the heading's id (`sec-<id>`): the first field's key.
    id: &'static str,
    title: &'static str,
    fields: Vec<FieldView>,
}

#[derive(Template)]
#[template(path = "settings_general.html")]
struct GeneralTemplate {
    ctx: PageCtx,
    site_name: String,
    base_url: String,
    timezone: String,
    language: String,
    timezones: Vec<String>,
    languages: Vec<(&'static str, String)>,
    site_name_error: String,
    base_url_error: String,
    timezone_error: String,
    sections: Vec<SectionView>,
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
        ]
        .iter()
        .any(|e| !e.is_empty())
            || self
                .sections
                .iter()
                .flat_map(|s| &s.fields)
                .any(|f| !f.error.is_empty())
    }
}

fn page_settings(core: &Core) -> Option<&'static GeneralSettings> {
    core.cfg.general
}

/// The page with the stored values.
async fn template(
    core: &Core,
    ctx: PageCtx,
    general: &'static GeneralSettings,
) -> ApiResult<GeneralTemplate> {
    let (site, values) = core
        .with_conn(move |c| {
            let site = SiteSettings::load(c)?;
            let values = general
                .fields()
                .map(|f| f.load(c))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((site, values))
        })
        .await?;
    let mut values = values.into_iter();
    let sections = general
        .sections
        .iter()
        .map(|s| SectionView {
            id: s.fields.first().map_or("product", |f| f.key),
            title: crate::i18n::t(s.title),
            fields: s
                .fields
                .iter()
                .map(|f| FieldView::new(f, values.next().unwrap_or_default()))
                .collect(),
        })
        .collect();
    Ok(GeneralTemplate {
        ctx,
        site_name: site.site_name_or_product(),
        base_url: site.base_url.clone().unwrap_or_default(),
        timezone: site.timezone_or_utc().to_string(),
        language: site.language.unwrap_or_default().code().to_string(),
        timezones: crate::tz::timezones(),
        languages: crate::i18n::language_choices(),
        site_name_error: String::new(),
        base_url_error: String::new(),
        timezone_error: String::new(),
        sections,
    })
}

async fn context(
    core: &Core,
    user: &CurrentUser,
    jar: &SignedCookieJar,
) -> (PageCtx, SignedCookieJar) {
    PageCtx::build(
        core,
        Some(user),
        jar,
        crate::i18n::t("nav.general"),
        "settings",
        "general",
    )
    .await
}

pub(super) async fn page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    user.require_admin()?;
    let Some(general) = page_settings(&core) else {
        return Err(crate::http::ApiError::NotFound);
    };
    let (ctx, jar) = context(&core, &user, &jar).await;
    let page = template(&core, ctx, general).await?;
    Ok((jar, HtmlTemplate(page)).into_response())
}

pub(super) async fn save(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<Vec<(String, String)>>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let Some(general) = page_settings(&core) else {
        return Err(crate::http::ApiError::NotFound);
    };
    let value = |name: &str| {
        form.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let (ctx, jar) = context(&core, &user, &jar).await;
    let mut page = template(&core, ctx, general).await?;

    let site_name = value(db_settings::SITE_NAME)
        .unwrap_or("")
        .trim()
        .to_string();
    page.site_name = site_name.clone();
    if site_name.chars().count() > 80 {
        page.site_name_error = crate::i18n::t("general.err_site_name").into();
    }
    let raw_url = value(db_settings::BASE_URL).unwrap_or("");
    page.base_url = raw_url.trim().to_string();
    let base_url = match normalize_base_url(raw_url) {
        Ok(url) => url,
        Err(msg) => {
            page.base_url_error = msg.into();
            None
        }
    };
    let timezone = value(db_settings::TIMEZONE)
        .unwrap_or("")
        .trim()
        .to_string();
    page.timezone = timezone.clone();
    if !crate::tz::valid_timezone(&timezone) {
        page.timezone_error = crate::i18n::t("general.err_timezone").into();
    }
    let lang = value(db_settings::LANGUAGE)
        .and_then(Lang::from_exact_code)
        .unwrap_or_default();
    page.language = lang.code().to_string();

    let mut product = Vec::new();
    for (field, view) in general
        .fields()
        .zip(page.sections.iter_mut().flat_map(|s| s.fields.iter_mut()))
    {
        let submitted = value(field.key);
        match field.parse(submitted) {
            Ok(stored) => {
                view.value = stored.clone();
                product.push((field, stored));
            }
            Err(()) => {
                view.value = submitted.unwrap_or("").trim().to_string();
                view.error = crate::i18n::t(field.error).into();
            }
        }
    }
    if page.has_errors() {
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, HtmlTemplate(page)).into_response());
    }

    let uid = user.id;
    let own_lang = core
        .with_conn(move |c| {
            let tx = c.transaction()?;
            db_settings::set_opt(&tx, db_settings::SITE_NAME, Some(&site_name))?;
            db_settings::set_opt(&tx, db_settings::BASE_URL, base_url.as_deref())?;
            db_settings::set(&tx, db_settings::TIMEZONE, &timezone)?;
            db_settings::set(&tx, db_settings::LANGUAGE, lang.code())?;
            for (field, stored) in &product {
                field.store(&tx, stored)?;
            }
            tx.commit()?;
            Ok(db_users::get_user(c, uid)?.and_then(|u| u.language))
        })
        .await?;
    crate::i18n::set_site_default(lang);
    (general.after_save)(&core);
    tracing::info!(user = %user.username, "general settings saved");
    // Confirm in the language the next page will use.
    let next = own_lang
        .as_deref()
        .and_then(Lang::from_code)
        .unwrap_or(lang);
    let jar = flash(jar, "success", crate::i18n::t_in(next, "general.saved"));
    Ok((jar, Redirect::to("/settings/general")).into_response())
}
