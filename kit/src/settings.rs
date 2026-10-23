//! The instance settings every product has (site name, public URL, time
//! zone, language) and the kit's General settings page that edits them,
//! with fields a product adds ([`GeneralSettings`]).
//!
//! The values live in the `settings` table under the keys of
//! [`crate::db::settings`] (`site_name`, `base_url`, `timezone`,
//! `language`). A product without the page reads and writes them with
//! [`SiteSettings`], [`set_site_language`] and the accessors there.

use rusqlite::Connection;

use crate::db::settings;
use crate::http::ApiResult;
use crate::i18n::Lang;
use crate::state::Core;

/// The kit's General page: the core settings, then the product's own
/// sections. Set it as [`KitConfig::general`](crate::config::KitConfig::general)
/// and add [`SettingsTab::GENERAL`](crate::page::SettingsTab::GENERAL) to the
/// settings tabs.
pub struct GeneralSettings {
    /// The product's sections, after the kit's "Site" and "Region".
    pub sections: &'static [SettingsSection],
    /// Runs after the settings were saved (to drop a cache, say).
    pub after_save: fn(&Core),
}

impl GeneralSettings {
    /// Just the core settings.
    pub const CORE_ONLY: GeneralSettings = GeneralSettings {
        sections: &[],
        after_save: nothing_after_save,
    };

    /// Every product field.
    pub fn fields(&self) -> impl Iterator<Item = &'static SettingField> + use<> {
        let sections: &'static [SettingsSection] = self.sections;
        sections.iter().flat_map(|s| s.fields.iter())
    }

    /// What's wrong with the product's sections: field keys that are
    /// invalid, taken by the kit or used twice, texts `has` doesn't know,
    /// and defaults the field itself would refuse. Checked by
    /// [`crate::config::install`].
    pub fn problems(&self, has: &impl Fn(&str) -> bool) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for section in self.sections {
            if !has(section.title) {
                problems.push(format!(
                    "General section: no English text for {}",
                    section.title
                ));
            }
            for field in section.fields {
                let key = field.key;
                let what = format!("General field {key:?}");
                let valid = (1..=64).contains(&key.len())
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
                if !valid {
                    problems.push(format!("{what}: invalid key"));
                }
                if RESERVED.contains(&key) || seen.contains(&key) {
                    problems.push(format!("{what}: key already used"));
                }
                seen.push(key);
                let mut texts = vec![field.label, field.help, field.error];
                match field.kind {
                    FieldKind::Number { unit, .. } => texts.push(unit),
                    FieldKind::Select { options } => texts.extend(options.iter().map(|o| o.1)),
                    FieldKind::Text { .. } | FieldKind::Switch => {}
                }
                for text in texts.into_iter().filter(|t| !t.is_empty()) {
                    if !has(text) {
                        problems.push(format!("{what}: no English text for {text}"));
                    }
                }
                if field.error.is_empty() && !matches!(field.kind, FieldKind::Switch) {
                    problems.push(format!("{what}: needs an error text"));
                }
                if field.parse(Some(field.default)).is_err() {
                    problems.push(format!("{what}: its default is not valid"));
                }
            }
        }
        problems
    }
}

fn nothing_after_save(_: &Core) {}

/// Form names the kit's own fields use.
const RESERVED: &[&str] = &[
    settings::SITE_NAME,
    settings::BASE_URL,
    settings::TIMEZONE,
    settings::LANGUAGE,
];

/// A titled group of product fields on the General page.
pub struct SettingsSection {
    /// Translation key of the title.
    pub title: &'static str,
    pub fields: &'static [SettingField],
}

/// One product setting on the General page, stored in the `settings` table
/// under its `key` (which is also the form field's name).
pub struct SettingField {
    /// Settings key and form name: 1–64 characters of `a-z`, `0-9` and `_`,
    /// none of the kit's (`site_name`, `base_url`, `timezone`, `language`).
    pub key: &'static str,
    /// Translation key of the label.
    pub label: &'static str,
    /// Translation key of the help text under it; "" for none.
    pub help: &'static str,
    pub kind: FieldKind,
    /// The value while none is stored (`"1"`/`"0"` for a switch).
    pub default: &'static str,
    /// Translation key of the message for a value the field refuses
    /// ("Enter a number of days between 1 and 3650."); "" for a switch.
    pub error: &'static str,
}

