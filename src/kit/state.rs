//! The state every kit handler and middleware works with.
//!
//! The product has its own, larger application state and implements
//! `FromRef<ProductState> for Core`, so kit code can take `State<Core>`
//! while the product's routers stay `Router<ProductState>`.

use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;
use deadpool_sqlite::Pool;

use super::config::KitConfig;

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
