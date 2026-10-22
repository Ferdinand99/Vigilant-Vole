//! Interface translations.
//!
//! The languages are a registry ([`lang::LANGS`]): one entry and one
//! `locales/<code>.toml` per language. Strings live in those files (compiled
//! in, parsed once). A key missing from a language falls back to English, key
//! by key. The
//! language of the current request is a task-local set by [`layer`] (site
//! default / `Accept-Language`) and refined by the auth middleware (a user's
//! own choice), so templates and handlers can translate without threading a
//! language through every struct:
//!
//! ```ignore
//! // Rust (handlers, flash messages, validation errors)
//! let msg = i18n::t("monitors.saved");
//! let msg = i18n::tf("monitors.deleted", &[("name", &monitor.name)]);
//! // Templates (any template: `Tr` is implemented for every type)
//! {{ tr("nav.dashboard") }}  {{ tr1("monitors.count", "n", 3) }}  {{ trn("dash.monitors", n) }}
//! ```
//!
//! Anything rendered outside a request (background jobs, tests) uses English
//! unless it opts in with [`scope`] / [`sync_scope`].

pub mod catalog;
mod format;
mod lang;
mod middleware;
mod negotiate;

use std::fmt::Display;
use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
#[cfg(debug_assertions)]
use std::sync::{Mutex, PoisonError};

pub use format::*;
pub use lang::{Dir, LANGS, Lang, LangInfo, Numbers, Plural, PluralRule, language_choices};
pub use middleware::{layer, site_language};
pub use negotiate::{negotiate, parse_accept_language};

tokio::task_local! {
    static LANG: Lang;
}

/// Last known site default (Settings → General), for work that runs outside
/// a request, like notifications sent by the scheduler. A registry index.
static SITE_DEFAULT: AtomicU8 = AtomicU8::new(0);

/// Language of the current request (English outside a request scope).
pub fn current() -> Lang {
    LANG.try_with(|l| *l).unwrap_or_default()
}

/// The request's language, or the site default outside a request.
pub fn current_or_site() -> Lang {
    LANG.try_with(|l| *l).unwrap_or_else(|_| site_default())
}

/// The cached site default language (English until known).
pub fn site_default() -> Lang {
    Lang::all()
        .nth(usize::from(SITE_DEFAULT.load(Ordering::Relaxed)))
        .unwrap_or_default()
}

/// Updates the cached site default (done whenever settings are read).
pub fn set_site_default(lang: Lang) {
    SITE_DEFAULT.store(u8::try_from(lang.index()).unwrap_or(0), Ordering::Relaxed);
}

/// Runs `fut` with `lang` as the current language.
pub async fn scope<F: Future>(lang: Lang, fut: F) -> F::Output {
    LANG.scope(lang, fut).await
}

/// Runs `f` with `lang` as the current language (rendering outside a request,
/// e.g. SSE updates or notifications).
pub fn sync_scope<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    LANG.sync_scope(lang, f)
}

/// Looks a key up in `lang`, falling back to English per key (and saying so
/// once per key in debug builds).
pub fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
    if let Some(text) = catalog::get(lang, key) {
        return Some(text);
    }
    let english = catalog::get(Lang::EN, key);
    #[cfg(debug_assertions)]
    if english.is_some() && lang != Lang::EN {
        note_fallback(lang, key);
    }
    english
}

/// Logs a missing translation the first time it is asked for.
#[cfg(debug_assertions)]
fn note_fallback(lang: Lang, key: &str) {
    static SEEN: Mutex<Vec<(u8, String)>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    let id = (u8::try_from(lang.index()).unwrap_or(0), key.to_string());
    if !seen.contains(&id) {
        tracing::debug!(lang = lang.code(), key, "no translation, using English");
        seen.push(id);
    }
}

