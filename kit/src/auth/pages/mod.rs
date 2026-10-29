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

mod general;
mod passkeys;
mod security;
mod sign_in;
mod tokens;
mod users;

use axum::Router;
use axum::extract::FromRef;
use axum::routing::{delete, get, post};
use axum_extra::extract::cookie::Key;

pub use passkeys::relying_party;
pub use sign_in::verify_second_factor;

use crate::docs::page as docs;
use crate::state::Core;

/// Setup, sign-in, the second factor and sign-out: no session needed.
pub fn public_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    Core: FromRef<S>,
    Key: FromRef<S>,
{
    Router::new()
        .route(
            "/setup",
            get(sign_in::setup_form).post(sign_in::setup_submit),
        )
        .route(
            "/login",
            get(sign_in::login_form).post(sign_in::login_submit),
        )
        .route(
            "/login/2fa",
            get(sign_in::two_factor_form).post(sign_in::two_factor_submit),
        )
        .route("/login/passkey/options", post(passkeys::sign_in_options))
        .route("/login/passkey", post(passkeys::sign_in))
        .route("/logout", post(sign_in::logout))
}

/// Users, API tokens (with [`KitFeatures::api_tokens`](crate::config::KitFeatures::api_tokens)),
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
    let router = router
        .route("/settings/users", get(users::list).post(users::create))
        .route("/settings/users/{id}", post(users::update))
        .route("/settings/users/{id}/password", post(users::reset_password))
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
            "/settings/security/password",
            post(security::change_password),
        )
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
