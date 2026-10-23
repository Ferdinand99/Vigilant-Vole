//! Token buckets in memory, for request budgets per key: an API key, a
//! client IP address, a project. Each key has a bucket that holds `burst`
//! requests and refills continuously at `burst` per `per`; a request takes
//! one token or learns how long until one is free. Nothing is written to
//! disk, so a restart forgets every bucket.
//!
//! ```ignore
//! static BY_IP: LazyLock<TokenBuckets<IpAddr>> = LazyLock::new(TokenBuckets::new);
//!
//! if let Err(wait) = BY_IP.take(&ip, Rate::per_minute(600), Instant::now()) {
//!     return too_many_requests(wait);
//! }
//! ```
//!
//! The sign-in backoff is [`crate::auth::rate_limit`]; this is for the
//! steady budgets of machine endpoints.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::rest::V1Error;

/// Buckets kept before full ones are dropped (a full bucket is the same as
/// none) and, if that isn't enough, all of them.
pub const DEFAULT_MAX_KEYS: usize = 50_000;

/// How many requests a key may make: `burst` at once, refilled at `burst`
/// per `per`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// Requests a full bucket holds (at least 1).
    pub burst: u32,
    /// How long an empty bucket takes to fill up again.
    pub per: Duration,
}

impl Rate {
    /// `n` requests a minute, all of them at once if the bucket is full.
    pub const fn per_minute(n: u32) -> Self {
        Rate {
            burst: n,
            per: Duration::from_secs(60),
        }
    }

    /// `n` requests a second.
    pub const fn per_second(n: u32) -> Self {
        Rate {
            burst: n,
            per: Duration::from_secs(1),
        }
    }

    fn capacity(self) -> f64 {
        f64::from(self.burst.max(1))
    }

