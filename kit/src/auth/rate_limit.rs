//! In-memory login throttling with exponential backoff.
//!
//! Each key (`ip:…`, `user:…`, `2fa:…`, …) gets a number of free failures; after
//! that every further failure doubles the wait before the next attempt is
//! even checked, up to a cap. Failures are forgotten after a quiet period.
//! Backoff (instead of a hard lock) limits guessing without letting an
//! attacker lock a known account out for long.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Failures allowed before backoff starts.
    pub free_failures: u32,
    /// Wait after the first failure beyond the free ones; doubles each time.
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// A key with no failures for this long starts over.
    pub forget_after: Duration,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    last_failure: Instant,
    blocked_until: Instant,
}

/// Upper bound on tracked keys; beyond it, stale entries are pruned first and
/// then the oldest are dropped (bounded memory under a spray attack).
const MAX_KEYS: usize = 50_000;

#[derive(Debug)]
pub struct RateLimiter {
    policy: Policy,
    entries: Mutex<HashMap<String, Entry>>,
}

impl RateLimiter {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // A poisoned map only holds counters; keep using it.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `Some(wait)` while `key` is in backoff.
    pub fn retry_after(&self, key: &str, now: Instant) -> Option<Duration> {
        let entries = self.lock();
        let entry = entries.get(key)?;
        (entry.blocked_until > now).then(|| entry.blocked_until - now)
    }

    /// Records a failed attempt and returns the resulting wait (zero while
    /// failures are still free).
    pub fn record_failure(&self, key: &str, now: Instant) -> Duration {
        let mut entries = self.lock();
        self.record_locked(&mut entries, key, now)
    }

    fn record_locked(
        &self,
        entries: &mut HashMap<String, Entry>,
        key: &str,
        now: Instant,
    ) -> Duration {
        if entries.len() >= MAX_KEYS && !entries.contains_key(key) {
            self.prune(entries, now);
        }
        let policy = self.policy;
        let entry = entries.entry(key.to_string()).or_insert(Entry {
            failures: 0,
            last_failure: now,
            blocked_until: now,
        });
        if now.saturating_duration_since(entry.last_failure) > policy.forget_after {
            entry.failures = 0;
        }
        entry.failures = entry.failures.saturating_add(1);
        entry.last_failure = now;
        let delay = policy.delay_for(entry.failures);
        entry.blocked_until = now + delay;
        delay
    }

    /// Check and count an attempt in one step, *before* the slow password
    /// verification: `Err(wait)` while `key` is in backoff, otherwise the
    /// attempt is recorded as a failure up front. A burst of parallel
    /// requests therefore can't all slip past the check before any of them
    /// is recorded. On success, call [`clear`](Self::clear) or
    /// [`release`](Self::release).
    pub fn reserve(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let mut entries = self.lock();
        if let Some(entry) = entries.get(key)
            && entry.blocked_until > now
        {
            return Err(entry.blocked_until - now);
        }
        // Same lock as the check: no other request can slip in between.
        self.record_locked(&mut entries, key, now);
        Ok(())
    }

    /// Undo one [`reserve`](Self::reserve) (the attempt turned out fine, but
    /// the key shouldn't be forgotten entirely, e.g. a shared IP).
    pub fn release(&self, key: &str) {
        let mut entries = self.lock();
        let policy = self.policy;
        if let Some(entry) = entries.get_mut(key) {
            entry.failures = entry.failures.saturating_sub(1);
            let allowed = entry.last_failure + policy.delay_for(entry.failures);
            entry.blocked_until = entry.blocked_until.min(allowed);
            if entry.failures == 0 {
                entries.remove(key);
            }
        }
    }

    /// Forget `key` (after a successful login).
    pub fn clear(&self, key: &str) {
        self.lock().remove(key);
    }

    fn prune(&self, entries: &mut HashMap<String, Entry>, now: Instant) {
        let forget = self.policy.forget_after;
        entries.retain(|_, e| now.saturating_duration_since(e.last_failure) <= forget);
        if entries.len() >= MAX_KEYS {
            let mut by_age: Vec<(String, Instant)> = entries
                .iter()
                .map(|(k, e)| (k.clone(), e.last_failure))
                .collect();
            by_age.sort_by_key(|(_, t)| *t);
            for (k, _) in by_age.into_iter().take(MAX_KEYS / 10) {
                entries.remove(&k);
            }
        }
    }
}

