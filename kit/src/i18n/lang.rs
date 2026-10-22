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

    pub const ALL: [Plural; 6] = [
        Plural::Zero,
        Plural::One,
        Plural::Two,
        Plural::Few,
        Plural::Many,
        Plural::Other,
    ];
}

/// CLDR cardinal plural rules, for whole numbers (every count in the app is
/// an integer). A language names the rule it follows; a rule names the
/// categories a translation must provide ([`PluralRule::categories`]).
///
/// To add a family (Welsh, Irish, …) add a variant here, its arm in
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
    /// `one` (1), `few` (2–4), `other`: cs, sk. CLDR also has `many` for
    /// fractions; no count in the app is fractional, but files provide it.
    CzechSlovak,
    /// `one` (1), `few` (0 and ending 02–19), `other`: ro.
    Romanian,
    /// `one` (1), `two` (2), `other`: he.
    Hebrew,
    /// `one` for 0 and 1: fa, hi.
    ZeroOne,
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
            PluralRule::FrenchLike | PluralRule::SpanishLike
                if n != 0 && n.is_multiple_of(1_000_000) =>
            {
                Plural::Many
            }
            PluralRule::SpanishLike if n == 1 => Plural::One,
            PluralRule::FrenchLike | PluralRule::SpanishLike => Plural::Other,
            PluralRule::Polish if n == 1 => Plural::One,
            PluralRule::Polish if (2..=4).contains(&m10) && !(12..=14).contains(&m100) => {
                Plural::Few
            }
            PluralRule::Polish => Plural::Many,
            PluralRule::EastSlavic if m10 == 1 && m100 != 11 => Plural::One,
            PluralRule::EastSlavic if (2..=4).contains(&m10) && !(12..=14).contains(&m100) => {
                Plural::Few
            }
            PluralRule::EastSlavic => Plural::Many,
            PluralRule::CzechSlovak if n == 1 => Plural::One,
            PluralRule::CzechSlovak if (2..=4).contains(&n) => Plural::Few,
            PluralRule::CzechSlovak => Plural::Other,
            PluralRule::Romanian if n == 1 => Plural::One,
            PluralRule::Romanian if n == 0 || (1..=19).contains(&m100) => Plural::Few,
            PluralRule::Romanian => Plural::Other,
            PluralRule::Hebrew if n == 1 => Plural::One,
            PluralRule::Hebrew if n == 2 => Plural::Two,
            PluralRule::Hebrew => Plural::Other,
            PluralRule::ZeroOne if n <= 1 => Plural::One,
            PluralRule::ZeroOne => Plural::Other,
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
            PluralRule::CzechSlovak => &[One, Few, Many, Other],
            PluralRule::Romanian => &[One, Few, Other],
            PluralRule::Hebrew => &[One, Two, Other],
            PluralRule::ZeroOne => &[One, Other],
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
    /// The percent sign: `%`, or `٪` (Arabic percent sign) in Persian.
    pub percent_sign: char,
    /// The sign comes before the number ("%12", tr).
    pub percent_before: bool,
    /// Zero of the digit set: `'0'`, or `'۰'` (Persian digits). The other
    /// digits follow it in code-point order.
    pub zero: char,
}

impl Numbers {
    /// Latin digits and a trailing `%`; `with_*` adjust the exceptions.
    pub const fn new(
        decimal: char,
        group: char,
        min_grouping: usize,
        percent_space: Option<char>,
    ) -> Self {
        Self {
            decimal,
            group,
            min_grouping,
            percent_space,
            percent_sign: '%',
            percent_before: false,
            zero: '0',
        }
    }

    /// "%12" instead of "12%".
    pub const fn percent_before(mut self) -> Self {
        self.percent_before = true;
        self
    }

