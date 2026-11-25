//! A product on the side-menu shell (`Shell::Sidebar`) with its own sections
//! and a switch to a second area: signed-in pages get the menu, the account
//! menu sits at its foot, the settings tabs move into it, and everything the
//! page policy cares about stays as it was. This is its own test binary
//! because the kit's configuration is installed once per process.

mod common;

use axum::http::{StatusCode, header};
use common::{App, set_up, side_current, side_hrefs};
use vole_kit::auth::Role;

async fn app() -> App {
    set_up(&kit_demo::KIT_SIDEBAR).await
}

#[tokio::test]
async fn signed_in_pages_have_the_side_menu_and_no_top_bar() {
    let mut app = app().await;
    for path in [
        "/",
        "/settings/security",
        "/settings/users",
        "/settings/tokens",
    ] {
        let page = app.get(path).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
        let html = &page.body;
        assert!(html.contains("<body class=\"has-side\">"), "{path}");
        assert!(
            html.contains("<link rel=\"stylesheet\" href=\"/static/shell.css\">"),
            "{path}"
        );
        assert!(
            html.contains("<script src=\"/static/shell.js\"></script>"),
            "{path}"
        );
        assert!(html.contains("class=\"side\" id=\"side\""), "{path}");
        assert!(html.contains("<nav class=\"side-nav\""), "{path}");
        assert!(!html.contains("class=\"topbar\""), "{path}: no top bar");
        assert!(!html.contains("class=\"tabbar\""), "{path}: no tab bar");
        // The page itself is the kit's, inside the shell.
        assert!(html.contains("<main id=\"main\""), "{path}");
        assert!(html.contains("class=\"side-body\""), "{path}");
    }
}

#[tokio::test]
async fn the_menu_has_the_products_sections_and_marks_the_current_page() {
    let mut app = app().await;
    let home = app.get("/").await.body;
    assert_eq!(
        side_hrefs(&home),
        [
            "/",
            "/settings/general",
            "/settings/users",
            "/settings/security",
            "/settings/tokens"
        ]
    );
    assert_eq!(side_current(&home), ["/"]);
    // One heading, for the settings; the first group has none.
    assert_eq!(home.matches("class=\"side-heading\"").count(), 1);
    assert!(home.contains(">Settings</h2>"));

    let users = app.get("/settings/users").await.body;
    assert_eq!(side_current(&users), ["/settings/users"]);
    let security = app.get("/settings/security").await.body;
    assert_eq!(side_current(&security), ["/settings/security"]);
}

#[tokio::test]
async fn the_settings_pages_lose_their_tab_column() {
    let mut app = app().await;
    let page = app.get("/settings/security").await.body;
    assert!(!page.contains("settings-side"));
    assert!(!page.contains("settings-nav"));
    assert!(page.contains("class=\"settings settings-flat\""));
    assert!(page.contains("id=\"settings-content\""));
}

#[tokio::test]
async fn the_switch_is_a_labelled_link_the_product_configures() {
    let mut app = app().await;
    let html = app.get("/").await.body;
    let at = html.find("<a class=\"side-switch\"").expect("the switch");
    let tag = &html[at..at + html[at..].find('>').unwrap()];
    assert!(tag.contains("href=\"/admin\""), "{tag}");
    // Its name is its visible text (both areas); the tooltip says what it does.
    assert!(!tag.contains("aria-label"), "{tag}");
    assert!(tag.contains("title=\"Switch to Admin\""), "{tag}");
    let switch = &html[at..at + html[at..].find("</a>").unwrap()];
    // The current area's name, then the other's.
    let here = switch.find(">Notes<").expect("current area");
    let there = switch.find(">Admin<").expect("other area");
    assert!(here < there);
    // It sits above the entries and outside the boosted navigation: it leaves
    // the area, so it is a plain navigation.
    assert!(at < html.find("<nav class=\"side-nav\"").unwrap());
}

#[tokio::test]
async fn the_account_menu_is_at_the_foot() {
    let mut app = app().await;
    let html = app.get("/").await.body;
    let foot = &html[html.find("<div class=\"side-foot\">").expect("the foot")..];
    let foot = &foot[..foot.find("<div class=\"side-body\">").unwrap()];
    assert!(foot.contains("popovertarget=\"user-menu\""));
    assert!(foot.contains("class=\"side-account-name\">ada<"));
    assert!(foot.contains("id=\"user-menu\" popover"));
    for item in ["/settings/security", "/settings/tokens", "/api-docs"] {
        assert!(foot.contains(&format!("href=\"{item}\"")), "{item}");
    }
    // The theme picker (both themes, and Auto) and Log out.
    for theme in ["auto", "light", "dark"] {
        assert!(
            foot.contains(&format!("data-theme-set=\"{theme}\"")),
            "{theme}"
        );
    }
    assert!(foot.contains("action=\"/logout\""));
    // The menu is the same one the top bar shows, and there is one of it.
    assert_eq!(html.matches("id=\"user-menu\"").count(), 1);
}

