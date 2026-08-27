use std::time::{SystemTime, UNIX_EPOCH};

pub const SECS_PER_DAY: u64 = 86_400;

pub fn now_utc_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }

pub fn utc_day_from_secs(ts_secs: u64) -> String {
    chrono::DateTime::from_timestamp_secs(i64::try_from(ts_secs).unwrap_or(0))
        .map_or_else(|| "1970-01-01".to_string(), |dt| dt.format("%Y-%m-%d").to_string())
}

pub fn current_utc_day() -> String { utc_day_from_secs(now_utc_secs()) }

/// Compute Unix seconds until the next UTC midnight after `from_secs`.
pub fn secs_until_next_utc_midnight(from_secs: u64) -> u64 { SECS_PER_DAY - (from_secs % SECS_PER_DAY) }
