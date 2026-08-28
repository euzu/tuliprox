//! Metadata-update defaults: paths, queue/log intervals, probe/resolve backoff,
//! queue/backoff/attempt knobs, ffprobe sizes & durations.

use crate::model::ByteSize;

pub const DEFAULT_METADATA_PATH: &str = "metadata";
pub fn default_metadata_path() -> String {
    DEFAULT_METADATA_PATH.to_string()
}
pub fn is_default_metadata_path(s: &str) -> bool {
    s == DEFAULT_METADATA_PATH
}

// All queue/log/cooldown/retry-duration defaults are human-readable strings
// (e.g. "30s", "1h", "7d"); the macro emits a non-allocating comparison via
// the cached `&'static str` returned by the default arm.
default_eq_fns!(
    default_metadata_queue_log_interval, is_default_metadata_queue_log_interval, str, "30s";
    default_metadata_progress_log_interval, is_default_metadata_progress_log_interval, str, "15s";
    default_metadata_max_resolve_retry_backoff, is_default_metadata_max_resolve_retry_backoff, str, "1h";
    default_metadata_resolve_min_retry_base, is_default_metadata_resolve_min_retry_base, str, "5s";
    default_metadata_resolve_exhaustion_reset_gap, is_default_metadata_resolve_exhaustion_reset_gap, str, "1h";
    default_metadata_probe_cooldown, is_default_metadata_probe_cooldown, str, "7d";
    default_metadata_retry_delay, is_default_metadata_retry_delay, str, "2s";
    default_metadata_probe_retry_load_retry_delay, is_default_metadata_probe_retry_load_retry_delay, str, "1m";
    default_metadata_worker_idle_timeout, is_default_metadata_worker_idle_timeout, str, "1m";
    default_metadata_probe_retry_backoff_step_1, is_default_metadata_probe_retry_backoff_step_1, str, "10m";
    default_metadata_probe_retry_backoff_step_2, is_default_metadata_probe_retry_backoff_step_2, str, "30m";
    default_metadata_probe_retry_backoff_step_3, is_default_metadata_probe_retry_backoff_step_3, str, "1h";
);

default_eq_fns!(
    default_metadata_max_attempts_resolve, is_default_metadata_max_attempts_resolve, u8, 3;
    default_metadata_max_attempts_probe, is_default_metadata_max_attempts_probe, u8, 3;
    default_metadata_backoff_jitter_percent, is_default_metadata_backoff_jitter_percent, u8, 20;
    default_metadata_max_queue_size, is_default_metadata_max_queue_size, usize, 100_000;
    default_metadata_no_change_cache_ttl_secs, is_default_metadata_no_change_cache_ttl_secs, u64, 3600;
    default_metadata_probe_fairness_resolve_burst, is_default_metadata_probe_fairness_resolve_burst, usize, 200;
);

default_eq_fns!(
    default_metadata_ffprobe_analyze_duration, is_default_metadata_ffprobe_analyze_duration, str, "10s";
    default_metadata_ffprobe_live_analyze_duration, is_default_metadata_ffprobe_live_analyze_duration, str, "5s";
);

// `ByteSize` defaults — non-numeric, non-`str`, kept as manual impls. Keep
// them minimal; if a third `ByteSize` default appears, extend the macro with
// a `byte_size` arm instead of repeating this pattern.
pub fn default_metadata_ffprobe_probe_size() -> ByteSize {
    ByteSize::new("10MB")
}
pub fn is_default_metadata_ffprobe_probe_size(v: &ByteSize) -> bool {
    v == &default_metadata_ffprobe_probe_size()
}
pub fn default_metadata_ffprobe_live_probe_size() -> ByteSize {
    ByteSize::new("5MB")
}
pub fn is_default_metadata_ffprobe_live_probe_size(v: &ByteSize) -> bool {
    v == &default_metadata_ffprobe_live_probe_size()
}

pub const fn default_probe_user_priority() -> i8 {
    127
}
pub const fn is_default_probe_user_priority(v: &i8) -> bool {
    *v == default_probe_user_priority()
}
pub const fn default_user_priority() -> i8 {
    0
}
pub const fn is_default_user_priority(v: &i8) -> bool {
    *v == default_user_priority()
}

pub fn is_none_or_empty_metadata_update(metadata_update: &Option<crate::model::MetadataUpdateConfigDto>) -> bool {
    metadata_update.as_ref().is_none_or(crate::model::MetadataUpdateConfigDto::is_empty)
}
