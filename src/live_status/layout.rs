//! Turns what a board shows into Discord/Fluxer messages: embeds within the
//! platforms' limits, laid out the same way every time so edits stay stable.
//!
//! The limits (both platforms agree): 10 embeds per message; title 256,
//! description 4096, footer 2048 characters; 6000 characters over all the
//! embeds of one message. Text is measured in UTF-16 units, which is never
//! less than the characters or code points either platform might count.
//!
//! Layout: one header embed (title, overall status, the board's description),
//! then one embed per active incident or maintenance, then the sections: one
//! embed per section, with a line per monitor (a long section continues in
//! another embed with the same title). Embeds fill a message in order until
//! the embed or character limit; the next one starts the next message.

use serde_json::{Value, json};

/// Embeds in one message.
pub const MAX_EMBEDS: usize = 10;
/// Characters over all embeds of one message.
pub const MAX_MESSAGE_CHARS: usize = 6000;
/// What the layout fills a message to (a margin under the limit).
const MESSAGE_BUDGET: usize = 5600;
pub const MAX_TITLE: usize = 256;
pub const MAX_DESCRIPTION: usize = 4096;
pub const MAX_FOOTER: usize = 2048;
/// What a section embed's lines fill (well under [`MAX_DESCRIPTION`]).
const SECTION_BUDGET: usize = 3500;
/// Most messages a board uses; the rest is cut with a note.
pub const MAX_MESSAGES: usize = 10;
/// Longest monitor name shown.
const MAX_NAME: usize = 100;
/// Longest incident or maintenance text shown.
const MAX_NOTICE_BODY: usize = 700;

/// Colours (embed `color`): the status colours of the design system.
pub const GREEN: u32 = 0x34C759;
pub const RED: u32 = 0xFF3B30;
pub const YELLOW: u32 = 0xFFCC00;
pub const BLUE: u32 = 0x0A84FF;
pub const GRAY: u32 = 0x8E8E93;
pub const PURPLE: u32 = 0x7C5CFF;

/// Status class → colour: `up`, `down`, `warning`/`pending`, `maintenance`, anything else gray.
pub fn colour_of_class(class: &str) -> u32 {
    match class {
        "up" | "success" => GREEN,
        "down" | "danger" | "error" => RED,
        "warning" | "pending" => YELLOW,
        "maintenance" | "info" => BLUE,
        _ => GRAY,
    }
}

/// A board's own colour setting (`""` follows the status): a palette name or `#rrggbb`.
pub fn parse_colour(s: &str) -> Option<u32> {
    match s {
        "green" => Some(GREEN),
        "red" => Some(RED),
        "yellow" => Some(YELLOW),
        "blue" => Some(BLUE),
        "gray" => Some(GRAY),
        "purple" => Some(PURPLE),
        _ => {
            let hex = s.strip_prefix('#')?;
            (hex.len() == 6)
                .then(|| u32::from_str_radix(hex, 16).ok())
                .flatten()
        }
    }
}

/// One monitor line.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Status class (`up`, `down`, `pending`, `maintenance`, `paused`, `unknown`).
    pub class: &'static str,
    pub name: String,
    /// The status in words; shown unless the monitor is up.
    pub status: String,
    /// "99.98 % (24 h) · 123 ms": already formatted, may be empty.
    pub details: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub name: Option<String>,
    pub rows: Vec<Row>,
}

/// An active incident or maintenance window.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    /// `info`, `warning`, `danger`, `success` or `maintenance`.
    pub class: String,
    pub title: String,
    pub body: String,
    pub meta: String,
}

/// Everything a board shows, in the board's language.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardView {
    pub title: String,
    pub description: String,
    pub overall_class: String,
    pub overall_title: String,
    pub overall_detail: String,
    pub sections: Vec<Section>,
    pub notices: Vec<Notice>,
    /// RFC 3339.
    pub updated_at: String,
    pub footer: String,
    /// "… and {n} more" in the board's language.
    pub more_label: String,
    /// The board's own colour (`""` follows the status).
    pub colour: String,
}

