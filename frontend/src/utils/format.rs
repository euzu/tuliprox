pub fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

pub fn format_bandwidth(rate_kbps: u32) -> String {
    if rate_kbps == 0 {
        return "-".to_string();
    }
    if rate_kbps >= 1_048_576 {
        format!("{:.1} GB/s", f64::from(rate_kbps) / 1_048_576.0)
    } else if rate_kbps >= 1024 {
        format!("{:.1} MB/s", f64::from(rate_kbps) / 1024.0)
    } else {
        format!("{rate_kbps} KB/s")
    }
}

pub fn format_transferred(total_kb: u32) -> String {
    if total_kb == 0 {
        return "-".to_string();
    }
    if total_kb >= 1_048_576 {
        format!("{:.2} GB", f64::from(total_kb) / 1_048_576.0)
    } else if total_kb >= 1024 {
        format!("{:.1} MB", f64::from(total_kb) / 1024.0)
    } else {
        format!("{total_kb} KB")
    }
}

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
