use super::QuietHours;

#[test]
fn a_same_day_window_contains_only_its_own_range() {
    let window = QuietHours::parse("09:00-17:00").expect("parse");
    assert!(!window.contains(8 * 60 + 59));
    assert!(window.contains(9 * 60));
    assert!(window.contains(16 * 60 + 59));
    // End is exclusive, so 17:00 sharp is out.
    assert!(!window.contains(17 * 60));
}

#[test]
fn an_overnight_window_wraps_past_midnight() {
    // The case people actually configure.
    let window = QuietHours::parse("23:00-07:00").expect("parse");
    assert!(window.contains(23 * 60));
    assert!(window.contains(0));
    assert!(window.contains(6 * 60 + 59));
    assert!(!window.contains(7 * 60));
    assert!(!window.contains(12 * 60));
}

#[test]
fn a_zero_width_window_silences_nothing() {
    // Treating it as "always" would mute the channel entirely on a typo.
    let window = QuietHours::parse("08:00-08:00").expect("parse");
    for minute in [0, 8 * 60, 12 * 60, 23 * 60 + 59] {
        assert!(!window.contains(minute), "zero-width window muted {minute}");
    }
}

#[test]
fn minutes_until_end_handles_both_directions() {
    let same_day = QuietHours::parse("09:00-17:00").expect("parse");
    assert_eq!(same_day.minutes_until_end(10 * 60), 7 * 60);
    assert_eq!(same_day.minutes_until_end(18 * 60), 0, "outside the window there is nothing to wait for");

    let overnight = QuietHours::parse("23:00-07:00").expect("parse");
    assert_eq!(overnight.minutes_until_end(23 * 60), 8 * 60);
    assert_eq!(overnight.minutes_until_end(60), 6 * 60);
}

#[test]
fn malformed_windows_are_rejected_rather_than_guessed() {
    for bad in ["", "23:00", "25:00-07:00", "23:60-07:00", "23-07", "abc-def", "23:00_07:00"] {
        assert!(QuietHours::parse(bad).is_none(), "accepted malformed window `{bad}`");
    }
}

#[test]
fn whitespace_is_tolerated() {
    assert_eq!(QuietHours::parse(" 23:00 - 07:00 "), QuietHours::parse("23:00-07:00"));
}