    /// Tokens per second.
    fn refill(self) -> f64 {
        self.capacity() / self.per.as_secs_f64().max(1e-3)
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token buckets by key. Share one per budget (one for API keys, another
/// for IP addresses); a key's rate is passed with every call, so it can
/// differ per key (a per-key setting) and change at once.
#[derive(Debug)]
pub struct TokenBuckets<K> {
    map: Mutex<HashMap<K, Bucket>>,
    max_keys: usize,
}

impl<K: Hash + Eq + Clone> Default for TokenBuckets<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq + Clone> TokenBuckets<K> {
    /// Buckets for up to [`DEFAULT_MAX_KEYS`] keys.
    pub fn new() -> Self {
        Self::with_max_keys(DEFAULT_MAX_KEYS)
    }

    /// Buckets for up to `max_keys` keys.
    pub fn with_max_keys(max_keys: usize) -> Self {
        TokenBuckets {
            map: Mutex::new(HashMap::new()),
            max_keys: max_keys.max(1),
        }
    }

    /// Takes one token from `key`'s bucket. `Err` holds how long until one
    /// is free (use it for `Retry-After`).
    pub fn take(&self, key: &K, rate: Rate, now: Instant) -> Result<(), Duration> {
        self.take_n(key, 1, rate, now)
    }

    /// Takes `n` tokens at once (a batch that counts as `n` requests). A
    /// batch larger than the bucket can never pass (its wait only runs
    /// until the bucket is full), so cap batches at [`Rate::burst`].
    pub fn take_n(&self, key: &K, n: u32, rate: Rate, now: Instant) -> Result<(), Duration> {
        let capacity = rate.capacity();
        let refill = rate.refill();
        let want = f64::from(n.max(1));
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if map.len() >= self.max_keys && !map.contains_key(key) {
            // Full buckets are the same as no bucket.
            map.retain(|_, b| {
                now.saturating_duration_since(b.last).as_secs_f64() * refill + b.tokens < capacity
            });
            if map.len() >= self.max_keys {
                map.clear();
            }
        }
        let bucket = map.entry(key.clone()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill).min(capacity);
        bucket.last = now;
        if bucket.tokens >= want {
            bucket.tokens -= want;
            Ok(())
        } else {
            let missing = want.min(capacity) - bucket.tokens;
            Err(Duration::from_secs_f64(missing.max(0.0) / refill))
        }
    }

    /// Forgets `key`'s bucket (its next request finds it full).
    pub fn reset(&self, key: &K) {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }

    /// Keys with a bucket.
    pub fn len(&self) -> usize {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// No key has a bucket.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whole seconds to wait, rounded up and at least one: the value of
/// `Retry-After`.
pub fn retry_after_secs(wait: Duration) -> u64 {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    secs.max(1)
}

/// The `Retry-After` header for `wait`, to add to a response of the
/// product's own (`(StatusCode::TOO_MANY_REQUESTS, [retry_after(wait)], body)`).
pub fn retry_after(wait: Duration) -> (HeaderName, HeaderValue) {
    (
        header::RETRY_AFTER,
        HeaderValue::from(retry_after_secs(wait)),
    )
}

/// `429 Too Many Requests` with `Retry-After` and the REST error envelope
/// (`{"error": {"code": "rate_limited", "message": …}}`). Products that
/// document their REST errors add [`crate::docs::RATE_LIMITED`] to their
/// error codes.
pub fn too_many_requests(wait: Duration) -> Response {
    let secs = retry_after_secs(wait);
    let mut res = V1Error::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!("Too many requests. Try again in {secs} s."),
    )
    .into_response();
    let (name, value) = retry_after(wait);
    res.headers_mut().insert(name, value);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_empties_and_refills() {
        let b = TokenBuckets::new();
        let t0 = Instant::now();
        let rate = Rate::per_minute(60);
        for _ in 0..60 {
            assert!(b.take(&1, rate, t0).is_ok());
        }
        let wait = b.take(&1, rate, t0).unwrap_err();
        assert_eq!(retry_after_secs(wait), 1);
        assert!(b.take(&2, rate, t0).is_ok(), "buckets are per key");
        assert!(b.take(&1, rate, t0 + Duration::from_secs(1)).is_ok());
        assert!(b.take(&1, rate, t0 + Duration::from_secs(1)).is_err());
        b.reset(&1);
        assert!(b.take(&1, rate, t0 + Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn slow_buckets_say_how_long() {
        let b = TokenBuckets::new();
        let t0 = Instant::now();
        let rate = Rate::per_minute(2);
        b.take(&"k", rate, t0).unwrap();
        b.take(&"k", rate, t0).unwrap();
        assert_eq!(retry_after_secs(b.take(&"k", rate, t0).unwrap_err()), 30);
        let fast = Rate::per_second(10);
        for _ in 0..10 {
            b.take(&"f", fast, t0).unwrap();
        }
        let wait = b.take(&"f", fast, t0).unwrap_err();
        assert_eq!(wait, Duration::from_millis(100));
        assert_eq!(retry_after_secs(wait), 1, "never zero");
    }

    #[test]
    fn batches_take_several_tokens() {
        let b = TokenBuckets::new();
        let t0 = Instant::now();
        let rate = Rate::per_minute(10);
        assert!(b.take_n(&"k", 8, rate, t0).is_ok());
        let wait = b.take_n(&"k", 4, rate, t0).unwrap_err();
        assert_eq!(retry_after_secs(wait), 12, "two tokens are missing");
        assert!(b.take_n(&"k", 2, rate, t0).is_ok());
        let too_big = b.take_n(&"x", 50, rate, t0).unwrap_err();
        assert_eq!(retry_after_secs(too_big), 1, "a full bucket can't serve 50");
    }

    #[test]
    fn full_buckets_are_dropped_when_there_are_too_many_keys() {
        let b = TokenBuckets::with_max_keys(3);
        let t0 = Instant::now();
        let rate = Rate::per_minute(5);
        for key in 0..3 {
            b.take(&key, rate, t0).unwrap();
        }
        assert_eq!(b.len(), 3);
        // A minute later every bucket is full again: room for a new key.
        b.take(&9, rate, t0 + Duration::from_secs(60)).unwrap();
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn retry_after_rounds_up() {
        assert_eq!(retry_after_secs(Duration::ZERO), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1500)), 2);
        assert_eq!(retry_after_secs(Duration::from_secs(30)), 30);
    }

    #[tokio::test]
    async fn too_many_requests_says_when_to_come_back() {
        let res = too_many_requests(Duration::from_millis(2500));
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers()[header::RETRY_AFTER], "3");
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "rate_limited");
        assert_eq!(
            json["error"]["message"],
            "Too many requests. Try again in 3 s."
        );
    }
}
