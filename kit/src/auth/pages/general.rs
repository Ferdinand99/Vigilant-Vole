//! The General settings page (admins), for a product that turns it on
//! ([`KitConfig::general`](crate::config::KitConfig::general)):
//! - `GET  /settings/general`: site name, public URL, time zone, language
//!   and the product's own fields
//! - `POST /settings/general`: validate and save; errors re-render the page
//!   with 422. Fields that are not in the form keep their saved value (a
//!   stale page or a script that sends part of the form does not wipe the
//!   mail settings). With
//!   [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes),
//!   changing the mail settings or the public URL (where a one-time code
//!   is sent or linked) asks for the admin's password, or with
//!   `CredentialMode::External` for a fresh grant
//! - `POST /settings/general/smtp-test`: with the same feature, sends a test
//!   email through the instance's mail settings (five per ten minutes per
//!   admin)

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;

use crate::auth::{CurrentUser, reauth};
use crate::db::{settings as db_settings, sign_in_codes as db_codes, users as db_users};
use crate::http::{ApiResult, HtmlTemplate};
use crate::i18n::{Lang, Tr as _};
use crate::mail::{self, Smtp};
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

/// The instance's mail settings as the page shows them
/// ([`crate::mail`]; only with
/// [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes)).
struct SmtpView {
    host: String,
    port: String,
    security: String,
    username: String,
    from: String,
    /// A password is saved (it is never shown).
    password_set: bool,
    /// The saved password can't be opened with this instance's key.
    password_unreadable: bool,
    host_error: String,
    port_error: String,
    username_error: String,
    from_error: String,
    /// Where the test email goes by default: the admin's own address.
    test_to: String,
}

impl SmtpView {
    fn new(smtp: &Smtp, key: &axum_extra::extract::cookie::Key, test_to: String) -> Self {
        Self {
            host: smtp.host.clone(),
            port: smtp.port.clone(),
            security: smtp.security.clone(),
            username: smtp.username.clone(),
            from: smtp.from.clone(),
            password_set: smtp.has_password(),
            password_unreadable: !smtp.password_readable(key),
            host_error: String::new(),
            port_error: String::new(),
            username_error: String::new(),
            from_error: String::new(),
            test_to,
        }
    }

    fn selected(&self, security: &str) -> bool {
        self.security == security
    }