/// Translation of `key` in `lang`; unknown keys render as the key itself
/// (and warn in debug builds).
pub fn t_in(lang: Lang, key: &'static str) -> &'static str {
    match lookup(lang, key) {
        Some(s) => s,
        None => {
            #[cfg(debug_assertions)]
            tracing::warn!(key, "missing translation key");
            key
        }
    }
}

/// Translation of `key` in the current language.
pub fn t(key: &'static str) -> &'static str {
    t_in(current(), key)
}

/// Translation with `{name}` placeholders filled in.
pub fn tf(key: &'static str, args: &[(&str, &str)]) -> String {
    interpolate(t(key), args)
}

/// Plural form: `{key}.<category>` for the language's CLDR plural category
/// of `n` (`one`, `few`, `many`, `other`, …), falling back to `{key}.other`,
/// with `{n}` replaced by the formatted number.
pub fn tn(key: &'static str, n: i64) -> String {
    tn_in(current(), key, n)
}

pub fn tn_in(lang: Lang, key: &str, n: i64) -> String {
    // A language that hasn't translated this key at all uses English's forms
    // (and English's plural rule), not a half-matching mix.
    let forms = if catalog::get(lang, &format!("{key}.other")).is_some() {
        lang
    } else {
        Lang::EN
    };
    let category = forms.plural_category(n).as_str();
    let text = catalog::get(forms, &format!("{key}.{category}"))
        .or_else(|| catalog::get(forms, &format!("{key}.other")))
        .unwrap_or_else(|| {
            #[cfg(debug_assertions)]
            tracing::warn!(key, "missing plural translation key");
            ""
        });
    if text.is_empty() {
        return format!("{n} {key}");
    }
    interpolate(text, &[("n", &fmt_int_in(lang, n))])
}

/// Like [`tn`] with extra placeholders.
pub fn tnf(key: &'static str, n: i64, args: &[(&str, &str)]) -> String {
    interpolate(&tn(key, n), args)
}

/// Replaces each `{name}` in `text`.
pub fn interpolate(text: &str, args: &[(&str, &str)]) -> String {
    let mut out = text.to_string();
    for (name, value) in args {
        let needle = format!("{{{name}}}");
        if out.contains(&needle) {
            out = out.replace(&needle, value);
        }
    }
    out
}

/// Translated status word for a machine value (`up`, `down`, `pending`,
/// `maintenance`, `paused`, …); unknown values are returned as is.
pub fn status_label(status: &str) -> String {
    let key = format!("status.{status}");
    match lookup(current(), &key) {
        Some(s) => s.to_string(),
        None => status.to_string(),
    }
}

/// Dynamic key lookup in the current language (e.g. a provider field
/// label); returns `fallback` when the key doesn't exist.
pub fn t_or<'a>(key: &str, fallback: &'a str) -> &'a str {
    lookup(current(), key).unwrap_or(fallback)
}

/// JSON object with the `js.*` strings for `static/app.js`
/// (`window.VV_I18N`), safe to embed in a `<script>` element.
pub fn js_bundle(lang: Lang) -> String {
    let map: serde_json::Map<String, serde_json::Value> =
        catalog::keys_with_prefix(Lang::EN, "js.")
            .into_iter()
            .map(|key| {
                let value = lookup(lang, key).unwrap_or_default();
                (
                    key.trim_start_matches("js.").to_string(),
                    serde_json::Value::from(value),
                )
            })
            .collect();
    serde_json::to_string(&map)
        .unwrap_or_else(|_| "{}".into())
        .replace("</", "<\\/")
        .replace("<!--", "<\\!--")
}

/// Count types accepted by the template plural helper.
pub trait Count {
    fn as_count(&self) -> i64;
}

macro_rules! impl_count {
    ($($t:ty),*) => {$(
        impl Count for $t {
            fn as_count(&self) -> i64 { i64::try_from(*self).unwrap_or(i64::MAX) }
        }
    )*};
}
impl_count!(i64, i32, u32, u64, usize, u16, u8);

impl<T: Count + ?Sized> Count for &T {
    fn as_count(&self) -> i64 {
        (**self).as_count()
    }
}

/// Numbers (optionally missing) accepted by the template number helpers.
pub trait OptNum {
    fn as_opt_f64(&self) -> Option<f64>;
}