    /// Another digit set and percent sign (Persian).
    pub const fn script(mut self, zero: char, percent_sign: char) -> Self {
        self.zero = zero;
        self.percent_sign = percent_sign;
        self
    }
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
    /// Other tags that resolve to this language: a primary subtag (`no`, `nn`)
    /// or a longer prefix (`zh-hant`, `zh-hk`).
    pub aliases: &'static [&'static str],
    /// The language's locale file, as registered by the product.
    pub source: &'static LocaleText,
}

const EN: LangInfo = LangInfo {
    code: "en",
    source: &LocaleText::new("en"),
    english_name: "English",
    native_name: "English",
    dir: Dir::Ltr,
    beta: false,
    plural: PluralRule::OneOther,
    numbers: Numbers::new('.', ',', 1, None),
    aliases: &[],
};
const NB: LangInfo = LangInfo {
    code: "nb",
    source: &LocaleText::new("nb"),
    english_name: "Norwegian Bokmål",
    native_name: "Norsk bokmål",
    dir: Dir::Ltr,
    beta: false,
    plural: PluralRule::OneOther,
    numbers: Numbers::new(',', NBSP, 2, Some(NBSP)),
    aliases: &["no", "nn"],
};
const DE: LangInfo = LangInfo {
    code: "de",
    source: &LocaleText::new("de"),
    english_name: "German",
    native_name: "Deutsch",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers::new(',', '.', 1, Some(NBSP)),
    aliases: &[],
};
const FR: LangInfo = LangInfo {
    code: "fr",
    source: &LocaleText::new("fr"),
    english_name: "French",
    native_name: "Français",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::FrenchLike,
    numbers: Numbers::new(',', NNBSP, 1, Some(NNBSP)),
    aliases: &[],
};
const ES: LangInfo = LangInfo {
    code: "es",
    source: &LocaleText::new("es"),
    english_name: "Spanish",
    native_name: "Español",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::SpanishLike,
    numbers: Numbers::new(',', '.', 2, Some(NBSP)),
    aliases: &[],
};
const IT: LangInfo = LangInfo {
    code: "it",
    source: &LocaleText::new("it"),
    english_name: "Italian",
    native_name: "Italiano",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::SpanishLike,
    numbers: Numbers::new(',', '.', 1, None),
    aliases: &[],
};
const NL: LangInfo = LangInfo {
    code: "nl",
    source: &LocaleText::new("nl"),
    english_name: "Dutch",
    native_name: "Nederlands",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers::new(',', '.', 1, None),
    aliases: &[],
};
const PT_BR: LangInfo = LangInfo {
    code: "pt-BR",
    source: &LocaleText::new("pt-BR"),
    english_name: "Portuguese (Brazil)",
    native_name: "Português (Brasil)",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::FrenchLike,
    numbers: Numbers::new(',', '.', 1, None),
    aliases: &[],
};
const PL: LangInfo = LangInfo {
    code: "pl",
    source: &LocaleText::new("pl"),
    english_name: "Polish",
    native_name: "Polski",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::Polish,
    numbers: Numbers::new(',', NBSP, 2, None),
    aliases: &[],
};
const SV: LangInfo = LangInfo {
    code: "sv",
    source: &LocaleText::new("sv"),
    english_name: "Swedish",
    native_name: "Svenska",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers::new(',', NBSP, 1, Some(NBSP)),
    aliases: &[],
};
const DA: LangInfo = LangInfo {
    code: "da",
    source: &LocaleText::new("da"),
    english_name: "Danish",
    native_name: "Dansk",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers::new(',', '.', 1, Some(NBSP)),
    aliases: &[],
};

const AR: LangInfo = LangInfo {
    code: "ar",
    source: &LocaleText::new("ar"),
    english_name: "Arabic",
    native_name: "العربية",
    dir: Dir::Rtl,
    beta: true,
    plural: PluralRule::Arabic,
    numbers: Numbers::new('.', ',', 1, None),
    aliases: &[],
};

const CS: LangInfo = LangInfo {
    code: "cs",
    source: &LocaleText::new("cs"),
    english_name: "Czech",
    native_name: "Čeština",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::CzechSlovak,
    numbers: Numbers::new(',', NBSP, 1, Some(NBSP)),
    aliases: &[],
};

