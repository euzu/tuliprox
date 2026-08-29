//! Backoff for repeated failed sign-ins.
//!
//! `/auth/token` verifies an argon2 hash and answers 401. Nothing counted how
//! often that happened, so an attacker could work a password list against it as
//! fast as the hash function allows, for as long as they liked. The
//! reverse-proxy rate limiter is opt-in, disabled by default, and applies a
//! single blanket budget to every route - it is not a credential-stuffing
//! control.
//!
//! Two dimensions, because either alone is defeatable:
//!
//! * **by client address** - stops one host grinding through a password list.
//!   An attacker with a pool of addresses gets around it.
//! * **by username** - stops a distributed attack converging on one account.
//!   On its own it is a denial-of-service lever: anyone who knows a username
//!   can lock its owner out. So the username block is time-bounded and short,
//!   and a correct password clears it immediately.
//!
//! The address dimension is only as trustworthy as the address, which this
//! server currently takes from `X-Forwarded-For` without a trusted-proxy
//! allowlist. Until that is fixed, treat the username dimension as the one
//! doing the work.

use dashmap::DashMap;
use std::time::{Duration, Instant};

/// Failures tolerated before backoff starts.
const FREE_ATTEMPTS: u32 = 3;

/// The first backoff, doubling per failure beyond [`FREE_ATTEMPTS`].
const BASE_BACKOFF: Duration = Duration::from_secs(2);

/// Ceiling on a single backoff.
const MAX_BACKOFF: Duration = Duration::from_mins(15);

/// Entries idle for this long are dropped by the sweep.
const IDLE_EVICTION: Duration = Duration::from_hours(1);

