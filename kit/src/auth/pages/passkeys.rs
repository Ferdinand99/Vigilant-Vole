//! Passkeys: sign-in without a password, and managing them on the
//! Security page.
//!
//! - `POST /login/passkey/options`, `POST /login/passkey` (no session)
//! - `POST /settings/security/passkeys/options` (with the password),
//!   `POST /settings/security/passkeys`
//! - `POST /settings/security/passkeys/{id}/rename` · `/{id}/delete`
//! - `POST /settings/users/{id}/passkeys/reset` (admins, in `users`)
//!
//! The `options` endpoints answer with `{"publicKey": …}` for
//! `navigator.credentials.get/create` (binary fields in base64url); the
//! others take the credential as JSON and answer `{"redirect": …}` or
//! `{"error": …}` (`static/passkeys.js`). The ceremonies are verified in
//! [`crate::auth::webauthn`].

use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::Instant;

use axum::Form;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::guard::{PeerAddr, login_jar, secure_cookies};
use crate::auth::webauthn::{self, RelyingParty, StoredCredential, b64, unb64};
use crate::auth::{CurrentUser, session};
use crate::db::passkeys::{self as db_passkeys, NewPasskey, Purpose};
use crate::db::users as db_users;
use crate::http::ratelimit::{Rate, TokenBuckets};
use crate::http::{ApiError, ApiResult};
use crate::page::flash;
use crate::state::Core;

/// What the browser may take for a ceremony, in milliseconds: the
/// challenge's lifetime.
const TIMEOUT_MS: i64 = db_passkeys::CHALLENGE_TTL_SECS * 1000;

/// Longest passkey name, in characters.
pub(crate) const MAX_NAME_CHARS: usize = 60;

/// Sign-in starts per client address: the page asks for a challenge on
/// every visit (the browser's passkey autofill), so this is a request
/// budget rather than the failure backoff of the sign-in itself.
static SIGN_IN_STARTS: LazyLock<TokenBuckets<Option<IpAddr>>> = LazyLock::new(TokenBuckets::new);
const SIGN_IN_STARTS_RATE: Rate = Rate::per_minute(30);

/// The relying party of this instance: its public URL, when passkeys can
/// work there ([`RelyingParty::from_public_url`]).
pub async fn relying_party(core: &Core) -> Option<RelyingParty> {
    core.base_url()
        .await
        .and_then(|url| RelyingParty::from_public_url(&url))
}

fn new_challenge() -> Vec<u8> {
    let mut challenge = [0u8; 32];
    rand::fill(&mut challenge);
    challenge.to_vec()
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn too_many(wait: std::time::Duration) -> Response {
    json_error(
        StatusCode::TOO_MANY_REQUESTS,
        super::sign_in::wait_message(wait),
    )
}

fn unavailable() -> Response {
    json_error(StatusCode::NOT_FOUND, crate::i18n::t("sec.pk_no_url"))
}

/// The browser's credential, as `PublicKeyCredential.toJSON()` writes it.
#[derive(Deserialize)]
struct Credential {
    id: String,
    #[serde(rename = "rawId")]
    raw_id: String,
    #[serde(rename = "type")]
    kind: String,
    response: CredentialResponse,
}

#[derive(Deserialize)]
struct CredentialResponse {
    #[serde(rename = "clientDataJSON")]
    client_data_json: String,
    #[serde(rename = "attestationObject", default)]
    attestation_object: Option<String>,
    #[serde(default)]
    transports: Vec<String>,
    #[serde(rename = "authenticatorData", default)]
    authenticator_data: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(rename = "userHandle", default)]
    user_handle: Option<String>,
}

impl Credential {
    /// The credential id; `id` and `rawId` must agree.
    fn raw_id(&self) -> Result<Vec<u8>, webauthn::Error> {
        let raw = unb64(&self.raw_id, "credential id")?;
        if self.kind != "public-key"
            || self.id != b64(&raw)
            || raw.len() > webauthn::MAX_CREDENTIAL_ID
        {
            return Err(webauthn::Error::Malformed("credential"));
        }
        Ok(raw)
    }
}

