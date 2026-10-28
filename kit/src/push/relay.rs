//! The client of the Hedgerow push relay ("Postal Pigeon"): registering this
//! server, sending a push to every registered device and cleaning up after
//! the answers. The relay's wire formats are in its `docs/api.md`.
//!
//! * **Registration** happens by itself the first time a `hedgerow` channel
//!   fires: fetch a challenge, find a nonce (proof of work, about a second,
//!   on a blocking thread), `POST /v1/instances`, and keep the credential
//!   (`hrk_…`) sealed in the settings ([`SETTING_CREDENTIAL`]). It runs as a
//!   task of its own, so a slow proof of work finishes even if the alert that
//!   asked for it has given up. A relay that answers `401 unknown_instance`
//!   (it lost its database) gets one new registration per alert.
//! * **Sending** is one `POST /v1/push` per batch of 25 handles with a fresh
//!   `event_id` per call. `gone` forgets the handle; `limited`,
//!   `unavailable` and the like are logged and given up.
//! * **Time:** the whole delivery has a deadline ([`Context::deadline`]), so
//!   a relay that is down, slow or suspended never holds up the other
//!   channels of an alert.
//! * **Outcome:** [`deliver`] is `Ok` when at least one handle was accepted
//!   or nothing needed delivering (every handle gone or already sent), and
//!   `Err` when nothing got through. A product that retries on `Err` (Keen
//!   Kestrel's queue) therefore never repeats a push that reached someone.
//!
//! Every request goes through the kit's guarded client
//! ([`crate::notify::http`]); replies are read up to [`REPLY_CAP`] bytes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use axum_extra::extract::cookie::Key;
use deadpool_sqlite::Pool;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{devices, events};
use crate::crypto::Sealer;
use crate::db::{self, settings};
use crate::notify::alert::{Alert, AlertKind};
use crate::notify::http as out;
use crate::state::Core;

/// Where the relay of the Hedgerow project runs.
pub const DEFAULT_RELAY_URL: &str = "https://pigeon.hedgerow.tools";
/// Environment variable naming another relay (one you run yourself).
pub const RELAY_URL_ENV: &str = "KIT_PUSH_RELAY_URL";
/// Setting that names another relay. Beats [`RELAY_URL_ENV`].
pub const SETTING_RELAY_URL: &str = "push_relay_url";
/// Setting holding the instance credential (`hrk_…`), sealed.
pub const SETTING_CREDENTIAL: &str = "push_relay_credential";
/// Setting holding the relay the credential belongs to. A credential is
/// only used with the relay it was issued by.
pub const SETTING_CREDENTIAL_URL: &str = "push_relay_credential_url";
/// The purpose the instance credential is sealed for.
pub const PURPOSE: &str = "push-relay";

/// The relay takes at most this many handles per request.
pub const MAX_HANDLES: usize = 25;
/// How long Apple keeps trying, in seconds.
pub const PUSH_TTL: u32 = 3600;
/// Replies are read up to this size; a larger one is an error.
pub const REPLY_CAP: usize = 64 * 1024;
/// A whole delivery (registration included) is given up after this.
pub const DELIVERY_DEADLINE: Duration = Duration::from_secs(30);
/// The proof of work is given up after this (a challenge lives five minutes).
const SOLVE_LIMIT: Duration = Duration::from_secs(240);
/// After a failed registration the next attempt waits this long.
const REGISTER_BACKOFF: Duration = Duration::from_secs(60);
/// After `403 suspended` nothing is sent for this long.
const SUSPENDED_FOR: Duration = Duration::from_secs(3600);
/// The hardest challenge accepted: the relay asks for 20 to 32 bits.
const MAX_DIFFICULTY: u32 = 40;

/// The relay product id of a product's slug, or `None` for a product the
/// relay doesn't know.
pub fn relay_product(slug: &str) -> Option<&'static str> {
    match slug {
        "vigilant-vole" => Some("vigilant-vole"),
        "keen-kestrel" => Some("keen-kestrel"),
        "wise-wren" => Some("wise-wren"),
        _ => None,
    }
}

