use super::PROCESS_START;
use crate::BUILD_TIMESTAMP;
use chrono::{DateTime, Utc};

pub fn get_server_time() -> String {
    chrono::offset::Local::now().with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %Z").to_string()
}

/// Anchors the uptime clock; call once at process startup.
pub fn init_uptime_clock() { let _ = *PROCESS_START; }

pub fn get_uptime_secs() -> u64 { PROCESS_START.elapsed().as_secs() }

pub fn get_build_time() -> Option<String> {
    BUILD_TIMESTAMP
        .to_string()
        .parse::<DateTime<Utc>>()
        .ok()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string())
}
