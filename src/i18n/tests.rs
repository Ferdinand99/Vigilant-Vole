use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::catalog;
use super::middleware::parse_accept_language;
use super::*;

/// Languages whose locale file must be complete. Every language except
/// English (the source) and the test-only RTL language.
fn translated() -> impl Iterator<Item = Lang> {
    Lang::all().filter(|l| *l != Lang::EN && l.code() != "ar")
}

fn plural_base(key: &str) -> Option<&str> {
    let (base, category) = key.rsplit_once('.')?;
    Plural::ALL.iter().any(|p| p.as_str() == category).then_some(base)
}

/// Plural bases of the English source: keys that come as `.one` + `.other`.
fn english_plural_bases() -> BTreeSet<String> {
    let en = catalog::all(Lang::EN);
    en.keys()
        .filter_map(|k| k.strip_suffix(".one"))
        .filter(|base| en.contains_key(&format!("{base}.other")))
        .map(str::to_string)
        .collect()
}

/// Keys of English that are not plural variants.
fn english_plain_keys() -> BTreeSet<String> {
    let bases = english_plural_bases();
    catalog::all(Lang::EN)
        .keys()
        .filter(|k| !plural_base(k).is_some_and(|b| bases.contains(b)))
        .cloned()
        .collect()
}

fn placeholder_names(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else { break };
        out.insert(rest[start + 1..start + len].to_string());
        rest = &rest[start + len..];
    }
    out
}

#[test]
fn every_catalog_parses() {
    for lang in Lang::all() {
        if let Err(err) = catalog::parse(catalog::source(lang)) {
            panic!("locales/{}.toml: {err}", lang.code());
        }
    }
}

#[test]
fn every_language_has_a_locale_file_named_after_its_code() {
    let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/locales"));
    for lang in Lang::all().filter(|l| l.code() != "ar") {
        assert!(dir.join(format!("{}.toml", lang.code())).is_file(), "locales/{}.toml is missing", lang.code());
    }
    let codes: BTreeSet<&str> = LANGS.iter().map(|l| l.code).collect();
    assert_eq!(codes.len(), LANGS.len(), "duplicate language codes");
    assert_eq!(LANGS[0].code, "en", "English is the first entry (source and fallback)");
}

#[test]
fn no_language_has_keys_english_lacks_or_empty_values() {
    let en = english_plain_keys();
    let bases = english_plural_bases();
    for lang in translated() {
        let cat = catalog::all(lang);
        let extra: Vec<_> = cat
            .keys()
            .filter(|k| !en.contains(*k) && !plural_base(k).is_some_and(|b| bases.contains(b)))
            .collect();
        assert!(extra.is_empty(), "{lang}: keys not in en {extra:?}");
        let empty: Vec<_> = cat.iter().filter(|(_, v)| v.trim().is_empty()).map(|(k, _)| k).collect();
        assert!(empty.is_empty(), "{lang}: empty values {empty:?}");
    }
}

/// Placeholders must match English, or a value would go missing.
#[test]
fn placeholders_match_english() {
    let bases = english_plural_bases();
    let en = catalog::all(Lang::EN);
    for lang in translated() {
        for (key, value) in catalog::all(lang) {
            if let Some(base) = plural_base(key).filter(|b| bases.contains(*b)) {
                // A plural variant carries the placeholders of English `.one` or `.other`.
                let ok = [format!("{base}.one"), format!("{base}.other")]
                    .iter()
                    .any(|k| placeholder_names(&en[k]) == placeholder_names(value));
                assert!(ok, "{lang}: placeholders of {key} don't match English");
            } else if let Some(source) = en.get(key) {
                assert_eq!(placeholder_names(source), placeholder_names(value), "{lang}: placeholders differ in {key}");
            }
        }
    }
}

/// Every plural key a language translates has exactly the categories its CLDR
/// rule needs (and for English: one and other).
#[test]
fn plural_categories_match_each_languages_rule() {
    let bases = english_plural_bases();
    assert!(bases.len() >= 20, "found {} plural keys", bases.len());
    for lang in Lang::all().filter(|l| l.code() != "ar") {
        let cat = catalog::all(lang);
        let want: BTreeSet<Plural> = lang.plural_rule().categories().iter().copied().collect();
        for base in &bases {
            let have: BTreeSet<Plural> = Plural::ALL.into_iter().filter(|p| cat.contains_key(&format!("{base}.{}", p.as_str()))).collect();
            if have.is_empty() && lang != Lang::EN {
                continue; // not translated yet: English forms are used
            }
            assert_eq!(have, want, "{lang}: plural forms of {base}");
        }
    }
}