    fn has_errors(&self) -> bool {
        [
            &self.host_error,
            &self.port_error,
            &self.username_error,
            &self.from_error,
        ]
        .iter()
        .any(|e| !e.is_empty())
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
    /// The mail settings, with
    /// [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes).
    smtp: Option<SmtpView>,
    /// Where the page asks for the admin's password (with the mail
    /// settings).
    reauth: reauth::View,
    password_error: String,
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
            &self.password_error,
        ]
        .iter()
        .any(|e| !e.is_empty())
            || self.smtp.as_ref().is_some_and(SmtpView::has_errors)
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
    user: &CurrentUser,
    ctx: PageCtx,
    reauth: reauth::View,
    general: &'static GeneralSettings,
) -> ApiResult<GeneralTemplate> {
    let codes = core.cfg.features.sign_in_codes;
    let uid = user.id;
    let (site, values, smtp) = core
        .with_conn(move |c| {
            let site = SiteSettings::load(c)?;
            let values = general
                .fields()
                .map(|f| f.load(c))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let smtp = if codes {
                Some((Smtp::load(c)?, db_codes::email(c, uid)?.unwrap_or_default()))
            } else {
                None
            };
            Ok((site, values, smtp))
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
        smtp: smtp.map(|(smtp, email)| SmtpView::new(&smtp, &core.session_key, email)),
        reauth,
        password_error: String::new(),
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
    let view = reauth::View::new(&jar, &core.session_key, &user, "/settings/general");
    let (ctx, jar) = context(&core, &user, &jar).await;
    let page = template(&core, &user, ctx, view, general).await?;
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
    let view = reauth::View::new(&jar, &core.session_key, &user, "/settings/general");
    let (ctx, jar) = context(&core, &user, &jar).await;
    let mut page = template(&core, &user, ctx, view, general).await?;
    // The public URL as it is stored, to tell whether this save changes it.
    let stored_base_url = page.base_url.clone();

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
    // The mail settings: what is typed stays on the page, the saved
    // password never is. A field the form does not have keeps its saved
    // value; a field that is there and empty clears it.
    let mut mail_to_store = None;
    let mut mail_changed = false;
    if let Some(view) = page.smtp.as_mut() {
        let saved = core.with_conn(|c| Smtp::load(c)).await?;
        let field = |name: &str, saved: &str| value(name).unwrap_or(saved).to_string();
        let form = mail::Form {
            host: field("smtp_host", &saved.host),
            port: field("smtp_port", &saved.port),
            security: field("smtp_security", &saved.security),
            username: field("smtp_username", &saved.username),
            password: value("smtp_password").unwrap_or("").to_string(),
            clear_password: value("smtp_password_clear").is_some_and(db_settings::parse_bool),
            from: field("smtp_from", &saved.from),
        };
        view.host = form.host.trim().to_string();
        view.port = form.port.trim().to_string();
        if mail::SECURITIES.contains(&form.security.as_str()) {
            view.security = form.security.clone();
        }
        view.username = form.username.trim().to_string();
        view.from = form.from.trim().to_string();
        match mail::parse(&form, &saved, &core.session_key) {
            Ok(smtp) => {
                mail_changed = smtp != saved;
                mail_to_store = Some(smtp);
            }
            Err(problems) => {
                let text = |key: Option<&'static str>| key.map(crate::i18n::t).unwrap_or_default();
                view.host_error = text(problems.host).into();
                view.port_error = text(problems.port).into();
                view.username_error = text(problems.username).into();
                view.from_error = text(problems.from).into();
            }
        }
    }
    if page.has_errors() {
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, HtmlTemplate(page)).into_response());
    }

    // Where a sign-in code is sent (the mail server) and linked (the public
    // URL) decides who gets it: changing either is as good as taking the
    // accounts, so it asks for the admin's password again.
    let confirm = page.smtp.is_some()
        && (mail_changed || base_url.as_deref().unwrap_or("") != stored_base_url);
    if confirm {
        let password = value("current_password").unwrap_or("").to_string();
        if let Err(msg) = super::security::require_password(&core, &user, &jar, password).await? {
            page.password_error = msg;
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, HtmlTemplate(page)).into_response());
        }
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
            if let Some(smtp) = &mail_to_store {
                smtp.store(&tx)?;
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
    // A confirmation that changed where codes go is used up.
    let jar = if confirm { reauth::spend(jar) } else { jar };
    Ok((jar, Redirect::to("/settings/general")).into_response())
}

#[derive(serde::Deserialize)]
pub struct TestForm {
    #[serde(default)]
    to: String,
}

/// Sends a test email through the saved mail settings.
pub(super) async fn smtp_test(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<TestForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let back = |jar: SignedCookieJar, kind: &str, message: String| {
        (flash(jar, kind, message), Redirect::to("/settings/general")).into_response()
    };
    if !core.cfg.features.sign_in_codes {
        return Err(crate::http::ApiError::NotFound);
    }
    // Five per ten minutes per admin: the button sends mail to any address.
    let key = format!("mail-test:{}", user.id);
    if let Err(wait) = core
        .limits
        .mail_test
        .reserve(&key, std::time::Instant::now())
    {
        let minutes = wait.as_secs().div_ceil(60).max(1).to_string();
        return Ok(back(
            jar,
            "error",
            crate::i18n::tf("sign_in_code.test_limited", &[("minutes", &minutes)]),
        ));
    }
    let to = form.to.trim().to_string();
    if !mail::is_address(&to) {
        return Ok(back(
            jar,
            "error",
            crate::i18n::t("sign_in_code.err_email").into(),
        ));
    }
    if mail::load(&core).await?.is_none() {
        return Ok(back(
            jar,
            "error",
            crate::i18n::t("sign_in_code.smtp_not_set").into(),
        ));
    }
    let site = super::sign_in::site_name(&core).await;
    let (subject, body) = (
        crate::i18n::tf("sign_in_code.test_subject", &[("site", &site)]),
        crate::i18n::tf("sign_in_code.test_body", &[("site", &site)]),
    );
    Ok(match mail::send(&core, &to, &subject, &body).await {
        Ok(()) => {
            tracing::info!(user = %user.username, "test email sent");
            back(
                jar,
                "success",
                crate::i18n::tf("sign_in_code.test_sent", &[("email", &to)]),
            )
        }
        Err(error) => {
            tracing::warn!(user = %user.username, %error, "test email failed");
            back(
                jar,
                "error",
                crate::i18n::tf("sign_in_code.test_failed", &[("error", &error)]),
            )
        }
    })
}
