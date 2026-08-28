pub fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

pub fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m {}s", seconds % 60)
    }
}

/// Shared body of `format_bandwidth` and `format_transferred`.
/// `gb_decimals` controls the precision used in the GB range; `unit_suffix`
/// is appended to every unit (e.g. `/s` for bandwidth, empty for transferred).
fn format_rate_unit(value_kb: u32, gb_decimals: usize, unit_suffix: &str) -> String {
    if value_kb == 0 {
        return "-".to_string();
    }
    if value_kb >= 1_048_576 {
        // Precision is dynamic, so the format spec is built at runtime.
        let scaled = f64::from(value_kb) / 1_048_576.0;
        match gb_decimals {
            1 => format!("{scaled:.1} GB{unit_suffix}"),
            2 => format!("{scaled:.2} GB{unit_suffix}"),
            _ => format!("{scaled} GB{unit_suffix}"),
        }
    } else if value_kb >= 1024 {
        format!("{:.1} MB{unit_suffix}", f64::from(value_kb) / 1024.0)
    } else {
        format!("{value_kb} KB{unit_suffix}")
    }
}

#[inline]
pub fn format_bandwidth(rate_kbps: u32) -> String {
    format_rate_unit(rate_kbps, 1, "/s")
}

#[inline]
pub fn format_transferred(total_kb: u32) -> String {
    format_rate_unit(total_kb, 2, "")
}

/// Format a UTC unix timestamp as "YYYY-MM-DD HH:MM:SS" in the browser's local timezone.
pub fn format_ts(ts: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64 * 1000.0));
    if !date.get_time().is_finite() {
        return ts.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date(),
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds()
    )
}

