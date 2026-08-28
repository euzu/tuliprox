//! Resource-monitoring defaults (warn/critical percentages, repeat interval).

pub fn default_warn_percent() -> f64 {
    80.0
}
pub fn default_critical_percent() -> f64 {
    95.0
}
pub fn default_repeat_interval_secs() -> u64 {
    3600
}
