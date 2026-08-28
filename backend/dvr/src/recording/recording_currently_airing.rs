//! Currently-airing recording window helper.
//!
//! A recording whose padded start is already in the past when the
//! request reaches the service is a *currently airing* programme:
//! - preserve `scheduled_start` for display and conflict history;
//! - start at `max(now, scheduled_start)`;
//! - keep `scheduled_end` unchanged;
//! - reserve quota only for the remaining effective duration;
//! - reject when `now >= scheduled_end`.
//!
//! This module is the pure core: no I/O, no service state, no time
//! side effects. The wiring into `RecordingService::create_recording`
//! and the EPG action is the caller's responsibility.
//!
//! The helpers are tested in isolation. They are public so the
//! service and the EPG action can call them once the wiring lands;
//! the `dead_code` allowance below covers the test surface.

/// The effective window used by the runtime when the user submits a
/// recording for a currently-airing programme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentlyAiringWindow {
    /// Always `scheduled_start`. The display value never advances;
    /// the runtime starts at `start_at = max(now, scheduled_start)`.
    pub scheduled_start: i64,
    /// Unchanged from the caller's padded end.
    pub scheduled_end: i64,
    /// `max(now, scheduled_start)`. The runtime starts here.
    pub start_at: i64,
    /// `max(0, scheduled_end - start_at)`. The quota charge and the
    /// filename template both use this.
    pub remaining_duration_secs: u64,
}

/// Pure: classify a candidate against `now`.
///
/// `scheduled_start` and `scheduled_end` are the *padded* interval
/// (i.e. `program_start - pre_roll`, `program_end + post_roll`). `now`
/// is the current server time in Unix seconds. When the padded end
/// has already passed, `start_at` is clamped to `scheduled_end` so
/// the helper still returns a sane value; the caller checks
/// `is_window_elapsed` to decide whether to reject.
pub fn resolve_window(scheduled_start: i64, scheduled_end: i64, now: i64) -> CurrentlyAiringWindow {
    let start_at = now.max(scheduled_start).min(scheduled_end);
    let remaining = (scheduled_end - start_at).max(0);
    CurrentlyAiringWindow {
        scheduled_start,
        scheduled_end,
        start_at,
        remaining_duration_secs: remaining.unsigned_abs(),
    }
}

/// `true` when the candidate's padded end has already passed, i.e.
/// there is no remaining window to record.
pub fn is_window_elapsed(scheduled_end: i64, now: i64) -> bool {
    now >= scheduled_end
}

/// Total remaining quota bytes for the candidate. Mirrors the
/// per-minute fallback the quota ledger uses. Callers may pass `None`
/// when the bitrate is unknown; the helper returns a conservative
/// estimate.
pub fn estimate_remaining_bytes(
    remaining_duration_secs: u64,
    known_bitrate_bps: Option<u64>,
    fallback_bytes_per_minute: u64,
) -> u64 {
    if remaining_duration_secs == 0 {
        return 0;
    }
    if let Some(bps) = known_bitrate_bps {
        // `bytes = ceil(duration_secs * bps / 8)` — but we saturate on
        // overflow so a pathologically large duration cannot panic.
        let numerator = u128::from(remaining_duration_secs).saturating_mul(u128::from(bps));
        u64::try_from(numerator.div_ceil(8)).unwrap_or(u64::MAX)
    } else {
        let minutes = remaining_duration_secs.div_ceil(60);
        minutes.saturating_mul(fallback_bytes_per_minute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_window_does_not_advance_scheduled_start() {
        let w = resolve_window(1_000, 2_000, 500);
        assert_eq!(w.scheduled_start, 1_000);
        assert_eq!(w.start_at, 1_000);
        assert_eq!(w.scheduled_end, 2_000);
        assert_eq!(w.remaining_duration_secs, 1_000);
    }

    #[test]
    fn resolve_window_starts_at_now_when_already_airing() {
        let w = resolve_window(1_000, 2_000, 1_500);
        assert_eq!(w.scheduled_start, 1_000);
        assert_eq!(w.start_at, 1_500);
        assert_eq!(w.scheduled_end, 2_000);
        assert_eq!(w.remaining_duration_secs, 500);
    }

    #[test]
    fn resolve_window_clamps_to_zero_when_fully_elapsed() {
        let w = resolve_window(1_000, 2_000, 2_500);
        assert_eq!(w.start_at, 2_000);
        assert_eq!(w.remaining_duration_secs, 0);
    }

    #[test]
    fn is_window_elapsed_only_when_now_meets_or_passes_scheduled_end() {
        assert!(!is_window_elapsed(2_000, 1_999));
        assert!(is_window_elapsed(2_000, 2_000));
        assert!(is_window_elapsed(2_000, 2_001));
    }

    #[test]
    fn estimate_remaining_bytes_uses_known_bitrate() {
        // 60s * 8_000_000 bps = 60_000_000 bytes.
        let bytes = estimate_remaining_bytes(60, Some(8_000_000), 8_388_608);
        assert_eq!(bytes, 60_000_000);
    }

    #[test]
    fn estimate_remaining_bytes_uses_fallback_when_bitrate_unknown() {
        // 120s -> 2 minutes -> 2 * 8 MiB.
        let bytes = estimate_remaining_bytes(120, None, 8 * 1_048_576);
        assert_eq!(bytes, 16 * 1_048_576);
    }

    #[test]
    fn estimate_remaining_bytes_rounds_up_partial_minutes() {
        // 61s -> 2 minutes (1 minute + 1 second round up).
        let bytes = estimate_remaining_bytes(61, None, 60);
        assert_eq!(bytes, 120);
    }

    #[test]
    fn estimate_remaining_bytes_zero_when_remaining_is_zero() {
        assert_eq!(estimate_remaining_bytes(0, Some(8_000_000), 8), 0);
        assert_eq!(estimate_remaining_bytes(0, None, 8), 0);
    }

    #[test]
    fn estimate_remaining_bytes_saturates_on_overflow() {
        // u64::MAX seconds, huge bitrate — must not panic.
        let _ = estimate_remaining_bytes(u64::MAX, Some(u64::MAX), u64::MAX);
    }
}
