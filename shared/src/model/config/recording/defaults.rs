use super::RecordingContainerFormat;
use crate::defaults::F64_DEFAULT_EPSILON;

pub(super) const fn default_retry_backoff_initial_secs() -> u64 { 3 }

pub(super) const fn default_retry_backoff_multiplier() -> f64 { 3.0 }

pub(super) const fn default_retry_backoff_max_secs() -> u64 { 30 }

pub(super) const fn default_retry_backoff_jitter_percent() -> u8 { 20 }

pub(super) const fn default_retry_max_attempts() -> u8 { 5 }

pub(super) fn is_default_retry_backoff_initial_secs(value: &u64) -> bool {
    *value == default_retry_backoff_initial_secs()
}

pub(super) fn is_default_retry_backoff_multiplier(value: &f64) -> bool {
    (*value - default_retry_backoff_multiplier()).abs() < F64_DEFAULT_EPSILON
}

pub(super) fn is_default_retry_backoff_max_secs(value: &u64) -> bool { *value == default_retry_backoff_max_secs() }

pub(super) fn is_default_retry_backoff_jitter_percent(value: &u8) -> bool {
    *value == default_retry_backoff_jitter_percent()
}

pub(super) fn is_default_retry_max_attempts(value: &u8) -> bool { *value == default_retry_max_attempts() }

pub(super) fn is_zero_u8(value: &u8) -> bool { *value == 0 }

pub(super) fn is_zero_i8(value: &i8) -> bool { *value == 0 }

// Recording configuration constants.
const DEFAULT_RECORDING_DIRECTORY_SUFFIX: &str = "recordings";

const DEFAULT_RECORDING_TIMEZONE: &str = "UTC";

const DEFAULT_RECORDING_FILENAME_TEMPLATE: &str = "{channel}_{program_title}_{start_time}";

const DEFAULT_RECORDING_MAX_PRE_ROLL_SECS: u64 = 15 * 60;

// 15 minutes
const DEFAULT_RECORDING_MAX_POST_ROLL_SECS: u64 = 30 * 60;

// 30 minutes
const DEFAULT_RECORDING_CLEANUP_INTERVAL_SECS: u64 = 60 * 60;

// 1 hour
const DEFAULT_RECORDING_DISK_SAFETY_BYTES: u64 = 1024 * 1024 * 1024;

// 1 GiB
const DEFAULT_RECORDING_FALLBACK_BYTES_PER_MINUTE: u64 = 8 * 1024 * 1024;

// 8 MiB
const DEFAULT_RECORDING_RETENTION_SWEEP_INTERVAL_SECS: u64 = 60 * 60;

// 1 hour
const DEFAULT_RECORDING_NOTIFICATION_OUTBOX_BUFFER: usize = 1024;

const DEFAULT_RECORDING_NOTIFICATION_MAX_ATTEMPTS: u32 = 6;

const DEFAULT_RECORDING_NOTIFICATION_BACKOFF_INITIAL_SECS: u64 = 5;

const DEFAULT_RECORDING_NOTIFICATION_BACKOFF_MAX_SECS: u64 = 900;

// 15 minutes
pub(super) const MAX_FILENAME_TEMPLATE_BYTES: usize = 240;

pub fn default_recording_directory(download_dir: &str) -> String {
    format!("{download_dir}/{DEFAULT_RECORDING_DIRECTORY_SUFFIX}")
}

pub fn default_recording_timezone() -> String { DEFAULT_RECORDING_TIMEZONE.to_string() }

pub fn default_recording_filename_template() -> String { DEFAULT_RECORDING_FILENAME_TEMPLATE.to_string() }

pub const fn default_recording_max_pre_roll_secs() -> u64 { DEFAULT_RECORDING_MAX_PRE_ROLL_SECS }

pub const fn default_recording_max_post_roll_secs() -> u64 { DEFAULT_RECORDING_MAX_POST_ROLL_SECS }

pub const fn default_recording_cleanup_interval_secs() -> u64 { DEFAULT_RECORDING_CLEANUP_INTERVAL_SECS }

pub const fn default_recording_disk_safety_bytes() -> u64 { DEFAULT_RECORDING_DISK_SAFETY_BYTES }

pub const fn default_recording_fallback_bytes_per_minute() -> u64 { DEFAULT_RECORDING_FALLBACK_BYTES_PER_MINUTE }

pub(super) fn is_default_recording_max_pre_roll_secs(value: &u64) -> bool {
    *value == default_recording_max_pre_roll_secs()
}

pub(super) fn is_default_recording_max_post_roll_secs(value: &u64) -> bool {
    *value == default_recording_max_post_roll_secs()
}

pub const fn default_recording_retention_sweep_interval_secs() -> u64 {
    DEFAULT_RECORDING_RETENTION_SWEEP_INTERVAL_SECS
}

pub const fn default_recording_notification_outbox_buffer() -> usize { DEFAULT_RECORDING_NOTIFICATION_OUTBOX_BUFFER }

pub const fn default_recording_notification_max_attempts() -> u32 { DEFAULT_RECORDING_NOTIFICATION_MAX_ATTEMPTS }

pub const fn default_recording_notification_backoff_initial_secs() -> u64 {
    DEFAULT_RECORDING_NOTIFICATION_BACKOFF_INITIAL_SECS
}

pub const fn default_recording_notification_backoff_max_secs() -> u64 {
    DEFAULT_RECORDING_NOTIFICATION_BACKOFF_MAX_SECS
}

pub const fn default_recording_enabled() -> bool { true }

pub(super) fn is_default_recording_fallback_bytes_per_minute(value: &u64) -> bool {
    *value == default_recording_fallback_bytes_per_minute()
}

pub(super) fn is_default_recording_retention_sweep_interval_secs(value: &u64) -> bool {
    *value == default_recording_retention_sweep_interval_secs()
}

pub(super) fn is_default_recording_notification_outbox_buffer(value: &usize) -> bool {
    *value == default_recording_notification_outbox_buffer()
}

pub(super) fn is_default_recording_notification_max_attempts(value: &u32) -> bool {
    *value == default_recording_notification_max_attempts()
}

pub(super) fn is_default_recording_notification_backoff_initial_secs(value: &u64) -> bool {
    *value == default_recording_notification_backoff_initial_secs()
}

pub(super) fn is_default_recording_notification_backoff_max_secs(value: &u64) -> bool {
    *value == default_recording_notification_backoff_max_secs()
}

pub(super) fn is_recording_enabled(value: &bool) -> bool { *value }

pub(super) fn is_default_recording_container_format(value: &RecordingContainerFormat) -> bool {
    *value == RecordingContainerFormat::default()
}

// `skip_serializing_if` predicates that distinguish "field absent" from
// "field present with a zero value". An explicit `Some(0)` is a real
// configuration choice and must round-trip; only `None` means "absent".
pub(super) fn is_zero_u64_opt(value: &Option<u64>) -> bool { value.is_none() }

pub(super) fn is_zero_u32_opt(value: &Option<u32>) -> bool { value.is_none() }

pub(super) fn is_zero_u8_opt(value: &Option<u8>) -> bool { value.is_none() }