const FA: LangInfo = LangInfo {
    code: "fa",
    source: &LocaleText::new("fa"),
    english_name: "Persian",
    native_name: "فارسی",
    dir: Dir::Rtl,
    beta: true,
    plural: PluralRule::ZeroOne,
    numbers: Numbers::new('٫', '٬', 1, None).script('۰', '٪'),
    aliases: &[],
};

const FI: LangInfo = LangInfo {
    code: "fi",
    source: &LocaleText::new("fi"),
    english_name: "Finnish",
    native_name: "Suomi",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers::new(',', NBSP, 1, Some(NBSP)),
    aliases: &[],
};

const HE: LangInfo = LangInfo {
    code: "he",
    source: &LocaleText::new("he"),
    english_name: "Hebrew",
    native_name: "עברית",
    dir: Dir::Rtl,
    beta: true,
    plural: PluralRule::Hebrew,
    numbers: Numbers::new('.', ',', 1, None),
    aliases: &["iw"],
};

const HU: LangInfo = LangInfo {
    code: "hu",
    source: &LocaleText::new("hu"),
    english_name: "Hungarian",
    native_name: "Magyar",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers::new(',', NBSP, 1, None),
    aliases: &[],
};

const JA: LangInfo = LangInfo {
    code: "ja",
    source: &LocaleText::new("ja"),
    english_name: "Japanese",
    native_name: "日本語",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::Other,
    numbers: Numbers::new('.', ',', 1, None),
    aliases: &[],
};

const KO: LangInfo = LangInfo {
    code: "ko",
    source: &LocaleText::new("ko"),
    english_name: "Korean",
    native_name: "한국어",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::Other,
    numbers: Numbers::new('.', ',', 1, None),
    aliases: &[],
};

const PT_PT: LangInfo = LangInfo {
    code: "pt-PT",
    source: &LocaleText::new("pt-PT"),
    english_name: "Portuguese (Portugal)",
    native_name: "Português (Portugal)",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::SpanishLike,
    numbers: Numbers::new(',', NBSP, 2, None),
    aliases: &["pt-ao", "pt-mz"],
};

const RO: LangInfo = LangInfo {
    code: "ro",
    source: &LocaleText::new("ro"),
    english_name: "Romanian",
    native_name: "Română",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::Romanian,
    numbers: Numbers::new(',', '.', 1, Some(NBSP)),
    aliases: &[],
};

const RU: LangInfo = LangInfo {
    code: "ru",
    source: &LocaleText::new("ru"),
    english_name: "Russian",
    native_name: "Русский",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::EastSlavic,
    numbers: Numbers::new(',', NBSP, 1, Some(NBSP)),
    aliases: &[],
};

const TR: LangInfo = LangInfo {
    code: "tr",
    source: &LocaleText::new("tr"),
    english_name: "Turkish",
    native_name: "Türkçe",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::OneOther,
    numbers: Numbers::new(',', '.', 1, None).percent_before(),
    aliases: &[],
};

const UK: LangInfo = LangInfo {
    code: "uk",
    source: &LocaleText::new("uk"),
    english_name: "Ukrainian",
    native_name: "Українська",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::EastSlavic,
    numbers: Numbers::new(',', NBSP, 1, None),
    aliases: &[],
};

const ZH_CN: LangInfo = LangInfo {
    code: "zh-CN",
    source: &LocaleText::new("zh-CN"),
    english_name: "Chinese (Simplified)",
    native_name: "简体中文",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::Other,
    numbers: Numbers::new('.', ',', 1, None),
    aliases: &["zh-hans", "zh-sg"],
};

const ZH_TW: LangInfo = LangInfo {
    code: "zh-TW",
    source: &LocaleText::new("zh-TW"),
    english_name: "Chinese (Traditional)",
    native_name: "繁體中文",
    dir: Dir::Ltr,
    beta: true,
    plural: PluralRule::Other,
    numbers: Numbers::new('.', ',', 1, None),
    aliases: &["zh-hant", "zh-hk", "zh-mo"],
};

