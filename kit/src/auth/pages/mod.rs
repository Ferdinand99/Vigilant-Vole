//! The pages every product needs, with their handlers: the setup wizard,
//! sign-in with two-factor authentication and sign-out
//! ([`public_router`]), and user management, API tokens, the personal
//! Security page, the API reference and, for a product that turns it on,
//! the General settings page ([`router`], behind
//! [`require_auth`](super::guard::require_auth)).
//!
//! The routers work with any product state `S` the kit's [`Core`] (and the
//! cookie [`Key`]) can be taken from. Templates are in `templates/`; the
//! product's parts (name, logo, navigation, settings tabs) come from its
//! [`KitConfig`](crate::config::KitConfig).

mod external;
mod general;
mod invite;
mod passkeys;
mod security;
mod sign_in;
mod sign_in_code;
mod tokens;
mod users;

use axum::Router;
use axum::extract::FromRef;
use axum::routing::{delete, get, post};
use axum_extra::extract::cookie::Key;

pub use passkeys::relying_party;
pub use sign_in::{SetupTokenError, check_setup_token, insert_first_admin, verify_second_factor};

use crate::config::CredentialMode;
use crate::docs::page as docs;
use crate::state::Core;

/// Setup, sign-in, the second factor and sign-out: no session needed.
///
/// With [`CredentialMode::External`] `/setup` and `/login` only point to the
/// product's own pages (`POST /login` is 404); the second factor, the
/// passkey sign-in and sign-out stay. With
/// [`KitFeatures::invites`](crate::config::KitFeatures::invites) there is
/// also `/invite/{token}`, where an invited person creates their account
/// (in that mode it leads on to the product's setup page), and with
/// [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes)
/// `/sign-in-code`, where someone redeems a one-time code and chooses a new
/// password (in that mode it leads on to the product's password page).
pub fn public_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    Core: FromRef<S>,
    Key: FromRef<S>,
{
    let router = match crate::config::get().features.credentials {
        CredentialMode::Plain => Router::new()
            .route(
                "/setup",
                get(sign_in::setup_form).post(sign_in::setup_submit),
            )
            .route(
                "/login",
                get(sign_in::login_form).post(sign_in::login_submit),
            ),
        CredentialMode::External(_) => Router::new()
            .route("/setup", get(external::setup).post(external::setup))
            .route("/login", get(external::login).post(external::gone)),
    };
    // Invitation links: the invitee's page (in `External` mode it leads on
    // to the product's setup page).
    let router = if crate::config::get().features.invites {
        router.route("/invite/{token}", get(invite::form).post(invite::submit))
    } else {
        router
    };
    // One-time sign-in codes: where a user redeems one (in `External` mode
    // it leads on to the product's password page).
    let router = if crate::config::get().features.sign_in_codes {
        router.route(
            "/sign-in-code",
            get(sign_in_code::form).post(sign_in_code::submit),
        )
    } else {
        router
    };
    router
        .route(
            "/login/2fa",
            get(sign_in::two_factor_form).post(sign_in::two_factor_submit),
        )
        .route("/login/passkey/options", post(passkeys::sign_in_options))
        .route("/login/passkey", post(passkeys::sign_in))
        .route("/logout", post(sign_in::logout))
}

/// Users (and, with [`KitFeatures::invites`](crate::config::KitFeatures::invites),
/// their invitations and, with
/// [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes),
/// their one-time sign-in codes and email addresses), API tokens (with [`KitFeatures::api_tokens`](crate::config::KitFeatures::api_tokens)),
/// Security, the API reference (with
/// [`KitFeatures::api_docs`](crate::config::KitFeatures::api_docs)) and, with
/// [`KitConfig::general`](crate::config::KitConfig::general), General
/// (`/settings/general`). Mount it behind
/// [`require_auth`](super::guard::require_auth): the handlers take the
/// signed-in [`CurrentUser`](super::CurrentUser).
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    Core: FromRef<S>,
    Key: FromRef<S>,
{
    let cfg = crate::config::get();
    let router = if cfg.general.is_some() {
        Router::new().route("/settings/general", get(general::page).post(general::save))
    } else {
        Router::new()
    };
    // With external credentials the kit sets no passwords: no temporary
    // password for a new user, no admin reset, no change of one's own.
    let external = matches!(cfg.features.credentials, CredentialMode::External(_));
    let router = if external {
        router.route("/settings/users", get(users::list).post(external::gone))
    } else {
        router
            .route("/settings/users", get(users::list).post(users::create))
            .route("/settings/users/{id}/password", post(users::reset_password))
            .route(
                "/settings/security/password",
                post(security::change_password),
            )
    };
    let router = router
        .route("/settings/users/{id}", post(users::update))
        .route(
            "/settings/users/{id}/2fa/reset",
            post(users::reset_two_factor),
        )
        .route(
            "/settings/users/{id}/passkeys/reset",
            post(users::reset_passkeys),
        )
        .route("/settings/users/{id}/enable", post(users::enable))
        .route("/settings/users/{id}/disable", post(users::disable))
        .route("/settings/users/{id}/delete", post(users::delete))
        .route("/settings/security", get(security::security_page))
        .route("/settings/security/profile", post(security::save_profile))
        .route(
            "/settings/security/sessions",
            post(security::sign_out_others),
        )
        .route("/settings/security/2fa/setup", post(security::totp_setup))
        .route("/settings/security/2fa/cancel", post(security::totp_cancel))
        .route("/settings/security/2fa/enable", post(security::totp_enable))
        .route(
            "/settings/security/2fa/disable",
            post(security::totp_disable),
        )
        .route(
            "/settings/security/2fa/recovery",
            post(security::totp_new_recovery_codes),
        )
        .route(
            "/settings/security/passkeys/options",
            post(passkeys::register_options),
        )
        .route("/settings/security/passkeys", post(passkeys::register))
        .route(
            "/settings/security/passkeys/{id}/rename",
            post(passkeys::rename),
        )
        .route(
            "/settings/security/passkeys/{id}/delete",
            post(passkeys::delete),
        );
    let router = if cfg.features.invites {
        router
            .route("/settings/invites", post(invite::create))
            .route("/settings/invites/{id}/revoke", post(invite::revoke))
    } else {
        router
    };
    let router = if cfg.features.sign_in_codes {
        let router = router.route(
            "/settings/users/{id}/sign-in-code",
            post(sign_in_code::create),
        );
        if cfg.general.is_some() {
            router.route("/settings/general/smtp-test", post(general::smtp_test))
        } else {
            router
        }
    } else {
        router
    };
    let router = if cfg.features.api_tokens {
        router
            .route("/settings/tokens", get(tokens::list).post(tokens::create))
            .route("/settings/tokens/{id}", delete(tokens::revoke))
    } else {
        router
    };
    if cfg.features.api_docs {
        router
            .route("/api-docs", get(docs::page))
            .route("/api-docs.md", get(docs::markdown_doc))
            .route("/llms.txt", get(docs::llms_txt))
            .route(crate::docs::spec().openapi_path, get(docs::openapi))
    } else {
        router
    }
}