macro_rules! impl_opt_num {
    ($($t:ty),*) => {$(
        impl OptNum for $t {
            #[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
            fn as_opt_f64(&self) -> Option<f64> { Some(*self as f64) }
        }
        impl OptNum for Option<$t> {
            #[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
            fn as_opt_f64(&self) -> Option<f64> { self.map(|v| v as f64) }
        }
    )*};
}
impl_opt_num!(f64, f32, i64, i32, u32, u64, usize);

impl<T: OptNum + ?Sized> OptNum for &T {
    fn as_opt_f64(&self) -> Option<f64> {
        (**self).as_opt_f64()
    }
}

/// Template helpers. Askama turns a bare `tr("key")` into `self.tr("key")`,
/// and this trait is implemented for every type, so every template (and
/// every macro) can call these without extra fields. The template's module
/// needs `use crate::i18n::Tr as _;`.
pub trait Tr {
    /// `{{ tr("nav.dashboard") }}`
    fn tr(&self, key: &'static str) -> &'static str {
        t(key)
    }
    /// `{{ tr1("monitors.delete_q", "name", monitor.name) }}`
    fn tr1(&self, key: &'static str, name: &str, value: impl Display) -> String {
        tf(key, &[(name, &value.to_string())])
    }
    fn tr2(
        &self,
        key: &'static str,
        n1: &str,
        v1: impl Display,
        n2: &str,
        v2: impl Display,
    ) -> String {
        tf(key, &[(n1, &v1.to_string()), (n2, &v2.to_string())])
    }
    #[allow(clippy::too_many_arguments)]
    fn tr3(
        &self,
        key: &'static str,
        n1: &str,
        v1: impl Display,
        n2: &str,
        v2: impl Display,
        n3: &str,
        v3: impl Display,
    ) -> String {
        tf(
            key,
            &[
                (n1, &v1.to_string()),
                (n2, &v2.to_string()),
                (n3, &v3.to_string()),
            ],
        )
    }
    /// Plural: `{{ trn("dash.monitor_count", rows.len()) }}`.
    fn trn(&self, key: &'static str, n: impl Count) -> String {
        tn(key, n.as_count())
    }
    /// Translated status word (`up` → "Up"/"Oppe").
    fn status_text(&self, status: impl Display) -> String {
        status_label(&status.to_string())
    }
    /// `<html lang>` code of the current language.
    fn lang_code(&self) -> &'static str {
        current().code()
    }
    /// `<html dir>`: `ltr` or `rtl`.
    fn lang_dir(&self) -> &'static str {
        current().dir().as_str()
    }
    /// The product's layout settings (logo, navigation, settings tabs).
    fn kit_ui(&self) -> &'static crate::page::Ui {
        crate::config::get().ui
    }
    /// The product's name ("Vigilant Vole").
    fn product_name(&self) -> &'static str {
        crate::config::get().product
    }
    /// `window.VV_I18N` payload (embed with `|safe`).
    fn i18n_js(&self) -> String {
        js_bundle(current())
    }
    /// "99.95%" / "99,95 %"; `None` → em dash.
    fn pct(&self, value: impl OptNum) -> String {
        value.as_opt_f64().map_or_else(|| "—".to_string(), fmt_pct2)
    }
    /// "120 ms" / "1 234 ms"; `None` → em dash.
    #[allow(clippy::cast_possible_truncation)]
    fn millis(&self, value: impl OptNum) -> String {
        value
            .as_opt_f64()
            .map_or_else(|| "—".to_string(), |v| fmt_ms(v.round() as i64))
    }
    /// Locale-formatted integer ("1,234" / "1 234").
    fn num(&self, value: impl Count) -> String {
        fmt_int(value.as_count())
    }
    /// Locale-formatted decimal with `decimals` digits; `None` → em dash.
    fn dec(&self, value: impl OptNum, decimals: usize) -> String {
        value
            .as_opt_f64()
            .map_or_else(|| "—".to_string(), |v| fmt_dec(v, decimals))
    }
    /// "3 h 4 min" / "3 t 4 min".
    fn dur(&self, secs: impl Count) -> String {
        fmt_duration(secs.as_count())
    }
}

impl<T: ?Sized> Tr for T {}