impl Policy {
    pub fn delay_for(&self, failures: u32) -> Duration {
        let Some(over) = failures.checked_sub(self.free_failures).filter(|n| *n > 0) else {
            return Duration::ZERO;
        };
        let factor = 1u32.checked_shl(over - 1).unwrap_or(u32::MAX);
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// Per account (username or user id for the 2FA step): 5 free failures,
/// then 2 s, 4 s, 8 s … capped at 15 minutes.
pub const ACCOUNT_POLICY: Policy = Policy {
    free_failures: 5,
    base_delay: Duration::from_secs(2),
    max_delay: Duration::from_secs(15 * 60),
    forget_after: Duration::from_secs(60 * 60),
};

/// Per client IP: more generous (offices share an address), same cap.
pub const IP_POLICY: Policy = Policy {
    free_failures: 20,
    base_delay: Duration::from_secs(1),
    max_delay: Duration::from_secs(15 * 60),
    forget_after: Duration::from_secs(60 * 60),
};

/// Per client IP for failed passkey sign-ins: far more generous than
/// [`IP_POLICY`]. A failed passkey attempt guesses nothing (the attacker
/// can't forge a signature), so this only bounds the load, and a flood of
/// bad passkey requests (or one shared proxy address) must not lock people
/// out of password sign-in, nor the other way round.
pub const PASSKEY_POLICY: Policy = Policy {
    free_failures: 100,
    base_delay: Duration::from_secs(1),
    max_delay: Duration::from_secs(5 * 60),
    forget_after: Duration::from_secs(60 * 60),
};

/// Per account for one-time sign-in codes. A code has 160 bits, so a guess
/// is hopeless and the throttle only bounds the load; it is also a way to keep
/// somebody from using their code, so it is laxer than [`ACCOUNT_POLICY`] and
/// caps at five minutes. Like all of these, a backoff and no hard lock.
pub const CODE_ACCOUNT_POLICY: Policy = Policy {
    free_failures: 10,
    base_delay: Duration::from_secs(2),
    max_delay: Duration::from_secs(5 * 60),
    forget_after: Duration::from_secs(60 * 60),
};

/// Test emails per admin: five, then ten minutes of waiting (the
/// [`reserve`](RateLimiter::reserve) that lets the fifth through is the
/// one that starts the wait). `forget_after` is a second short of the wait,
/// so that the count starts over when the wait is over.
pub const MAIL_TEST_POLICY: Policy = Policy {
    free_failures: 4,
    base_delay: Duration::from_secs(10 * 60),
    max_delay: Duration::from_secs(10 * 60),
    forget_after: Duration::from_secs(10 * 60 - 1),
};

/// The sign-in throttles of one running instance: per account
/// ([`ACCOUNT_POLICY`]), per client IP ([`IP_POLICY`]) and per client IP for
/// passkeys ([`PASSKEY_POLICY`]). It lives in
/// [`Core`](crate::state::Core), so two servers in one process (tests) keep
/// separate counters; clones share them.
#[derive(Debug, Clone)]
pub struct SignInLimits {
    /// Keys `user:<name>` (password step), `2fa:<id>` (second factor, also
    /// on the Security page), `reauth:<id>` and `2fa-setup:<id>`.
    pub account: Arc<RateLimiter>,
    /// Keys `ip:<address>` (password step) and `setup:<address>` (wrong
    /// setup tokens).
    pub ip: Arc<RateLimiter>,
    /// Keys `ip:<address>`: failed passkey sign-ins, apart from `ip`.
    pub passkey: Arc<RateLimiter>,
    /// Keys `code:<name>` (one-time sign-in codes, [`CODE_ACCOUNT_POLICY`]):
    /// apart from `account`, so that guessing codes can't eat the login
    /// budget of an account, nor the other way round.
    pub code_account: Arc<RateLimiter>,
    /// Keys `code:<address>` (an IPv6 address by its /64), apart from `ip`.
    pub code_ip: Arc<RateLimiter>,
    /// Keys `mail-test:<admin id>`: the test email of the General page
    /// ([`MAIL_TEST_POLICY`]).
    pub mail_test: Arc<RateLimiter>,
}

impl SignInLimits {
    /// Fresh counters with the production policies.
    pub fn new() -> Self {
        Self::with_policies(ACCOUNT_POLICY, IP_POLICY)
    }

    /// Fresh counters with other policies (tests of the throttling itself).
    pub fn with_policies(account: Policy, ip: Policy) -> Self {
        Self {
            account: Arc::new(RateLimiter::new(account)),
            ip: Arc::new(RateLimiter::new(ip)),
            passkey: Arc::new(RateLimiter::new(PASSKEY_POLICY)),
            code_account: Arc::new(RateLimiter::new(CODE_ACCOUNT_POLICY)),
            code_ip: Arc::new(RateLimiter::new(IP_POLICY)),
            mail_test: Arc::new(RateLimiter::new(MAIL_TEST_POLICY)),
        }
    }
}

impl Default for SignInLimits {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> RateLimiter {
        RateLimiter::new(Policy {
            free_failures: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            forget_after: Duration::from_secs(600),
        })
    }

    #[test]
    fn the_test_email_allows_five_and_then_waits_ten_minutes() {
        let l = RateLimiter::new(MAIL_TEST_POLICY);
        let t = Instant::now();
        for i in 0..5 {
            assert!(l.reserve("admin", t).is_ok(), "{i}");
        }
        let wait = l.reserve("admin", t).unwrap_err();
        assert_eq!(wait, Duration::from_secs(600));
        assert!(l.reserve("other", t).is_ok(), "per admin");
        // Still waiting after nine minutes, free again after ten.
        assert!(l.reserve("admin", t + Duration::from_secs(540)).is_err());
        assert!(l.reserve("admin", t + Duration::from_secs(600)).is_ok());
        for _ in 0..4 {
            assert!(l.reserve("admin", t + Duration::from_secs(601)).is_ok());
        }
        assert!(l.reserve("admin", t + Duration::from_secs(601)).is_err());
    }

    #[test]
    fn free_failures_do_not_block() {
        let l = limiter();
        let t = Instant::now();
        for _ in 0..3 {
            l.record_failure("k", t);
        }
        assert_eq!(l.retry_after("k", t), None);
    }

    #[test]
    fn backoff_doubles_after_free_failures() {
        let l = limiter();
        let t = Instant::now();
        let delays: Vec<u64> = (0..6).map(|_| l.record_failure("k", t).as_secs()).collect();
        assert_eq!(delays, vec![0, 0, 0, 2, 4, 8]);
    }

    #[test]
    fn backoff_is_capped() {
        let l = limiter();
        let t = Instant::now();
        let last = (0..40).map(|_| l.record_failure("k", t)).last().unwrap();
        assert_eq!(last, Duration::from_secs(60));
    }

    #[test]
    fn blocked_key_reports_remaining_wait() {
        let l = limiter();
        let t = Instant::now();
        for _ in 0..4 {
            l.record_failure("k", t);
        }
        assert_eq!(
            l.retry_after("k", t + Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(l.retry_after("k", t + Duration::from_secs(2)), None);
    }

    #[test]
    fn keys_are_independent() {
        let l = limiter();
        let t = Instant::now();
        for _ in 0..5 {
            l.record_failure("a", t);
        }
        assert_eq!(l.retry_after("b", t), None);
    }

    #[test]
    fn clear_resets_a_key() {
        let l = limiter();
        let t = Instant::now();
        for _ in 0..5 {
            l.record_failure("k", t);
        }
        l.clear("k");
        assert_eq!(l.retry_after("k", t), None);
        assert_eq!(l.record_failure("k", t), Duration::ZERO);
    }

    #[test]
    fn failures_are_forgotten_after_quiet_period() {
        let l = limiter();
        let t = Instant::now();
        for _ in 0..5 {
            l.record_failure("k", t);
        }
        assert_eq!(
            l.record_failure("k", t + Duration::from_secs(601)),
            Duration::ZERO
        );
    }

    #[test]
    fn reserve_counts_attempts_before_they_finish() {
        let l = limiter();
        let t = Instant::now();
        // Nothing has *finished* yet, but each reservation already counts:
        // free ones, then the one that starts the backoff, then refusals.
        for _ in 0..4 {
            assert_eq!(l.reserve("k", t), Ok(()));
        }
        assert_eq!(l.reserve("k", t), Err(Duration::from_secs(2)));
    }

    #[test]
    fn release_undoes_a_reservation() {
        let l = limiter();
        let t = Instant::now();
        for _ in 0..4 {
            l.reserve("k", t).unwrap();
        }
        l.release("k");
        assert_eq!(l.retry_after("k", t), None);
        l.release("k");
        l.release("k");
        l.release("k");
        assert!(
            l.lock().get("k").is_none(),
            "fully released keys are dropped"
        );
        l.release("missing");
    }

    #[test]
    fn separate_instances_keep_separate_counters() {
        let (a, b) = (SignInLimits::new(), SignInLimits::new());
        let t = Instant::now();
        for _ in 0..30 {
            a.account.record_failure("user:ada", t);
            a.ip.record_failure("ip:127.0.0.1", t);
        }
        assert!(a.account.retry_after("user:ada", t).is_some());
        assert_eq!(b.account.retry_after("user:ada", t), None);
        assert_eq!(b.ip.retry_after("ip:127.0.0.1", t), None);
        // Clones share them.
        assert!(a.clone().ip.retry_after("ip:127.0.0.1", t).is_some());
    }

    #[test]
    fn delay_for_handles_huge_failure_counts() {
        let p = limiter().policy;
        assert_eq!(p.delay_for(u32::MAX), Duration::from_secs(60));
    }
}
