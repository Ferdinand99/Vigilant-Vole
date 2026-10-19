use std::collections::BTreeSet;
use std::path::Path;

use super::catalog::{self, SOURCES};
use super::middleware::parse_accept_language;
use super::*;

#[test]
fn every_catalog_parses() {
    for (lang, src) in SOURCES {
        if let Err(err) = catalog::parse(src) {
            panic!("locales/{}.toml: {err}", lang.code());
        }
    }
}

#[test]
fn catalogs_have_identical_keys_and_no_empty_values() {
    let en: BTreeSet<&String> = catalog::all(Lang::En).keys().collect();
    for lang in Lang::ALL {
        let cat = catalog::all(lang);
        let keys: BTreeSet<&String> = cat.keys().collect();
        let missing: Vec<_> = en.difference(&keys).collect();
        let extra: Vec<_> = keys.difference(&en).collect();
        assert!(missing.is_empty(), "{lang}: missing keys {missing:?}");
        assert!(extra.is_empty(), "{lang}: keys not in en {extra:?}");
        let empty: Vec<_> = cat
            .iter()
            .filter(|(_, v)| v.trim().is_empty())
            .map(|(k, _)| k)
            .collect();
        assert!(empty.is_empty(), "{lang}: empty values {empty:?}");
    }
}

/// Placeholders must match between languages, or a value would go missing.
#[test]
fn placeholders_match_across_languages() {
    let names = |s: &str| -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = s;
        while let Some(start) = rest.find('{') {
            let Some(len) = rest[start..].find('}') else { break };
            out.insert(rest[start + 1..start + len].to_string());
            rest = &rest[start + len..];
        }
        out
    };
    for (key, en) in catalog::all(Lang::En) {
        for lang in Lang::ALL {
            let other = catalog::all(lang).get(key).map_or("", String::as_str);
            assert_eq!(names(en), names(other), "{lang}: placeholders differ in {key}");
        }
    }
}

#[test]
fn plural_keys_come_in_pairs() {
    for key in catalog::all(Lang::En).keys() {
        if let Some(base) = key.strip_suffix(".one") {
            assert!(
                catalog::all(Lang::En).contains_key(&format!("{base}.other")),
                "{key} has no .other"
            );
        }
    }
}

fn template_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("templates dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            template_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "html") {
            out.push(path);
        }
    }
}

/// Keys used as string literals in templates (`tr("…")`, `tr1("…"`, `trn("…"`)
/// and in Rust (`i18n::t("…")`, `tf("…"`, `tn("…"`).
fn referenced_keys(src: &str, calls: &[&str]) -> Vec<(String, bool)> {
    let mut keys = Vec::new();
    for call in calls {
        let needle = format!("{call}(\"");
        let mut rest = src;
        while let Some(i) = rest.find(&needle) {
            // Skip identifiers that merely end in the call name (e.g. `str(`).
            let before = rest[..i].chars().next_back();
            rest = &rest[i + needle.len()..];
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            if let Some(end) = rest.find('"') {
                let plural = matches!(*call, "trn" | "i18n::tn" | "i18n::tnf");
                keys.push((rest[..end].to_string(), plural));
            }
        }
    }
    keys
}

fn assert_keys_exist(file: &Path, src: &str, calls: &[&str]) -> usize {
    let cat = catalog::all(Lang::En);
    let mut n = 0;
    for (key, plural) in referenced_keys(src, calls) {
        n += 1;
        if plural {
            assert!(
                cat.contains_key(&format!("{key}.one")) && cat.contains_key(&format!("{key}.other")),
                "{}: plural key {key}.one/.other missing",
                file.display()
            );
        } else {
            assert!(cat.contains_key(&key), "{}: key {key} missing", file.display());
        }
    }
    n
}