fn emoji(class: &str) -> &'static str {
    match class {
        "up" => "\u{1f7e2}",
        "down" => "\u{1f534}",
        "pending" | "warning" => "\u{1f7e1}",
        "maintenance" => "\u{1f535}",
        _ => "\u{26aa}",
    }
}

fn notice_emoji(class: &str) -> &'static str {
    match class {
        "danger" => "\u{1f534}",
        "warning" => "\u{1f7e1}",
        "success" => "\u{1f7e2}",
        "maintenance" => "\u{1f527}",
        _ => "\u{2139}\u{fe0f}",
    }
}

/// Length in UTF-16 units.
pub fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s` cut to at most `max` UTF-16 units, with an ellipsis when it was cut.
pub fn clip(s: &str, max: usize) -> String {
    if units(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.len_utf16();
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// Escapes what Markdown would act on in a name or title typed by a person.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '*' | '_' | '~' | '|' | '`' | '>' | '[' | ']' | '(' | ')' | '#' | '<' => {
                out.push('\\');
                out.push(c);
            }
            '\r' => {}
            '\n' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

fn row_line(row: &Row) -> String {
    let mut line = format!(
        "{} **{}**",
        emoji(row.class),
        clip(&escape(&row.name), MAX_NAME)
    );
    if row.class != "up" && !row.status.is_empty() {
        line.push_str(" \u{2014} ");
        line.push_str(&clip(&row.status, 60));
    }
    if !row.details.is_empty() {
        line.push_str(" \u{b7} ");
        line.push_str(&clip(&row.details, 120));
    }
    line
}

/// The most severe class of a section, for its colour.
fn worst_class(rows: &[Row]) -> &'static str {
    let rank = |c: &str| match c {
        "down" => 4,
        "pending" => 3,
        "maintenance" => 2,
        "up" => 1,
        _ => 0,
    };
    rows.iter()
        .map(|r| r.class)
        .max_by_key(|c| rank(c))
        .unwrap_or("unknown")
}

/// An embed with its measured size, so messages can be filled.
#[derive(Debug, Clone)]
struct Embed {
    json: Value,
    size: usize,
}

fn embed(title: Option<&str>, description: &str, colour: u32, footer: Option<&str>) -> Embed {
    let title = title.map(|t| clip(t, MAX_TITLE));
    let description = clip(description, MAX_DESCRIPTION);
    let footer = footer.map(|f| clip(f, MAX_FOOTER));
    let mut size = description.encode_utf16().count();
    let mut json = json!({ "color": colour });
    if let Some(t) = &title {
        size += units(t);
        json["title"] = json!(t);
    }
    if !description.is_empty() {
        json["description"] = json!(description);
    }
    if let Some(f) = &footer {
        size += units(f);
        json["footer"] = json!({ "text": f });
    }
    Embed { json, size }
}

