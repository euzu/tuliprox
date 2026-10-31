//! Per-plugin log ring buffers, fed by the `log` host fn (plan §7 *Per-plugin
//! logs*), plus the per-plugin rate limiter, secret redaction and message cap
//! that sit in front of both the ring and the backend logger.

use shared::model::REDACTED_SECRET;
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
};

/// Entries one plugin's ring keeps.
pub(crate) const PLUGIN_LOG_CAPACITY: usize = 1000;
/// Lines one plugin may log per rate-limit window, across all of its instances.
pub(crate) const PLUGIN_LOG_RATE_LIMIT: u32 = 100;
/// Length of the rate-limit window.
pub(crate) const PLUGIN_LOG_WINDOW_MS: u64 = 1000;
/// Longest message kept, in bytes. Longer ones are cut (on a char boundary) and
/// marked.
pub(crate) const PLUGIN_LOG_MAX_MESSAGE_BYTES: usize = 4096;

const TRUNCATION_MARKER: &str = "…[truncated]";

/// One buffered line. `message` is already redacted.
#[derive(Debug, Clone)]
pub(crate) struct PluginLogEntry {
    pub(crate) ts_ms: u64,
    pub(crate) level: log::Level,
    pub(crate) message: String,
}

/// Filters for [`PluginLog::entries`] (the `…/logs` endpoint).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LogQuery {
    /// Least severe level returned (`Info` returns info, warn and error).
    pub(crate) min_level: Option<log::Level>,
    pub(crate) since_ms: Option<u64>,
    /// Newest `limit` entries; all when `None`.
    pub(crate) limit: Option<usize>,
}

/// What the rate limiter decided about one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admission {
    /// The line may be logged.
    pub(crate) allowed: bool,
    /// A window just closed in which this many lines were dropped; the caller
    /// logs one warning about it.
    pub(crate) report_dropped: Option<u64>,
}

#[derive(Debug, Default)]
struct LogState {
    ring: VecDeque<PluginLogEntry>,
    window_start_ms: Option<u64>,
    window_count: u32,
    window_dropped: u64,
}

/// One plugin's ring buffer and rate limiter. Shared by all of its instances
/// and kept across registry reloads.
#[derive(Debug, Default)]
pub(crate) struct PluginLog {
    state: Mutex<LogState>,
    dropped_total: AtomicU64,
}

impl PluginLog {
    fn lock(&self) -> MutexGuard<'_, LogState> { self.state.lock().unwrap_or_else(PoisonError::into_inner) }

    /// Lines dropped by the rate limiter since the plugin first logged.
    pub(crate) fn dropped_total(&self) -> u64 { self.dropped_total.load(Ordering::Relaxed) }

    /// Count one line against the current window (a fixed window of
    /// [`PLUGIN_LOG_WINDOW_MS`] opening at the first line after the previous one
    /// closed). Lines past [`PLUGIN_LOG_RATE_LIMIT`] are dropped and counted.
    pub(crate) fn admit(&self, now_ms: u64) -> Admission {
        let mut state = self.lock();
        let mut report_dropped = None;
        let expired = state.window_start_ms.is_none_or(|start| now_ms.saturating_sub(start) >= PLUGIN_LOG_WINDOW_MS);
        if expired {
            let dropped = std::mem::take(&mut state.window_dropped);
            if dropped > 0 {
                report_dropped = Some(dropped);
            }
            state.window_start_ms = Some(now_ms);
            state.window_count = 0;
        }
        let allowed = state.window_count < PLUGIN_LOG_RATE_LIMIT;
        if allowed {
            state.window_count += 1;
        } else {
            state.window_dropped += 1;
            let _ = self.dropped_total.fetch_add(1, Ordering::Relaxed);
        }
        Admission { allowed, report_dropped }
    }

    /// Buffer a line that the caller has already redacted and rate-limited.
    pub(crate) fn push(&self, ts_ms: u64, level: log::Level, message: &str) {
        let mut state = self.lock();
        while state.ring.len() >= PLUGIN_LOG_CAPACITY {
            let _ = state.ring.pop_front();
        }
        state.ring.push_back(PluginLogEntry { ts_ms, level, message: message.to_string() });
    }

    /// Oldest first.
    pub(crate) fn entries(&self, query: &LogQuery) -> Vec<PluginLogEntry> {
        let state = self.lock();
        let mut picked: Vec<PluginLogEntry> = state
            .ring
            .iter()
            .rev()
            .filter(|entry| query.min_level.is_none_or(|min| entry.level <= min))
            .filter(|entry| query.since_ms.is_none_or(|since| entry.ts_ms >= since))
            .take(query.limit.unwrap_or(usize::MAX))
            .cloned()
            .collect();
        picked.reverse();
        picked
    }

