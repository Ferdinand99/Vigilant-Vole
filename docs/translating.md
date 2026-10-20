# Helping with translations

Vigilant Vole's interface exists in English (the source), Norwegian (bokmål), German, French, Spanish, Italian,
Dutch, Brazilian Portuguese, Polish, Swedish and Danish. The newest nine are marked *beta* in the language
pickers until native speakers have reviewed them. A corrected phrase is one of the easiest and most useful
contributions.

## Fix or improve a translation

1. Open `locales/<code>.toml` (`de`, `fr`, `es`, `it`, `nl`, `pt-BR`, `pl`, `sv`, `da`, `nb`). Search for the text
   you want to change; the key above it tells you where it appears (`nav.*` is the menu, `monitors.*` …).
2. Edit the value inside the quotes. Keep every `{placeholder}` exactly as it is (you may move it), and keep
   product names and technical terms (HTTP, TLS, OAuth2, Docker, Prometheus …) as they are.
3. Check the wording against `locales/glossary/<code>.md`, which records which word was chosen for each core
   concept (monitor, heartbeat, status page, escalation …) and the tone of the language. If you change a term,
   change it everywhere and update the glossary.
4. Run `cargo test --lib i18n` (or open a pull request: CI runs the same checks). The tests catch a broken file,
   a missing or extra key, an empty value, a placeholder that doesn't match English, wrong plural forms and
   malformed date/month lists.
5. To see your change in the app, run it (`cargo run`) and pick the language under **Settings → Security**.

Writing guidelines (they come from the design system, §10): short, calm Apple-style copy; sentence case where the
language uses it; the language's own quotation marks and typography; an informal or neutral form of address
(never stiff); no exclamation marks unless the English has one. Buttons, tabs and badges are narrow, so prefer the
shorter of two good wordings. Write every string yourself from the English: do not copy text from other projects.

## The file format

A small TOML subset: `[section]` headers, `key = "value"` lines, `#` comments. Inside a value only `\"`, `\\`, `\n`
and `\t` are escapes; typographic quotes can be typed directly. The sections, keys and order mirror
`locales/en.toml`. A key missing from a language falls back to English, key by key, so a partial translation
works (debug builds log each fallback once), but every shipped language is kept at 100 %.

### Plurals

Counted nouns come as `base.<category>` keys, for example `count.monitors.one` / `count.monitors.other`. The
categories a language needs follow the Unicode CLDR plural rules for whole numbers, and the tests check that a
language has exactly those:

| Rule | Languages | Categories | Which numbers |
|---|---|---|---|
| one / other | en, nb, de, nl, sv, da | `one`, `other` | `one` is exactly 1 |
| French-like | fr, pt-BR | `one`, `many`, `other` | `one` is 0 and 1; `many` is whole millions ("1 000 000 **de** moniteurs") |
| Spanish-like | es, it | `one`, `many`, `other` | `one` is exactly 1; `many` is whole millions |
| Polish | pl | `one`, `few`, `many`, `other` | `one` is 1; `few` ends in 2–4 but not 12–14; `many` is the rest of the integers; `other` is the fraction/fallback form |
| East Slavic | ru, uk | `one`, `few`, `many`, `other` | `one` ends in 1 but not 11; `few` ends in 2–4 but not 12–14; `many` is the rest (0, 5–20, 25–30 …); `other` is the fraction form (use the genitive singular) |
| Czech | cs | `one`, `few`, `many`, `other` | `one` is 1; `few` is 2–4; whole numbers 0 and 5+ use `other` (genitive plural); `many` is the fraction form (genitive singular), so it is never reached for the integer counts in the app but is kept for completeness |
| Romanian | ro | `one`, `few`, `other` | `one` is 1; `few` is 0 and numbers ending 02–19 ("{n} monitoare"); `other` is 20+ ("{n} **de** monitoare") |
| Hebrew | he | `one`, `two`, `other` | `one` is 1, `two` is 2 |
| Arabic | ar | `zero`, `one`, `two`, `few`, `many`, `other` | 0; 1; 2; last two digits 03–10; last two digits 11–99; the rest (100–102, 200–202 …) |
| Zero-or-one | fa | `one`, `other` | `one` is 0 and 1 |
| Portuguese (Portugal) | pt-PT | `one`, `many`, `other` | like Spanish-like: `one` is exactly 1 (0 is `other`) |
| No plurals | ja, ko, zh-CN, zh-TW | `other` | one form for every count |
| one / other | fi, tr, hu | `one`, `other` | `one` is exactly 1 (Turkish and Hungarian nouns stay singular, so both forms read the same) |