/// The messages of a board: each is a webhook payload (`embeds`,
/// `allowed_mentions` that mentions nobody).
pub fn build(view: &BoardView) -> Vec<Value> {
    let status_colour = colour_of_class(&view.overall_class);
    let colour = parse_colour(&view.colour).unwrap_or(status_colour);

    let mut embeds: Vec<Embed> = Vec::new();

    // Header.
    let mut head = format!("**{}**", escape(&view.overall_title));
    if !view.overall_detail.is_empty() {
        head.push('\n');
        head.push_str(&escape(&view.overall_detail));
    }
    if !view.description.trim().is_empty() {
        head.push_str("\n\n");
        head.push_str(view.description.trim());
    }
    let mut header = embed(
        Some(&escape(&view.title)),
        &head,
        colour,
        Some(&view.footer),
    );
    header.json["timestamp"] = json!(view.updated_at);
    embeds.push(header);

    // Active incidents and maintenance.
    for n in &view.notices {
        let mut text = clip(n.body.trim(), MAX_NOTICE_BODY);
        if !n.meta.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&clip(&n.meta, 200));
        }
        embeds.push(embed(
            Some(&format!("{} {}", notice_emoji(&n.class), escape(&n.title))),
            &text,
            colour_of_class(&n.class),
            None,
        ));
    }

    // Sections.
    for s in &view.sections {
        if s.rows.is_empty() {
            continue;
        }
        let title = s
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(escape);
        let section_colour =
            parse_colour(&view.colour).unwrap_or_else(|| colour_of_class(worst_class(&s.rows)));
        let mut text = String::new();
        let mut used = 0;
        for row in &s.rows {
            let line = row_line(row);
            let w = units(&line) + 1;
            if used > 0 && used + w > SECTION_BUDGET {
                embeds.push(embed(title.as_deref(), &text, section_colour, None));
                text.clear();
                used = 0;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&line);
            used += w;
        }
        if !text.is_empty() {
            embeds.push(embed(title.as_deref(), &text, section_colour, None));
        }
    }

    pack(embeds, &view.more_label)
}

/// Fills messages with embeds in order. A board needing more than
/// [`MAX_MESSAGES`] is cut there; its last embed says how many embeds were left out.
fn pack(embeds: Vec<Embed>, more_label: &str) -> Vec<Value> {
    let mut messages: Vec<Vec<Embed>> = vec![Vec::new()];
    let mut size = 0;
    let total = embeds.len();
    let mut placed = 0;
    'fill: for e in embeds {
        let current = messages.last_mut().expect("one message exists");
        if !current.is_empty() && (current.len() >= MAX_EMBEDS || size + e.size > MESSAGE_BUDGET) {
            if messages.len() >= MAX_MESSAGES {
                break 'fill;
            }
            messages.push(Vec::new());
            size = 0;
        }
        size += e.size;
        messages.last_mut().expect("one message exists").push(e);
        placed += 1;
    }
    if placed < total
        && let Some(last) = messages.last_mut().and_then(|m| m.last_mut())
    {
        let note = more_label.replace("{n}", &(total - placed).to_string());
        let text = last.json["description"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let joined = if text.is_empty() {
            note
        } else {
            format!("{text}\n{note}")
        };
        // The note replaces the tail of a full embed rather than overflowing it.
        let joined = clip(&joined, MAX_DESCRIPTION.min(SECTION_BUDGET + 200));
        last.json["description"] = json!(joined);
    }
    messages
        .into_iter()
        .map(|m| {
            json!({
                "embeds": m.into_iter().map(|e| e.json).collect::<Vec<_>>(),
                "allowed_mentions": { "parse": [] },
            })
        })
        .collect()
}

