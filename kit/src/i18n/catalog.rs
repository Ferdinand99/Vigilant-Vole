//! Locale catalogs, compiled in and parsed once. A language's catalog is
//! the kit's core strings (`kit/locales/<code>.toml`) plus the product's
//! (registered in its `KitConfig`); a key may be defined in only one of
//! them ([`validate`] checks, at start-up). A few texts on the kit's shared
//! pages are the product's to word ([`PRODUCT_KEYS`]).
//!
//! The files use a small TOML subset: `[section]` headers (dotted names
//! allowed), `key = "value"` lines with `\"`, `\\`, `\n` and `\u{…}`-free
//! escapes, `#` comments and blank lines. The full key is `section.key`.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use super::lang::LANGS;
use super::{Lang, Plural};

/// Keys the kit's shared pages use but the product's catalog defines,
/// because their words depend on the product. Every product's English
/// catalog must have them ([`validate`] checks, at start-up); the kit's
/// catalog never does.
pub const PRODUCT_KEYS: &[&str] = &[
    // The sign-in page's subtitle ("Welcome back. Log in to see your monitors.").
    "login.subtitle",
    // Placeholder of the site name on the setup page ("Acme status…").
    "setup.site_name_placeholder",
    // After the time zone on the setup page: "Times are shown in
    // <zone>" + this (". You can change this in Settings.").
    "setup.tz_after",
    // Under the user list: what each role may do.
    "users.roles_help",
    // The Tokens page's subtitle: what tokens are for.
    "tokens.sub",
    // What a read-only token can read ("Monitors, status, metrics").
    "tokens.scope_read_sub",
    // The flash message after setup ("You're all set. …").
    "auth.all_set",
    // The locked setup page: how to find the setup token
    // (`<PREFIX>_SETUP_TOKEN`).
    "auth.setup_token_required",
    // The error page's link to `/` ("Back to dashboard").
    "error.back",
];

/// Texts of the kit's General settings page that depend on the product,
/// required (like [`PRODUCT_KEYS`]) only from a product that turns the page
/// on ([`KitConfig::general`](crate::config::KitConfig::general)).
pub const GENERAL_PRODUCT_KEYS: &[&str] = &[
    // The page's subtitle: what these settings cover ("How this instance
    // names itself, tells time and keeps data.").
    "general.sub",
    // Under the public URL: what it is used for ("… Used for links in
    // notifications and push URLs.").
    "general.base_url_help",
];

pub type Catalog = HashMap<String, String>;

static CATALOGS: OnceLock<Vec<Catalog>> = OnceLock::new();

/// One parsed catalog per registered language, in registry order.
fn catalogs() -> &'static [Catalog] {
    CATALOGS.get_or_init(|| {
        LANGS
            .iter()
            .map(|info| {
                parse(info.source).unwrap_or_else(|err| {
                    // The test suite parses every catalog, so this only happens
                    // with a hand-edited release build.
                    tracing::error!(lang = info.code, %err, "invalid locale file");
                    HashMap::new()
                })
            })
            .collect()
    })
}

macro_rules! core_texts {
    ($($code:literal),*) => {
        &[$(($code, include_str!(concat!("../../locales/", $code, ".toml")))),*]
    };
}

/// The kit's own strings: shared pages, forms, formats, providers.
static CORE: &[(&str, &str)] = core_texts!(
    "en", "nb", "ar", "cs", "da", "de", "es", "fa", "fi", "fr", "he", "hu", "it", "ja", "ko", "nl",
    "pl", "pt-BR", "pt-PT", "ro", "ru", "sv", "tr", "uk", "zh-CN", "zh-TW"
);

/// The kit's core catalog text for `code` (empty for an unknown code).
pub fn core_text(code: &str) -> &'static str {
    CORE.iter().find(|(c, _)| *c == code).map_or("", |(_, t)| t)
}

