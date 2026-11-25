//! Rust side of the layout contract: `PageCtx` (required by `layout.html`),
//! the current-user view, one-shot flash messages and [`Ui`], what the
//! shared layouts show for the product (logo, navigation, settings tabs).

use axum_extra::extract::cookie::{Cookie, SameSite, SignedCookieJar};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use axum::extract::FromRef;

use super::auth::{CurrentUser, Role};
use super::config::KitConfig;
use super::db::settings;
use super::state::Core;

fn flash_cookie() -> &'static str {
    crate::config::get().flash_cookie
}

/// Everything `layout.html` needs. Every full-page template struct has a
/// `ctx: PageCtx` field.
#[derive(Debug, Clone)]
pub struct PageCtx {
    /// Page title; the layout renders `<title>{title} · {site_name}</title>`.
    pub title: String,
    /// Active main navigation item: a [`NavItem::id`] of the product's
    /// [`Ui`] ("settings" on settings pages), or "".
    pub nav: &'static str,
    /// Active settings tab: a [`SettingsTab::id`], or "".
    pub subnav: &'static str,
    pub user: Option<UserView>,
    pub site_name: String,
    /// One-shot message set by the previous request via [`flash`].
    pub flash: Option<Flash>,
    /// `<html lang>`: the request's language (`i18n::current()`).
    pub lang: String,
    /// `<html dir>`: `ltr` or `rtl`.
    pub dir: &'static str,
}

/// What the kit's layouts (`templates/*layout.html`) show for the product.
/// Part of [`KitConfig`](crate::config::KitConfig); templates reach it with
/// `kit_ui()` (a [`Tr`](crate::i18n::Tr) helper).
pub struct Ui {
    /// Logo in the top bar and on the sign-in card (an image URL).
    pub logo: &'static str,
    /// `<link rel="icon">` (SVG).
    pub favicon: &'static str,
    /// `<link rel="apple-touch-icon">`; empty for none.
    pub apple_touch_icon: &'static str,
    /// `<link rel="manifest">`; empty for none.
    pub manifest: &'static str,
    /// Main navigation in order: the top bar, and the tab bar on phones.
    pub nav: &'static [NavItem],
    /// Settings tabs in order. The kit's own pages have theirs as
    /// [`SettingsTab::USERS`], [`SettingsTab::SECURITY`] and
    /// [`SettingsTab::TOKENS`]; leave one out and its page has no tab.
    pub settings_tabs: &'static [SettingsTab],
    /// Where the product name in the public pages' "Powered by" footer links.
    pub home_url: &'static str,
}

/// The frame around the signed-in pages
/// ([`KitFeatures::shell`](crate::config::KitFeatures::shell)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// The kit as it has always been: a top bar with the main navigation
    /// (a tab bar on phones), and the settings tabs in a column beside the
    /// settings pages.
    TopBar,
    /// A side menu: the product's name and a fold button on top, an
    /// optional switch to a sibling area (Admin ⇅ Vault), the navigation in
    /// sections, and the account menu (Security, API tokens, theme, log out)
    /// at the foot. On a phone the menu is a drawer that a bar at the top
    /// opens. The settings pages lose their tab column: the tabs are entries
    /// of the menu.
    Sidebar(&'static Sidebar),
}

/// What the side menu ([`Shell::Sidebar`]) shows besides the account menu.
///
/// ```ignore
/// features: &KitFeatures {
///     shell: Shell::Sidebar(&Sidebar::DEFAULT),
///     ..KitFeatures::DEFAULT
/// },
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct Sidebar {
    /// A switch to a sibling area, shown above the entries; `None` for no
    /// switch.
    pub switcher: Option<&'static Switcher>,
    /// The entries, in sections. Empty: the menu is made from [`Ui::nav`]
    /// (without the item with the id `settings`, which the next section
    /// replaces) and, under the heading `nav.settings`, the settings tabs the
    /// user may see ([`Ui::settings_tabs`]).
    pub sections: &'static [SidebarSection],
}

impl Sidebar {
    /// The menu made from [`Ui::nav`] and [`Ui::settings_tabs`], with no
    /// switch.
    pub const DEFAULT: Sidebar = Sidebar {
        switcher: None,
        sections: &[],
    };
}

/// A switch between two areas of a product that share the side menu
/// (Admin ⇅ Vault): a link, with the current area's name on the left and the
/// other's on the right.
#[derive(Debug, PartialEq, Eq)]
pub struct Switcher {
    /// Translation key of the name of the area these pages are in ("Admin").
    pub here: &'static str,
    /// Translation key of the name of the area the link leads to ("Vault").
    pub there: &'static str,
    /// Where the link leads: a path of the product, or an address.
    pub href: &'static str,
    /// Icon from `icons.svg` in front of the current area's name.
    pub icon: &'static str,
}

