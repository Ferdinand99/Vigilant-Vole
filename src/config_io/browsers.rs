//! The `remote_browsers` section: CDP endpoints for real-browser monitors,
//! keyed by `name`. Monitors refer to them with the `remote_browser` option
//! (a name); without one they use the first remote browser.
//!
//! A token or password in the URL is exported as [`SECRET_MASK`] unless
//! secrets are included; importing a masked URL keeps the stored secret as
//! long as the URL still points at the same scheme, host and port.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{Action, KIND_BROWSER, Mode, Placeholders, Plan, norm, resolve};
use crate::db::remote_browsers::{self as db_rb, RemoteBrowser};
use crate::monitor::checkers::real_browser::endpoint;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteBrowserDoc {
    pub name: String,
    /// `ws(s)://…/devtools/browser/<id>`, `http(s)://host:port` or `wss://host?token=…`.
    pub url: String,
}

#[derive(Debug, Clone)]
pub(super) enum BrowserOp {
    Create { ph: i64, name: String, url: String },
    Update { id: i64, name: String, url: String },
}

/// The section as exported.
pub(super) fn export(conn: &Connection, include_secrets: bool) -> rusqlite::Result<Vec<RemoteBrowserDoc>> {
    Ok(db_rb::list(conn)?
        .into_iter()
        .map(|b| RemoteBrowserDoc { url: if include_secrets { b.url } else { endpoint::masked(&b.url) }, name: b.name })
        .collect())
}

/// Plans the section. Returns every remote browser of the final state as
/// `(normalised name, id or placeholder)`, oldest first (the first one is
/// the default for monitors that don't name one).
pub(super) fn plan(
    cur: &[RemoteBrowser],
    docs: Option<&[RemoteBrowserDoc]>,
    mode: Mode,
    plan: &mut Plan,
    ops: &mut Vec<BrowserOp>,
    deletes: &mut Vec<i64>,
    ph: &mut Placeholders,
) -> Vec<(String, i64)> {
    let mut finals: Vec<(String, i64)> = cur.iter().map(|b| (norm(&b.name), b.id)).collect();
    let Some(docs) = docs else {
        return finals;
    };
    let mut seen = HashSet::new();
    for d in docs {
        let name = d.name.trim();
        if name.is_empty() || name.chars().count() > db_rb::MAX_NAME_LEN {
            plan.error(KIND_BROWSER, name, Some("name"), format!("Remote browser names need 1 to {} characters.", db_rb::MAX_NAME_LEN));
            continue;
        }
        if !seen.insert(norm(name)) {
            plan.error(KIND_BROWSER, name, None, "This remote browser is listed twice.");
            continue;
        }
        let existing = cur.iter().find(|b| norm(&b.name) == norm(name));
        let url = match endpoint::unmask(&d.url, existing.map(|b| b.url.as_str())) {
            Ok(u) => u,
            Err(endpoint::Unmask::Reenter) => {
                plan.error(KIND_BROWSER, name, Some("url"), "The address was exported without secrets; fill in its token or password.");
                continue;
            }
        };
        if let Err(msg) = endpoint::parse(&url) {
            plan.error(KIND_BROWSER, name, Some("url"), format!("{msg}."));
            continue;
        }
        match existing {
            Some(b) => {
                let mut changes = Vec::new();
                if b.name != name {
                    changes.push("name".to_string());
                }
                if !same_url(&b.url, &url) {
                    changes.push("url".to_string());
                }
                if changes.is_empty() {
                    plan.item(KIND_BROWSER, name, Action::Unchanged, changes);
                } else {
                    plan.item(KIND_BROWSER, name, Action::Update, changes);
                    ops.push(BrowserOp::Update { id: b.id, name: name.to_string(), url });
                }
            }
            None => {
                let id = ph.next();
                plan.item(KIND_BROWSER, name, Action::Create, Vec::new());
                ops.push(BrowserOp::Create { ph: id, name: name.to_string(), url });
                finals.push((norm(name), id));
            }
        }
    }
    if mode == Mode::Replace {
        for b in cur {
            if !seen.contains(&norm(&b.name)) {
                plan.item(KIND_BROWSER, &b.name, Action::Delete, Vec::new());
                deletes.push(b.id);
                finals.retain(|(_, id)| *id != b.id);
            }
        }
    }
    finals
}

