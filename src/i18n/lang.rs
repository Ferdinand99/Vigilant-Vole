//! The language registry: one [`LangInfo`] per interface language.
//!
//! Adding a language means adding one entry to [`LANGS`] and one
//! `locales/<code>.toml`. The registry only holds what isn't text: the BCP 47
//! code, names, writing direction, the CLDR plural rule and the number
//! conventions. Everything a translator writes (month names, date patterns,
//! duration units, relative-time phrases) lives in the locale file's `[fmt]`
//! section.

use std::fmt::Display;

/// Writing direction of a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Ltr,
    Rtl,
}

impl Dir {
    /// The value of the HTML `dir` attribute.
    pub fn as_str(self) -> &'static str {
        match self {
            Dir::Ltr => "ltr",
            Dir::Rtl => "rtl",
        }
    }
}

/// A CLDR plural category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Plural {
    Zero,
    One,
    Two,
    Few,
    Many,
    Other,
}

impl Plural {
    /// The key suffix: `monitors.one`, `monitors.few`, …
    pub fn as_str(self) -> &'static str {
        match self {
            Plural::Zero => "zero",
            Plural::One => "one",
            Plural::Two => "two",
            Plural::Few => "few",
            Plural::Many => "many",
            Plural::Other => "other",
        }
    }

    pub const ALL: [Plural; 6] = [Plural::Zero, Plural::One, Plural::Two, Plural::Few, Plural::Many, Plural::Other];
}

/// CLDR cardinal plural rules, for whole numbers (every count in the app is
/// an integer). A language names the rule it follows; a rule names the
/// categories a translation must provide ([`PluralRule::categories`]).
///
/// To add a family (Welsh, Hebrew, …) add a variant here, its arm in
/// [`PluralRule::category`] and [`PluralRule::categories`], and a test vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralRule {
    /// No plural forms: ja, zh, ko, vi, th, id.
    Other,
    /// `one` for exactly 1: en, de, nl, sv, da, nb, fi, …
    OneOther,
    /// `one` for 0 and 1, `many` for whole millions: fr, pt.
    FrenchLike,
    /// `one` for 1, `many` for whole millions: es, it, ca.
    SpanishLike,
    /// `one`, `few` (2–4 except 12–14) and `many`: pl.
    Polish,
    /// `one` (1, 21, 31 … except 11), `few` (2–4 except 12–14), `many`: ru, uk, be.
    EastSlavic,
    /// `one` (1), `few` (2–4), `other`: cs, sk.
    CzechSlovak,
    /// `zero`, `one`, `two`, `few` (3–10), `many` (11–99), `other`: ar.
    Arabic,
}

impl PluralRule {
    pub fn category(self, n: i64) -> Plural {
        let n = n.unsigned_abs();
        let (m10, m100) = (n % 10, n % 100);
        match self {
            PluralRule::Other => Plural::Other,
            PluralRule::OneOther if n == 1 => Plural::One,
            PluralRule::OneOther => Plural::Other,
            PluralRule::FrenchLike if n <= 1 => Plural::One,
            PluralRule::FrenchLike | PluralRule::SpanishLike if n != 0 && n.is_multiple_of(1_000_000) => Plural::Many,
            PluralRule::SpanishLike if n == 1 => Plural::One,
            PluralRule::FrenchLike | PluralRule::SpanishLike => Plural::Other,
            PluralRule::Polish if n == 1 => Plural::One,
            PluralRule::Polish if (2..=4).contains(&m10) && !(12..=14).contains(&m100) => Plural::Few,
            PluralRule::Polish => Plural::Many,
            PluralRule::EastSlavic if m10 == 1 && m100 != 11 => Plural::One,
            PluralRule::EastSlavic if (2..=4).contains(&m10) && !(12..=14).contains(&m100) => Plural::Few,
            PluralRule::EastSlavic => Plural::Many,
            PluralRule::CzechSlovak if n == 1 => Plural::One,
            PluralRule::CzechSlovak if (2..=4).contains(&n) => Plural::Few,
            PluralRule::CzechSlovak => Plural::Other,
            PluralRule::Arabic if n == 0 => Plural::Zero,
            PluralRule::Arabic if n == 1 => Plural::One,
            PluralRule::Arabic if n == 2 => Plural::Two,
            PluralRule::Arabic if (3..=10).contains(&m100) => Plural::Few,
            PluralRule::Arabic if (11..=99).contains(&m100) => Plural::Many,
            PluralRule::Arabic => Plural::Other,
        }
    }