#[test]
fn every_key_used_in_templates_exists() {
    let mut files = Vec::new();
    template_files(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/templates")), &mut files);
    assert!(!files.is_empty());
    let mut total = 0;
    for file in files {
        let src = std::fs::read_to_string(&file).expect("read template");
        total += assert_keys_exist(&file, &src, &["tr", "tr1", "tr2", "tr3", "trn"]);
    }
    assert!(total > 100, "only {total} keys referenced from templates");
}

#[test]
fn every_key_used_in_rust_exists() {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("src dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut files);
    for file in files {
        // The i18n module's own docs and tests use example keys.
        if file.parent().is_some_and(|d| d.ends_with("i18n")) {
            continue;
        }
        let src = std::fs::read_to_string(&file).expect("read source");
        assert_keys_exist(&file, &src, &["i18n::t", "i18n::tf", "i18n::tn", "i18n::tnf"]);
    }
}

#[test]
fn lookup_falls_back_and_scopes_apply() {
    assert_eq!(current(), Lang::En);
    assert_eq!(t("nav.dashboard"), "Dashboard");
    sync_scope(Lang::Nb, || {
        assert_eq!(current(), Lang::Nb);
        assert_eq!(t("nav.dashboard"), "Oversikt");
        assert_eq!(status_label("up"), "Oppe");
        assert_eq!(status_label("weird"), "weird");
    });
    assert_eq!(t("no.such.key"), "no.such.key");
}

#[tokio::test]
async fn async_scope_sets_language() {
    let got = scope(Lang::Nb, async { current() }).await;
    assert_eq!(got, Lang::Nb);
    assert_eq!(current(), Lang::En);
}

#[test]
fn interpolation_and_plurals() {
    assert_eq!(interpolate("Delete “{name}”?", &[("name", "API")]), "Delete “API”?");
    assert_eq!(interpolate("{a} and {a}", &[("a", "x")]), "x and x");
    assert_eq!(tn_in(Lang::En, "count.monitors", 1), "1 monitor");
    assert_eq!(tn_in(Lang::En, "count.monitors", 2), "2 monitors");
    assert_eq!(tn_in(Lang::Nb, "count.monitors", 1), "1 overvåking");
    assert_eq!(tn_in(Lang::Nb, "count.monitors", 0), "0 overvåkinger");
    assert_eq!(tn_in(Lang::Nb, "count.monitors", 12_000), "12\u{a0}000 overvåkinger");
}

#[test]
fn language_codes_and_accept_language() {
    assert_eq!(Lang::from_code("nb"), Some(Lang::Nb));
    assert_eq!(Lang::from_code("no"), Some(Lang::Nb));
    assert_eq!(Lang::from_code("nn-NO"), Some(Lang::Nb));
    assert_eq!(Lang::from_code("en_GB"), Some(Lang::En));
    assert_eq!(Lang::from_code("de"), None);
    assert_eq!(Lang::from_code(""), None);
    assert_eq!(parse_accept_language("nb-NO,nb;q=0.9,en;q=0.8"), Some(Lang::Nb));
    assert_eq!(parse_accept_language("de-DE, en;q=0.5, no;q=0.7"), Some(Lang::Nb));
    assert_eq!(parse_accept_language("en-US,en;q=0.9"), Some(Lang::En));
    assert_eq!(parse_accept_language("fr, de"), None);
    assert_eq!(parse_accept_language("nb;q=0, en"), Some(Lang::En));
    assert_eq!(parse_accept_language(""), None);
}

#[test]
fn numbers_follow_the_locale() {
    assert_eq!(fmt_int_in(Lang::En, 1_234_567), "1,234,567");
    assert_eq!(fmt_int_in(Lang::Nb, 1_234), "1234");
    assert_eq!(fmt_int_in(Lang::Nb, 12_345), "12\u{a0}345");
    assert_eq!(fmt_int_in(Lang::En, -1_000), "-1,000");
    assert_eq!(fmt_dec_in(Lang::En, 99.951, 2), "99.95");
    assert_eq!(fmt_dec_in(Lang::Nb, 99.951, 2), "99,95");
    assert_eq!(fmt_dec_in(Lang::Nb, -0.001, 2), "0,00");
    assert_eq!(fmt_pct2_in(Lang::En, 99.999), "99.99%");
    assert_eq!(fmt_pct2_in(Lang::Nb, 99.95), "99,95\u{a0}%");
    assert_eq!(fmt_pct2_in(Lang::Nb, 100.0), "100\u{a0}%");
}

#[test]
fn durations_follow_the_locale() {
    assert_eq!(fmt_duration_in(Lang::En, 45), "45 sec");
    assert_eq!(fmt_duration_in(Lang::En, 3 * 3600 + 240), "3 h 4 min");
    assert_eq!(fmt_duration_in(Lang::En, 2 * 86_400 + 5 * 3600), "2 d 5 h");
    assert_eq!(fmt_duration_in(Lang::Nb, 45), "45 sek");
    assert_eq!(fmt_duration_in(Lang::Nb, 3 * 3600 + 240), "3 t 4 min");
    assert_eq!(fmt_duration_in(Lang::Nb, 86_400), "1 d");
    assert_eq!(fmt_relative_in(Lang::En, 10), "just now");
    assert_eq!(fmt_relative_in(Lang::En, 190), "3 min ago");
    assert_eq!(fmt_relative_in(Lang::Nb, 190), "for 3 min siden");
    assert_eq!(fmt_relative_in(Lang::Nb, -7200), "om 2 t");
}

#[test]
fn dates_follow_the_locale() {
    let date = jiff::civil::date(2026, 10, 19);
    assert_eq!(fmt_day_month_in(Lang::En, date), "Oct 19");
    assert_eq!(fmt_day_month_in(Lang::Nb, date), "19. okt.");
    assert_eq!(fmt_day_month_long_in(Lang::Nb, date), "19. oktober");
    assert_eq!(fmt_date_in(Lang::En, date), "Oct 19, 2026");
    assert_eq!(fmt_date_in(Lang::Nb, jiff::civil::date(2026, 3, 1)), "1. mars 2026");
    let z = date.at(14, 5, 0, 0).to_zoned(jiff::tz::TimeZone::UTC).expect("zoned");
    assert_eq!(fmt_datetime_in(Lang::En, &z), "Oct 19, 2026, 14:05");
    assert_eq!(fmt_datetime_in(Lang::Nb, &z), "19. okt. 2026, 14:05");
    assert_eq!(fmt_short_datetime_in(Lang::Nb, &z), "19. okt., 14:05");
    assert_eq!(weekday_name_in(Lang::Nb, 1, true), "mandag");
    assert_eq!(weekday_name_in(Lang::En, 7, false), "Sun");
    assert_eq!(fmt_weekday_day_month_in(Lang::Nb, date), "man. 19. okt.");
}

#[test]
fn js_bundle_is_embeddable_json() {
    let json = js_bundle(Lang::Nb);
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert_eq!(parsed["live"], "Direkte");
    assert!(!json.contains("</"));
}

#[test]
fn parser_rejects_bad_lines() {
    assert!(catalog::parse("[a]\nx = \"ok\" # note\n").is_ok());
    assert!(catalog::parse("x = unquoted").is_err());
    assert!(catalog::parse("x = \"open").is_err());
    assert!(catalog::parse("x = \"a\"\nx = \"b\"").is_err());
    assert!(catalog::parse("[bad section]").is_err());
    let parsed = catalog::parse("[s]\nk = \"say \\\"hi\\\" \\\\ ok\"").expect("parse");
    assert_eq!(parsed["s.k"], "say \"hi\" \\ ok");
}
