use crate::model::macros;
use shared::model::{
    StreamHistoryConfigDto,
};
use shared::utils::{default_stream_history_batch_size, default_stream_history_retention_days};

#[derive(Debug, Clone)]
pub struct StreamHistoryConfig {
    pub stream_history_enabled: bool,
    pub stream_history_batch_size: usize,
    pub stream_history_retention_days: u16,
    pub stream_history_directory: String,
}

macros::from_impl!(StreamHistoryConfig);

impl From<&StreamHistoryConfigDto> for StreamHistoryConfig {
    fn from(dto: &StreamHistoryConfigDto) -> Self {
        Self {
            stream_history_enabled: dto.stream_history_enabled,
            stream_history_batch_size: dto.stream_history_batch_size,
            stream_history_retention_days: dto.stream_history_retention_days,
            stream_history_directory: dto.stream_history_directory.clone(),
        }
    }
}

impl From<&StreamHistoryConfig> for StreamHistoryConfigDto {
    fn from(instance: &StreamHistoryConfig) -> Self {
        Self {
            stream_history_enabled: instance.stream_history_enabled,
            stream_history_batch_size: instance.stream_history_batch_size,
            stream_history_retention_days: instance.stream_history_retention_days,
            stream_history_directory: instance.stream_history_directory.clone(),
        }
    }
}

impl Default for StreamHistoryConfig {
    fn default() -> Self {
        Self {
            stream_history_enabled: false,
            stream_history_batch_size: default_stream_history_batch_size(),
            stream_history_retention_days: default_stream_history_retention_days(),
            stream_history_directory: String::new(),
        }
    }
}