    /// The whole `log` host fn: rate-limit, redact `secrets`, cap, then emit
    /// through the backend logger under target `plugin::<id>` (so the global log
    /// ring, the websocket tail and syslog all see it) and buffer in the ring.
    /// Dropped lines are never redacted, logged or buffered. When a window that
    /// dropped lines closes, one warning with the count is logged in its place.
    pub(crate) fn record(&self, id: &str, secrets: &[String], now_ms: u64, level: log::Level, message: &str) {
        let admission = self.admit(now_ms);
        let target = format!("plugin::{id}");
        if let Some(dropped) = admission.report_dropped {
            let note = format!(
                "log rate limit of {PLUGIN_LOG_RATE_LIMIT} lines/s exceeded: dropped {dropped} line(s) in the previous window"
            );
            log::log!(target: &target, log::Level::Warn, "{note}");
            self.push(now_ms, log::Level::Warn, &note);
        }
        if !admission.allowed {
            return;
        }
        // Redact before capping: a cut through the middle of a secret must not
        // leave its prefix behind.
        let redacted = redact(message, secrets);
        let capped = cap_message(&redacted);
        log::log!(target: &target, level, "{capped}");
        self.push(now_ms, level, &capped);
    }
}

/// `message` with every non-empty value of `secrets` replaced by
/// [`REDACTED_SECRET`]. Longer secrets go first so one that contains another is
/// masked whole.
pub(crate) fn redact<'a>(message: &'a str, secrets: &[String]) -> Cow<'a, str> {
    let mut present: Vec<&str> =
        secrets.iter().map(String::as_str).filter(|secret| !secret.is_empty() && message.contains(secret)).collect();
    if present.is_empty() {
        return Cow::Borrowed(message);
    }
    present.sort_by_key(|secret| Reverse(secret.len()));
    let mut redacted = message.to_string();
    for secret in present {
        redacted = redacted.replace(secret, REDACTED_SECRET);
    }
    Cow::Owned(redacted)
}

/// `message` cut to [`PLUGIN_LOG_MAX_MESSAGE_BYTES`] (marker included) on a char
/// boundary.
pub(crate) fn cap_message(message: &str) -> Cow<'_, str> {
    if message.len() <= PLUGIN_LOG_MAX_MESSAGE_BYTES {
        return Cow::Borrowed(message);
    }
    let mut end = PLUGIN_LOG_MAX_MESSAGE_BYTES - TRUNCATION_MARKER.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut capped = String::with_capacity(PLUGIN_LOG_MAX_MESSAGE_BYTES);
    capped.push_str(&message[..end]);
    capped.push_str(TRUNCATION_MARKER);
    Cow::Owned(capped)
}

/// Every plugin's ring, by id.
#[derive(Debug, Default)]
pub(crate) struct LogRegistry {
    plugins: Mutex<HashMap<Arc<str>, Arc<PluginLog>>>,
}

impl LogRegistry {
    fn lock(&self) -> MutexGuard<'_, HashMap<Arc<str>, Arc<PluginLog>>> {
        self.plugins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The ring of `id`, created on first use.
    pub(crate) fn plugin(&self, id: &Arc<str>) -> Arc<PluginLog> {
        Arc::clone(self.lock().entry(Arc::clone(id)).or_default())
    }