    /// The categories a locale file must provide for every plural key:
    /// those a whole number can reach, plus `other`, which is also the
    /// fallback for any category a file leaves out.
    pub fn categories(self) -> &'static [Plural] {
        use Plural::{Few, Many, One, Other, Two, Zero};
        match self {
            PluralRule::Other => &[Other],
            PluralRule::OneOther => &[One, Other],
            PluralRule::FrenchLike | PluralRule::SpanishLike => &[One, Many, Other],
            PluralRule::Polish | PluralRule::EastSlavic => &[One, Few, Many, Other],
            PluralRule::CzechSlovak => &[One, Few, Other],
            PluralRule::Arabic => &[Zero, One, Two, Few, Many, Other],
        }
    }
}

/// Number conventions (CLDR).
#[derive(Debug, Clone, Copy)]
pub struct Numbers {
    pub decimal: char,
    pub group: char,
    /// Digits that must precede the first group separator: 1 groups from
    /// 1 000, 2 from 10 000 (nb, es, pl).
    pub min_grouping: usize,
    /// What sits between a number and `%`: `None` → "99,95%".
    pub percent_space: Option<char>,
}

const NBSP: char = '\u{a0}';
const NNBSP: char = '\u{202f}';

/// A supported interface language.
#[derive(Debug)]
pub struct LangInfo {
    /// BCP 47: `en`, `pt-BR`. Also the locale file name.
    pub code: &'static str,
    pub english_name: &'static str,
    /// The language's own name, as shown in the pickers.
    pub native_name: &'static str,
    pub dir: Dir,
    /// New or machine-assisted translations are labelled "(beta)".
    pub beta: bool,
    pub plural: PluralRule,
    pub numbers: Numbers,
    /// Other primary subtags that resolve to this language (`no`, `nn`).
    pub aliases: &'static [&'static str],
    /// The compiled-in `locales/<code>.toml`.
    pub source: &'static str,
}

const EN: LangInfo = LangInfo {
    code: "en",
    english_name: "English",
    native_name: "English",
    dir: Dir::Ltr,
    beta: false,
    plural: PluralRule::OneOther,
    numbers: Numbers { decimal: '.', group: ',', min_grouping: 1, percent_space: None },
    aliases: &[],
    source: include_str!("../../locales/en.toml"),
};
const NB: LangInfo = LangInfo {
    code: "nb",
    english_name: "Norwegian Bokmål",
    native_name: "Norsk bokmål",
    dir: Dir::Ltr,
    beta: false,
    plural: PluralRule::OneOther,
    numbers: Numbers { decimal: ',', group: NBSP, min_grouping: 2, percent_space: Some(NBSP) },
    aliases: &["no", "nn"],
    source: include_str!("../../locales/nb.toml"),
};
const DE: LangInfo = LangInfo {
    code: "de",
    english_name: "German",
    native_name: "Deutsch",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers { decimal: ',', group: '.', min_grouping: 1, percent_space: Some(NBSP) },
    aliases: &[],
    source: include_str!("../../locales/de.toml"),
};
const FR: LangInfo = LangInfo {
    code: "fr",
    english_name: "French",
    native_name: "Français",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::FrenchLike,
    numbers: Numbers { decimal: ',', group: NNBSP, min_grouping: 1, percent_space: Some(NNBSP) },
    aliases: &[],
    source: include_str!("../../locales/fr.toml"),
};
const ES: LangInfo = LangInfo {
    code: "es",
    english_name: "Spanish",
    native_name: "Español",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::SpanishLike,
    numbers: Numbers { decimal: ',', group: '.', min_grouping: 2, percent_space: Some(NBSP) },
    aliases: &[],
    source: include_str!("../../locales/es.toml"),
};
const IT: LangInfo = LangInfo {
    code: "it",
    english_name: "Italian",
    native_name: "Italiano",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::SpanishLike,
    numbers: Numbers { decimal: ',', group: '.', min_grouping: 1, percent_space: None },
    aliases: &[],
    source: include_str!("../../locales/it.toml"),
};
const NL: LangInfo = LangInfo {
    code: "nl",
    english_name: "Dutch",
    native_name: "Nederlands",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers { decimal: ',', group: '.', min_grouping: 1, percent_space: None },
    aliases: &[],
    source: include_str!("../../locales/nl.toml"),
};
const PT_BR: LangInfo = LangInfo {
    code: "pt-BR",
    english_name: "Portuguese (Brazil)",
    native_name: "Português (Brasil)",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::FrenchLike,
    numbers: Numbers { decimal: ',', group: '.', min_grouping: 1, percent_space: None },
    aliases: &[],
    source: include_str!("../../locales/pt-BR.toml"),
};
const PL: LangInfo = LangInfo {
    code: "pl",
    english_name: "Polish",
    native_name: "Polski",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::Polish,
    numbers: Numbers { decimal: ',', group: NBSP, min_grouping: 2, percent_space: None },
    aliases: &[],
    source: include_str!("../../locales/pl.toml"),
};
const SV: LangInfo = LangInfo {
    code: "sv",
    english_name: "Swedish",
    native_name: "Svenska",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers { decimal: ',', group: NBSP, min_grouping: 1, percent_space: Some(NBSP) },
    aliases: &[],
    source: include_str!("../../locales/sv.toml"),
};
const DA: LangInfo = LangInfo {
    code: "da",
    english_name: "Danish",
    native_name: "Dansk",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers { decimal: ',', group: '.', min_grouping: 1, percent_space: Some(NBSP) },
    aliases: &[],
    source: include_str!("../../locales/da.toml"),
};