/// One group of entries in the side menu.
#[derive(Debug, PartialEq, Eq)]
pub struct SidebarSection {
    /// Translation key of the heading; `None` for a group with no heading.
    pub heading: Option<&'static str>,
    pub items: &'static [SidebarItem],
}

/// An entry in the side menu.
#[derive(Debug, PartialEq, Eq)]
pub enum SidebarItem {
    /// An item of the main navigation; current on the pages whose
    /// [`PageCtx::nav`] is its id.
    Nav(&'static NavItem),
    /// A settings tab (the kit's [`SettingsTab::USERS`] and the like, or the
    /// product's own); current on the pages whose [`PageCtx::subnav`] is its
    /// id, and left out for a user who may not see it.
    Tab(&'static SettingsTab),
}

/// The side menu as a page shows it ([`PageCtx::sidebar`]): the entries a
/// user sees, with the current one marked.
#[derive(Debug, PartialEq, Eq)]
pub struct SidebarView {
    pub switcher: Option<&'static Switcher>,
    pub sections: Vec<SectionView>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SectionView {
    /// Translation key of the heading, if the group has one.
    pub heading: Option<&'static str>,
    pub items: Vec<ItemView>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ItemView {
    pub href: &'static str,
    /// Translation key of the label.
    pub label: &'static str,
    pub icon: &'static str,
    pub current: bool,
}

/// One item of the main navigation.
#[derive(Debug, PartialEq, Eq)]
pub struct NavItem {
    /// Matches [`PageCtx::nav`] on the item's pages.
    pub id: &'static str,
    pub href: &'static str,
    /// Translation key of the label in the top bar.
    pub label: &'static str,
    /// Translation key of the (shorter) label in the phone tab bar.
    pub short_label: &'static str,
    /// Icon (from `icons.svg`) in the phone tab bar.
    pub icon: &'static str,
}

/// One settings tab.
#[derive(Debug, PartialEq, Eq)]
pub struct SettingsTab {
    /// Matches [`PageCtx::subnav`] on the tab's pages.
    pub id: &'static str,
    pub href: &'static str,
    /// Translation key of the label.
    pub label: &'static str,
    /// Icon from `icons.svg`.
    pub icon: &'static str,
    /// Who sees the tab.
    pub access: TabAccess,
}

/// Who sees a settings tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAccess {
    Everyone,
    Editors,
    Admins,
}

impl SettingsTab {
    /// User management (`/settings/users`, admins).
    pub const USERS: SettingsTab = SettingsTab {
        id: "users",
        href: "/settings/users",
        label: "nav.users",
        icon: "users",
        access: TabAccess::Admins,
    };
    /// Your profile, password, 2FA and sessions (`/settings/security`).
    pub const SECURITY: SettingsTab = SettingsTab {
        id: "security",
        href: "/settings/security",
        label: "nav.security",
        icon: "shield",
        access: TabAccess::Everyone,
    };
    /// The kit's General page (`/settings/general`, admins), for a product
    /// that turns it on ([`KitConfig::general`](crate::config::KitConfig::general)).
    pub const GENERAL: SettingsTab = SettingsTab {
        id: "general",
        href: "/settings/general",
        label: "nav.general",
        icon: "sliders",
        access: TabAccess::Admins,
    };
    /// API tokens (`/settings/tokens`).
    pub const TOKENS: SettingsTab = SettingsTab {
        id: "tokens",
        href: "/settings/tokens",
        label: "nav.api_tokens",
        icon: "key",
        access: TabAccess::Everyone,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserView {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    /// One or two uppercase letters for the avatar.
    pub initials: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    /// "success" | "error" | "info".
    pub kind: &'static str,
    pub message: String,
}

impl PageCtx {
    /// Builds the context for a page and consumes any pending flash message.
    /// Return the jar alongside the page so the flash cookie gets cleared:
    ///
    /// ```ignore
    /// let (ctx, jar) = PageCtx::build(&state, Some(&user), &jar, "Dashboard", "dashboard", "").await;
    /// Ok((jar, HtmlTemplate(DashboardTemplate { ctx, .. })))
    /// ```
    pub async fn build<S>(
        state: &S,
        user: Option<&CurrentUser>,
        jar: &SignedCookieJar,
        title: impl Into<String>,
        nav: &'static str,
        subnav: &'static str,
    ) -> (Self, SignedCookieJar)
    where
        Core: FromRef<S>,
    {
        let core = Core::from_ref(state);
        let loaded = core
            .with_conn(|c| settings::get(c, settings::SITE_NAME))
            .await;
        let default_name = core.cfg.product.to_string();
        let site_name = match loaded {
            Ok(site) => site
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(default_name),
            Err(err) => {
                tracing::warn!(%err, "failed to load site settings for page");
                default_name
            }
        };
        // Chosen per request by `i18n::layer` and the auth middleware.
        let lang = crate::i18n::current().code().to_string();
        let dir = crate::i18n::current().dir().as_str();

        let (flash, jar) = take_flash(jar.clone());
        let ctx = PageCtx {
            title: title.into(),
            nav,
            subnav,
            user: user.map(UserView::from),
            site_name,
            flash,
            lang,
            dir,
        };
        (ctx, jar)
    }

    pub fn can_edit(&self) -> bool {
        self.user.as_ref().is_some_and(|u| u.role.can_edit())
    }

    pub fn is_admin(&self) -> bool {
        self.user.as_ref().is_some_and(|u| u.role.is_admin())
    }

    /// Whether the layouts use the side menu for this page: the product chose
    /// [`Shell::Sidebar`] and a user is signed in.
    pub fn has_sidebar(&self) -> bool {
        self.user.is_some() && matches!(crate::config::get().features.shell, Shell::Sidebar(_))
    }

    /// The side menu this page shows: `Some` when the product chose
    /// [`Shell::Sidebar`]
    /// ([`KitFeatures::shell`](crate::config::KitFeatures::shell)) and a user
    /// is signed in, with the entries that user sees.
    pub fn sidebar(&self) -> Option<SidebarView> {
        self.sidebar_in(crate::config::get())
    }

    /// [`Self::sidebar`] for the configuration `cfg`.
    pub fn sidebar_in(&self, cfg: &KitConfig) -> Option<SidebarView> {
        self.user.as_ref()?;
        let Shell::Sidebar(side) = cfg.features.shell else {
            return None;
        };
        let nav_item = |nav: &NavItem| ItemView {
            href: nav.href,
            label: nav.label,
            icon: nav.icon,
            current: self.nav == nav.id,
        };
        let tab_item = |tab: &SettingsTab| ItemView {
            href: tab.href,
            label: tab.label,
            icon: tab.icon,
            current: self.subnav == tab.id,
        };
        let mut sections = Vec::new();
        if side.sections.is_empty() {
            let nav: Vec<ItemView> = cfg
                .ui
                .nav
                .iter()
                .filter(|n| n.id != "settings")
                .map(nav_item)
                .collect();
            if !nav.is_empty() {
                sections.push(SectionView {
                    heading: None,
                    items: nav,
                });
            }
            let tabs: Vec<ItemView> = cfg
                .ui
                .settings_tabs
                .iter()
                .filter(|t| self.can_see(t))
                .map(tab_item)
                .collect();
            if !tabs.is_empty() {
                sections.push(SectionView {
                    heading: Some("nav.settings"),
                    items: tabs,
                });
            }
        } else {
            for section in side.sections {
                let items: Vec<ItemView> = section
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        SidebarItem::Nav(nav) => Some(nav_item(nav)),
                        SidebarItem::Tab(tab) => self.can_see(tab).then(|| tab_item(tab)),
                    })
                    .collect();
                if !items.is_empty() {
                    sections.push(SectionView {
                        heading: section.heading,
                        items,
                    });
                }
            }
        }
        Some(SidebarView {
            switcher: side.switcher,
            sections,
        })
    }

    /// Whether the signed-in user sees `tab`.
    pub fn can_see(&self, tab: &SettingsTab) -> bool {
        if tab.id == SettingsTab::TOKENS.id && !crate::config::get().features.api_tokens {
            return false;
        }
        match tab.access {
            TabAccess::Everyone => true,
            TabAccess::Editors => self.can_edit(),
            TabAccess::Admins => self.is_admin(),
        }
    }
}

impl From<&CurrentUser> for UserView {
    fn from(user: &CurrentUser) -> Self {
        let display_name = if user.display_name.trim().is_empty() {
            user.username.clone()
        } else {
            user.display_name.clone()
        };
        UserView {
            id: user.id,
            username: user.username.clone(),
            initials: initials(&display_name),
            display_name,
            role: user.role,
        }
    }
}

/// "Ada Lovelace" → "AL", "admin" → "A", "" → "?".
pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().and_then(|w| w.chars().next());
    let last = words.last().and_then(|w| w.chars().next());
    let s: String = [first, last]
        .into_iter()
        .flatten()
        .flat_map(char::to_uppercase)
        .collect();
    if s.is_empty() { "?".into() } else { s }
}

#[derive(Serialize, Deserialize)]
struct FlashCookie {
    k: String,
    m: String,
}

/// Queue a flash message for the next rendered page. `kind` is "success",
/// "error" or "info" (anything else becomes "info").
///
/// ```ignore
/// let jar = flash(jar, "success", "Monitor saved");
/// Ok((jar, Redirect::to("/")))
/// ```
pub fn flash(jar: SignedCookieJar, kind: &str, message: impl Into<String>) -> SignedCookieJar {
    let payload = FlashCookie {
        k: normalize_kind(kind).to_string(),
        m: message.into(),
    };
    let Ok(json) = serde_json::to_vec(&payload) else {
        return jar;
    };
    let mut cookie = Cookie::new(flash_cookie(), URL_SAFE_NO_PAD.encode(json));
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    // No request at hand here: `Secure` follows the configured public URL.
    cookie.set_secure(crate::auth::session::base_url_is_https());
    jar.add(cookie)
}

fn take_flash(jar: SignedCookieJar) -> (Option<Flash>, SignedCookieJar) {
    let Some(cookie) = jar.get(flash_cookie()) else {
        return (None, jar);
    };
    let flash = decode_flash(cookie.value());
    let mut removal = Cookie::from(flash_cookie());
    removal.set_path("/");
    (flash, jar.remove(removal))
}

fn decode_flash(value: &str) -> Option<Flash> {
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    let payload: FlashCookie = serde_json::from_slice(&bytes).ok()?;
    Some(Flash {
        kind: normalize_kind(&payload.k),
        message: payload.m,
    })
}

fn normalize_kind(kind: &str) -> &'static str {
    match kind {
        "success" => "success",
        "error" => "error",
        _ => "info",
    }
}

#[cfg(test)]
mod tests {
    use axum_extra::extract::cookie::Key;