/// Checks every language's catalogs: both parse, no key is defined by the
/// kit and the product alike, and the product's English catalog has every
/// one of [`PRODUCT_KEYS`]. Called when the configuration is installed, so
/// a broken, clashing or incomplete catalog stops the start.
pub fn validate() -> Result<(), String> {
    let mut problems = Vec::new();
    for info in LANGS {
        if let Err(e) = check_pair(info.source.core(), info.source.product()) {
            problems.push(format!("{}: {e}", info.code));
        }
    }
    if let Err(e) = check_product_keys(Lang::EN.info().source.product()) {
        problems.push(format!("en: {e}"));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// `core` and `product` each parse, and share no key.
pub fn check_pair(core: &str, product: &str) -> Result<(), String> {
    let core = parse(core).map_err(|e| format!("kit catalog: {e}"))?;
    let product = parse(product).map_err(|e| format!("product catalog: {e}"))?;
    let mut both: Vec<&String> = product.keys().filter(|k| core.contains_key(*k)).collect();
    if both.is_empty() {
        return Ok(());
    }
    both.sort();
    Err(format!(
        "{} key(s) defined by both the kit and the product: {}",
        both.len(),
        both.iter()
            .take(10)
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// `product` (a product's English catalog) defines every one of
/// [`PRODUCT_KEYS`].
pub fn check_product_keys(product: &str) -> Result<(), String> {
    let product = parse(product).map_err(|e| format!("product catalog: {e}"))?;
    let missing: Vec<&str> = PRODUCT_KEYS
        .iter()
        .copied()
        .filter(|k| !product.contains_key(*k))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the product's catalog must define {} key(s) the kit's pages use: {}",
        missing.len(),
        missing.join(", ")
    ))
}

pub fn get(lang: Lang, key: &str) -> Option<&'static str> {
    catalogs()[lang.index()].get(key).map(String::as_str)
}

/// Keys of `lang` that start with `prefix`, sorted.
pub fn keys_with_prefix(lang: Lang, prefix: &str) -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = catalogs()[lang.index()]
        .keys()
        .filter(|k| k.starts_with(prefix))
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

pub fn all(lang: Lang) -> &'static Catalog {
    &catalogs()[lang.index()]
}

/// The text of a locale file as compiled in (tests re-parse it).
pub fn source(lang: Lang) -> &'static str {
    lang.info().source
}

/// Parses one locale file. Errors name the 1-based line.
pub fn parse(src: &str) -> Result<Catalog, String> {
    let mut map = Catalog::new();
    let mut section = String::new();
    for (i, raw) in src.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            let name = name
                .strip_suffix(']')
                .ok_or_else(|| format!("line {line_no}: unterminated section header"))?
                .trim();
            if name.is_empty() || !name.chars().all(is_key_char) {
                return Err(format!("line {line_no}: invalid section name {name:?}"));
            }
            section = name.to_string();
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line_no}: expected key = \"value\""))?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(is_key_char) {
            return Err(format!("line {line_no}: invalid key {key:?}"));
        }
        let value = parse_string(rest.trim()).map_err(|e| format!("line {line_no}: {e}"))?;
        let full = if section.is_empty() {
            key.to_string()
        } else {
            format!("{section}.{key}")
        };
        if map.insert(full.clone(), value).is_some() {
            return Err(format!("line {line_no}: duplicate key {full}"));
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

/// How completely a language translates the English catalog: the kit's and
/// the product's strings together, as the app looks them up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub lang: Lang,
    /// Strings the language needs: every plain English key, and every
    /// plural key once per form its CLDR rule has.
    pub total: usize,
    /// Of those, the ones it translates.
    pub done: usize,
    /// Everything wrong, one line each: missing keys, keys English doesn't
    /// have, plural forms the language doesn't use, empty values and
    /// placeholders other than English's.
    pub problems: Vec<String>,
}

impl Coverage {
    /// Every string translated and nothing wrong.
    pub fn is_complete(&self) -> bool {
        self.problems.is_empty() && self.done == self.total
    }

    /// `done` as a percentage of `total`.
    #[allow(clippy::cast_precision_loss)] // counts of a few thousand
    pub fn percent(&self) -> f64 {
        if self.total == 0 {
            100.0
        } else {
            100.0 * self.done as f64 / self.total as f64
        }
    }
}

/// `(base, category)` when `key` ends in a plural category.
fn plural_parts(key: &str) -> Option<(&str, Plural)> {
    let (base, category) = key.rsplit_once('.')?;
    Plural::ALL
        .into_iter()
        .find(|p| p.as_str() == category)
        .map(|p| (base, p))
}

/// The `{name}` placeholders of a text.
fn placeholder_names(text: &str) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        out.insert(&rest[start + 1..start + len]);
        rest = &rest[start + len..];
    }
    out
}

