//! The state every kit handler and middleware works with.
//!
//! The product has its own, larger application state and implements
//! `FromRef<ProductState> for Core`, so kit code can take `State<Core>`
//! while the product's routers stay `Router<ProductState>`.

use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;
use deadpool_sqlite::Pool;

use super::config::KitConfig;
use super::http::ApiError;

/// The parts of the application state the kit needs.
#[derive(Clone)]
pub struct Core {
    pub db: Pool,
    /// Signs and encrypts cookies.
    pub session_key: Key,
    /// Shared outbound HTTP client.
    pub http: reqwest::Client,
    pub cfg: &'static KitConfig,
}

impl FromRef<Core> for Key {
    fn from_ref(core: &Core) -> Key {
        core.session_key.clone()
    }
}

impl Core {
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
        self.with_conn(|c| super::db::settings::base_url(c))
            .await
            .ok()
            .flatten()
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
