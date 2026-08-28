//! Stream-history defaults.

pub const DEFAULT_STREAM_HISTORY_BATCH_SIZE: usize = 128;
pub const DEFAULT_STREAM_HISTORY_RETENTION_DAYS: u16 = 30;
pub const DEFAULT_STREAM_HISTORY_DIR: &str = "stream_history";

pub fn default_stream_history_batch_size() -> usize { DEFAULT_STREAM_HISTORY_BATCH_SIZE }
pub fn default_stream_history_retention_days() -> u16 { DEFAULT_STREAM_HISTORY_RETENTION_DAYS }
pub fn default_stream_history_directory() -> String { DEFAULT_STREAM_HISTORY_DIR.to_string() }

pub fn is_default_stream_history_batch_size(batch_size: &usize) -> bool {
    *batch_size == DEFAULT_STREAM_HISTORY_BATCH_SIZE
}
pub fn is_default_stream_history_retention_days(retention_days: &u16) -> bool {
    *retention_days == DEFAULT_STREAM_HISTORY_RETENTION_DAYS
}
pub fn is_blank_stream_history_directory(directory: &str) -> bool { directory.trim().is_empty() }