#[test]
fn plural_rules_follow_cldr() {
    use Plural::{Few, Many, One, Other, Two, Zero};
    let check = |rule: PluralRule, cases: &[(i64, Plural)]| {
        for (n, want) in cases {
            assert_eq!(rule.category(*n), *want, "{rule:?} for {n}");
        }
    };
    check(PluralRule::Other, &[(0, Other), (1, Other), (7, Other)]);
    check(PluralRule::OneOther, &[(0, Other), (1, One), (2, Other), (21, Other), (1_000_000, Other)]);
    check(PluralRule::FrenchLike, &[(0, One), (1, One), (2, Other), (999_999, Other), (1_000_000, Many), (5_000_000, Many)]);
    check(PluralRule::SpanishLike, &[(0, Other), (1, One), (2, Other), (1_000_000, Many), (1_000_001, Other)]);
    check(
        PluralRule::Polish,
        &[(0, Many), (1, One), (2, Few), (4, Few), (5, Many), (12, Many), (14, Many), (21, Many), (22, Few), (24, Few), (25, Many), (112, Many), (122, Few), (1000, Many)],
    );
    check(
        PluralRule::EastSlavic,
        &[(0, Many), (1, One), (2, Few), (5, Many), (11, Many), (12, Many), (21, One), (22, Few), (25, Many), (101, One), (111, Many)],
    );
    check(PluralRule::CzechSlovak, &[(0, Other), (1, One), (2, Few), (4, Few), (5, Other), (21, Other)]);
    check(
        PluralRule::Arabic,
        &[(0, Zero), (1, One), (2, Two), (3, Few), (10, Few), (11, Many), (99, Many), (100, Other), (102, Other), (103, Few), (111, Many)],
    );
    // Negative counts use their magnitude.
    assert_eq!(PluralRule::OneOther.category(-1), One);
    // Every category a rule can return for a whole number is one it requires.
    for rule in [
        PluralRule::Other,
        PluralRule::OneOther,
        PluralRule::FrenchLike,
        PluralRule::SpanishLike,
        PluralRule::Polish,
        PluralRule::EastSlavic,
        PluralRule::CzechSlovak,
        PluralRule::Arabic,
    ] {
        let allowed: BTreeSet<Plural> = rule.categories().iter().copied().collect();
        assert!(allowed.contains(&Other), "{rule:?} requires `other`");
        for n in (0..2_100).chain([1_000_000, 2_000_000, 1_000_001]) {
            assert!(allowed.contains(&rule.category(n)), "{rule:?}: {n} is {:?}", rule.category(n));
        }
    }
}

#[test]
fn plural_lookup_uses_the_language_rule_and_falls_back() {
    assert_eq!(tn_in(Lang::EN, "count.monitors", 1), "1 monitor");
    assert_eq!(tn_in(Lang::EN, "count.monitors", 2), "2 monitors");
    assert_eq!(tn_in(Lang::NB, "count.monitors", 1), "1 overvåking");
    assert_eq!(tn_in(Lang::NB, "count.monitors", 0), "0 overvåkinger");
    assert_eq!(tn_in(Lang::NB, "count.monitors", 12_000), "12\u{a0}000 overvåkinger");
    // The test-only Arabic has no file: English forms (and rule) are used, numbers in its style.
    let ar = Lang::from_code("ar").expect("test language");
    assert_eq!(tn_in(ar, "count.monitors", 1), "1 monitor");
    assert_eq!(tn_in(ar, "count.monitors", 3), "3 monitors");
    // Unknown keys render as number + key, never panic.
    assert_eq!(tn_in(Lang::EN, "no.such.plural", 3), "3 no.such.plural");
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
    let cat = catalog::all(Lang::EN);
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
    assert_eq!(current(), Lang::EN);
    assert_eq!(t("nav.dashboard"), "Dashboard");
    sync_scope(Lang::NB, || {
        assert_eq!(current(), Lang::NB);
        assert_eq!(t("nav.dashboard"), "Oversikt");
        assert_eq!(status_label("up"), "Oppe");
        assert_eq!(status_label("weird"), "weird");
    });
    assert_eq!(t("no.such.key"), "no.such.key");
    // A key a language lacks falls back to English, per key (the test-only
    // Arabic file is empty).
    let ar = Lang::from_code("ar").expect("test language");
    assert_eq!(lookup(ar, "nav.dashboard"), Some("Dashboard"));
}