/// Decodes a field, refusing absurd sizes before any parsing.
fn field(
    value: Option<&str>,
    limit: usize,
    what: &'static str,
) -> Result<Vec<u8>, webauthn::Error> {
    let value = value.ok_or(webauthn::Error::Malformed(what))?;
    if value.len() > limit * 4 / 3 + 4 {
        return Err(webauthn::Error::Malformed(what));
    }
    unb64(value, what)
}

fn client_ip(headers: &HeaderMap, peer: PeerAddr) -> Option<IpAddr> {
    session::client_ip(headers, peer.0, session::trust_proxy())
}

// ---------------------------------------------------------------------------
// Sign-in
// ---------------------------------------------------------------------------

pub(super) async fn sign_in_options(
    State(core): State<Core>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> ApiResult<Response> {
    let Some(rp) = relying_party(&core).await else {
        return Ok(unavailable());
    };
    let ip = client_ip(&headers, peer);
    let now = Instant::now();
    // The passkeys' own bucket: password failures from this address (or
    // from everybody behind one proxy) don't keep passkeys out.
    if let Some(key) = ip.map(|ip| format!("ip:{ip}"))
        && let Some(wait) = core.limits.passkey.retry_after(&key, now)
    {
        return Ok(too_many(wait));
    }
    if let Err(wait) = SIGN_IN_STARTS.take(&ip, SIGN_IN_STARTS_RATE, now) {
        return Ok(too_many(wait));
    }
    let challenge = new_challenge();
    let stored = b64(&challenge);
    let at = session::now_unix();
    core.with_conn(move |c| {
        db_passkeys::put_challenge(c, &stored, Purpose::SignIn, None, None, at)
    })
    .await?;
    Ok(Json(json!({
        "publicKey": {
            "challenge": b64(&challenge),
            "rpId": rp.id,
            "timeout": TIMEOUT_MS,
            "userVerification": "required",
            "allowCredentials": [],
        }
    }))
    .into_response())
}

#[derive(Deserialize)]
struct SignInBody {
    credential: Credential,
    #[serde(default)]
    next: Option<String>,
}

pub(super) async fn sign_in(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    peer: PeerAddr,
    body: Bytes,
) -> ApiResult<Response> {
    let Some(rp) = relying_party(&core).await else {
        return Ok(unavailable());
    };
    let ip = client_ip(&headers, peer);
    let ip_key = ip.map(|ip| format!("ip:{ip}"));
    // Counted up front like a password attempt; success undoes it. A
    // bucket of its own, far more generous than the password one.
    if let Some(key) = &ip_key
        && let Err(wait) = core.limits.passkey.reserve(key, Instant::now())
    {
        tracing::warn!(ip = ?ip, "passkey sign-in throttled");
        return Ok(too_many(wait));
    }
    let outcome = match serde_json::from_slice::<SignInBody>(&body) {
        Ok(body) => check_sign_in(&core, &rp, body).await?,
        Err(_) => Err(webauthn::Error::Malformed("request")),
    };
    let (user, next) = match outcome {
        Ok(found) => found,
        Err(err) => {
            tracing::info!(ip = ?ip, error = %err, "failed passkey sign-in");
            return Ok(json_error(
                StatusCode::UNAUTHORIZED,
                crate::i18n::t("login.passkey_failed"),
            ));
        }
    };
    if let Some(key) = &ip_key {
        core.limits.passkey.release(key);
    }
    let id = user.id;
    let codes = core.cfg.features.sign_in_codes;
    core.with_conn(move |c| {
        db_users::touch_last_login(c, id)?;
        // They can sign in: a code an admin made for them is not needed.
        if codes {
            crate::db::sign_in_codes::revoke_for_user(c, id)?;
        }
        Ok(())
    })
    .await?;
    tracing::info!(user = %user.username, "login with a passkey");
    // The authenticator verified the user: both factors in one, so no TOTP step.
    let jar = login_jar(jar, user.id, user.session_version, secure_cookies(&headers));
    Ok((jar, Json(json!({ "redirect": next }))).into_response())
}

/// Verifies a sign-in (§7.2) and records the passkey's use: the user and
/// where to go next.
async fn check_sign_in(
    core: &Core,
    rp: &RelyingParty,
    body: SignInBody,
) -> ApiResult<Result<(db_users::User, String), webauthn::Error>> {
    let next = session::sanitize_next(body.next.as_deref());
    let credential = body.credential;
    let parsed = (|| -> Result<_, webauthn::Error> {
        let raw_id = credential.raw_id()?;
        let r = &credential.response;
        let client_data = field(Some(r.client_data_json.as_str()), 8192, "client data")?;
        let data = field(r.authenticator_data.as_deref(), 8192, "authenticator data")?;
        let sig = field(r.signature.as_deref(), 1024, "signature")?;
        let handle = match r.user_handle.as_deref() {
            Some(h) if !h.is_empty() => Some(field(Some(h), 64, "user handle")?),
            _ => None,
        };
        let challenge = webauthn::ClientData::parse(&client_data)?.challenge_b64();
        Ok((raw_id, client_data, data, sig, handle, challenge))
    })();
    let (raw_id, client_data, data, sig, handle, challenge) = match parsed {
        Ok(parts) => parts,
        Err(err) => return Ok(Err(err)),
    };

    // Single use: taken (and gone) before anything else is looked at.
    let at = session::now_unix();
    let issued = challenge.clone();
    let fresh = core
        .with_conn(move |c| db_passkeys::take_challenge(c, &issued, Purpose::SignIn, None, at))
        .await?;
    if fresh.is_none() {
        return Ok(Err(webauthn::Error::Challenge));
    }
    let Some(stored) = core
        .with_conn(move |c| db_passkeys::by_credential_id(c, &raw_id))
        .await?
    else {
        return Ok(Err(webauthn::Error::UnknownCredential));
    };
    let user_id = stored.user_id;
    let Some(user) = core
        .with_conn(move |c| db_users::get_user(c, user_id))
        .await?
        .filter(|u| u.active)
    else {
        return Ok(Err(webauthn::Error::UnknownCredential));
    };
    let challenge = unb64(&challenge, "challenge").map_err(ApiError::internal)?;
    let verified = webauthn::verify_assertion(
        rp,
        &challenge,
        StoredCredential {
            public_key: &stored.public_key,
            user_handle: &stored.user_handle,
            sign_count: stored.sign_count,
            backup_eligible: stored.backup_eligible,
        },
        &client_data,
        &data,
        &sig,
        handle.as_deref(),
    );
    let assertion = match verified {
        Ok(a) => a,
        Err(err) => return Ok(Err(err)),
    };
    let (id, seen) = (stored.id, stored.sign_count);
    let recorded = core
        .with_conn(move |c| {
            db_passkeys::record_use(c, id, seen, assertion.sign_count, assertion.backed_up)
        })
        .await?;
    if !recorded {
        // Another sign-in with this passkey moved the counter meanwhile.
        return Ok(Err(webauthn::Error::Counter));
    }
    Ok(Ok((user, next)))
}

// ---------------------------------------------------------------------------
// Adding a passkey (Security page)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AddForm {
    #[serde(default)]
    password: String,
}

