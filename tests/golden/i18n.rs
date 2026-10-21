//! Interface languages: the registry, `Accept-Language` negotiation, the
//! `js.*` bundle for `app.js`, and the key set of every catalog (so that
//! moving or splitting catalogs can't silently drop keys).

use axum::http::{HeaderMap, HeaderValue, header};
use insta::assert_snapshot;
use vigilant_vole::i18n::{self, LANGS, Lang};

use crate::norm;

/// Full keys (`section.key`) of a compiled-in catalog, sorted. The catalogs
/// use a small TOML subset (see `src/i18n/catalog.rs`); only the key side is
/// read here, the values come from the app's own lookup.
fn catalog_keys(source: &str) -> Vec<String> {
    let mut section = String::new();
    let mut keys = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            section = name.trim_end_matches(']').trim().to_string();
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            let key = key.trim();
            keys.push(if section.is_empty() {
                key.to_string()
            } else {
                format!("{section}.{key}")
            });
        }
    }
    keys.sort();
    keys
}

#[test]
fn language_registry() {
    let mut out = String::new();
    for info in LANGS {
        out.push_str(&format!(
            "{} | {} | {} | dir={} | beta={} | plural={:?} | aliases={:?}\n",
            info.code,
            info.english_name,
            info.native_name,
            info.dir.as_str(),
            info.beta,
            info.plural,
            info.aliases,
        ));
    }
    out.push_str(&format!("\npickers: {:?}\n", i18n::language_choices()));
    assert_snapshot!("i18n_registry", out);
}

#[test]
fn accept_language_negotiation() {
    let mut out = String::new();
    for value in [
        "",
        "*",
        "en",
        "en-US,en;q=0.9",
        "ar",
        "ar-EG,ar;q=0.9,en;q=0.8",
        "he-IL",
        "fa-IR,fa;q=0.9",
        "nb-NO,nb;q=0.9,no;q=0.8",
        "no",
        "nn",
        "da-DK, nb;q=0.9",
        "pt",
        "pt-PT",
        "pt-br",
        "zh",
        "zh-CN",
        "zh-HK",
        "zh-Hant-TW",
        "zh-TW",
        "de-AT;q=0.5, fr;q=0.8",
        "xx, fr;q=0.1",
        "en;q=0, de",
        "sr-Latn, hr;q=0.9",
        "en_GB",
    ] {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static(value));
        let lang = i18n::negotiate(&h);
        out.push_str(&format!(
            "{value:?} -> {}\n",
            lang.map(|l| format!("{} ({})", l.code(), l.dir().as_str()))
                .unwrap_or_else(|| "none (site default, then en)".into())
        ));
    }
    assert_snapshot!("i18n_negotiation", out);
}

#[test]
fn js_bundle_english() {
    let raw = i18n::js_bundle(Lang::EN);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("js bundle is JSON");
    // The exact encoding: compact JSON, sorted keys, `</` and `<!--` escaped
    // so it can sit inside a <script> element.
    let expected = serde_json::to_string(&parsed)
        .unwrap()
        .replace("</", "<\\/")
        .replace("<!--", "<\\!--");
    assert_eq!(raw, expected, "js_bundle encoding changed");
    let mut out = format!(
        "keys: {}\nsha256: {}\n\n",
        parsed.as_object().unwrap().len(),
        norm::sha256(raw.as_bytes())
    );
    out.push_str(&serde_json::to_string_pretty(&parsed).unwrap());
    let rtl = i18n::js_bundle(Lang::from_code("ar").unwrap());
    let rtl_keys = serde_json::from_str::<serde_json::Value>(&rtl).unwrap();
    out.push_str(&format!(
        "\n\nar: keys {}, sha256 {}\n",
        rtl_keys.as_object().unwrap().len(),
        norm::sha256(rtl.as_bytes())
    ));
    assert_snapshot!("i18n_js_bundle_en", out);
}

/// Per language: how many keys its catalog has, which English keys it lacks
/// (those fall back to English), and hashes of the key set and of every
/// translated value as the app looks it up.
#[test]
fn catalog_keys_per_language() {
    let english = catalog_keys(Lang::EN.info().source);
    let mut out = String::new();
    for lang in Lang::all() {
        let keys = catalog_keys(lang.info().source);
        let mut values = String::new();
        for key in &keys {
            let value = i18n::lookup(lang, key)
                .unwrap_or_else(|| panic!("{}: {key} is in the file but not found", lang.code()));
            values.push_str(&format!("{key}={value}\n"));
        }
        let missing = english.iter().filter(|k| !keys.contains(k)).count();
        // Extra keys are plural forms English doesn't have (few, many…).
        let extra = keys.iter().filter(|k| !english.contains(k)).count();
        out.push_str(&format!(
            "{:6} keys={:5} missing_vs_en={:3} extra_vs_en={:3} keys_sha256={} values_sha256={}\n",
            lang.code(),
            keys.len(),
            missing,
            extra,
            &norm::sha256(keys.join("\n").as_bytes())[..16],
            &norm::sha256(values.as_bytes())[..16],
        ));
    }
    assert_snapshot!("i18n_catalog_summary", out);
    assert_snapshot!("i18n_english_keys", english.join("\n"));
}
