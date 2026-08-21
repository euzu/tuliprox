//! Shared recording-window arithmetic.
//!
//! `start_at + duration_secs` is computed in three places — the queue
//! (missed-window detection), the worker (remaining duration), and the
//! service (padded interval). Each site used to guard the overflow its
//! own way (`saturating_add(i64::MAX)`, `checked_add`, an unchecked
//! cast), so "unbounded" meant three different things. This module is
//! the single representation.
//!
//! The chosen representation is *saturating*: a duration that does not
//! fit in `i64`, or a sum that overflows, yields `i64::MAX` — an end
//! instant no wall clock reaches, i.e. an effectively unbounded
//! window. Every caller therefore treats an absurd duration as
//! "still running" rather than as "already elapsed", which is the
//! conservative choice for a recorder.

/// The instant a recording that starts at `start_at` and runs for
/// `duration_secs` ends, saturating at `i64::MAX`.
pub fn recording_end_at(start_at: i64, duration_secs: u64) -> i64 {
    start_at.saturating_add(sat_i64_from_u64(duration_secs))
}

/// Cast `u64` to `i64` with saturation: anything that does not fit
/// (including `u64::MAX`) becomes `i64::MAX`.
///
/// This is the only correct choice for arithmetic on time math: a
/// non-saturating cast would panic, and a saturating cast to a
/// per-site-specific floor (30, 3600, …) would silently change meaning
/// across callers. Use [`recording_math::recording_end_at`],
/// `saturating_sub`, and the like on the result — never an unchecked
/// arithmetic op.
pub fn sat_i64_from_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `true` when `now_ts` is at or past the end of the window.
pub fn window_elapsed(start_at: i64, duration_secs: u64, now_ts: i64) -> bool {
    now_ts >= recording_end_at(start_at, duration_secs)
}

/// Seconds left in the window at `now_ts`.
///
/// - `None` when the window has already elapsed.
/// - The full `duration_secs` when `now_ts` is at or before
///   `start_at` (the recording has not begun yet).
/// - Otherwise the remaining tail of the window.
pub fn remaining_window_secs(start_at: i64, duration_secs: u64, now_ts: i64) -> Option<u64> {
    let end_at = recording_end_at(start_at, duration_secs);
    if now_ts >= end_at {
        return None;
    }
    if now_ts <= start_at {
        return Some(duration_secs);
    }
    u64::try_from(end_at.saturating_sub(now_ts)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_at_adds_duration() {
        assert_eq!(recording_end_at(1_000, 60), 1_060);
    }

    #[test]
    fn sat_i64_from_u64_passes_small_values_through() {
        assert_eq!(sat_i64_from_u64(0), 0);
        assert_eq!(sat_i64_from_u64(60), 60);
        assert_eq!(sat_i64_from_u64(i64::MAX as u64), i64::MAX);
    }

    #[test]
    fn sat_i64_from_u64_saturates_for_overflowing_values() {
        assert_eq!(sat_i64_from_u64(u64::MAX), i64::MAX);
        assert_eq!(sat_i64_from_u64((i64::MAX as u64) + 1), i64::MAX);
    }

    #[test]
    fn end_at_saturates_on_unrepresentable_duration() {
        assert_eq!(recording_end_at(0, u64::MAX), i64::MAX);
        assert_eq!(recording_end_at(i64::MAX, 60), i64::MAX);
    }

    #[test]
    fn unbounded_window_never_elapses() {
        assert!(!window_elapsed(0, u64::MAX, i64::MAX - 1));
        assert!(window_elapsed(1_000, 60, 1_060));
        assert!(!window_elapsed(1_000, 60, 1_059));
    }

    #[test]
    fn remaining_covers_before_during_and_after() {
        assert_eq!(remaining_window_secs(1_000, 60, 900), Some(60));
        assert_eq!(remaining_window_secs(1_000, 60, 1_000), Some(60));
        assert_eq!(remaining_window_secs(1_000, 60, 1_030), Some(30));
        assert_eq!(remaining_window_secs(1_000, 60, 1_060), None);
        assert_eq!(remaining_window_secs(1_000, 60, 2_000), None);
    }
}