/// Measures `lang` against English: the kit's and the product's strings,
/// as joined for that language. A language the product ships no catalog for
/// lacks the product's keys (they fall back to English).
pub fn coverage(lang: Lang) -> Coverage {
    let en = all(Lang::EN);
    let cat = all(lang);
    // Plural bases: English has `.one` and `.other`.
    let bases: BTreeSet<&str> = en
        .keys()
        .filter_map(|k| k.strip_suffix(".one"))
        .filter(|base| en.contains_key(&format!("{base}.other")))
        .collect();
    let is_variant = |key: &str| plural_parts(key).is_some_and(|(b, _)| bases.contains(b));
    let plain: BTreeSet<&str> = en
        .keys()
        .map(String::as_str)
        .filter(|k| !is_variant(k))
        .collect();
    let forms = lang.plural_rule().categories();

    let mut problems = Vec::new();
    let mut missing = Vec::new();
    for key in &plain {
        if !cat.contains_key(*key) {
            missing.push((*key).to_string());
        }
    }
    for base in &bases {
        for form in forms {
            let key = format!("{base}.{}", form.as_str());
            if !cat.contains_key(&key) {
                missing.push(key);
            }
        }
    }
    let total = plain.len() + bases.len() * forms.len();
    let done = total - missing.len();
    missing.sort();
    problems.extend(missing.into_iter().map(|k| format!("missing {k}")));

    let mut keys: Vec<&String> = cat.keys().collect();
    keys.sort();
    for key in keys {
        let value = &cat[key];
        if value.trim().is_empty() {
            problems.push(format!("{key} is empty"));
        }
        if let Some((base, form)) = plural_parts(key).filter(|(b, _)| bases.contains(b)) {
            if !forms.contains(&form) {
                problems.push(format!("{key}: the language has no {} form", form.as_str()));
            }
            // A plural form carries the placeholders of English `.one` or `.other`.
            let names = placeholder_names(value);
            let matches = ["one", "other"]
                .iter()
                .any(|f| placeholder_names(&en[&format!("{base}.{f}")]) == names);
            if !matches {
                problems.push(format!("{key} has other placeholders than English"));
            }
        } else if let Some(english) = en.get(key) {
            if placeholder_names(english) != placeholder_names(value) {
                problems.push(format!("{key} has other placeholders than English"));
            }
        } else {
            problems.push(format!("{key} is not in English"));
        }
    }
    Coverage {
        lang,
        total,
        done,
        problems,
    }
}

/// Panics, listing every gap, unless each language the product ships a
/// catalog for ([`KitConfig::locales`](crate::config::KitConfig)) is
/// complete ([`coverage`]). Call it from a product's test suite after
/// installing its configuration:
///
/// ```ignore
/// #[test]
/// fn every_language_is_complete() {
///     my_product::install();
///     vole_kit::i18n::catalog::assert_full_coverage();
/// }
/// ```
pub fn assert_full_coverage() {
    let mut report = Vec::new();
    let mut gaps = 0;
    for lang in Lang::all() {
        let shipped = crate::config::get()
            .locales
            .iter()
            .any(|(code, _)| *code == lang.code());
        if !shipped {
            continue;
        }
        let c = coverage(lang);
        if !c.is_complete() {
            gaps += c.problems.len();
            report.push(format!(
                "{}: {}/{} ({:.1}%)\n  {}",
                lang.code(),
                c.done,
                c.total,
                c.percent(),
                c.problems.join("\n  ")
            ));
        }
    }
    assert!(
        report.is_empty(),
        "{gaps} translation problem(s):\n{}",
        report.join("\n")
    );
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// A basic TOML string: `"…"` with escapes, optionally followed by a comment.
fn parse_string(s: &str) -> Result<String, String> {
    let mut chars = s.chars();
    if chars.next() != Some('"') {
        return Err("value must be a double-quoted string".into());
    }
    let mut out = String::new();
    loop {
        match chars.next() {
            None => return Err("unterminated string".into()),
            Some('"') => break,
            Some('\\') => match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                other => return Err(format!("unsupported escape \\{}", other.unwrap_or(' '))),
            },
            Some(c) => out.push(c),
        }
    }
    let tail = chars.as_str().trim();
    if !tail.is_empty() && !tail.starts_with('#') {
        return Err(format!("unexpected text after the value: {tail:?}"));
    }
    Ok(out)
}

#[cfg(test)]
mod split_tests {
    use super::*;

    #[test]
    fn kit_and_product_catalogs_are_disjoint() {
        validate().unwrap();
    }

    #[test]
    fn a_key_in_both_catalogs_is_an_error() {
        let err =
            check_pair("[common]\nsave = \"Save\"\n", "[common]\nsave = \"Keep\"\n").unwrap_err();
        assert!(err.contains("common.save"), "{err}");
        assert!(check_pair("[common]\nsave = \"Save\"\n", "[detail]\nsave = \"Keep\"\n").is_ok());
        assert!(check_pair("[common\n", "").is_err());
    }