#[tokio::test]
async fn the_drawer_controls_are_labelled() {
    let mut app = app().await;
    let html = app.get("/").await.body;
    for needle in [
        "data-side-open aria-controls=\"side\" aria-expanded=\"false\" aria-label=\"Open menu\"",
        "data-side-collapse aria-controls=\"side\" aria-expanded=\"true\" aria-label=\"Collapse menu\"",
        "data-side-close aria-label=\"Close\"",
        "<nav class=\"side-nav\" aria-label=\"Main\"",
    ] {
        assert!(html.contains(needle), "{needle}");
    }
}

#[tokio::test]
async fn signed_out_pages_keep_the_kits_own_layouts() {
    let mut app = app().await;
    app.post("/logout", &[]).await;
    let login = app.get("/login").await;
    assert_eq!(login.status, StatusCode::OK);
    assert!(!login.body.contains("has-side"));
    assert!(!login.body.contains("shell.css"));
}

#[tokio::test]
async fn the_policy_and_the_page_stay_free_of_inline_code() {
    let mut app = app().await;
    for path in ["/", "/settings/security"] {
        let page = app.get(path).await;
        // The same policy as every signed-in page: no new source.
        assert_eq!(
            page.headers[header::CONTENT_SECURITY_POLICY],
            vole_kit::http::headers::APP_CSP,
            "{path}"
        );
        let html = &page.body;
        // No inline style attribute, handler or script: the shell's script
        // and style are files.
        assert!(!html.contains(" style=\""), "{path}");
        for attr in [
            " onclick=",
            " onload=",
            " onerror=",
            " onchange=",
            "javascript:",
        ] {
            assert!(!html.contains(attr), "{path}: {attr}");
        }
        for script in html.split("<script").skip(1) {
            let tag = script.split('>').next().unwrap();
            assert!(
                tag.contains("src=\"/static/") || tag.contains("type=\"application/json\""),
                "{path}: {tag}"
            );
        }
    }
}

#[tokio::test]
async fn the_shell_files_are_served_with_a_validator() {
    let mut app = app().await;
    for (path, kind, needle) in [
        ("/static/shell.css", "text/css", ".side-switch"),
        ("/static/shell.js", "text/javascript", "data-side-collapse"),
    ] {
        let res = app.get(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}");
        assert!(
            res.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(kind)
        );
        assert!(res.body.contains(needle), "{path}");
        assert!(res.headers.contains_key(header::ETAG), "{path}");
    }
    // The design system's own stylesheet does not carry the shell.
    assert!(
        !app.get("/static/style.css")
            .await
            .body
            .contains(".side-switch")
    );
}

#[tokio::test]
async fn the_menu_shows_only_what_a_user_may_see() {
    let mut app = app().await;
    app.sign_in_as("vic", Role::Viewer).await;
    let page = app.get("/settings/security").await;
    assert_eq!(page.status, StatusCode::OK);
    // A viewer has no General or Users page.
    assert_eq!(
        side_hrefs(&page.body),
        ["/", "/settings/security", "/settings/tokens"]
    );
    assert!(page.body.contains("class=\"side-account-meta\">Viewer<"));
}

#[tokio::test]
async fn the_menu_follows_the_site_language() {
    let mut app = app().await;
    vole_kit::settings::set_site_language(&app.core, Some(vole_kit::i18n::Lang::NB))
        .await
        .unwrap();
    let html = app.get("/settings/security").await.body;
    assert!(html.contains("lang=\"nb\""), "the language was set");
    assert!(html.contains("aria-label=\"Åpne menyen\""), "{html}");
    assert!(html.contains("title=\"Bytt til Admin\""), "{html}");
    vole_kit::settings::set_site_language(&app.core, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_brand_leads_to_the_start_page_the_product_configures() {
    let mut app = app().await;
    for path in ["/", "/settings/security"] {
        let html = app.get(path).await.body;
        // The bar on phones and the head of the menu.
        assert_eq!(
            html.matches("<a class=\"brand\" href=\"/notes\">").count(),
            2,
            "{path}"
        );
        assert!(!html.contains("<a class=\"brand\" href=\"/\">"), "{path}");
    }
}
