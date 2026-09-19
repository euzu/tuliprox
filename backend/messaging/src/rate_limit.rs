//! Per-channel suppression and rate limiting.
//!
//! Notification volume was low enough that none of this mattered: six
//! emitters, none of them chatty. Bridging the internal event bus and
//! adding provider/stream events raises that by an order of magnitude, and
//! the providers have hard limits - Discord webhooks allow about 5 requests
//! per 2s, Telegram 20 messages per chat per minute. Shaping traffic before
//! the `429` beats discovering it afterwards.
//!
//! State lives here rather than on the channel because it must survive
//! config reloads and channel rebuilds.

use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
    time::{Duration, Instant},
};

#[derive(Debug, Default)]
struct ChannelState {
    /// Recent `dedup_key`s and when they were last sent.
    seen: HashMap<String, Instant>,
    /// Send timestamps inside the current hour window, for `max_per_hour`.
    sent_at: Vec<Instant>,
    /// Whether the hourly ceiling has already been reported.
    ceiling_reported: bool,
}

impl ChannelState {
    /// Record a send at `now` and decide whether it is allowed.
    ///
    /// Pure in `now` so the windows are testable without sleeping.
    fn admit(
        &mut self,
        now: Instant,
        dedup_key: Option<&str>,
        dedup_window: Option<Duration>,
        max_per_hour: Option<u32>,
    ) -> Option<Suppression> {
        // Suppression by dedup key.
        if let (Some(key), Some(window)) = (dedup_key, dedup_window) {
            self.seen.retain(|_, last| now.duration_since(*last) < window);
            if self.seen.contains_key(key) {
                return Some(Suppression::Duplicate);
            }
        }

        // Hourly ceiling.
        if let Some(max) = max_per_hour {
            let hour = Duration::from_hours(1);
            self.sent_at.retain(|at| now.duration_since(*at) < hour);
            if self.sent_at.len() >= max as usize {
                // Report the ceiling exactly once per window, so the operator
                // can tell suppression from a notifier that has died.
                if self.ceiling_reported {
                    return Some(Suppression::RateLimited);
                }
                self.ceiling_reported = true;
                return Some(Suppression::RateLimitReached);
            }
            self.ceiling_reported = false;
            self.sent_at.push(now);
        }

        if let (Some(key), Some(_)) = (dedup_key, dedup_window) {
            self.seen.insert(key.to_string(), now);
        }
        None
    }
}

static STATE: OnceLock<RwLock<HashMap<String, ChannelState>>> = OnceLock::new();

fn state() -> &'static RwLock<HashMap<String, ChannelState>> { STATE.get_or_init(|| RwLock::new(HashMap::new())) }

/// Forget all suppression and rate-limit state. Called on config reload.
pub fn reset() {
    if let Ok(mut guard) = state().write() {
        guard.clear();
    }
}

/// Why a notification was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppression {
    /// An identical `dedup_key` was sent inside the window.
    Duplicate,
    /// The channel's hourly ceiling is reached.
    RateLimited,
    /// The channel's hourly ceiling was reached *just now* - the caller
    /// should emit one "suppressing further notifications" message so the
    /// silence is distinguishable from a broken notifier.
    RateLimitReached,
}

/// Record a send and decide whether it is allowed.
///
/// Returns `None` when the notification should go out.
pub fn admit(
    channel_id: &str,
    dedup_key: Option<&str>,
    dedup_window: Option<Duration>,
    max_per_hour: Option<u32>,
) -> Option<Suppression> {
    let mut guard = state().write().ok()?;
    guard.entry(channel_id.to_string()).or_default().admit(Instant::now(), dedup_key, dedup_window, max_per_hour)
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
mod tests {
    use super::{ChannelState, Suppression};
    use std::time::{Duration, Instant};

    const WINDOW: Duration = Duration::from_mins(1);

    fn win() -> Option<Duration> { Some(WINDOW) }

    #[test]
    fn a_repeated_dedup_key_inside_the_window_is_suppressed() {
        let mut state = ChannelState::default();
        let now = Instant::now();
        assert_eq!(state.admit(now, Some("disk:critical"), win(), None), None);
        assert_eq!(state.admit(now, Some("disk:critical"), win(), None), Some(Suppression::Duplicate));
    }

    #[test]
    fn the_same_key_is_allowed_again_once_the_window_has_passed() {
        let mut state = ChannelState::default();
        let now = Instant::now();
        assert_eq!(state.admit(now, Some("k"), win(), None), None);
        let later = now + Duration::from_secs(61);
        assert_eq!(state.admit(later, Some("k"), win(), None), None, "window did not expire");
    }

    #[test]
    fn different_dedup_keys_do_not_suppress_each_other() {
        let mut state = ChannelState::default();
        let now = Instant::now();
        assert_eq!(state.admit(now, Some("a"), win(), None), None);
        assert_eq!(state.admit(now, Some("b"), win(), None), None);
    }

    #[test]
    fn an_event_with_no_dedup_key_is_never_suppressed_as_a_duplicate() {
        let mut state = ChannelState::default();
        let now = Instant::now();
        for _ in 0..5 {
            assert_eq!(state.admit(now, None, win(), None), None);
        }
    }

    #[test]
    fn no_window_means_no_suppression() {
        let mut state = ChannelState::default();
        let now = Instant::now();
        assert_eq!(state.admit(now, Some("k"), None, None), None);
        assert_eq!(state.admit(now, Some("k"), None, None), None);
    }

    #[test]
    fn the_hourly_ceiling_reports_once_then_stays_quiet() {
        let mut state = ChannelState::default();
        let now = Instant::now();
        assert_eq!(state.admit(now, None, None, Some(2)), None);
        assert_eq!(state.admit(now, None, None, Some(2)), None);
        // The third trips it, and says so exactly once so the operator can
        // tell suppression from a dead notifier.
        assert_eq!(state.admit(now, None, None, Some(2)), Some(Suppression::RateLimitReached));
        assert_eq!(state.admit(now, None, None, Some(2)), Some(Suppression::RateLimited));
        assert_eq!(state.admit(now, None, None, Some(2)), Some(Suppression::RateLimited));
    }

    #[test]
    fn the_ceiling_clears_once_the_hour_window_rolls_off() {
        let mut state = ChannelState::default();
        let now = Instant::now();
        assert_eq!(state.admit(now, None, None, Some(1)), None);
        assert_eq!(state.admit(now, None, None, Some(1)), Some(Suppression::RateLimitReached));
        let later = now + Duration::from_hours(1) + Duration::from_secs(1);
        assert_eq!(state.admit(later, None, None, Some(1)), None, "hour window did not roll off");
    }

    #[test]
    fn no_ceiling_configured_means_unlimited() {
        let mut state = ChannelState::default();
        let now = Instant::now();
        for _ in 0..100 {
            assert_eq!(state.admit(now, None, None, None), None);
        }
    }
}