/// Characters of a message, counted the way the platforms do over all its
/// embeds (title, description, footer text, field names and values).
pub fn message_chars(message: &Value) -> usize {
    let mut total = 0;
    for e in message["embeds"].as_array().into_iter().flatten() {
        for key in ["title", "description"] {
            total += e[key].as_str().map_or(0, units);
        }
        total += e["footer"]["text"].as_str().map_or(0, units);
        for f in e["fields"].as_array().into_iter().flatten() {
            total += f["name"].as_str().map_or(0, units) + f["value"].as_str().map_or(0, units);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(sections: Vec<Section>) -> BoardView {
        BoardView {
            title: "Sylo status".into(),
            description: String::new(),
            overall_class: "up".into(),
            overall_title: "All systems operational".into(),
            overall_detail: "All 2 services are running normally.".into(),
            sections,
            notices: Vec::new(),
            updated_at: "2026-11-19T10:00:00Z".into(),
            footer: "Vigilant Vole".into(),
            more_label: "\u{2026} and {n} more".into(),
            colour: String::new(),
        }
    }

    fn rows(n: usize, name_len: usize) -> Vec<Row> {
        (0..n)
            .map(|i| Row {
                class: if i % 7 == 3 { "down" } else { "up" },
                name: format!("{i:04}{}", "x".repeat(name_len)),
                status: "Down".into(),
                details: "99.98 % (24 h) \u{b7} 99.5 % (30 d) \u{b7} 123 ms".into(),
            })
            .collect()
    }

    fn check_limits(messages: &[Value]) {
        assert!(!messages.is_empty() && messages.len() <= MAX_MESSAGES);
        for m in messages {
            let embeds = m["embeds"].as_array().unwrap();
            assert!(
                !embeds.is_empty() && embeds.len() <= MAX_EMBEDS,
                "{}",
                embeds.len()
            );
            assert!(
                message_chars(m) <= MAX_MESSAGE_CHARS,
                "{}",
                message_chars(m)
            );
            for e in embeds {
                assert!(units(e["title"].as_str().unwrap_or("")) <= MAX_TITLE);
                assert!(units(e["description"].as_str().unwrap_or("")) <= MAX_DESCRIPTION);
                assert!(units(e["footer"]["text"].as_str().unwrap_or("")) <= MAX_FOOTER);
            }
            assert_eq!(m["allowed_mentions"], json!({"parse": []}));
        }
    }

    #[test]
    fn a_small_board_is_one_message_with_header_and_section() {
        let v = view(vec![Section {
            name: Some("Core".into()),
            rows: rows(4, 4),
        }]);
        let m = build(&v);
        assert_eq!(m.len(), 1);
        let e = m[0]["embeds"].as_array().unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0]["title"], "Sylo status");
        assert_eq!(e[0]["color"], GREEN);
        assert_eq!(e[0]["timestamp"], "2026-11-19T10:00:00Z");
        assert!(
            e[0]["description"]
                .as_str()
                .unwrap()
                .contains("All systems operational")
        );
        assert_eq!(e[1]["title"], "Core");
        // The down row turns the section red and says why.
        let text = e[1]["description"].as_str().unwrap();
        assert_eq!(e[1]["color"], RED);
        assert!(
            text.contains("\u{1f534} **0003xxxx** \u{2014} Down \u{b7} 99.98 %"),
            "{text}"
        );
        assert!(
            text.starts_with("\u{1f7e2} **0000xxxx** \u{b7} 99.98 %"),
            "{text}"
        );
        check_limits(&m);
    }

    #[test]
    fn layout_is_deterministic() {
        let v = view(vec![Section {
            name: Some("A".into()),
            rows: rows(120, 20),
        }]);
        assert_eq!(build(&v), build(&v));
    }

    #[test]
    fn a_long_section_continues_in_more_embeds_and_messages_within_the_limits() {
        let v = view(vec![
            Section {
                name: Some("Big".into()),
                rows: rows(400, 30),
            },
            Section {
                name: None,
                rows: rows(5, 3),
            },
        ]);
        let m = build(&v);
        assert!(m.len() > 1, "{}", m.len());
        check_limits(&m);
        // Continuations keep the section's title.
        let titles: Vec<&str> = m
            .iter()
            .flat_map(|m| m["embeds"].as_array().unwrap())
            .filter_map(|e| e["title"].as_str())
            .collect();
        assert!(titles.iter().filter(|t| **t == "Big").count() > 1);
        // Nothing is lost while it fits: every row is there once.
        let all: String = m
            .iter()
            .flat_map(|m| m["embeds"].as_array().unwrap())
            .filter_map(|e| e["description"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            all.matches("**0399").count(),
            1,
            "the last big row is missing"
        );
    }

    #[test]
    fn more_than_ten_embeds_start_a_new_message() {
        let sections: Vec<Section> = (0..25)
            .map(|i| Section {
                name: Some(format!("S{i}")),
                rows: rows(1, 2),
            })
            .collect();
        let m = build(&view(sections));
        assert_eq!(m.len(), 3, "26 embeds make 10 + 10 + 6");
        assert_eq!(m[0]["embeds"].as_array().unwrap().len(), 10);
        assert_eq!(m[2]["embeds"].as_array().unwrap().len(), 6);
        check_limits(&m);
    }

    #[test]
    fn a_board_too_big_for_ten_messages_is_cut_with_a_note() {
        let sections: Vec<Section> = (0..300)
            .map(|i| Section {
                name: Some(format!("S{i}")),
                rows: rows(2, 2),
            })
            .collect();
        let m = build(&view(sections));
        assert_eq!(m.len(), MAX_MESSAGES);
        check_limits(&m);
        let last = m.last().unwrap()["embeds"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert!(
            last["description"].as_str().unwrap().contains("and 2"),
            "{last}"
        );
    }

    #[test]
    fn huge_text_is_clipped_and_astral_characters_count_double() {
        let mut v = view(vec![Section {
            name: Some("\u{1f600}".repeat(400)),
            rows: vec![Row {
                class: "up",
                name: "\u{1f600}".repeat(500),
                status: String::new(),
                details: "d".repeat(500),
            }],
        }]);
        v.title = "T".repeat(1000);
        v.description = "\u{1f600}".repeat(5000);
        v.notices.push(Notice {
            class: "danger".into(),
            title: "N".repeat(1000),
            body: "b".repeat(5000),
            meta: "m".repeat(5000),
        });
        let m = build(&v);
        check_limits(&m);
        assert!(units("\u{1f600}") == 2);
    }

    #[test]
    fn names_cannot_inject_markdown_and_messages_ping_nobody() {
        let v = view(vec![Section {
            name: Some("**bold** @everyone".into()),
            rows: vec![Row {
                class: "up",
                name: "[click](https://evil.test) _x_ <@123>".into(),
                status: String::new(),
                details: String::new(),
            }],
        }]);
        let m = build(&v);
        let text = m[0]["embeds"][1]["description"].as_str().unwrap();
        assert!(
            text.contains("\\[click\\]\\(https://evil.test\\) \\_x\\_ \\<@123\\>"),
            "{text}"
        );
        assert_eq!(m[0]["embeds"][1]["title"], "\\*\\*bold\\*\\* @everyone");
        assert_eq!(m[0]["allowed_mentions"]["parse"], json!([]));
        assert!(m[0]["allowed_mentions"].get("roles").is_none());
    }

    #[test]
    fn notices_get_their_own_coloured_embeds_after_the_header() {
        let mut v = view(Vec::new());
        v.overall_class = "down".into();
        v.notices.push(Notice {
            class: "maintenance".into(),
            title: "Database upgrade".into(),
            body: "Short break".into(),
            meta: "In progress".into(),
        });
        let m = build(&v);
        let e = m[0]["embeds"].as_array().unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0]["color"], RED);
        assert_eq!(e[1]["color"], BLUE);
        assert!(
            e[1]["title"]
                .as_str()
                .unwrap()
                .ends_with("Database upgrade")
        );
    }

    #[test]
    fn a_board_colour_overrides_the_status_colours() {
        let mut v = view(vec![Section {
            name: Some("S".into()),
            rows: rows(4, 2),
        }]);
        v.colour = "#112233".into();
        let m = build(&v);
        assert_eq!(m[0]["embeds"][0]["color"], 0x112233);
        assert_eq!(m[0]["embeds"][1]["color"], 0x112233);
        assert_eq!(parse_colour("purple"), Some(PURPLE));
        assert_eq!(parse_colour("#12345"), None);
        assert_eq!(parse_colour("#gggggg"), None);
        assert_eq!(parse_colour(""), None);
    }

    #[test]
    fn an_empty_board_is_still_one_message() {
        let m = build(&view(Vec::new()));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["embeds"].as_array().unwrap().len(), 1);
    }
}