pub(super) async fn register_options(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<AddForm>,
) -> ApiResult<Response> {
    let Some(rp) = relying_party(&core).await else {
        return Ok(unavailable());
    };
    // A fresh confirmation, as for the other sensitive changes here.
    if let Err(msg) = super::security::confirm_password(&core, &user, &jar, form.password).await? {
        return Ok(json_error(StatusCode::FORBIDDEN, msg));
    }
    let id = user.id;
    let (handle, existing) = core
        .with_conn(move |c| {
            Ok((
                db_passkeys::user_handle(c, id)?,
                db_passkeys::list_for_user(c, id)?,
            ))
        })
        .await?;
    let handle = handle.unwrap_or_else(new_challenge);
    let challenge = new_challenge();
    let (stored, stored_handle) = (b64(&challenge), handle.clone());
    let at = session::now_unix();
    core.with_conn(move |c| {
        db_passkeys::put_challenge(
            c,
            &stored,
            Purpose::Register,
            Some(id),
            Some(&stored_handle),
            at,
        )
    })
    .await?;
    let exclude: Vec<Value> = existing
        .iter()
        .map(|p| {
            let transports: Vec<&str> = p.transports.split(',').filter(|t| !t.is_empty()).collect();
            json!({ "type": "public-key", "id": b64(&p.credential_id), "transports": transports })
        })
        .collect();
    let params: Vec<Value> = webauthn::ALGORITHMS
        .iter()
        .map(|alg| json!({ "type": "public-key", "alg": alg }))
        .collect();
    Ok(Json(json!({
        "publicKey": {
            "rp": { "id": rp.id, "name": super::sign_in::site_name(&core).await },
            "user": { "id": b64(&handle), "name": user.username, "displayName": user.display_name },
            "challenge": b64(&challenge),
            "pubKeyCredParams": params,
            "timeout": TIMEOUT_MS,
            "attestation": "none",
            "authenticatorSelection": {
                "residentKey": "required",
                "requireResidentKey": true,
                "userVerification": "required",
            },
            "excludeCredentials": exclude,
        }
    }))
    .into_response())
}