/// A relay URL checked and without a trailing slash. `https`, or `http` for
/// a relay on this machine or the local network (`localhost` or a private
/// or loopback address).
pub fn validate_relay_url(raw: &str) -> Result<String, String> {
    let url = url::Url::parse(raw.trim()).map_err(|_| "the relay URL is not a URL".to_string())?;
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("the relay URL has no credentials, query or fragment".into());
    }
    let host = url.host_str().unwrap_or_default();
    let local = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| !crate::net::is_public(ip));
    match url.scheme() {
        "https" if !host.is_empty() => {}
        "http" if local => {}
        _ => return Err("the relay URL must be https (http only on the local network)".into()),
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

// ---------------------------------------------------------------------------
// The context: what delivery needs besides the alert
// ---------------------------------------------------------------------------

/// Everything delivery needs besides the alert: the database, the instance
/// key and which product this is. A provider is a plain function of the
/// alert, so the context is registered once per process ([`install`]).
pub struct Context {
    pool: Pool,
    key: Key,
    product: &'static str,
    version: Option<String>,
    env_url: Option<String>,
    deadline: Duration,
    /// Held while a registration runs, so there is only ever one.
    registering: Arc<tokio::sync::Mutex<()>>,
    /// No registration attempt before this.
    backoff_until: Mutex<Option<Instant>>,
    /// The relay suspended this server: nothing is sent before this.
    suspended_until: Mutex<Option<Instant>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Context {
    /// The context of `core`'s product. Fails for a product the relay
    /// doesn't know ([`relay_product`]) and for an invalid
    /// [`RELAY_URL_ENV`].
    pub fn new(core: &Core) -> Result<Context, String> {
        let slug = core.cfg.slug;
        let product = relay_product(slug).ok_or_else(|| {
            format!("the push relay doesn't know the product '{slug}' (known: vigilant-vole, keen-kestrel, wise-wren)")
        })?;
        let env_url = match std::env::var(RELAY_URL_ENV) {
            Ok(raw) if !raw.trim().is_empty() => {
                Some(validate_relay_url(&raw).map_err(|e| format!("{RELAY_URL_ENV}: {e}"))?)
            }
            _ => None,
        };
        Ok(Context {
            pool: core.db.clone(),
            key: core.session_key.clone(),
            product,
            version: Some(core.cfg.version.to_string()).filter(|v| valid_version(v)),
            env_url,
            deadline: DELIVERY_DEADLINE,
            registering: Arc::default(),
            backoff_until: Mutex::new(None),
            suspended_until: Mutex::new(None),
        })
    }

    /// The same context with another delivery deadline (tests).
    #[cfg(test)]
    pub(crate) fn with_deadline(mut self, deadline: Duration) -> Context {
        self.deadline = deadline;
        self
    }

    /// The deadline of a whole delivery.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    fn sealer(&self) -> Sealer {
        Sealer::new(&self.key, PURPOSE)
    }

    fn is_suspended(&self) -> bool {
        lock(&self.suspended_until).is_some_and(|until| Instant::now() < until)
    }

    fn suspend(&self) {
        *lock(&self.suspended_until) = Some(Instant::now() + SUSPENDED_FOR);
    }

    /// The relay to use: the setting, else the environment, else the default.
    async fn relay_url(&self) -> Result<String, String> {
        let stored = db::interact(&self.pool, |c| settings::get(c, SETTING_RELAY_URL))
            .await
            .map_err(|e| e.to_string())?
            .filter(|v| !v.trim().is_empty());
        match stored {
            Some(raw) => validate_relay_url(&raw),
            None => Ok(self
                .env_url
                .clone()
                .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string())),
        }
    }

    /// The stored credential, if it is for `url` and can be opened.
    async fn stored_credential(&self, url: &str) -> Option<String> {
        let sealer = self.sealer();
        let stored = db::interact(&self.pool, |c| {
            Ok((
                settings::get(c, SETTING_CREDENTIAL)?,
                settings::get(c, SETTING_CREDENTIAL_URL)?,
            ))
        })
        .await
        .ok()?;
        let (Some(sealed), Some(for_url)) = stored else {
            return None;
        };
        if for_url != url {
            return None;
        }
        match sealer.open(&sealed) {
            Ok(credential) => Some(credential),
            Err(err) => {
                tracing::warn!(%err, "the push relay credential can't be opened; registering again");
                None
            }
        }
    }
}

fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+_-".contains(&b))
}

// -- the registered context --------------------------------------------------

