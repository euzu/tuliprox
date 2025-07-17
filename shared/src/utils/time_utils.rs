use std::time::{SystemTime, UNIX_EPOCH};
use chrono::{DateTime};

pub fn current_time_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn unix_ts_to_str(ts: i64) -> Option<String> {
    if ts > 0 {
        let normalized_ts = if ts > 4102444800 {
            ts / 1000
        } else {
            ts
        };
        DateTime::from_timestamp(normalized_ts, 0).map(|dt| dt.format("%d.%m.%Y").to_string())
    } else {
        None
    }
}