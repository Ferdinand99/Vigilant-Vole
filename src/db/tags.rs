//! Tags and monitor<->tag links (`tags`, `monitor_tags`). Owned by the monitors agent.
//!
//! Tag colours are design-system palette *names* (see [`PALETTE`]), never hex.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

/// Palette names understood by `.tag-{color}` in the stylesheet, in picker order.
pub const PALETTE: &[&str] = &[
    "red", "orange", "yellow", "green", "mint", "teal", "cyan", "blue", "indigo", "purple", "pink",
    "brown", "gray",
];

/// Colour used when none (or an unknown one) is given.
pub const DEFAULT_COLOR: &str = "blue";

/// Longest tag name / value we accept.
pub const MAX_NAME_LEN: usize = 40;
pub const MAX_VALUE_LEN: usize = 60;

/// A row of `tags`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    /// Palette name (`blue`, `green`, …).
    pub color: String,
}

/// A tag plus how many monitors use it (tags page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagWithCount {
    pub tag: Tag,
    pub monitors: i64,
}

/// A tag as attached to one monitor (with its optional value, e.g. `env: prod`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorTag {
    pub tag_id: i64,
    pub name: String,
    pub color: String,
    pub value: String,
}

/// Maps any stored/imported colour to a palette name. Unknown names and hex
/// values fall back to the nearest sensible palette entry.
pub fn normalize_color(color: &str) -> &'static str {
    let c = color.trim().to_ascii_lowercase();
    if let Some(p) = PALETTE.iter().find(|p| **p == c) {
        return p;
    }
    if let Some(hex) = c.strip_prefix('#') {
        return nearest_palette(hex).unwrap_or(DEFAULT_COLOR);
    }
    match c.as_str() {
        "grey" => "gray",
        "violet" => "purple",
        "aqua" => "cyan",
        _ => DEFAULT_COLOR,
    }
}

/// Nearest palette name for a `rrggbb` / `rgb` hex string (by hue, gray for
/// unsaturated colours). Used for imported Uptime Kuma tags.
fn nearest_palette(hex: &str) -> Option<&'static str> {
    let expand = |s: &str| -> Option<(f64, f64, f64)> {
        let v = |i: usize, len: usize| u8::from_str_radix(&s[i..i + len], 16).ok();
        match s.len() {
            6 => Some((v(0, 2)? as f64, v(2, 2)? as f64, v(4, 2)? as f64)),
            3 => Some((
                v(0, 1)? as f64 * 17.0,
                v(1, 1)? as f64 * 17.0,
                v(2, 1)? as f64 * 17.0,
            )),
            _ => None,
        }
    };
    let (r, g, b) = expand(hex)?;
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    if max < 0.0001 || delta / max < 0.2 {
        return Some("gray");
    }
    let hue = if (max - r).abs() < f64::EPSILON {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < f64::EPSILON {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    // Dark oranges read as brown.
    if (15.0..50.0).contains(&hue) && max < 0.6 {
        return Some("brown");
    }
    Some(match hue {
        h if h < 15.0 => "red",
        h if h < 40.0 => "orange",
        h if h < 65.0 => "yellow",
        h if h < 150.0 => "green",
        h if h < 170.0 => "mint",
        h if h < 185.0 => "teal",
        h if h < 205.0 => "cyan",
        h if h < 245.0 => "blue",
        h if h < 258.0 => "indigo",
        h if h < 300.0 => "purple",
        h if h < 345.0 => "pink",
        _ => "red",
    })
}

fn row_to_tag(row: &Row) -> rusqlite::Result<Tag> {
    let color: String = row.get("color")?;
    Ok(Tag {
        id: row.get("id")?,
        name: row.get("name")?,
        color: normalize_color(&color).to_string(),
    })
}

/// Every tag, alphabetically (case-insensitive).
pub fn list_tags(conn: &Connection) -> rusqlite::Result<Vec<Tag>> {
    let mut stmt =
        conn.prepare("SELECT id, name, color FROM tags ORDER BY name COLLATE NOCASE, id")?;
    let rows = stmt.query_map([], row_to_tag)?;
    rows.collect()
}

/// Every tag with its usage count.
pub fn list_tags_with_counts(conn: &Connection) -> rusqlite::Result<Vec<TagWithCount>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.color, COUNT(mt.monitor_id) AS monitors
         FROM tags t LEFT JOIN monitor_tags mt ON mt.tag_id = t.id
         GROUP BY t.id ORDER BY t.name COLLATE NOCASE, t.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(TagWithCount {
            tag: row_to_tag(row)?,
            monitors: row.get("monitors")?,
        })
    })?;
    rows.collect()
}