#[tokio::test]
async fn async_scope_sets_language() {
    let got = scope(Lang::NB, async { current() }).await;
    assert_eq!(got, Lang::NB);
    assert_eq!(current(), Lang::EN);
}

#[test]
fn interpolation() {
    assert_eq!(interpolate("Delete “{name}”?", &[("name", "API")]), "Delete “API”?");
    assert_eq!(interpolate("{a} and {a}", &[("a", "x")]), "x and x");
}

#[test]
fn language_codes_and_accept_language() {
    let code = |tag: &str| Lang::from_code(tag).map(Lang::code);
    // Bokmål and its relatives.
    for tag in ["nb", "no", "nn-NO", "nb-NO", "NO", "nb_NO"] {
        assert_eq!(code(tag), Some("nb"), "{tag}");
    }
    assert_eq!(code("en_GB"), Some("en"));
    // Regions and reserves.
    assert_eq!(code("pt"), Some("pt-BR"));
    assert_eq!(code("pt-PT"), Some("pt-BR"));
    assert_eq!(code("pt-br"), Some("pt-BR"));
    assert_eq!(code("pt_BR"), Some("pt-BR"));
    assert_eq!(code("de-AT"), Some("de"));
    assert_eq!(code("DE-ch"), Some("de"));
    assert_eq!(code("fr-CA"), Some("fr"));
    assert_eq!(code("es-419"), Some("es"));
    for (tag, want) in [("it", "it"), ("nl-BE", "nl"), ("pl-PL", "pl"), ("sv-FI", "sv"), ("da-DK", "da")] {
        assert_eq!(code(tag), Some(want), "{tag}");
    }
    assert_eq!(code("ja"), None);
    assert_eq!(code("*"), None);
    assert_eq!(code(""), None);
    // The picker only accepts canonical codes.
    assert_eq!(Lang::from_exact_code("pt-BR"), Some(Lang::from_code("pt").unwrap()));
    assert_eq!(Lang::from_exact_code("pt"), None);
    assert_eq!(Lang::from_exact_code("PT-br"), None);

    let negotiate = |header: &str| parse_accept_language(header).map(Lang::code);
    assert_eq!(negotiate("nb-NO,nb;q=0.9,en;q=0.8"), Some("nb"));
    assert_eq!(negotiate("de-DE, en;q=0.5, no;q=0.7"), Some("de"));
    assert_eq!(negotiate("en-US,en;q=0.9"), Some("en"));
    assert_eq!(negotiate("pt-PT,pt;q=0.9,en;q=0.8"), Some("pt-BR"));
    assert_eq!(negotiate("ja, de-AT;q=0.8"), Some("de"));
    assert_eq!(negotiate("fr-CH, fr;q=0.9, en;q=0.8"), Some("fr"));
    assert_eq!(negotiate("zh, ja"), None);
    assert_eq!(negotiate("nb;q=0, en"), Some("en"));
    assert_eq!(negotiate("*"), None);
    assert_eq!(negotiate(""), None);
}

#[test]
fn registry_entries_are_consistent() {
    for lang in Lang::all() {
        let info = lang.info();
        assert!(!info.native_name.is_empty() && !info.english_name.is_empty());
        assert_eq!(Lang::from_exact_code(info.code), Some(lang));
        assert!(info.numbers.min_grouping >= 1);
        assert_ne!(info.numbers.decimal, info.numbers.group, "{lang}");
    }
    let choices = language_choices();
    assert!(choices.iter().any(|(code, label)| *code == "en" && label == "English"));
    assert!(choices.iter().any(|(code, label)| *code == "de" && label == "Deutsch (beta)"));
    assert!(choices.iter().any(|(code, label)| *code == "pt-BR" && label == "Português (Brasil) (beta)"));
    assert!(choices.iter().any(|(code, label)| *code == "nb" && label == "Norsk bokmål"), "{choices:?}");
    assert_eq!(Lang::EN.dir().as_str(), "ltr");
    assert_eq!(Lang::from_code("ar").unwrap().dir().as_str(), "rtl");
}

