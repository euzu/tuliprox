pub fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
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
pub fn format_bandwidth(rate_kbps: u32) -> String { format_rate_unit(rate_kbps, 1, "/s") }

#[inline]
pub fn format_transferred(total_kb: u32) -> String { format_rate_unit(total_kb, 2, "") }

pub fn format_ts(ts: u64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map_or_else(|| ts.to_string(), |dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
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
    use super::{format_bandwidth, format_transferred};

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
}