    /// The ring of `id` if it has one.
    pub(crate) fn get(&self, id: &str) -> Option<Arc<PluginLog>> { self.lock().get(id).map(Arc::clone) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(values: &[&str]) -> Vec<String> { values.iter().map(|value| (*value).to_string()).collect() }

    #[test]
    fn limiter_admits_the_limit_then_drops_and_counts() {
        let log = PluginLog::default();
        for _ in 0..PLUGIN_LOG_RATE_LIMIT {
            assert!(log.admit(5_000).allowed);
        }
        assert!(!log.admit(5_000).allowed);
        assert!(!log.admit(5_999).allowed);
        assert_eq!(log.dropped_total(), 2);
    }

    #[test]
    fn limiter_reports_the_drop_count_once_when_the_window_closes() {
        let log = PluginLog::default();
        for _ in 0..PLUGIN_LOG_RATE_LIMIT + 7 {
            let _ = log.admit(1_000);
        }
        let next = log.admit(1_000 + PLUGIN_LOG_WINDOW_MS);
        assert_eq!(next, Admission { allowed: true, report_dropped: Some(7) });
        // The report is made once; a window without drops reports nothing.
        assert_eq!(log.admit(1_000 + PLUGIN_LOG_WINDOW_MS + 1), Admission { allowed: true, report_dropped: None });
        assert_eq!(log.dropped_total(), 7);
    }

    #[test]
    fn limiter_window_is_shared_by_every_user_of_the_plugin_log() {
        let log = Arc::new(PluginLog::default());
        let other = Arc::clone(&log);
        for _ in 0..PLUGIN_LOG_RATE_LIMIT / 2 {
            assert!(log.admit(0).allowed);
        }
        for _ in 0..PLUGIN_LOG_RATE_LIMIT / 2 {
            assert!(other.admit(10).allowed);
        }
        assert!(!log.admit(20).allowed);
    }

    #[test]
    fn limiter_tolerates_a_clock_that_goes_backwards() {
        let log = PluginLog::default();
        assert!(log.admit(10_000).allowed);
        assert!(log.admit(9_000).allowed);
    }

    #[test]
    fn redact_masks_every_secret_and_ignores_empty_ones() {
        let secrets = secrets(&["hunter2", "", "tok-123"]);
        let redacted = redact("login hunter2 with tok-123 then hunter2 again", &secrets);
        assert_eq!(redacted, format!("login {REDACTED_SECRET} with {REDACTED_SECRET} then {REDACTED_SECRET} again"));
        assert!(matches!(redact("nothing to hide", &secrets), Cow::Borrowed("nothing to hide")));
    }

    #[test]
    fn redact_masks_the_longer_secret_whole_when_one_contains_another() {
        let secrets = secrets(&["abc", "abcdef"]);
        assert_eq!(redact("key=abcdef", &secrets), format!("key={REDACTED_SECRET}"));
    }

    #[test]
    fn cap_keeps_short_messages_and_cuts_long_ones_on_a_char_boundary() {
        assert!(matches!(cap_message("short"), Cow::Borrowed("short")));
        // 3-byte chars so the cut position is never a boundary by luck.
        let long = "€".repeat(PLUGIN_LOG_MAX_MESSAGE_BYTES);
        let capped = cap_message(&long);
        assert!(capped.len() <= PLUGIN_LOG_MAX_MESSAGE_BYTES);
        assert!(capped.ends_with(TRUNCATION_MARKER));
        assert!(capped.starts_with('€'));
    }

    #[test]
    fn record_redacts_before_buffering_and_caps_after_redacting() {
        let log = PluginLog::default();
        let secret = "s".repeat(32);
        // The secret straddles the cut point; it must be masked, not truncated.
        let message = format!("{}{secret}{}", "a".repeat(PLUGIN_LOG_MAX_MESSAGE_BYTES - 20), "b".repeat(100));
        log.record("demo", std::slice::from_ref(&secret), 1, log::Level::Info, &message);
        let entries = log.entries(&LogQuery::default());
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].message.contains("ssss"));
        assert!(entries[0].message.len() <= PLUGIN_LOG_MAX_MESSAGE_BYTES);
    }

    #[test]
    fn record_drops_excess_lines_and_buffers_one_warning_per_window() {
        let log = PluginLog::default();
        let total = PLUGIN_LOG_RATE_LIMIT + 5;
        for i in 0..total {
            log.record("demo", &[], 100, log::Level::Info, &format!("line {i}"));
        }
        assert_eq!(log.entries(&LogQuery::default()).len(), PLUGIN_LOG_RATE_LIMIT as usize);
        assert_eq!(log.dropped_total(), 5);
        log.record("demo", &[], 100 + PLUGIN_LOG_WINDOW_MS, log::Level::Info, "next window");
        let entries = log.entries(&LogQuery { min_level: Some(log::Level::Warn), ..LogQuery::default() });
        assert_eq!(entries.len(), 1);
        assert!(entries[0].message.contains("dropped 5 line(s)"));
    }

    #[test]
    fn ring_evicts_the_oldest_past_capacity() {
        let log = PluginLog::default();
        for i in 0..PLUGIN_LOG_CAPACITY + 10 {
            log.push(i as u64, log::Level::Info, &format!("m{i}"));
        }
        let entries = log.entries(&LogQuery::default());
        assert_eq!(entries.len(), PLUGIN_LOG_CAPACITY);
        assert_eq!(entries[0].message, "m10");
        assert_eq!(entries.last().map(|entry| entry.message.as_str()), Some("m1009"));
    }

    #[test]
    fn entries_filter_by_level_since_and_newest_limit() {
        let log = PluginLog::default();
        log.push(10, log::Level::Debug, "debug");
        log.push(20, log::Level::Info, "info");
        log.push(30, log::Level::Warn, "warn");
        log.push(40, log::Level::Error, "error");

        let messages = |query: &LogQuery| log.entries(query).into_iter().map(|entry| entry.message).collect::<Vec<_>>();
        assert_eq!(
            messages(&LogQuery { min_level: Some(log::Level::Info), ..LogQuery::default() }),
            ["info", "warn", "error"]
        );
        assert_eq!(messages(&LogQuery { since_ms: Some(30), ..LogQuery::default() }), ["warn", "error"]);
        assert_eq!(messages(&LogQuery { limit: Some(2), ..LogQuery::default() }), ["warn", "error"]);
        assert_eq!(
            messages(&LogQuery { min_level: Some(log::Level::Warn), since_ms: Some(0), limit: Some(1) }),
            ["error"]
        );
        assert_eq!(messages(&LogQuery { limit: Some(0), ..LogQuery::default() }), Vec::<String>::new());
    }

    #[test]
    fn registry_creates_one_ring_per_id_and_gets_only_existing_ones() {
        let registry = LogRegistry::default();
        let id: Arc<str> = Arc::from("demo");
        assert!(registry.get("demo").is_none());
        let first = registry.plugin(&id);
        assert!(Arc::ptr_eq(&first, &registry.plugin(&id)));
        assert!(registry.get("demo").is_some_and(|found| Arc::ptr_eq(&found, &first)));
        assert!(registry.get("other").is_none());
    }
}