#[derive(Deserialize)]
struct RegisterBody {
    credential: Credential,
    #[serde(default)]
    name: String,
}

/// Transport hints the browser may report; anything else is dropped.
const TRANSPORTS: &[&str] = &["ble", "hybrid", "internal", "nfc", "smart-card", "usb"];

pub(super) async fn register(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let Some(rp) = relying_party(&core).await else {
        return Ok(unavailable());
    };
    let key = format!("passkey-setup:{}", user.id);
    // Counted up front like a password attempt; a verified passkey clears it.
    if let Err(wait) = core.limits.account.reserve(&key, Instant::now()) {
        return Ok(too_many(wait));
    }
    let body = match serde_json::from_slice::<RegisterBody>(&body) {
        Ok(body) => body,
        Err(_) => return Ok(failed(&user, webauthn::Error::Malformed("request"))),
    };
    let name = body.name.trim().to_string();
    if name.chars().count() > MAX_NAME_CHARS {
        // Not a failed ceremony: don't count it.
        core.limits.account.release(&key);
        return Ok(json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            crate::i18n::t("sec.pk_name_bad"),
        ));
    }
    let parsed = (|| -> Result<_, webauthn::Error> {
        let raw_id = body.credential.raw_id()?;
        let r = &body.credential.response;
        let client_data = field(Some(r.client_data_json.as_str()), 8192, "client data")?;
        let attestation = field(r.attestation_object.as_deref(), 65536, "attestation object")?;
        let challenge = webauthn::ClientData::parse(&client_data)?.challenge_b64();
        Ok((raw_id, client_data, attestation, challenge))
    })();
    let (raw_id, client_data, attestation, challenge) = match parsed {
        Ok(parts) => parts,
        Err(err) => return Ok(failed(&user, err)),
    };
    let (id, at, issued) = (user.id, session::now_unix(), challenge.clone());
    let handle = core
        .with_conn(move |c| {
            db_passkeys::take_challenge(c, &issued, Purpose::Register, Some(id), at)
        })
        .await?
        .flatten();
    let Some(handle) = handle else {
        return Ok(failed(&user, webauthn::Error::Challenge));
    };
    let challenge = unb64(&challenge, "challenge").map_err(ApiError::internal)?;
    let reg = match webauthn::verify_registration(&rp, &challenge, &client_data, &attestation) {
        Ok(reg) if reg.credential_id == raw_id => reg,
        Ok(_) => {
            return Ok(failed(&user, webauthn::Error::Malformed("credential id")));
        }
        Err(err) => return Ok(failed(&user, err)),
    };
    core.limits.account.clear(&key);

    let mut transports: Vec<&str> = body
        .credential
        .response
        .transports
        .iter()
        .map(String::as_str)
        .filter(|t| TRANSPORTS.contains(t))
        .collect();
    transports.sort_unstable();
    transports.dedup();
    let transports = transports.join(",");
    let provider = webauthn::provider_name(&reg.aaguid);
    let inserted = core
        .with_conn(move |c| {
            let name = if !name.is_empty() {
                name
            } else if let Some(provider) = provider {
                provider.to_string()
            } else {
                let n = db_passkeys::list_for_user(c, id)?.len() + 1;
                crate::i18n::tf("sec.pk_default_name", &[("n", &n.to_string())])
            };
            db_passkeys::insert(
                c,
                &NewPasskey {
                    user_id: id,
                    credential_id: &reg.credential_id,
                    public_key: &reg.public_key,
                    alg: reg.alg,
                    user_handle: &handle,
                    name: &name,
                    sign_count: reg.sign_count,
                    transports: &transports,
                    backup_eligible: reg.backup_eligible,
                    backed_up: reg.backed_up,
                },
            )
        })
        .await?;
    if inserted.is_none() {
        return Ok(json_error(
            StatusCode::CONFLICT,
            crate::i18n::t("sec.pk_exists"),
        ));
    }
    tracing::info!(user = %user.username, "passkey added; other sessions signed out");
    let jar = super::security::sign_out_others_keep_this(&core, &user, jar, &headers).await?;
    let jar = flash(jar, "success", crate::i18n::t("sec.pk_added"));
    Ok((
        jar,
        Json(json!({ "redirect": "/settings/security#passkeys" })),
    )
        .into_response())
}