    use super::*;

    #[test]
    fn flash_round_trips_through_the_signed_jar() {
        let jar = SignedCookieJar::new(Key::generate());
        let jar = flash(jar, "success", "Saved; \"quotes\", commas & ünïcode");
        let (got, jar) = take_flash(jar);
        assert_eq!(
            got,
            Some(Flash {
                kind: "success",
                message: "Saved; \"quotes\", commas & ünïcode".into()
            })
        );
        assert!(jar.get(flash_cookie()).is_none());
    }

    #[test]
    fn unknown_flash_kind_becomes_info() {
        let jar = flash(SignedCookieJar::new(Key::generate()), "weird", "x");
        assert_eq!(take_flash(jar).0.map(|f| f.kind), Some("info"));
    }

    fn ctx(nav: &'static str, subnav: &'static str, role: Option<Role>) -> PageCtx {
        PageCtx {
            title: "T".into(),
            nav,
            subnav,
            user: role.map(|role| UserView {
                id: 1,
                username: "ada".into(),
                display_name: "Ada".into(),
                role,
                initials: "A".into(),
            }),
            site_name: "Site".into(),
            flash: None,
            lang: "en".into(),
            dir: "ltr",
        }
    }

    static DEFAULT_FEATURES: crate::config::KitFeatures = crate::config::KitFeatures {
        shell: Shell::Sidebar(&Sidebar::DEFAULT),
        ..crate::config::KitFeatures::DEFAULT
    };
    static DEFAULT_SIDE: KitConfig = KitConfig {
        features: &DEFAULT_FEATURES,
        ..crate::testing::CONFIG
    };