/// What a [`SettingField`] takes.
#[derive(Debug, Clone, Copy)]
pub enum FieldKind {
    /// One line of text, at most `max_chars` characters (trimmed; empty
    /// stores nothing, so the default applies again).
    Text {
        max_chars: usize,
        /// Placeholder (not translated); "" for none.
        placeholder: &'static str,
    },
    /// A whole number from `min` to `max`.
    Number {
        min: i64,
        max: i64,
        /// Translation key of the unit after the box ("days"); "" for none.
        unit: &'static str,
    },
    /// On or off, stored as `"1"` / `"0"`.
    Switch,
    /// One of `options`: `(stored value, translation key of its label)`.
    Select {
        options: &'static [(&'static str, &'static str)],
    },
}

impl SettingField {
    /// The value to store for a submitted form value (`None`: not in the
    /// form, as an unchecked switch), or `Err(())` when the field refuses it.
    #[allow(clippy::result_unit_err)] // the field's own `error` text is the message
    pub fn parse(&self, submitted: Option<&str>) -> Result<String, ()> {
        let value = submitted.unwrap_or("").trim();
        match self.kind {
            FieldKind::Text { max_chars, .. } => {
                if value.chars().count() <= max_chars {
                    Ok(value.to_string())
                } else {
                    Err(())
                }
            }
            FieldKind::Number { min, max, .. } => value
                .parse::<i64>()
                .ok()
                .filter(|n| (min..=max).contains(n))
                .map(|n| n.to_string())
                .ok_or(()),
            FieldKind::Switch => Ok(if settings::parse_bool(value) {
                "1"
            } else {
                "0"
            }
            .into()),
            FieldKind::Select { options } => options
                .iter()
                .find(|(v, _)| *v == value)
                .map(|(v, _)| (*v).to_string())
                .ok_or(()),
        }
    }

    /// The stored value, or the default.
    pub fn load(&self, conn: &Connection) -> rusqlite::Result<String> {
        Ok(settings::get(conn, self.key)?
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| self.default.to_string()))
    }

    /// Stores a value [`Self::parse`] returned (empty text deletes it).
    pub fn store(&self, conn: &Connection, value: &str) -> rusqlite::Result<()> {
        settings::set_opt(conn, self.key, Some(value))
    }
}

/// The core settings as stored. Each is `None` until set (setup stores the
/// site name and time zone; nothing stores a language before an admin
/// picks one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteSettings {
    /// The instance's name; pages fall back to the product's name.
    pub site_name: Option<String>,
    /// The public URL, without a trailing slash.
    pub base_url: Option<String>,
    /// IANA time zone.
    pub timezone: Option<String>,
    /// The site language: the default for everyone who hasn't picked one,
    /// for public pages and for messages sent outside a request.
    pub language: Option<Lang>,
}

impl SiteSettings {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let get = |key| -> rusqlite::Result<Option<String>> {
            Ok(settings::get(conn, key)?.filter(|v| !v.trim().is_empty()))
        };
        Ok(Self {
            site_name: get(settings::SITE_NAME)?,
            base_url: settings::base_url(conn)?,
            timezone: get(settings::TIMEZONE)?,
            language: get(settings::LANGUAGE)?
                .as_deref()
                .and_then(Lang::from_code),
        })
    }

    /// The site name, else the product's name.
    pub fn site_name_or_product(&self) -> String {
        self.site_name
            .clone()
            .unwrap_or_else(|| crate::config::get().product.to_string())
    }

    /// The time zone, else UTC.
    pub fn timezone_or_utc(&self) -> &str {
        self.timezone.as_deref().unwrap_or("UTC")
    }
}

/// Sets (or, with `None`, clears) the site language and refreshes the
/// cached default ([`crate::i18n::site_default`]), for products that set it
/// without the General page (from their own configuration, say).
pub async fn set_site_language(core: &Core, lang: Option<Lang>) -> ApiResult<()> {
    let code = lang.map(Lang::code);
    core.with_conn(move |c| settings::set_opt(c, settings::LANGUAGE, code))
        .await?;
    crate::i18n::set_site_default(lang.unwrap_or_default());
    Ok(())
}