static INSTALLED: RwLock<Option<Arc<Context>>> = RwLock::new(None);

tokio::task_local! {
    static SCOPED: Arc<Context>;
}

/// Makes `core`'s database and key the ones push delivery uses, for the
/// whole process. Call it once at start-up, after
/// [`config::install`](crate::config::install) and before alerts can fire.
/// (The kit's [`Core`] is often built per request, so this is not done by
/// `Core::new`.) Without it a `hedgerow` channel refuses to send.
///
/// # Panics
///
/// For a product the relay doesn't know ([`relay_product`]) and for an
/// invalid [`RELAY_URL_ENV`]: both are mistakes of the deployment, not
/// something to find out when the first alert fires.
pub fn install(core: &Core) {
    let ctx = Context::new(core).unwrap_or_else(|e| panic!("push: {e}"));
    *INSTALLED.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(ctx));
}

/// The context delivery uses now: [`scoped`] for this task, else the one
/// [`install`]ed.
pub fn current() -> Option<Arc<Context>> {
    SCOPED.try_with(Arc::clone).ok().or_else(|| {
        INSTALLED
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    })
}

/// Runs `fut` with `ctx` as the delivery context, whatever [`install`]ed.
/// For tests, which run several servers in one process.
pub async fn scoped<F: std::future::Future>(ctx: Arc<Context>, fut: F) -> F::Output {
    SCOPED.scope(ctx, fut).await
}

// ---------------------------------------------------------------------------
// Proof of work
// ---------------------------------------------------------------------------

fn leading_zero_bits(digest: &[u8]) -> u32 {
    let mut bits = 0;
    for byte in digest {
        if *byte == 0 {
            bits += 8;
        } else {
            bits += byte.leading_zeros();
            break;
        }
    }
    bits
}

/// The decimal text of `n` in `buf`.
fn decimal(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[i..];
        }
    }
}