    static HOME: NavItem = NavItem {
        id: "home",
        href: "/",
        label: "nav.main",
        short_label: "nav.main",
        icon: "pulse",
    };
    static OWN: Sidebar = Sidebar {
        switcher: Some(&Switcher {
            here: "nav.general",
            there: "nav.users",
            href: "/other",
            icon: "box",
        }),
        sections: &[
            SidebarSection {
                heading: None,
                items: &[SidebarItem::Nav(&HOME)],
            },
            SidebarSection {
                heading: Some("nav.settings"),
                items: &[SidebarItem::Tab(&SettingsTab::USERS)],
            },
            SidebarSection {
                heading: Some("nav.security"),
                items: &[SidebarItem::Tab(&SettingsTab::SECURITY)],
            },
        ],
    };
    static OWN_FEATURES: crate::config::KitFeatures = crate::config::KitFeatures {
        shell: Shell::Sidebar(&OWN),
        ..crate::config::KitFeatures::DEFAULT
    };
    static OWN_SIDE: KitConfig = KitConfig {
        features: &OWN_FEATURES,
        ..crate::testing::CONFIG
    };

    #[test]
    fn the_top_bar_is_the_default_and_has_no_side_menu() {
        assert_eq!(crate::config::KitFeatures::DEFAULT.shell, Shell::TopBar);
        let page = ctx("dashboard", "", Some(Role::Admin));
        assert!(!page.has_sidebar());
        assert_eq!(page.sidebar(), None);
    }

