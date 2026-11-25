//! A product that only switches the shell on (`Sidebar::DEFAULT`): the menu
//! is made from its navigation and its settings tabs, and with no switch
//! configured there is none. Its own test binary: the kit's configuration is
//! installed once per process.

mod common;

use common::{App, set_up, side_current, side_hrefs};
use vole_kit::auth::Role;

async fn app() -> App {
    set_up(&kit_demo::KIT_SIDEBAR_DEFAULT).await
}

#[tokio::test]
async fn the_menu_is_made_from_the_navigation_and_the_settings_tabs() {
    let mut app = app().await;
    let html = app.get("/settings/users").await.body;
    // The demo's navigation is "Notes" and "Settings"; the second is the
    // group of tabs, so it is not an entry of its own.
    assert_eq!(
        side_hrefs(&html),
        [
            "/",
            "/settings/general",
            "/settings/users",
            "/settings/security",
            "/settings/tokens"
        ]
    );
    assert_eq!(side_current(&html), ["/settings/users"]);
    assert_eq!(html.matches("class=\"side-heading\"").count(), 1);
}

#[tokio::test]
async fn no_switch_is_shown_unless_the_product_configures_one() {
    let mut app = app().await;
    let html = app.get("/").await.body;
    assert!(html.contains("class=\"side\" id=\"side\""));
    assert!(!html.contains("side-switch"));
    assert_eq!(side_current(&html), ["/"]);
}

#[tokio::test]
async fn an_editor_sees_no_admin_tabs() {
    let mut app = app().await;
    app.sign_in_as("eve", Role::Editor).await;
    let html = app.get("/settings/security").await.body;
    assert_eq!(
        side_hrefs(&html),
        ["/", "/settings/security", "/settings/tokens"]
    );
    assert!(html.contains("class=\"side-account-meta\">Editor<"));
}