/// Finds a nonce such that `SHA-256(challenge ‖ nonce)` (both as their UTF-8
/// text) has at least `difficulty` leading zero bits: the first one counting
/// up from 0 in decimal. `None` when `cancel` is set first.
pub fn solve(challenge: &str, difficulty: u32, cancel: &AtomicBool) -> Option<String> {
    let base = Sha256::new_with_prefix(challenge.as_bytes());
    let mut buf = [0u8; 20];
    for n in 0u64.. {
        if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let nonce = decimal(n, &mut buf);
        let mut hash = base.clone();
        hash.update(nonce);
        if leading_zero_bits(&hash.finalize()[..]) >= difficulty {
            return Some(String::from_utf8_lossy(nonce).into_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Talking to the relay
// ---------------------------------------------------------------------------

/// What went wrong with a request to the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// `403 suspended`: the relay refuses this server. Not retried.
    Suspended,
    /// `401 unknown_instance`: the relay doesn't know our credential.
    UnknownInstance,
    /// Might work later: no connection, a timeout, `429`, `503`, `5xx`.
    Transient(String),
    /// The relay refused the request (`400`, `401 invalid_credentials`, …).
    Rejected(String),
}

impl Failure {
    /// The text for the person who set the channel up ("Send test", logs).
    /// It names codes and statuses, never a URL or a secret.
    pub fn message(&self) -> String {
        match self {
            Failure::Suspended => crate::i18n::t("notif.hedgerow.err_suspended").to_string(),
            Failure::UnknownInstance => relay_error("the relay doesn't know this server"),
            Failure::Transient(detail) | Failure::Rejected(detail) => relay_error(detail),
        }
    }
}

fn relay_error(detail: &str) -> String {
    crate::i18n::tf("notif.hedgerow.err_relay", &[("detail", detail)])
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    retry_after: Option<u64>,
}

impl Reply {
    fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// The relay's error code (`{"error": "<code>", …}`).
    fn error_code(&self) -> Option<String> {
        self.json()?.get("error")?.as_str().map(str::to_string)
    }

    /// The failure a non-success reply stands for.
    fn failure(&self) -> Failure {
        let code = self.error_code();
        match (self.status, code.as_deref()) {
            (401, Some("unknown_instance")) => Failure::UnknownInstance,
            (403, Some("suspended")) => Failure::Suspended,
            (429 | 503, _) | (500.., _) => {
                let what = code.unwrap_or_else(|| format!("HTTP {}", self.status));
                Failure::Transient(match self.retry_after {
                    Some(secs) => format!("{what}, retry in {secs} s"),
                    None => what,
                })
            }
            _ => Failure::Rejected(code.unwrap_or_else(|| format!("HTTP {}", self.status))),
        }
    }
}

/// One request to the relay through the guarded client.
async fn call(
    method: Method,
    url: &str,
    bearer: Option<&str>,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Reply, Failure> {
    let transient = |e: String| Failure::Transient(e);
    let client = out::client().map_err(transient)?;
    // An IP in the URL never reaches the guarded resolver.
    if let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        && let Err(err) = crate::net::check_host(&host)
    {
        return Err(Failure::Rejected(err.to_string()));
    }
    let mut request = client.request(method, url).timeout(timeout);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .map_err(|e| transient(out::describe_reqwest(e)))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    let body = out::read_capped(response, REPLY_CAP).await;
    Ok(Reply {
        status,
        body,
        retry_after,
    })
}

#[derive(Deserialize)]
struct Challenge {
    challenge: String,
    difficulty: u32,
}

#[derive(Deserialize)]
struct Registration {
    instance_credential: String,
}

impl Context {
    /// Registers this server with the relay at `url` and keeps the
    /// credential. Not time-bounded by the alert's deadline (see
    /// [`Context::credential`]).
    async fn register(&self, url: &str) -> Result<String, Failure> {
        let reply = call(
            Method::GET,
            &format!("{url}/v1/challenge"),
            None,
            None,
            out::TIMEOUT,
        )
        .await?;
        if reply.status != 200 {
            return Err(reply.failure());
        }
        let challenge: Challenge = serde_json::from_slice(&reply.body)
            .map_err(|_| Failure::Transient("unreadable challenge".into()))?;
        if challenge.challenge.is_empty()
            || challenge.challenge.len() > 256
            || challenge.difficulty > MAX_DIFFICULTY
        {
            return Err(Failure::Rejected(
                "the relay's challenge is out of range".into(),
            ));
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let (text, difficulty, flag) = (
            challenge.challenge.clone(),
            challenge.difficulty,
            cancel.clone(),
        );
        let solving = tokio::task::spawn_blocking(move || solve(&text, difficulty, &flag));
        let nonce = match tokio::time::timeout(SOLVE_LIMIT, solving).await {
            Ok(Ok(Some(nonce))) => nonce,
            Ok(_) => return Err(Failure::Transient("proof of work failed".into())),
            Err(_) => {
                cancel.store(true, Ordering::Relaxed);
                return Err(Failure::Transient("proof of work took too long".into()));
            }
        };

        let mut body = json!({
            "challenge": challenge.challenge,
            "nonce": nonce,
            "product": self.product,
        });
        if let Some(version) = &self.version {
            body["version"] = json!(version);
        }
        let reply = call(
            Method::POST,
            &format!("{url}/v1/instances"),
            None,
            Some(&body),
            out::TIMEOUT,
        )
        .await?;
        if reply.status != 201 {
            return Err(reply.failure());
        }
        let registration: Registration = serde_json::from_slice(&reply.body)
            .map_err(|_| Failure::Transient("unreadable registration".into()))?;
        let credential = registration.instance_credential;
        if !credential.starts_with("hrk_") || credential.len() > 256 {
            return Err(Failure::Rejected(
                "the relay's credential is malformed".into(),
            ));
        }

        let sealed = self.sealer().seal(&credential);
        let url_owned = url.to_string();
        db::interact(&self.pool, move |c| {
            let tx = c.transaction()?;
            settings::set(&tx, SETTING_CREDENTIAL, &sealed)?;
            settings::set(&tx, SETTING_CREDENTIAL_URL, &url_owned)?;
            tx.commit()
        })
        .await
        .map_err(|e| Failure::Rejected(format!("couldn't keep the credential: {e}")))?;
        tracing::info!(product = self.product, "registered with the push relay");
        Ok(credential)
    }

    /// The credential for `url`: the stored one, or a new registration. A
    /// credential equal to `stale` (the relay just said it doesn't know it)
    /// doesn't count. Only one registration runs at a time and it goes on
    /// in a task of its own, so a deadline here leaves it to finish.
    async fn credential(
        self: &Arc<Self>,
        url: &str,
        stale: Option<&str>,
        deadline: Instant,
    ) -> Result<String, Failure> {
        if let Some(credential) = self.stored_credential(url).await
            && Some(credential.as_str()) != stale
        {
            return Ok(credential);
        }
        if lock(&self.backoff_until).is_some_and(|until| Instant::now() < until) {
            return Err(Failure::Transient(
                "the last registration failed; trying again shortly".into(),
            ));
        }
        let Ok(guard) = self.registering.clone().try_lock_owned() else {
            return Err(Failure::Transient("registration in progress".into()));
        };
        let (me, url, stale) = (self.clone(), url.to_string(), stale.map(str::to_string));
        let task = tokio::spawn(async move {
            let _guard = guard;
            if let Some(credential) = me.stored_credential(&url).await
                && Some(credential.as_str()) != stale.as_deref()
            {
                return Ok(credential);
            }
            let result = me.register(&url).await;
            *lock(&me.backoff_until) = match &result {
                Ok(_) => None,
                Err(failure) => {
                    tracing::warn!(?failure, "registering with the push relay failed");
                    Some(Instant::now() + REGISTER_BACKOFF)
                }
            };
            result
        });
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Failure::Transient("registration task failed".into())),
            Err(_) => Err(Failure::Transient(
                "registration is taking long; it goes on in the background".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

/// What the relay said about one handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Accepted,
    Duplicate,
    Gone,
    Limited(Option<u64>),
    /// Also what an unknown status counts as.
    Unavailable,
    UnsupportedKind,
}

impl Status {
    fn parse(value: &Value) -> Status {
        let retry = value.get("retry_after").and_then(Value::as_u64);
        match value.get("status").and_then(Value::as_str) {
            Some("accepted") => Status::Accepted,
            Some("duplicate") => Status::Duplicate,
            Some("gone") => Status::Gone,
            Some("limited") => Status::Limited(retry),
            Some("unsupported_kind") => Status::UnsupportedKind,
            _ => Status::Unavailable,
        }
    }
}

/// A random version 4 UUID, which the relay wants as `event_id`.
fn new_uuid() -> String {
    let mut b = [0u8; 16];
    rand::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// `POST /v1/push` for one batch: a status per handle, in order.
async fn send_batch(
    url: &str,
    credential: &str,
    handles: &[String],
    reference: &str,
    event_id: &str,
    timeout: Duration,
) -> Result<Vec<Status>, Failure> {
    let body = json!({
        "event_id": event_id,
        "handles": handles,
        "kind": "alert",
        "ref": reference,
        "ttl": PUSH_TTL,
    });
    let reply = call(
        Method::POST,
        &format!("{url}/v1/push"),
        Some(credential),
        Some(&body),
        timeout,
    )
    .await?;
    if reply.status != 200 {
        return Err(reply.failure());
    }
    let results = reply
        .json()
        .and_then(|v| v.get("results").and_then(Value::as_array).cloned())
        .ok_or_else(|| Failure::Transient("unreadable reply".into()))?;
    // A short answer leaves the rest unknown: treat them as unavailable.
    Ok((0..handles.len())
        .map(|i| results.get(i).map_or(Status::Unavailable, Status::parse))
        .collect())
}

/// What a delivery came to, batch by batch.
#[derive(Default)]
struct Tally {
    accepted: Vec<String>,
    gone: Vec<String>,
    duplicate: usize,
    unsupported: usize,
    limited: usize,
    unavailable: usize,
    retry_after: Option<u64>,
    failure: Option<Failure>,
}

impl Tally {
    fn record(&mut self, handles: &[String], statuses: Vec<Status>) {
        for (handle, status) in handles.iter().zip(statuses) {
            match status {
                Status::Accepted => self.accepted.push(handle.clone()),
                Status::Gone => self.gone.push(handle.clone()),
                Status::Duplicate => self.duplicate += 1,
                Status::UnsupportedKind => self.unsupported += 1,
                Status::Limited(retry) => {
                    self.limited += 1;
                    self.retry_after = self.retry_after.max(retry);
                }
                Status::Unavailable => self.unavailable += 1,
            }
        }
    }

    fn fail(&mut self, failure: Failure) {
        self.failure.get_or_insert(failure);
    }
}

/// Delivers `event` to every registered device. See the module docs for the
/// outcome. The error is safe to show: no URL, no handle, no credential.
pub async fn deliver(ctx: &Arc<Context>, event: &Alert) -> Result<(), String> {
    if ctx.is_suspended() {
        return Err(Failure::Suspended.message());
    }
    let sealer = Sealer::new(&ctx.key, devices::PURPOSE);
    let (handles, unreadable) = db::interact(&ctx.pool, move |c| devices::handles(c, &sealer))
        .await
        .map_err(|e| relay_error(&e.to_string()))?;
    if unreadable > 0 {
        tracing::warn!(
            count = unreadable,
            "push devices skipped: their handles can't be opened with this instance key"
        );
    }
    if handles.is_empty() {
        // Nobody registered yet: nothing to retry. A test message says so.
        return if event.kind == AlertKind::Test {
            Err(crate::i18n::t("notif.hedgerow.err_no_devices").to_string())
        } else {
            Ok(())
        };
    }

    let started = Instant::now();
    let deadline = started + ctx.deadline;
    let url = ctx.relay_url().await.map_err(|e| relay_error(&e))?;

    // The text goes in first: the app fetches it as soon as the push lands.
    let reference = events::new_ref();
    let stored = events::Event::from_alert(event);
    let ref_for_db = reference.clone();
    db::interact(&ctx.pool, move |c| events::insert(c, &ref_for_db, &stored))
        .await
        .map_err(|e| relay_error(&e.to_string()))?;

    let mut credential = ctx
        .credential(&url, None, deadline)
        .await
        .map_err(|f| f.message())?;

    let mut tally = Tally::default();
    let mut registered_again = false;
    'batches: for batch in handles.chunks(MAX_HANDLES) {
        let event_id = new_uuid();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                tally.fail(Failure::Transient("timed out".into()));
                break 'batches;
            }
            let timeout = remaining.min(out::TIMEOUT);
            match send_batch(&url, &credential, batch, &reference, &event_id, timeout).await {
                Ok(statuses) => {
                    tally.record(batch, statuses);
                    break;
                }
                Err(Failure::UnknownInstance) if !registered_again => {
                    registered_again = true;
                    tracing::warn!("the push relay doesn't know this server; registering again");
                    match ctx.credential(&url, Some(&credential), deadline).await {
                        Ok(new) => credential = new,
                        Err(failure) => {
                            tally.fail(failure);
                            break 'batches;
                        }
                    }
                }
                Err(Failure::Suspended) => {
                    ctx.suspend();
                    tracing::error!(
                        "the push relay has suspended this server; no push is sent for an hour"
                    );
                    tally.fail(Failure::Suspended);
                    break 'batches;
                }
                Err(failure) => {
                    tracing::warn!(?failure, "push batch failed");
                    tally.fail(failure);
                    break 'batches;
                }
            }
        }
    }

    // Housekeeping first: it does not depend on the outcome.
    if !tally.gone.is_empty() || !tally.accepted.is_empty() {
        let (gone, used) = (tally.gone.clone(), tally.accepted.clone());
        let housekeeping = db::interact(&ctx.pool, move |c| {
            devices::forget(c, &gone)?;
            devices::mark_used(c, &used)
        })
        .await;
        if let Err(err) = housekeeping {
            tracing::warn!(%err, "couldn't update the push devices");
        }
    }
    if !tally.gone.is_empty() {
        tracing::info!(
            count = tally.gone.len(),
            "forgot push devices the relay no longer knows"
        );
    }
    if tally.limited + tally.unavailable + tally.unsupported > 0 {
        tracing::warn!(
            limited = tally.limited,
            unavailable = tally.unavailable,
            unsupported = tally.unsupported,
            accepted = tally.accepted.len(),
            "some push devices were not reached"
        );
    }

    if !tally.accepted.is_empty() {
        return Ok(());
    }
    if let Some(failure) = tally.failure {
        return Err(failure.message());
    }
    if tally.limited + tally.unavailable > 0 {
        let detail = match tally.retry_after {
            Some(secs) => format!("limited or unavailable, retry in {secs} s"),
            None => "limited or unavailable".to_string(),
        };
        return Err(relay_error(&detail));
    }
    // Everything was gone, already sent or not supported: nothing a retry
    // would change.
    Ok(())
}