/// A refused registration (already counted against the user): logged, one text.
fn failed(user: &CurrentUser, err: webauthn::Error) -> Response {
    tracing::info!(user = %user.username, error = %err, "passkey registration refused");
    json_error(StatusCode::BAD_REQUEST, crate::i18n::t("sec.pk_failed"))
}

// ---------------------------------------------------------------------------
// Rename, delete
// ---------------------------------------------------------------------------

fn back(jar: SignedCookieJar, kind: &str, message: &str) -> Response {
    (
        flash(jar, kind, message),
        Redirect::to("/settings/security#passkeys"),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct RenameForm {
    #[serde(default)]
    name: String,
}

pub(super) async fn rename(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<RenameForm>,
) -> ApiResult<Response> {
    let name = form.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Ok(back(jar, "error", crate::i18n::t("sec.pk_name_bad")));
    }
    let user_id = user.id;
    let renamed = core
        .with_conn(move |c| db_passkeys::rename(c, user_id, id, &name))
        .await?;
    if !renamed {
        return Ok(back(jar, "error", crate::i18n::t("sec.pk_gone")));
    }
    Ok(back(jar, "success", crate::i18n::t("sec.pk_renamed")))
}

#[derive(Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    current_password: String,
}

pub(super) async fn delete(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<DeleteForm>,
) -> ApiResult<Response> {
    // Removing a passkey changes how the account can be reached: a fresh
    // confirmation, as for adding one.
    if let Err(msg) =
        super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", &msg));
    }
    let user_id = user.id;
    let deleted = core
        .with_conn(move |c| db_passkeys::delete(c, user_id, id))
        .await?;
    if !deleted {
        return Ok(back(jar, "error", crate::i18n::t("sec.pk_gone")));
    }
    tracing::info!(user = %user.username, "passkey deleted; other sessions signed out");
    let jar = super::security::sign_out_others_keep_this(&core, &user, jar, &headers).await?;
    Ok(back(jar, "success", crate::i18n::t("sec.pk_deleted")))
}