/// Sweep once the map passes this many entries.
const SWEEP_THRESHOLD: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Dimension {
    Address,
    Username,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key(Dimension, String);

#[derive(Debug)]
struct Attempts {
    consecutive_failures: u32,
    blocked_until: Option<Instant>,
    last_seen: Instant,
}

/// Per-address and per-username sign-in backoff.
#[derive(Debug, Default)]
pub struct LoginThrottle {
    entries: DashMap<Key, Attempts>,
}

impl LoginThrottle {
    pub fn new() -> Self { Self::default() }

    /// How long the caller must wait, or `None` if it may attempt now.
    ///
    /// Both dimensions are consulted and the longer wait wins.
    pub fn retry_after(&self, username: &str, client_ip: &str) -> Option<Duration> {
        self.retry_after_at(username, client_ip, Instant::now())
    }

    fn retry_after_at(&self, username: &str, client_ip: &str, now: Instant) -> Option<Duration> {
        [Key(Dimension::Address, client_ip.to_string()), Key(Dimension::Username, canonical(username))]
            .into_iter()
            .filter_map(|key| self.entries.get(&key).and_then(|entry| entry.blocked_until).filter(|until| *until > now))
            .map(|until| until.duration_since(now))
            .max()
    }

    /// Record a rejected attempt and extend the backoff on both dimensions.
    pub fn record_failure(&self, username: &str, client_ip: &str) {
        self.record_failure_at(username, client_ip, Instant::now());
    }

    fn record_failure_at(&self, username: &str, client_ip: &str, now: Instant) {
        for key in [Key(Dimension::Address, client_ip.to_string()), Key(Dimension::Username, canonical(username))] {
            let mut entry = self.entries.entry(key).or_insert(Attempts {
                consecutive_failures: 0,
                blocked_until: None,
                last_seen: now,
            });
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.last_seen = now;
            entry.blocked_until = backoff_for(entry.consecutive_failures).map(|wait| now + wait);
        }
        self.sweep(now);
    }

    /// Clear both dimensions. A correct password releases a username block
    /// immediately, which is what keeps the username dimension from being a
    /// lockout lever.
    pub fn record_success(&self, username: &str, client_ip: &str) {
        self.entries.remove(&Key(Dimension::Address, client_ip.to_string()));
        self.entries.remove(&Key(Dimension::Username, canonical(username)));
    }

    /// Drop idle entries so a long grind cannot grow the map without bound.
    fn sweep(&self, now: Instant) {
        if self.entries.len() <= SWEEP_THRESHOLD {
            return;
        }
        self.entries.retain(|_, entry| now.duration_since(entry.last_seen) < IDLE_EVICTION);
    }
}

/// The backoff after `failures` consecutive rejections, or `None` while the
/// caller is still inside the free allowance.
fn backoff_for(failures: u32) -> Option<Duration> {
    let over = failures.checked_sub(FREE_ATTEMPTS)?;
    if over == 0 {
        return None;
    }
    // `over - 1` doublings: the first backoff past the allowance is BASE.
    let doublings = (over - 1).min(u32::BITS - 1);
    Some(BASE_BACKOFF.saturating_mul(1u32 << doublings).min(MAX_BACKOFF))
}

/// Usernames are matched case-insensitively everywhere else in the auth path,
/// so the throttle must match too - otherwise `Alice` and `alice` get separate
/// budgets against the same account.
fn canonical(username: &str) -> String { username.trim().to_ascii_lowercase() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_free_allowance_is_not_throttled() {
        let throttle = LoginThrottle::new();
        let now = Instant::now();
        for _ in 0..FREE_ATTEMPTS {
            throttle.record_failure_at("alice", "10.0.0.1", now);
        }
        assert!(throttle.retry_after_at("alice", "10.0.0.1", now).is_none());
    }

    #[test]
    fn backoff_starts_after_the_allowance_and_doubles() {
        assert_eq!(backoff_for(FREE_ATTEMPTS), None);
        assert_eq!(backoff_for(FREE_ATTEMPTS + 1), Some(BASE_BACKOFF));
        assert_eq!(backoff_for(FREE_ATTEMPTS + 2), Some(BASE_BACKOFF * 2));
        assert_eq!(backoff_for(FREE_ATTEMPTS + 3), Some(BASE_BACKOFF * 4));
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff_for(u32::MAX), Some(MAX_BACKOFF));
        assert_eq!(backoff_for(FREE_ATTEMPTS + 40), Some(MAX_BACKOFF));
    }

    #[test]
    fn a_blocked_caller_is_told_how_long_to_wait() {
        let throttle = LoginThrottle::new();
        let now = Instant::now();
        for _ in 0..=FREE_ATTEMPTS {
            throttle.record_failure_at("alice", "10.0.0.1", now);
        }
        assert_eq!(throttle.retry_after_at("alice", "10.0.0.1", now), Some(BASE_BACKOFF));
        // ...and is free again once it has elapsed.
        assert!(throttle.retry_after_at("alice", "10.0.0.1", now + BASE_BACKOFF).is_none());
    }

    #[test]
    fn the_username_dimension_follows_the_account_across_addresses() {
        let throttle = LoginThrottle::new();
        let now = Instant::now();
        // Each address stays inside its own allowance; the account does not.
        for (index, address) in ["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4"].into_iter().enumerate() {
            let _ = index;
            throttle.record_failure_at("alice", address, now);
        }
        assert_eq!(throttle.retry_after_at("alice", "10.0.0.99", now), Some(BASE_BACKOFF));
        // A different account from a fresh address is unaffected.
        assert!(throttle.retry_after_at("bob", "10.0.0.99", now).is_none());
    }

    #[test]
    fn a_correct_password_clears_the_block() {
        let throttle = LoginThrottle::new();
        let now = Instant::now();
        for _ in 0..=FREE_ATTEMPTS {
            throttle.record_failure_at("alice", "10.0.0.1", now);
        }
        assert!(throttle.retry_after_at("alice", "10.0.0.1", now).is_some());
        throttle.record_success("alice", "10.0.0.1");
        assert!(throttle.retry_after_at("alice", "10.0.0.1", now).is_none());
    }

    #[test]
    fn usernames_are_matched_case_insensitively() {
        let throttle = LoginThrottle::new();
        let now = Instant::now();
        for _ in 0..=FREE_ATTEMPTS {
            throttle.record_failure_at("Alice", "10.0.0.1", now);
        }
        assert!(throttle.retry_after_at("alice", "10.0.0.99", now).is_some());
    }
}
