#[cfg(target_arch = "wasm32")]
pub fn current_time_secs() -> u64 { (js_sys::Date::now() / 1000.0) as u64 }

#[cfg(not(target_arch = "wasm32"))]
pub fn current_time_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

pub fn unix_ts_to_str(ts: i64) -> Option<String> { unix_ts_to_str_with_format(ts, "%Y-%m-%d %H:%M:%S") }

/// Parse a duration string into seconds.
///
/// Supported formats:
/// - plain seconds (`"30"`) when `require_unit` is `false`
/// - suffixed units: `s`, `m`, `h`, `d` (for example `"30s"`, `"5m"`, `"1h"`, `"2d"`)
pub fn parse_duration_seconds(value: &str, require_unit: bool) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(seconds) = value.parse::<u64>() {
        return if require_unit { None } else { Some(seconds) };
    }

    if value.len() <= 1 {
        return None;
    }

    // split_at panics on non-char boundaries; a multi-byte trailing char is never a valid unit
    let split_pos = value.len() - 1;
    if !value.is_char_boundary(split_pos) {
        return None;
    }
    let (number_part, unit_part) = value.split_at(split_pos);
    let number = number_part.parse::<u64>().ok()?;
    match unit_part {
        "s" => Some(number),
        "m" => number.checked_mul(60),
        "h" => number.checked_mul(60 * 60),
        "d" => number.checked_mul(24 * 60 * 60),
        _ => None,
    }
}

/// Parse a duration string into milliseconds.
///
/// Accepts the same grammar as [`parse_duration_seconds`], plus an `ms` suffix. A plain
/// number without a unit (`"30"`) still means seconds when `require_unit` is `false`, so the
/// same config value yields the same duration from either parser.
///
/// Supported formats:
/// - plain seconds (`"30"`) when `require_unit` is `false`
/// - suffixed units: `ms`, `s`, `m`, `h`, `d` (for example `"250ms"`, `"30s"`, `"5m"`)
///
/// Returns `None` on empty or malformed input, or when the result overflows `u64`.
pub fn parse_duration_millis(value: &str, require_unit: bool) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(seconds) = value.parse::<u64>() {
        return if require_unit { None } else { seconds.checked_mul(1_000) };
    }

    if let Some(millis) = value.strip_suffix("ms") {
        return millis.parse::<u64>().ok();
    }

    parse_duration_seconds(value, true)?.checked_mul(1_000)
}

fn normalize_ts(ts: i64) -> Option<i64> {
    if ts >= 0 {
        // Timestamps > Jan 1, 2100 (in seconds) are assumed to be in milliseconds
        if ts > 4_102_444_800 {
            Some(ts / 1000)
        } else {
            Some(ts)
        }
    } else {
        None
    }
}

// Note: On wasm32, the `format` parameter is ignored; output is always `YYYY-MM-DD HH:MM:SS`.
#[cfg(target_arch = "wasm32")]
pub fn unix_ts_to_str_with_format(ts: i64, _format: &str) -> Option<String> {
    let normalized_ts = normalize_ts(ts)?;
    let date = js_sys::Date::new_0();
    date.set_time(normalized_ts as f64 * 1000.0);

    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date(),
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds()
    ))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn unix_ts_to_str_with_format(ts: i64, format: &str) -> Option<String> {
    let normalized_ts = normalize_ts(ts)?;
    chrono::DateTime::from_timestamp(normalized_ts, 0).map(|dt| dt.format(format).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_parser_accepts_ms_suffix_and_trims_whitespace() {
        assert_eq!(parse_duration_millis("250ms", false), Some(250));
        assert_eq!(parse_duration_millis("250ms", true), Some(250));
        assert_eq!(parse_duration_millis("0ms", true), Some(0));
        assert_eq!(parse_duration_millis("  250ms  ", true), Some(250));
    }

    #[test]
    fn millis_parser_treats_plain_numbers_as_seconds_like_the_seconds_parser() {
        assert_eq!(parse_duration_millis("30", false), Some(30_000));
        assert_eq!(parse_duration_millis("0", false), Some(0));
    }

    #[test]
    fn millis_parser_handles_mixed_units_consistently() {
        assert_eq!(parse_duration_millis("45s", true), Some(45_000));
        assert_eq!(parse_duration_millis("2m", true), Some(120_000));
        assert_eq!(parse_duration_millis("1h", true), Some(3_600_000));
        assert_eq!(parse_duration_millis("1d", true), Some(86_400_000));
        assert_eq!(parse_duration_millis("1s", true), parse_duration_millis("1000ms", true));
        assert_eq!(parse_duration_millis("1m30s", true), None, "compound durations are not supported");
    }

    #[test]
    fn millis_parser_agrees_with_seconds_parser_on_shared_grammar() {
        for input in ["0", "30", "5s", "5m", "2h", "1d", "garbage", ""] {
            for require_unit in [false, true] {
                assert_eq!(
                    parse_duration_millis(input, require_unit),
                    parse_duration_seconds(input, require_unit).map(|s| s * 1_000),
                    "input {input:?}, require_unit {require_unit}"
                );
            }
        }
        // `ms` is the only grammar difference: the seconds parser rejects it, the millis parser takes it.
        assert_eq!(parse_duration_seconds("5ms", true), None);
        assert_eq!(parse_duration_millis("5ms", true), Some(5));
    }

    #[test]
    fn seconds_parser_is_unchanged_and_still_rejects_ms() {
        assert_eq!(parse_duration_seconds("250ms", false), None);
        assert_eq!(parse_duration_seconds("250ms", true), None);
        assert_eq!(parse_duration_seconds("5ms", true), None);
        assert_eq!(parse_duration_seconds("5s", true), Some(5));
        assert_eq!(parse_duration_seconds("30", false), Some(30));
    }

    #[test]
    fn missing_unit_is_rejected_when_unit_is_required() {
        assert_eq!(parse_duration_millis("250", true), None);
        assert_eq!(parse_duration_millis("0", true), None);
        assert_eq!(parse_duration_millis("5", false), Some(5_000));
    }

    #[test]
    fn millis_parser_reports_overflow_as_none_instead_of_wrapping() {
        // u64::MAX milliseconds fits exactly.
        assert_eq!(parse_duration_millis("18446744073709551615ms", true), Some(u64::MAX));
        // One more does not fit in u64 at all.
        assert_eq!(parse_duration_millis("18446744073709551616ms", true), None);
        // Seconds scaled to milliseconds overflow even though the seconds value fits.
        assert_eq!(parse_duration_millis("18446744073709551s", true), Some(18_446_744_073_709_551_000));
        assert_eq!(parse_duration_millis("18446744073709552s", true), None);
        assert_eq!(parse_duration_millis("18446744073709551", false), Some(18_446_744_073_709_551_000));
        assert_eq!(parse_duration_millis("18446744073709552", false), None);
    }

    #[test]
    fn millis_parser_rejects_garbage_input() {
        for input in [
            "", "   ", "ms", "s", "m", "abc", "5x", "-5", "5.5s", "1.5m", "5 ms", "5 s", "5mss", "ms5", "x5ms",
            "1m30s", "é", "5é",
        ] {
            assert_eq!(parse_duration_millis(input, false), None, "input {input:?} should be rejected");
            assert_eq!(parse_duration_millis(input, true), None, "input {input:?} should be rejected");
        }
    }
}