#[test]
fn numbers_follow_each_languages_conventions() {
    let int = |code: &str, n: i64| fmt_int_in(Lang::from_code(code).unwrap(), n);
    let dec = |code: &str, v: f64| fmt_dec_in(Lang::from_code(code).unwrap(), v, 2);
    let pct = |code: &str, v: f64| fmt_pct2_in(Lang::from_code(code).unwrap(), v);
    // (code, 1 234, 12 345, 1 234 567) after CLDR.
    let nb = '\u{a0}';
    let nn = '\u{202f}';
    for (code, a, b, c) in [
        ("en", "1,234".to_string(), "12,345".to_string(), "1,234,567".to_string()),
        ("nb", "1234".into(), format!("12{nb}345"), format!("1{nb}234{nb}567")),
        ("de", "1.234".into(), "12.345".into(), "1.234.567".into()),
        ("fr", format!("1{nn}234"), format!("12{nn}345"), format!("1{nn}234{nn}567")),
        ("es", "1234".into(), "12.345".into(), "1.234.567".into()),
        ("it", "1.234".into(), "12.345".into(), "1.234.567".into()),
        ("nl", "1.234".into(), "12.345".into(), "1.234.567".into()),
        ("pt-BR", "1.234".into(), "12.345".into(), "1.234.567".into()),
        ("pl", "1234".into(), format!("12{nb}345"), format!("1{nb}234{nb}567")),
        ("sv", format!("1{nb}234"), format!("12{nb}345"), format!("1{nb}234{nb}567")),
        ("da", "1.234".into(), "12.345".into(), "1.234.567".into()),
    ] {
        assert_eq!((int(code, 1_234), int(code, 12_345), int(code, 1_234_567)), (a, b, c), "{code}");
        assert_eq!(int(code, -1_000_000), format!("-{}", int(code, 1_000_000)), "{code}");
        assert_eq!(int(code, 999), "999", "{code}");
    }
    assert_eq!(dec("en", 99.951), "99.95");
    for code in ["nb", "de", "fr", "es", "it", "nl", "pt-BR", "pl", "sv", "da"] {
        assert_eq!(dec(code, 99.951), "99,95", "{code}");
        assert_eq!(dec(code, -0.001), "0,00", "{code}");
    }
    assert_eq!(dec("de", 1234.5), "1.234,50");
    assert_eq!(dec("sv", 1234.5), format!("1{nb}234,50"));
    // Percent: spacing is language-specific.
    assert_eq!(pct("en", 99.999), "99.99%");
    for code in ["nb", "de", "es", "sv", "da"] {
        assert_eq!(pct(code, 99.95), format!("99,95{nb}%"), "{code}");
        assert_eq!(pct(code, 100.0), format!("100{nb}%"), "{code}");
    }
    assert_eq!(pct("fr", 99.95), format!("99,95{nn}%"));
    for code in ["it", "nl", "pt-BR", "pl"] {
        assert_eq!(pct(code, 99.95), "99,95%", "{code}");
    }
}

#[test]
fn fmt_lists_and_patterns_are_well_formed() {
    for lang in Lang::all().filter(|l| l.code() != "ar") {
        for (key, n) in [("months_short", 12), ("months_long", 12), ("weekdays_short", 7), ("weekdays_long", 7)] {
            let list = lookup(lang, &format!("fmt.{key}")).expect("fmt list");
            let items: Vec<&str> = list.split('|').collect();
            assert_eq!(items.len(), n, "{lang}: fmt.{key}");
            assert!(items.iter().all(|i| !i.trim().is_empty()), "{lang}: empty item in fmt.{key}");
            let unique: BTreeSet<&&str> = items.iter().collect();
            assert_eq!(unique.len(), n, "{lang}: repeated name in fmt.{key}");
        }
        for (key, needs) in [
            ("day_month", &["day", "month"][..]),
            ("day_month_long", &["day", "month"]),
            ("date", &["day_month", "year"]),
            ("datetime", &["date", "time"]),
            ("short_datetime", &["day_month", "time"]),
            ("weekday_day_month", &["weekday", "day_month"]),
            ("ago", &["span"]),
            ("future", &["span"]),
            ("dur_sec", &["n"]),
            ("dur_min", &["n"]),
            ("dur_hour", &["n"]),
            ("dur_day", &["n"]),
            ("compact_sec", &["n"]),
            ("compact_min", &["n"]),
            ("compact_hour", &["n"]),
            ("compact_day", &["n"]),
        ] {
            let text = lookup(lang, &format!("fmt.{key}")).expect("fmt pattern");
            let have = placeholder_names(text);
            let want: BTreeSet<String> = needs.iter().map(|s| (*s).to_string()).collect();
            assert_eq!(have, want, "{lang}: fmt.{key} = {text:?}");
        }
    }
}