/// Whether two URLs are the same address (`http://h:1` = `http://h:1/`).
fn same_url(a: &str, b: &str) -> bool {
    match (url::Url::parse(a.trim()), url::Url::parse(b.trim())) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
}

/// Writes the section; new ids go into `ids` (placeholder → id).
pub(super) fn execute(conn: &Connection, ops: &[BrowserOp], deletes: &[i64], ids: &mut HashMap<i64, i64>) -> rusqlite::Result<()> {
    for id in deletes {
        db_rb::delete(conn, *id)?;
    }
    for op in ops {
        match op {
            BrowserOp::Create { ph, name, url } => {
                let id = db_rb::insert(conn, name, url)?;
                ids.insert(*ph, id);
            }
            BrowserOp::Update { id, name, url } => {
                db_rb::update(conn, *id, name, url)?;
            }
        }
    }
    Ok(())
}

/// A monitor's `remote_browser` id in the written database.
pub(super) fn resolve_id(ids: &HashMap<i64, i64>, id: Option<i64>) -> rusqlite::Result<Option<i64>> {
    id.map(|b| resolve(ids, b)).transpose()
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::db::remote_browsers as db_rb;
    use crate::db::test_conn;

    const DOC: &str = r##"
remote_browsers:
  - name: Chrome
    url: http://chrome:9222
  - name: Browserless
    url: wss://browserless.example.com?token=s3cret
monitors:
  - name: Shop
    monitor_type: real_browser
    target: https://shop.example.com/
    remote_browser: Browserless
    browser_selector: "#app"
    browser_wait: network_idle
    browser_viewport: mobile
    browser_js_errors: true
  - name: Blog
    monitor_type: real_browser
    target: https://blog.example.com/
    keyword: Welcome
"##;

    fn applied(conn: &mut Connection, text: &str, mode: Mode) -> Applied {
        let doc = parse_document(text).unwrap();
        let out = apply(conn, &doc, mode).unwrap();
        out.unwrap_or_else(|p| panic!("{:?}", p.errors))
    }

    fn monitor(conn: &Connection, name: &str) -> Monitor {
        db_monitors::list_monitors(conn).unwrap().into_iter().find(|m| m.name == name).unwrap()
    }

    #[test]
    fn browsers_are_created_and_monitors_refer_to_them_by_name() {
        let mut conn = test_conn();
        applied(&mut conn, DOC, Mode::Merge);
        let browsers = db_rb::list(&conn).unwrap();
        assert_eq!(browsers.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["Chrome", "Browserless"]);
        let shop = monitor(&conn, "Shop");
        assert_eq!(shop.remote_browser_id, Some(browsers[1].id));
        assert_eq!((shop.browser_selector.as_deref(), shop.browser_wait.as_str(), shop.browser_viewport.as_str()), (Some("#app"), "network_idle", "mobile"));
        assert!(shop.browser_js_errors);
        // Without a name: the first (default) browser.
        let blog = monitor(&conn, "Blog");
        assert_eq!((blog.remote_browser_id, blog.keyword.as_deref()), (Some(browsers[0].id), Some("Welcome")));
    }

    #[test]
    fn exports_mask_tokens_round_trip_and_reapply_as_no_ops() {
        let mut conn = test_conn();
        applied(&mut conn, DOC, Mode::Merge);
        let masked = export(&conn, false).unwrap();
        let text = render(&masked, Format::Yaml).unwrap();
        assert!(!text.contains("s3cret") && text.contains("token=••••••"), "{text}");
        assert!(text.contains("remote_browser: Browserless"), "{text}");
        assert!(!text.contains("remote_browser_id"), "{text}");
        // The masked export applies to the same database without changes.
        let p = plan(&conn, &masked, Mode::Merge).unwrap();
        assert!(p.errors.is_empty() && !p.has_changes(), "{:?} {:?}", p.errors, p.items);
        // With secrets, onto a fresh instance, everything comes back.
        let full = export(&conn, true).unwrap();
        let mut fresh = test_conn();
        applied(&mut fresh, &render(&full, Format::Yaml).unwrap(), Mode::Merge);
        assert_eq!(db_rb::get_by_name(&fresh, "Browserless").unwrap().unwrap().url, "wss://browserless.example.com?token=s3cret");
        let shop = monitor(&fresh, "Shop");
        assert_eq!(shop.remote_browser_id, db_rb::get_by_name(&fresh, "Browserless").unwrap().map(|b| b.id));
        // The masked one can't create the browser elsewhere.
        let mut other = test_conn();
        let p = plan(&other, &masked, Mode::Merge).unwrap();
        assert!(p.errors.iter().any(|e| e.kind == KIND_BROWSER && e.field.as_deref() == Some("url")), "{:?}", p.errors);
        assert!(apply(&mut other, &masked, Mode::Merge).unwrap().is_err());
    }

    #[test]
    fn unknown_names_bad_urls_and_missing_browsers_are_reported() {
        let mut conn = test_conn();
        let bad = r#"
remote_browsers:
  - name: Broken
    url: ftp://nope
monitors:
  - name: Shop
    monitor_type: real_browser
    target: https://shop.example.com/
    remote_browser: Nowhere
"#;
        let p = plan(&conn, &parse_document(bad).unwrap(), Mode::Merge).unwrap();
        assert!(p.errors.iter().any(|e| e.kind == KIND_BROWSER && e.key == "Broken"), "{:?}", p.errors);
        assert!(p.errors.iter().any(|e| e.field.as_deref() == Some("remote_browser") && e.message.contains("Nowhere")), "{:?}", p.errors);
        // No browser at all: the monitor is imported with a warning.
        let lone = "monitors:\n  - name: Shop\n    monitor_type: real_browser\n    target: https://shop.example.com/\n";
        let done = applied(&mut conn, lone, Mode::Merge);
        assert!(done.plan.warnings.iter().any(|w| w.contains("Shop") && w.contains("remote browser")), "{:?}", done.plan.warnings);
        assert_eq!(monitor(&conn, "Shop").remote_browser_id, None);
        // remote_browser_id isn't a document field.
        let raw = "monitors:\n  - name: X\n    monitor_type: real_browser\n    target: https://x.example/\n    remote_browser_id: 1\n";
        assert!(!plan_of(&conn, raw).errors.is_empty());
    }

    fn plan_of(conn: &Connection, text: &str) -> Plan {
        plan(conn, &parse_document(text).unwrap(), Mode::Merge).unwrap()
    }

    #[test]
    fn replace_removes_unlisted_browsers_and_updates_changed_ones() {
        let mut conn = test_conn();
        applied(&mut conn, DOC, Mode::Merge);
        let next = r#"
remote_browsers:
  - name: Chrome
    url: ws://chrome:9222/devtools/browser/abc
"#;
        let p = plan_of(&conn, next);
        assert_eq!(p.action_for(KIND_BROWSER, "Chrome"), Some(Action::Update));
        let done = applied(&mut conn, next, Mode::Replace);
        assert_eq!(done.plan.action_for(KIND_BROWSER, "Browserless"), Some(Action::Delete));
        assert_eq!(db_rb::list(&conn).unwrap().len(), 1);
        assert_eq!(monitor(&conn, "Shop").remote_browser_id, None, "falls back to the default");
    }
}
