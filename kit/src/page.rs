//! Rust side of the layout contract: `PageCtx` (required by `layout.html`),
//! the current-user view, one-shot flash messages and [`Ui`], what the
//! shared layouts show for the product (logo, navigation, settings tabs).

use axum_extra::extract::cookie::{Cookie, SameSite, SignedCookieJar};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use axum::extract::FromRef;

use super::auth::{CurrentUser, Role};
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

/// One item of the main navigation.
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

    /// Whether the signed-in user sees `tab`.
    pub fn can_see(&self, tab: &SettingsTab) -> bool {
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

    #[test]
    fn initials_use_first_and_last_word() {
        assert_eq!(initials("ada king lovelace"), "AL");
        assert_eq!(initials("admin"), "A");
        assert_eq!(initials("  "), "?");
    }
}