/// Every interface language. The first entry is the source language
/// (English) and the fallback; the rest are in picker order.
pub static LANGS: &[LangInfo] = &[
    EN, NB, AR, CS, DA, DE, ES, FA, FI, FR, HE, HU, IT, JA, KO, NL, PL, PT_BR, PT_PT, RO, RU, SV,
    TR, UK, ZH_CN, ZH_TW,
];

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
    /// exactly (`pt-BR`, case-insensitive), then by alias, then by primary language, so
    /// `pt` gives `pt-BR`, `de-AT` gives `de`, `zh-HK` gives `zh-TW`, and
    /// `nb-NO`, `no` and `nn` give `nb`. When several registered languages share a primary
    /// subtag, an exact match wins and otherwise the first listed does.
    pub fn from_code(code: &str) -> Option<Lang> {
        let norm = code.trim().replace('_', "-").to_ascii_lowercase();
        if norm.is_empty() {
            return None;
        }
        if let Some(i) = LANGS
            .iter()
            .position(|l| l.code.eq_ignore_ascii_case(&norm))
        {
            return Some(Lang(u8::try_from(i).ok()?));
        }
        if let Some(i) = LANGS.iter().position(|l| {
            l.aliases
                .iter()
                .any(|a| a.contains('-') && (norm == *a || norm.starts_with(&format!("{a}-"))))
        }) {
            return Some(Lang(u8::try_from(i).ok()?));
        }
        let primary = norm.split('-').next()?;
        let i = LANGS.iter().position(|l| {
            l.code
                .split('-')
                .next()
                .is_some_and(|p| p.eq_ignore_ascii_case(primary))
                || l.aliases.contains(&primary)
        })?;
        Some(Lang(u8::try_from(i).ok()?))
    }

    /// Only a canonical registry code (`pt-BR`, not `pt`): for validating
    /// what a form posts.
    pub fn from_exact_code(code: &str) -> Option<Lang> {
        LANGS
            .iter()
            .position(|l| l.code == code)
            .and_then(|i| u8::try_from(i).ok())
            .map(Lang)
    }
}

impl Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// `(code, label)` for every picker (Settings → General, Settings →
/// Security): own names, "(beta)" where it applies.
pub fn language_choices() -> Vec<(&'static str, String)> {
    Lang::all().map(|l| (l.code(), l.picker_label())).collect()
}

/// The text of a language's catalog: the kit's core strings
/// (`kit/locales/<code>.toml`, compiled into the kit) followed by the
/// product's own (its [`KitConfig::locales`](crate::config::KitConfig)).
/// It derefs to `&str`; the two are joined once, when first read.
pub struct LocaleText {
    code: &'static str,
}

impl LocaleText {
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// The kit's core strings for this language.
    pub fn core(&self) -> &'static str {
        super::catalog::core_text(self.code)
    }

    /// The product's strings for this language (empty when it has none).
    pub fn product(&self) -> &'static str {
        crate::config::get()
            .locales
            .iter()
            .find(|(code, _)| *code == self.code)
            .map_or("", |(_, text)| text)
    }
}

impl std::ops::Deref for LocaleText {
    type Target = str;

    fn deref(&self) -> &str {
        use std::collections::HashMap;
        use std::sync::{Mutex, PoisonError};
        static JOINED: Mutex<Option<HashMap<&'static str, &'static str>>> = Mutex::new(None);
        let mut joined = JOINED.lock().unwrap_or_else(PoisonError::into_inner);
        joined
            .get_or_insert_with(HashMap::new)
            .entry(self.code)
            .or_insert_with(|| {
                let text = format!("{}\n{}", self.core(), self.product());
                Box::leak(text.into_boxed_str())
            })
    }
}

impl std::fmt::Debug for LocaleText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LocaleText({})", self.code)
    }
}
