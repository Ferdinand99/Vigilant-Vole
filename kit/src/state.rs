//! The state every kit handler and middleware works with.
//!
//! The product has its own, larger application state and implements
//! `FromRef<ProductState> for Core`, so kit code can take `State<Core>`
//! while the product's routers stay `Router<ProductState>`.

use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;
use deadpool_sqlite::Pool;

use super::auth::rate_limit::SignInLimits;
use super::config::KitConfig;
use super::http::ApiError;

/// The parts of the application state the kit needs.
///
/// Build it once per running instance ([`Core::new`]) and clone it: the
/// clones share the pool and the sign-in throttles. A product whose own
/// state builds a `Core` on demand (`FromRef`) keeps one [`SignInLimits`]
/// in that state and clones it in.
#[derive(Clone)]
pub struct Core {
    pub db: Pool,
    /// Signs and encrypts cookies.
    pub session_key: Key,
    /// Shared outbound HTTP client.
    pub http: reqwest::Client,
    pub cfg: &'static KitConfig,
    /// This instance's sign-in throttles (per account and per IP). Separate
    /// instances (test servers in one process) don't share them.
    pub limits: SignInLimits,
}

impl FromRef<Core> for Key {
    fn from_ref(core: &Core) -> Key {
        core.session_key.clone()
    }
}

impl Core {
    /// A new instance's state, with fresh sign-in throttles.
    pub fn new(db: Pool, session_key: Key, http: reqwest::Client, cfg: &'static KitConfig) -> Self {
        Self {
            db,
            session_key,
            http,
            cfg,
            limits: SignInLimits::new(),
        }
    }

    /// Runs a closure on a pooled connection, mapping every failure to
    /// [`ApiError`]. See [`with_conn`].
    pub async fn with_conn<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        with_conn(&self.db, f).await
    }

    /// The configured public URL (settings), if any.
    pub async fn base_url(&self) -> Option<String> {
        let read = self
            .with_conn(|c| super::db::settings::base_url(c))
            .await
            .ok()?;
        // Cookies are `Secure` when the public URL is https.
        super::auth::session::note_base_url(read.as_deref());
        read
    }
}

/// Runs a closure on a pooled connection, mapping every failure to
/// [`ApiError`]. The closure runs on a blocking thread, so the request's
/// language is carried along: labels built there are translated.
pub async fn with_conn<T, F>(pool: &Pool, f: F) -> Result<T, ApiError>
where
    F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let lang = super::i18n::current();
    Ok(super::db::interact(pool, move |c| super::i18n::sync_scope(lang, || f(c))).await?)
}
