//! Locale catalogs: `locales/*.toml`, compiled in and parsed once.
//!
//! The files use a small TOML subset: `[section]` headers (dotted names
//! allowed), `key = "value"` lines with `\"`, `\\`, `\n` and `\u{…}`-free
//! escapes, `#` comments and blank lines. The full key is `section.key`.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::Lang;

pub(super) const SOURCES: [(Lang, &str); 2] = [
    (Lang::En, include_str!("../../locales/en.toml")),
    (Lang::Nb, include_str!("../../locales/nb.toml")),
];

type Catalog = HashMap<String, String>;

static CATALOGS: OnceLock<[Catalog; 2]> = OnceLock::new();

fn catalogs() -> &'static [Catalog; 2] {
    CATALOGS.get_or_init(|| {
        SOURCES.map(|(lang, src)| {
            parse(src).unwrap_or_else(|err| {
                // The test suite parses every catalog, so this only happens
                // with a hand-edited release build.
                tracing::error!(lang = lang.code(), %err, "invalid locale file");
                HashMap::new()
            })
        })
    })
}

pub(super) fn get(lang: Lang, key: &str) -> Option<&'static str> {
    catalogs()[lang.index()].get(key).map(String::as_str)
}

/// Keys of `lang` that start with `prefix`, sorted.
pub(super) fn keys_with_prefix(lang: Lang, prefix: &str) -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = catalogs()[lang.index()]
        .keys()
        .filter(|k| k.starts_with(prefix))
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[cfg(test)]
pub(super) fn all(lang: Lang) -> &'static Catalog {
    &catalogs()[lang.index()]
}

/// Parses one locale file. Errors name the 1-based line.
pub(super) fn parse(src: &str) -> Result<Catalog, String> {
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
