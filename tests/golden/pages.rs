//! Rendered HTML of the main pages, in English and in a right-to-left
//! language (Arabic), so templates, layouts, `lang`/`dir` and translations
//! can't drift unnoticed. Volatile parts are normalised by `norm::page`.

use insta::assert_snapshot;
use vigilant_vole::i18n::Lang;

use crate::harness::{App, PAGE_SLUG, Res};
use crate::norm;

/// English, and Arabic for right-to-left.
const LANGS: [&str; 2] = ["en", "ar"];

fn lang(code: &str) -> Lang {
    Lang::from_code(code).unwrap()
}

fn render(res: &Res, code: &str) -> String {
    format!(
        "status: {}\ncontent-type: {}\n\n{}",
        res.status.as_u16(),
        res.header("content-type"),
        time_zone_options(&norm::page(&res.text(), lang(code)))
    )
}

/// The time zone picker lists the system's tz database, which differs
/// between machines: keep the selected option, check the rest is there and
/// replace it with a marker.
fn time_zone_options(html: &str) -> String {
    let select =
        regex::Regex::new(r#"(?s)(<select id="f-timezone"[^>]*>)(.*?)(\s*</select>)"#).unwrap();
    select
        .replace_all(html, |c: &regex::Captures| {
            let options = &c[2];
            assert!(
                options.contains(r#"<option value="Europe/Oslo">"#),
                "the time zone picker lists the tz database"
            );
            let selected: String = options
                .lines()
                .filter(|l| l.contains(" selected"))
                .map(|l| format!("\n{l}"))
                .collect();
            format!(
                "{}{selected}\n            <!-- other time zones: the system tz database -->{}",
                &c[1], &c[3]
            )
        })
        .into_owned()
}

#[tokio::test]
async fn setup_page() {
    let mut app = App::fresh("page-setup").await;
    for code in LANGS {
        let res = app.get_with("/setup", &[("accept-language", code)]).await;
        assert_eq!(res.status, 200, "{}", res.text());
        assert_snapshot!(format!("page_setup_{code}"), render(&res, code));
    }
}

#[tokio::test]
async fn signed_out_pages() {
    let mut app = App::seeded("page-signed-out").await;
    app.clear_cookies();
    let status_page = format!("/status/{PAGE_SLUG}");
    for code in LANGS {
        for (name, path) in [("login", "/login"), ("status_page", status_page.as_str())] {
            let res = app.get_with(path, &[("accept-language", code)]).await;
            assert_eq!(res.status, 200, "{path}: {}", res.text());
            assert_snapshot!(format!("page_{name}_{code}"), render(&res, code));
        }
    }
}

#[tokio::test]
async fn signed_in_pages() {
    let mut app = App::seeded("page-signed-in").await;
    for code in LANGS {
        for (name, path, accept) in [
            ("dashboard", "/", "text/html"),
            ("settings_general", "/settings/general", "text/html"),
            ("settings_users", "/settings/users", "text/html"),
            ("settings_tokens", "/settings/tokens", "text/html"),
            ("api_docs", "/api-docs", "text/html"),
            ("not_found", "/no-such-page", "text/html"),
        ] {
            let res = app
                .get_with(path, &[("accept-language", code), ("accept", accept)])
                .await;
            assert_eq!(
                res.status,
                if name == "not_found" { 404 } else { 200 },
                "{path}: {}",
                res.text()
            );
            assert_snapshot!(format!("page_{name}_{code}"), render(&res, code));
        }
    }
}