pub fn get_tag(conn: &Connection, id: i64) -> rusqlite::Result<Option<Tag>> {
    conn.query_row(
        "SELECT id, name, color FROM tags WHERE id = ?1",
        params![id],
        row_to_tag,
    )
    .optional()
}

/// Case-insensitive lookup by name.
pub fn find_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<Tag>> {
    conn.query_row(
        "SELECT id, name, color FROM tags WHERE name = ?1 COLLATE NOCASE ORDER BY id LIMIT 1",
        params![name.trim()],
        row_to_tag,
    )
    .optional()
}

/// Inserts a tag (colour normalised to the palette). Fails on a duplicate name.
pub fn insert_tag(conn: &Connection, name: &str, color: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO tags (name, color) VALUES (?1, ?2)",
        params![name.trim(), normalize_color(color)],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Returns the existing tag with this name (case-insensitive) or creates it.
pub fn get_or_create(conn: &Connection, name: &str, color: &str) -> rusqlite::Result<i64> {
    match find_by_name(conn, name)? {
        Some(tag) => Ok(tag.id),
        None => insert_tag(conn, name, color),
    }
}

/// Renames / recolours a tag. Returns false when it doesn't exist.
pub fn update_tag(conn: &Connection, id: i64, name: &str, color: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE tags SET name = ?2, color = ?3 WHERE id = ?1",
        params![id, name.trim(), normalize_color(color)],
    )?;
    Ok(n > 0)
}

/// Deletes a tag (and, via cascade, its monitor links).
pub fn delete_tag(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM tags WHERE id = ?1", params![id])? > 0)
}

/// Whether another tag (not `except_id`) already has this name (case-insensitive).
pub fn name_taken(conn: &Connection, name: &str, except_id: Option<i64>) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tags WHERE name = ?1 COLLATE NOCASE AND id IS NOT ?2)",
        params![name.trim(), except_id],
        |r| r.get(0),
    )
}

fn row_to_monitor_tag(row: &Row) -> rusqlite::Result<MonitorTag> {
    let color: String = row.get("color")?;
    Ok(MonitorTag {
        tag_id: row.get("tag_id")?,
        name: row.get("name")?,
        color: normalize_color(&color).to_string(),
        value: row.get("value")?,
    })
}

/// The tags of one monitor, alphabetically.
pub fn tags_for_monitor(conn: &Connection, monitor_id: i64) -> rusqlite::Result<Vec<MonitorTag>> {
    let mut stmt = conn.prepare(
        "SELECT mt.tag_id, t.name, t.color, mt.value
         FROM monitor_tags mt JOIN tags t ON t.id = mt.tag_id
         WHERE mt.monitor_id = ?1 ORDER BY t.name COLLATE NOCASE, t.id",
    )?;
    let rows = stmt.query_map(params![monitor_id], row_to_monitor_tag)?;
    rows.collect()
}

/// Tags of every monitor in one query, keyed by monitor id.
pub fn tags_by_monitor(conn: &Connection) -> rusqlite::Result<HashMap<i64, Vec<MonitorTag>>> {
    let mut stmt = conn.prepare(
        "SELECT mt.monitor_id, mt.tag_id, t.name, t.color, mt.value
         FROM monitor_tags mt JOIN tags t ON t.id = mt.tag_id
         ORDER BY t.name COLLATE NOCASE, t.id",
    )?;
    let mut map: HashMap<i64, Vec<MonitorTag>> = HashMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>("monitor_id")?, row_to_monitor_tag(row)?))
    })?;
    for row in rows {
        let (monitor_id, tag) = row?;
        map.entry(monitor_id).or_default().push(tag);
    }
    Ok(map)
}