`other` is always required: it is also the fallback for any category a file leaves out. To add a family, add a
variant to `PluralRule` in `src/i18n/lang.rs` (its `category` arm, its `categories`, and a test vector).

### Dates, durations and numbers (`[fmt]`)

The `[fmt]` section holds what is language-specific about formatting: month and weekday names (lists separated by
`|`, exactly 12 and 7 entries, Monday first), date patterns (`{day}`, `{month}`, `{year}`, `{time}`, …), duration
units (`{n} min`), compact units for notifications (`{n}h`) and relative-time phrases (`{span} ago`). Decimal and
thousands separators, the position of and space around `%`, and the digit set (Persian digits for `fa`; the other
languages, Arabic included, use 0–9) follow CLDR and live in the language registry (`src/i18n/lang.rs`). Dates are
Gregorian everywhere. CJK patterns carry no spaces (`2026年10月20日`), and `{span}` stays a plain number plus unit.

## Add a language

1. Copy `locales/en.toml` to `locales/<code>.toml` (BCP 47: `pt-BR`, `zh-Hans`) and translate it, starting with
   `[fmt]`. Write `locales/glossary/<code>.md` first: about 60 core terms plus the style decisions.
2. Add one entry to `LANGS` in `src/i18n/lang.rs`: the code, English and own name, writing direction, plural rule
   (add a variant to `PluralRule` with its CLDR categories if the family is missing, plus a test vector), number
   separators, `beta: true`, and `include_str!` of the file. Add the primary-language aliases (`aliases`) if people
   write the language under other tags.
3. `cargo test --lib i18n` and `cargo clippy --all-targets -- -D warnings`. The coverage test
   (`cargo test --lib coverage_report -- --nocapture`) prints the percentage per language.
4. Look at the main pages at desktop and phone width (1280 px and 390 px): menus, buttons, pills and table
   headings must not overflow. Shorten the translation rather than changing the design.

The language then shows up in both pickers, in `Accept-Language` negotiation (regions and reserves resolve to the
closest language: `pt` gives `pt-BR`, `pt-PT`/`pt-AO` give `pt-PT`, `de-AT` gives `de`, `zh` and `zh-SG` give
`zh-CN`, `zh-Hant`/`zh-HK`/`zh-MO` give `zh-TW`, `iw` gives `he`) and in every `<html lang>`.

### Right-to-left languages

No right-to-left language ships yet, but the groundwork is there: set `dir: Dir::Rtl` in the registry entry and
`<html dir>` follows. `static/style.css` uses logical properties (`margin-inline-start`, `inset-inline-end`,
`text-align: start`) and a `--dir` variable for transforms; the few places that are physical on purpose carry an
`rtl-ok` comment explaining why, and a test fails when a new physical declaration appears without one. Charts and
uptime bars keep a left-to-right time axis. Directional icons (chevrons) are not mirrored yet.

## Known limitations

* A few sentences are assembled from several keys around inline code (`push.help_1` / `push.help_2`,
  `backup.restore_help_*`, `docs.secrets_*` …), which assumes English word order. The translations reshape them to
  read correctly as rendered; a future change should turn them into single strings with a `{code}` placeholder.
* New English strings are added to `locales/en.toml` first. Until they are translated, the other languages show
  the English text for those keys; the coverage test lists what is missing.
