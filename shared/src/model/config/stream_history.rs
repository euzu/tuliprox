use crate::{
    error::{TuliproxError, TuliproxErrorKind},
    utils::{
        default_stream_history_batch_size, default_stream_history_retention_days, is_blank_stream_history_directory,
        is_default_stream_history_batch_size, is_default_stream_history_retention_days, is_false,
        DEFAULT_STREAM_HISTORY_DIR,
    },
};
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StreamHistoryConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream_history_enabled: bool,
    #[serde(
        default = "default_stream_history_batch_size",
        skip_serializing_if = "is_default_stream_history_batch_size"
    )]
    pub stream_history_batch_size: usize,
    #[serde(
        default = "default_stream_history_retention_days",
        skip_serializing_if = "is_default_stream_history_retention_days"
    )]
    pub stream_history_retention_days: u16,
    #[serde(default, skip_serializing_if = "is_blank_stream_history_directory")]
    pub stream_history_directory: String,
}

impl Default for StreamHistoryConfigDto {
    fn default() -> Self {
        Self {
            stream_history_enabled: false,
            stream_history_batch_size: default_stream_history_batch_size(),
            stream_history_retention_days: default_stream_history_retention_days(),
            stream_history_directory: String::new(),
        }
    }
}

impl StreamHistoryConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.stream_history_enabled
            && self.stream_history_batch_size == default_stream_history_batch_size()
            && self.stream_history_retention_days == default_stream_history_retention_days()
            && self.stream_history_directory.trim().is_empty()
    }

    pub(crate) fn prepare(&mut self, storage_dir: &str) -> Result<(), TuliproxError> {
        if !self.stream_history_enabled {
            return Ok(());
        }

        if self.stream_history_batch_size == 0 {
            return Err(TuliproxError::new(
                TuliproxErrorKind::Info,
                "`stream_history_batch_size` must be > 0 when stream history is enabled".to_string(),
            ));
        }
        if self.stream_history_retention_days == 0 {
            return Err(TuliproxError::new(
                TuliproxErrorKind::Info,
                "`stream_history_retention_days` must be > 0 when stream history is enabled".to_string(),
            ));
        }

        // Use the default subdirectory name when the user left the field blank.
        let directory = self.stream_history_directory.trim();
        let directory = if directory.is_empty() { DEFAULT_STREAM_HISTORY_DIR } else { directory };

        let directory_path = PathBuf::from(directory);
        self.stream_history_directory = if directory_path.is_absolute() {
            directory.to_string()
        } else {
            // Join with storage_dir, then resolve to an absolute path so that a
            // subsequent prepare() call (e.g. after a UI save-round-trip) sees an
            // absolute path and skips the join, preventing double-normalization
            // (e.g. "history" → "data/history" → "data/data/history").
            let joined = PathBuf::from(storage_dir).join(directory_path);
            std::path::absolute(&joined).unwrap_or(joined).to_string_lossy().to_string()
        };

        Ok(())
    }
}