#[test]
fn dates_and_durations_render_in_every_language() {
    let date = jiff::civil::date(2026, 3, 9);
    let z = date.at(14, 5, 0, 0).to_zoned(jiff::tz::TimeZone::UTC).expect("zoned");
    for lang in Lang::all().filter(|l| l.code() != "ar") {
        let dm = fmt_day_month_in(lang, date);
        assert!(dm.contains('9'), "{lang}: {dm}");
        assert!(!dm.contains('{'), "{lang}: unfilled pattern {dm}");
        let full = fmt_datetime_in(lang, &z);
        assert!(full.contains("2026") && full.ends_with("14:05") && !full.contains('{'), "{lang}: {full}");
        assert!(fmt_short_datetime_in(lang, &z).ends_with("14:05"), "{lang}");
        assert!(fmt_weekday_day_month_in(lang, date).contains('9'), "{lang}");
        assert!(!fmt_day_month_long_in(lang, date).contains('{'), "{lang}");
        assert_eq!(fmt_duration_in(lang, 0), fmt_duration_in(lang, 0));
        for secs in [45, 190, 3 * 3600 + 240, 2 * 86_400 + 5 * 3600, 86_400] {
            for text in [fmt_duration_in(lang, secs), fmt_duration_compact_in(lang, secs)] {
                assert!(!text.contains('{') && !text.trim().is_empty(), "{lang}: {text:?} for {secs}");
            }
        }
        for ago in [10, 190, -7200, 5 * 86_400] {
            let text = fmt_relative_in(lang, ago);
            assert!(!text.contains('{') && !text.trim().is_empty(), "{lang}: {text:?} for {ago}");
        }
        assert_ne!(fmt_relative_in(lang, 190), fmt_relative_in(lang, -190), "{lang}: past and future read the same");
        // Weekday names: Monday first, ISO numbering.
        assert_eq!(weekday_name_in(lang, 1, true), weekday_name_in(lang, 1, true));
        assert_ne!(weekday_name_in(lang, 1, true), weekday_name_in(lang, 7, true), "{lang}");
    }
}