/// Replaces a monitor's tags with `tags` (`(tag_id, value)`; duplicates keep the last value).
pub fn set_monitor_tags(
    conn: &Connection,
    monitor_id: i64,
    tags: &[(i64, String)],
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM monitor_tags WHERE monitor_id = ?1",
        params![monitor_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO monitor_tags (monitor_id, tag_id, value) VALUES (?1, ?2, ?3)
         ON CONFLICT (monitor_id, tag_id) DO UPDATE SET value = excluded.value",
    )?;
    for (tag_id, value) in tags {
        stmt.execute(params![monitor_id, tag_id, value.trim()])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewMonitor;
    use crate::db::{monitors, test_conn};

    fn monitor(conn: &Connection, name: &str) -> i64 {
        monitors::insert_monitor(
            conn,
            &NewMonitor {
                name: name.into(),
                ..NewMonitor::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn crud_round_trip() {
        let conn = test_conn();
        let id = insert_tag(&conn, "  prod ", "green").unwrap();
        assert_eq!(
            get_tag(&conn, id).unwrap(),
            Some(Tag {
                id,
                name: "prod".into(),
                color: "green".into()
            })
        );
        assert!(update_tag(&conn, id, "production", "purple").unwrap());
        assert_eq!(get_tag(&conn, id).unwrap().unwrap().name, "production");
        assert_eq!(get_tag(&conn, id).unwrap().unwrap().color, "purple");
        assert!(delete_tag(&conn, id).unwrap());
        assert!(get_tag(&conn, id).unwrap().is_none());
        assert!(!delete_tag(&conn, id).unwrap());
        assert!(!update_tag(&conn, id, "x", "red").unwrap());
    }

    #[test]
    fn names_are_unique_and_lookups_ignore_case() {
        let conn = test_conn();
        let id = insert_tag(&conn, "Prod", "red").unwrap();
        assert!(insert_tag(&conn, "Prod", "blue").is_err());
        assert_eq!(find_by_name(&conn, "prod").unwrap().map(|t| t.id), Some(id));
        assert!(name_taken(&conn, "PROD", None).unwrap());
        assert!(!name_taken(&conn, "prod", Some(id)).unwrap());
        assert_eq!(get_or_create(&conn, "prod", "blue").unwrap(), id);
        assert_ne!(get_or_create(&conn, "staging", "blue").unwrap(), id);
    }

    #[test]
    fn unknown_colours_map_to_the_palette() {
        assert_eq!(normalize_color("GREEN"), "green");
        assert_eq!(normalize_color("grey"), "gray");
        assert_eq!(normalize_color("#ff0000"), "red");
        assert_eq!(normalize_color("#00f"), "blue");
        assert_eq!(normalize_color("#808080"), "gray");
        assert_eq!(normalize_color("#34c759"), "green");
        assert_eq!(normalize_color("#7c3aed"), "purple");
        assert_eq!(normalize_color("#zzzzzz"), DEFAULT_COLOR);
        assert_eq!(normalize_color(""), DEFAULT_COLOR);
        let conn = test_conn();
        let id = insert_tag(&conn, "x", "#ff9500").unwrap();
        assert_eq!(get_tag(&conn, id).unwrap().unwrap().color, "orange");
    }

    #[test]
    fn monitor_tags_are_replaced_and_counted() {
        let conn = test_conn();
        let a = monitor(&conn, "a");
        let b = monitor(&conn, "b");
        let env = insert_tag(&conn, "env", "blue").unwrap();
        let team = insert_tag(&conn, "Team", "pink").unwrap();
        let unused = insert_tag(&conn, "unused", "gray").unwrap();

        set_monitor_tags(&conn, a, &[(env, "prod".into()), (team, String::new())]).unwrap();
        set_monitor_tags(&conn, b, &[(env, "staging".into()), (env, " dev ".into())]).unwrap();

        let a_tags = tags_for_monitor(&conn, a).unwrap();
        assert_eq!(a_tags.len(), 2);
        assert_eq!(
            (a_tags[0].name.as_str(), a_tags[0].value.as_str()),
            ("env", "prod")
        );
        assert_eq!(tags_for_monitor(&conn, b).unwrap()[0].value, "dev");

        let by_monitor = tags_by_monitor(&conn).unwrap();
        assert_eq!(by_monitor[&a].len(), 2);
        assert_eq!(by_monitor[&b].len(), 1);

        let counts: HashMap<i64, i64> = list_tags_with_counts(&conn)
            .unwrap()
            .into_iter()
            .map(|t| (t.tag.id, t.monitors))
            .collect();
        assert_eq!((counts[&env], counts[&team], counts[&unused]), (2, 1, 0));

        set_monitor_tags(&conn, a, &[]).unwrap();
        assert!(tags_for_monitor(&conn, a).unwrap().is_empty());

        // Deleting a tag removes its links; deleting a monitor removes its links.
        delete_tag(&conn, env).unwrap();
        assert!(tags_for_monitor(&conn, b).unwrap().is_empty());
        set_monitor_tags(&conn, b, &[(team, String::new())]).unwrap();
        monitors::delete_monitor(&conn, b).unwrap();
        assert_eq!(
            list_tags_with_counts(&conn)
                .unwrap()
                .iter()
                .map(|t| t.monitors)
                .sum::<i64>(),
            0
        );
    }
}