    fn files(dir: &std::path::Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                files(&path, ext, out);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(path);
            }
        }
    }

    /// `(key, plural)` for every `call("key"` in `src`.
    fn keys(src: &str, calls: &[&str]) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        for call in calls {
            let needle = format!("{call}(\"");
            let mut rest = src;
            while let Some(i) = rest.find(&needle) {
                let before = rest[..i].chars().next_back();
                rest = &rest[i + needle.len()..];
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                if let Some(end) = rest.find('"') {
                    out.push((rest[..end].to_string(), matches!(*call, "trn" | "i18n::tn")));
                }
            }
        }
        out
    }

    #[test]
    fn product_keys_are_the_products_alone() {
        for code in ["en", "nb", "ar", "ja", "zh-CN"] {
            let core = parse(core_text(code)).unwrap();
            for key in PRODUCT_KEYS.iter().chain(GENERAL_PRODUCT_KEYS) {
                assert!(!core.contains_key(*key), "{code}: the kit defines {key}");
            }
        }
        let all: String = PRODUCT_KEYS
            .iter()
            .map(|k| {
                let (section, key) = k.split_once('.').unwrap();
                format!("[{section}]\n{key} = \"x\"\n")
            })
            .collect();
        assert!(check_product_keys(&all).is_ok());
        let err = check_product_keys("[login]\nsubtitle = \"Hi\"\n").unwrap_err();
        assert!(
            err.contains("error.back") && !err.contains("login.subtitle"),
            "{err}"
        );
    }

    /// The kit's own strings name nothing of one product: no product name,
    /// no monitors or dashboards. Notification providers' own fields may
    /// use their service's words (a Pushover dashboard, PagerDuty
    /// incidents).
    #[test]
    fn the_kits_strings_name_no_products_concepts() {
        let core = parse(core_text("en")).unwrap();
        let mut found = Vec::new();
        for (key, value) in &core {
            if key.starts_with("notif.") && key.matches('.').count() > 1 {
                continue;
            }
            let lower = value.to_lowercase();
            for word in [
                "vigilant",
                "vole",
                "monitor",
                "dashboard",
                "status page",
                "heartbeat",
                "vv_",
            ] {
                if lower.contains(word) {
                    found.push(format!("{key}: {value}"));
                }
            }
        }
        found.sort();
        assert!(found.is_empty(), "{}", found.join("\n"));
    }

    /// The test configuration ships Vigilant Vole's catalogs, all complete.
    #[test]
    fn full_coverage_with_the_test_configuration() {
        assert_full_coverage();
        let en = coverage(Lang::EN);
        assert!(en.is_complete() && en.total > 1000, "{en:?}");
    }

    #[test]
    fn coverage_reads_plural_forms_and_placeholders() {
        let names = placeholder_names("{a} and {b}, {a}");
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(
            plural_parts("count.users.few").map(|(b, _)| b),
            Some("count.users")
        );
        assert_eq!(plural_parts("common.save"), None);
    }

    /// The kit's templates and code find every string they use in the kit's
    /// own catalog, so a product with none of Vigilant Vole's strings (Keen
    /// Kestrel) shows them too. Keys a product hands over (navigation and
    /// settings tab labels, the API reference subtitle, [`PRODUCT_KEYS`])
    /// are its own.
    #[test]
    fn the_kit_uses_only_its_own_strings() {
        let core = parse(core_text("en")).unwrap();
        let has = |key: &str, plural: bool| {
            if PRODUCT_KEYS.contains(&key) || GENERAL_PRODUCT_KEYS.contains(&key) {
                true
            } else if plural {
                core.contains_key(&format!("{key}.one"))
                    && core.contains_key(&format!("{key}.other"))
            } else {
                core.contains_key(key)
            }
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut missing = Vec::new();
        let mut checked = 0;
        let mut sources = Vec::new();
        files(&root.join("templates"), "html", &mut sources);
        let templates = sources.len();
        files(&root.join("src"), "rs", &mut sources);
        for (n, file) in sources.iter().enumerate() {
            if file.ends_with("i18n/mod.rs")
                || file.ends_with("tests.rs")
                || file.ends_with("testing.rs")
            {
                continue;
            }
            let src = std::fs::read_to_string(file).unwrap();
            let calls: &[&str] = if n < templates {
                &["tr", "tr1", "tr2", "tr3", "trn"]
            } else {
                &["i18n::t", "i18n::tf", "i18n::tn", "t_in(lang, "]
            };
            for (key, plural) in keys(&src, calls) {
                checked += 1;
                if !has(&key, plural) {
                    missing.push(format!("{}: {key}", file.display()));
                }
            }
        }
        assert!(checked > 200, "only {checked} keys found");
        assert!(
            missing.is_empty(),
            "not in the kit's catalog:\n{}",
            missing.join("\n")
        );
    }
}