#[test]
fn english_and_bokmal_formats_are_unchanged() {
    assert_eq!(fmt_duration_in(Lang::EN, 45), "45 sec");
    assert_eq!(fmt_duration_in(Lang::EN, 3 * 3600 + 240), "3 h 4 min");
    assert_eq!(fmt_duration_in(Lang::EN, 2 * 86_400 + 5 * 3600), "2 d 5 h");
    assert_eq!(fmt_duration_in(Lang::NB, 45), "45 sek");
    assert_eq!(fmt_duration_in(Lang::NB, 3 * 3600 + 240), "3 t 4 min");
    assert_eq!(fmt_duration_in(Lang::NB, 86_400), "1 d");
    assert_eq!(fmt_duration_compact_in(Lang::EN, 4 * 60 + 10), "4m 10s");
    assert_eq!(fmt_duration_compact_in(Lang::EN, 2 * 3600 + 5 * 60), "2h 5m");
    assert_eq!(fmt_duration_compact_in(Lang::EN, 3 * 86_400 + 4 * 3600), "3d 4h");
    assert_eq!(fmt_duration_compact_in(Lang::EN, 12), "12s");
    assert_eq!(fmt_duration_compact_in(Lang::NB, 2 * 3600 + 5 * 60), "2t 5m");
    assert_eq!(fmt_relative_in(Lang::EN, 10), "just now");
    assert_eq!(fmt_relative_in(Lang::EN, 190), "3 min ago");
    assert_eq!(fmt_relative_in(Lang::NB, 190), "for 3 min siden");
    assert_eq!(fmt_relative_in(Lang::NB, -7200), "om 2 t");
    assert_eq!(fmt_relative_in(Lang::NB, 10), "akkurat nå");

    let date = jiff::civil::date(2026, 10, 19);
    assert_eq!(fmt_day_month_in(Lang::EN, date), "Oct 19");
    assert_eq!(fmt_day_month_in(Lang::NB, date), "19. okt.");
    assert_eq!(fmt_day_month_long_in(Lang::NB, date), "19. oktober");
    assert_eq!(fmt_date_in(Lang::EN, date), "Oct 19, 2026");
    assert_eq!(fmt_date_in(Lang::NB, jiff::civil::date(2026, 3, 1)), "1. mars 2026");
    let z = date.at(14, 5, 0, 0).to_zoned(jiff::tz::TimeZone::UTC).expect("zoned");
    assert_eq!(fmt_datetime_in(Lang::EN, &z), "Oct 19, 2026, 14:05");
    assert_eq!(fmt_datetime_in(Lang::NB, &z), "19. okt. 2026, 14:05");
    assert_eq!(fmt_short_datetime_in(Lang::NB, &z), "19. okt., 14:05");
    assert_eq!(weekday_name_in(Lang::NB, 1, true), "mandag");
    assert_eq!(weekday_name_in(Lang::EN, 7, false), "Sun");
    assert_eq!(fmt_weekday_day_month_in(Lang::NB, date), "man. 19. okt.");
    assert_eq!(fmt_weekday_day_month_in(Lang::EN, date), "Mon, Oct 19");
}

#[test]
fn js_bundle_is_embeddable_json_in_every_language() {
    let en_keys: BTreeSet<String> = {
        let parsed: serde_json::Value = serde_json::from_str(&js_bundle(Lang::EN)).expect("valid json");
        parsed.as_object().expect("object").keys().cloned().collect()
    };
    assert!(en_keys.len() > 15);
    for lang in Lang::all() {
        let json = js_bundle(lang);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let keys: BTreeSet<String> = parsed.as_object().expect("object").keys().cloned().collect();
        assert_eq!(keys, en_keys, "{lang}");
        assert!(!json.contains("</"), "{lang}");
        let empty: Vec<&String> = parsed.as_object().unwrap().iter().filter(|(_, v)| v.as_str().is_none_or(str::is_empty)).map(|(k, _)| k).collect();
        assert!(empty.is_empty(), "{lang}: empty js strings {empty:?}");
    }
    assert_eq!(serde_json::from_str::<serde_json::Value>(&js_bundle(Lang::NB)).unwrap()["live"], "Direkte");
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

/// Prints the coverage of every language (`cargo test --lib coverage -- --nocapture`).
fn coverage(lang: Lang) -> (usize, usize) {
    let plain = english_plain_keys();
    let bases = english_plural_bases();
    let cat = catalog::all(lang);
    let required = lang.plural_rule().categories();
    let total = plain.len() + bases.len() * required.len();
    let done = plain.iter().filter(|k| cat.contains_key(*k)).count()
        + bases.iter().map(|b| required.iter().filter(|p| cat.contains_key(&format!("{b}.{}", p.as_str()))).count()).sum::<usize>();
    (done, total)
}

#[test]
fn coverage_report() {
    let mut rows = BTreeMap::new();
    for lang in Lang::all().filter(|l| l.code() != "ar") {
        let (done, total) = coverage(lang);
        #[allow(clippy::cast_precision_loss)]
        rows.insert(lang.code(), (done, total, 100.0 * done as f64 / total as f64));
    }
    for (code, (done, total, pct)) in &rows {
        println!("{code:>6}: {done:>5}/{total:<5} {pct:5.1}%");
    }
    assert_eq!(rows["en"].2, 100.0);
    // Every shipped translation is complete. New English strings added later
    // must be translated in the same change (or follow-up) before this passes.
    for lang in translated() {
        let (done, total, pct) = rows[lang.code()];
        assert_eq!(done, total, "{lang} is only {pct:.1}% translated: run `cargo test --lib coverage_report -- --nocapture` and see docs/translating.md");
    }
}