/// Convert a calendar date encoded as a UTC-midnight timestamp to the matching
/// local day boundary, then serialize that instant as UTC for the backend.
pub fn format_local_day_boundary_utc(ts: i64, end_of_day: bool) -> String {
    let Some(date) = chrono::DateTime::from_timestamp(ts, 0).map(|dt| dt.date_naive()) else {
        return String::new();
    };

    let Ok(year) = u32::try_from(chrono::Datelike::year(&date)) else {
        return String::new();
    };
    let (hour, minute, second) = if end_of_day { (23, 59, 59) } else { (0, 0, 0) };
    let local = js_sys::Date::new_with_year_month_day_hr_min_sec(
        year,
        chrono::Datelike::month0(&date).cast_signed(),
        chrono::Datelike::day(&date).cast_signed(),
        hour,
        minute,
        second,
    );
    let utc_millis = local.get_time();
    if !utc_millis.is_finite() {
        return String::new();
    }
    let utc_ts = (utc_millis / 1000.0) as i64;
    chrono::DateTime::from_timestamp(utc_ts, 0)
        .map_or_else(String::new, |dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use super::format_local_day_boundary_utc;
    use super::{format_bandwidth, format_transferred};
    #[cfg(target_arch = "wasm32")]
    use chrono::TimeZone;

    fn format_local_with_offset(ts: i64, js_offset_minutes_west: i32, fmt: &str) -> String {
        let utc = match chrono::DateTime::from_timestamp(ts, 0) {
            Some(dt) => dt,
            None => return ts.to_string(),
        };
        let east_secs = -i64::from(js_offset_minutes_west) * 60;
        match i32::try_from(east_secs).ok().and_then(chrono::FixedOffset::east_opt) {
            Some(offset) => utc.with_timezone(&offset).format(fmt).to_string(),
            None => utc.format(fmt).to_string(),
        }
    }

    #[test]
    fn format_bandwidth_below_kb_threshold_uses_kbps() {
        assert_eq!(format_bandwidth(1), "1 KB/s");
        assert_eq!(format_bandwidth(123), "123 KB/s");
        assert_eq!(format_bandwidth(1023), "1023 KB/s");
    }

    #[test]
    fn format_bandwidth_mbps_range_uses_one_decimal() {
        assert_eq!(format_bandwidth(1024), "1.0 MB/s");
        assert_eq!(format_bandwidth(15_360), "15.0 MB/s");
        assert_eq!(format_bandwidth(1_048_575), "1024.0 MB/s");
    }

    #[test]
    fn format_bandwidth_gbps_range_uses_one_decimal_and_per_second_suffix() {
        assert_eq!(format_bandwidth(1_048_576), "1.0 GB/s");
        assert_eq!(format_bandwidth(5_242_880), "5.0 GB/s");
    }

    #[test]
    fn format_bandwidth_zero_shows_dash() {
        assert_eq!(format_bandwidth(0), "-");
    }

    #[test]
    fn format_transferred_below_kb_threshold_uses_kb() {
        assert_eq!(format_transferred(1), "1 KB");
        assert_eq!(format_transferred(123), "123 KB");
        assert_eq!(format_transferred(1023), "1023 KB");
    }

    #[test]
    fn format_transferred_mbps_range_uses_one_decimal() {
        assert_eq!(format_transferred(1024), "1.0 MB");
        assert_eq!(format_transferred(1_048_575), "1024.0 MB");
    }

    #[test]
    fn format_transferred_gb_range_uses_two_decimals() {
        assert_eq!(format_transferred(1_048_576), "1.00 GB");
        assert_eq!(format_transferred(2_621_440), "2.50 GB");
        assert_eq!(format_transferred(10_485_760), "10.00 GB");
    }

    #[test]
    fn format_transferred_zero_shows_dash() {
        assert_eq!(format_transferred(0), "-");
    }

    // 2026-04-12 12:30:45 UTC
    fn ref_ts() -> i64 {
        utc_ts(2026, 4, 12, 12, 30, 45)
    }

    fn utc_ts(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_opt(h, m, s))
            .map(|dt| dt.and_utc().timestamp())
            .expect("test timestamp must be a valid date")
    }

    #[test]
    fn format_local_with_offset_utc_passes_through_unchanged() {
        // 0 minutes west of UTC = UTC
        assert_eq!(format_local_with_offset(ref_ts(), 0, "%Y-%m-%d %H:%M:%S"), "2026-04-12 12:30:45");
    }

    #[test]
    fn format_local_with_offset_cest_shifts_two_hours_east() {
        // Berlin in summer: getTimezoneOffset returns -120 (120 min west), so
        // local is 2h ahead of UTC.
        assert_eq!(format_local_with_offset(ref_ts(), -120, "%Y-%m-%d %H:%M:%S"), "2026-04-12 14:30:45");
    }

    #[test]
    fn format_local_with_offset_est_shifts_five_hours_west() {
        // New York in winter: getTimezoneOffset returns +300 (300 min west),
        // so local is 5h behind UTC.
        assert_eq!(format_local_with_offset(ref_ts(), 300, "%Y-%m-%d %H:%M:%S"), "2026-04-12 07:30:45");
    }

    #[test]
    fn format_local_with_offset_handles_date_rollover_east() {
        // 23:30 UTC + 2h east rolls over to the next local day.
        let late_ts = utc_ts(2026, 4, 12, 23, 30, 0);
        assert_eq!(format_local_with_offset(late_ts, -120, "%Y-%m-%d %H:%M:%S"), "2026-04-13 01:30:00");
    }

    #[test]
    fn format_local_with_offset_handles_date_rollover_west() {
        // 01:30 UTC - 5h west rolls back to the previous local day.
        let early_ts = utc_ts(2026, 4, 12, 1, 30, 0);
        assert_eq!(format_local_with_offset(early_ts, 300, "%Y-%m-%d %H:%M:%S"), "2026-04-11 20:30:00");
    }

    #[test]
    fn format_local_with_offset_falls_back_to_utc_on_out_of_range_offset() {
        // An offset that overflows i32 should fall back to UTC, not panic.
        assert_eq!(format_local_with_offset(ref_ts(), i32::MAX, "%Y-%m-%d %H:%M:%S"), "2026-04-12 12:30:45");
        // Likewise, an offset that fits in i32 but is outside chrono's ±14h
        // window falls back to UTC.
        assert_eq!(format_local_with_offset(ref_ts(), 60 * 24, "%Y-%m-%d %H:%M:%S"), "2026-04-12 12:30:45");
    }

    #[test]
    fn format_local_with_offset_invalid_timestamp_returns_raw_int() {
        // 0 is a valid timestamp (1970-01-01), so use a clearly out-of-range value.
        let bad = i64::MIN / 2;
        let out = format_local_with_offset(bad, 0, "%Y-%m-%d %H:%M:%S");
        assert_eq!(out, bad.to_string());
    }

    #[test]
    fn format_local_with_offset_date_only_format() {
        assert_eq!(format_local_with_offset(ref_ts(), -120, "%Y-%m-%d"), "2026-04-12");
        // Late-evening UTC becomes the next local day in CEST.
        let late_ts = utc_ts(2026, 4, 12, 23, 30, 0);
        assert_eq!(format_local_with_offset(late_ts, -120, "%Y-%m-%d"), "2026-04-13");
    }

    #[test]
    #[cfg(target_arch = "wasm32")]
    fn format_local_day_boundary_utc_uses_calendar_date() {
        let date_key = utc_ts(2026, 3, 23, 0, 0, 0);
        // The function extracts the calendar date from the UTC timestamp, then
        // computes the local day boundary for that date and serializes it as UTC.
        let utc = chrono::DateTime::from_timestamp(date_key, 0).expect("valid timestamp");
        let local_date = utc.with_timezone(&chrono::Local).date_naive();
        let start_local = local_date
            .and_hms_opt(0, 0, 0)
            .and_then(|ndt| chrono::Local.from_local_datetime(&ndt).earliest())
            .expect("valid local datetime");
        let end_local = local_date
            .and_hms_opt(23, 59, 59)
            .and_then(|ndt| chrono::Local.from_local_datetime(&ndt).earliest())
            .expect("valid local datetime");
        assert_eq!(
            format_local_day_boundary_utc(date_key, false),
            start_local.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S").to_string()
        );
        assert_eq!(
            format_local_day_boundary_utc(date_key, true),
            end_local.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S").to_string()
        );
    }
}