    #[test]
    fn a_signed_out_page_has_no_side_menu() {
        let page = ctx("", "", None);
        assert_eq!(page.sidebar_in(&DEFAULT_SIDE), None);
        assert_eq!(page.sidebar_in(&OWN_SIDE), None);
    }

    #[test]
    fn the_default_menu_is_the_navigation_and_the_settings_tabs() {
        let page = ctx("settings", "security", Some(Role::Admin));
        let view = page.sidebar_in(&DEFAULT_SIDE).unwrap();
        assert_eq!(view.switcher, None);
        // The navigation without "settings", then one section of tabs.
        assert_eq!(view.sections.len(), 2);
        assert_eq!(view.sections[0].heading, None);
        let nav: Vec<&str> = view.sections[0].items.iter().map(|i| i.href).collect();
        assert_eq!(nav, ["/", "/status-pages", "/maintenance"]);
        assert!(view.sections[0].items.iter().all(|i| !i.current));
        assert_eq!(view.sections[1].heading, Some("nav.settings"));
        let tabs = &view.sections[1].items;
        assert_eq!(tabs.len(), crate::testing::CONFIG.ui.settings_tabs.len());
        let current: Vec<&str> = tabs.iter().filter(|i| i.current).map(|i| i.href).collect();
        assert_eq!(current, ["/settings/security"]);
    }

    #[test]
    fn a_page_of_the_navigation_marks_its_entry() {
        let page = ctx("maintenance", "", Some(Role::Viewer));
        let view = page.sidebar_in(&DEFAULT_SIDE).unwrap();
        let current: Vec<&str> = view
            .sections
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| i.current)
            .map(|i| i.href)
            .collect();
        assert_eq!(current, ["/maintenance"]);
    }

    #[test]
    fn the_menu_shows_only_the_tabs_a_user_may_see() {
        let tabs = |role| {
            let page = ctx("settings", "", Some(role));
            let view = page.sidebar_in(&DEFAULT_SIDE).unwrap();
            view.sections[1]
                .items
                .iter()
                .map(|i| i.href)
                .collect::<Vec<_>>()
        };
        let admin = tabs(Role::Admin);
        let editor = tabs(Role::Editor);
        let viewer = tabs(Role::Viewer);
        assert!(admin.contains(&"/settings/users"));
        assert!(!editor.contains(&"/settings/users"));
        assert!(editor.contains(&"/settings/notifications"));
        assert!(!viewer.contains(&"/settings/notifications"));
        assert_eq!(viewer, ["/settings/security", "/settings/tokens"]);
    }

    #[test]
    fn the_products_own_sections_and_switch_are_used_and_empty_ones_dropped() {
        let admin = ctx("home", "users", Some(Role::Admin))
            .sidebar_in(&OWN_SIDE)
            .unwrap();
        assert_eq!(admin.switcher.map(|s| s.href), Some("/other"));
        assert_eq!(admin.sections.len(), 3);
        assert_eq!(admin.sections[0].items[0].href, "/");
        assert!(admin.sections[0].items[0].current);
        assert_eq!(admin.sections[1].heading, Some("nav.settings"));
        assert!(admin.sections[1].items[0].current);
        assert!(!admin.sections[2].items[0].current);
        // A viewer has no Users tab: its section is gone, the others stay.
        let viewer = ctx("home", "", Some(Role::Viewer))
            .sidebar_in(&OWN_SIDE)
            .unwrap();
        let headings: Vec<_> = viewer.sections.iter().map(|s| s.heading).collect();
        assert_eq!(headings, [None, Some("nav.security")]);
    }

    #[test]
    fn initials_use_first_and_last_word() {
        assert_eq!(initials("ada king lovelace"), "AL");
        assert_eq!(initials("admin"), "A");
        assert_eq!(initials("  "), "?");
    }
}
