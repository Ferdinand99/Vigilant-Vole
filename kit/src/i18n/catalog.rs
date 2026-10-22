//! Locale catalogs, compiled in and parsed once. A language's catalog is
//! the kit's core strings (`kit/locales/<code>.toml`) plus the product's
//! (registered in its `KitConfig`); a key may be defined in only one of
//! them ([`validate`] checks, at start-up).
//!
//! The files use a small TOML subset: `[section]` headers (dotted names
//! allowed), `key = "value"` lines with `\"`, `\\`, `\n` and `\u{…}`-free
//! escapes, `#` comments and blank lines. The full key is `section.key`.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::Lang;
use super::lang::LANGS;

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

/// Checks every language's catalogs: both parse, and no key is defined by
/// the kit and the product alike. Called when the configuration is
/// installed, so a broken or clashing catalog stops the start.
pub fn validate() -> Result<(), String> {
    let mut problems = Vec::new();
    for info in LANGS {
        if let Err(e) = check_pair(info.source.core(), info.source.product()) {
            problems.push(format!("{}: {e}", info.code));
        }
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

    /// The kit's templates and code find every string they use in the kit's
    /// own catalog, so a product with none of Vigilant Vole's strings (Keen
    /// Kestrel) shows them too. Keys a product hands over (navigation and
    /// settings tab labels, the API reference subtitle) are its own.
    #[test]
    fn the_kit_uses_only_its_own_strings() {
        let core = parse(core_text("en")).unwrap();
        let has = |key: &str, plural: bool| {
            if plural {
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