/// Validates and normalises a public base URL: `http(s)://host[:port][/path]`,
/// no credentials, query or fragment; trailing slash removed. Empty → `None`.
/// The error is a translated message.
pub fn normalize_base_url(input: &str) -> Result<Option<String>, &'static str> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let hint = crate::i18n::t("general.err_url");
    let url = url::Url::parse(input).map_err(|_| hint)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(hint);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(crate::i18n::t("general.err_url_credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(crate::i18n::t("general.err_url_query"));
    }
    Ok(Some(url.as_str().trim_end_matches('/').to_string()))
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
            normalize_base_url("http://10.0.0.5:3001/app/"),
            Ok(Some("http://10.0.0.5:3001/app".into()))
        );
        assert_eq!(normalize_base_url(""), Ok(None));
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

    const DAYS: SettingField = SettingField {
        key: "keep_days",
        label: "general.site",
        help: "",
        kind: FieldKind::Number {
            min: 1,
            max: 30,
            unit: "",
        },
        default: "7",
        error: "general.err_timezone",
    };

    #[test]
    fn fields_parse_what_they_take() {
        assert_eq!(DAYS.parse(Some(" 12 ")), Ok("12".into()));
        assert_eq!(DAYS.parse(Some("31")), Err(()));
        assert_eq!(DAYS.parse(None), Err(()));
        let switch = SettingField {
            kind: FieldKind::Switch,
            default: "0",
            ..DAYS
        };
        assert_eq!(switch.parse(Some("true")), Ok("1".into()));
        assert_eq!(switch.parse(None), Ok("0".into()));
        let text = SettingField {
            kind: FieldKind::Text {
                max_chars: 3,
                placeholder: "",
            },
            ..DAYS
        };
        assert_eq!(text.parse(Some(" æøå ")), Ok("æøå".into()));
        assert_eq!(text.parse(Some("abcd")), Err(()));
        let select = SettingField {
            kind: FieldKind::Select {
                options: &[("a", "general.site"), ("b", "general.site")],
            },
            ..DAYS
        };
        assert_eq!(select.parse(Some("b")), Ok("b".into()));
        assert_eq!(select.parse(Some("c")), Err(()));
    }

    #[test]
    fn fields_load_their_default_until_stored() {
        let conn = crate::db::test_conn();
        assert_eq!(DAYS.load(&conn).unwrap(), "7");
        DAYS.store(&conn, "12").unwrap();
        assert_eq!(DAYS.load(&conn).unwrap(), "12");
    }

    #[test]
    fn a_products_sections_are_checked() {
        static GOOD: &[SettingsSection] = &[SettingsSection {
            title: "general.site",
            fields: &[DAYS],
        }];
        let has = |k: &str| k.starts_with("general.");
        let good = GeneralSettings {
            sections: GOOD,
            ..GeneralSettings::CORE_ONLY
        };
        assert_eq!(good.problems(&has), Vec::<String>::new());
        assert_eq!(good.fields().count(), 1);

        static BAD: &[SettingsSection] = &[SettingsSection {
            title: "nope.title",
            fields: &[
                SettingField {
                    key: "timezone",
                    default: "99",
                    ..DAYS
                },
                SettingField {
                    key: "Bad Key",
                    label: "nope.label",
                    error: "",
                    ..DAYS
                },
            ],
        }];
        let bad = GeneralSettings {
            sections: BAD,
            ..GeneralSettings::CORE_ONLY
        };
        let problems = bad.problems(&has).join("\n");
        for expected in [
            "no English text for nope.title",
            "\"timezone\": key already used",
            "\"timezone\": its default is not valid",
            "\"Bad Key\": invalid key",
            "no English text for nope.label",
            "\"Bad Key\": needs an error text",
        ] {
            assert!(problems.contains(expected), "{expected}\n{problems}");
        }
    }

    #[test]
    fn site_settings_read_what_is_stored() {
        let conn = crate::db::test_conn();
        assert_eq!(SiteSettings::load(&conn).unwrap(), SiteSettings::default());
        settings::set(&conn, settings::SITE_NAME, "Acme").unwrap();
        settings::set(&conn, settings::BASE_URL, "https://x.example/").unwrap();
        settings::set(&conn, settings::TIMEZONE, "Europe/Oslo").unwrap();
        settings::set(&conn, settings::LANGUAGE, "nb").unwrap();
        let s = SiteSettings::load(&conn).unwrap();
        assert_eq!(s.site_name_or_product(), "Acme");
        assert_eq!(s.base_url.as_deref(), Some("https://x.example"));
        assert_eq!(s.timezone_or_utc(), "Europe/Oslo");
        assert_eq!(s.language, Lang::from_code("nb"));
    }
}