/// A right-to-left language that exists only in tests, so the RTL plumbing
/// (`dir`, logical CSS) can be exercised before a real RTL translation ships.
#[cfg(test)]
const AR_TEST: LangInfo = LangInfo {
    code: "ar",
    english_name: "Arabic (test only)",
    native_name: "العربية",
    dir: Dir::Rtl,
    beta: true,
    plural: PluralRule::Arabic,
    numbers: Numbers { decimal: '.', group: ',', min_grouping: 1, percent_space: None },
    aliases: &[],
    source: "",
};

/// Every interface language. The first entry is the source language
/// (English) and the fallback; the rest are in picker order.
#[cfg(not(test))]
pub static LANGS: &[LangInfo] = &[EN, NB, DA, DE, ES, FR, IT, NL, PL, PT_BR, SV];
#[cfg(test)]
pub static LANGS: &[LangInfo] = &[EN, NB, DA, DE, ES, FR, IT, NL, PL, PT_BR, SV, AR_TEST];

/// A supported interface language (an index into [`LANGS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Lang(u8);

impl Lang {
    /// English: the source language and the fallback.
    pub const EN: Lang = Lang(0);
    pub const NB: Lang = Lang(1);

    /// Every language, in picker order.
    pub fn all() -> impl Iterator<Item = Lang> {
        (0..LANGS.len()).map(|i| Lang(u8::try_from(i).unwrap_or(0)))
    }

    pub(super) fn index(self) -> usize {
        usize::from(self.0)
    }

    pub fn info(self) -> &'static LangInfo {
        LANGS.get(self.index()).unwrap_or(&LANGS[0])
    }

    /// BCP 47 code used in `<html lang>`, settings and locale file names.
    pub fn code(self) -> &'static str {
        self.info().code
    }

    /// The language's own name ("Deutsch", "Norsk bokmål").
    pub fn native_name(self) -> &'static str {
        self.info().native_name
    }

    pub fn dir(self) -> Dir {
        self.info().dir
    }

    pub fn plural_rule(self) -> PluralRule {
        self.info().plural
    }

    /// The plural category of the whole number `n` in this language.
    pub fn plural_category(self, n: i64) -> Plural {
        self.plural_rule().category(n)
    }

    /// Picker label: the own name, "(beta)" for translations still being reviewed.
    pub fn picker_label(self) -> String {
        if self.info().beta {
            format!("{} (beta)", self.native_name())
        } else {
            self.native_name().to_string()
        }
    }

    /// Resolves a stored code or a language tag. Tags match the registry
    /// exactly (`pt-BR`, case-insensitive), then by primary language, so
    /// `pt` and `pt-PT` give `pt-BR`, `de-AT` gives `de`, and `nb-NO`, `no`
    /// and `nn` give `nb`. When several registered languages share a primary
    /// subtag, an exact match wins and otherwise the first listed does.
    pub fn from_code(code: &str) -> Option<Lang> {
        let norm = code.trim().replace('_', "-").to_ascii_lowercase();
        if norm.is_empty() {
            return None;
        }
        if let Some(i) = LANGS.iter().position(|l| l.code.eq_ignore_ascii_case(&norm)) {
            return Some(Lang(u8::try_from(i).ok()?));
        }
        let primary = norm.split('-').next()?;
        let i = LANGS.iter().position(|l| {
            l.code.split('-').next().is_some_and(|p| p.eq_ignore_ascii_case(primary)) || l.aliases.contains(&primary)
        })?;
        Some(Lang(u8::try_from(i).ok()?))
    }

    /// Only a canonical registry code (`pt-BR`, not `pt`): for validating
    /// what a form posts.
    pub fn from_exact_code(code: &str) -> Option<Lang> {
        LANGS.iter().position(|l| l.code == code).and_then(|i| u8::try_from(i).ok()).map(Lang)
    }
}

impl Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// `(code, label)` for every picker (Settings → General, Settings →
/// Security): own names, "(beta)" where it applies. The test-only language
/// is left out of real builds by the registry itself.
pub fn language_choices() -> Vec<(&'static str, String)> {
    Lang::all().map(|l| (l.code(), l.picker_label())).collect()
}
