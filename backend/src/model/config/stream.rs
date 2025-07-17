use shared::model::{StreamBufferConfigDto, StreamConfigDto};
use shared::utils::parse_to_kbps;
use crate::api::model::streams::transport_stream_buffer::TransportStreamBuffer;
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct StreamBufferConfig {
    pub enabled: bool,
    pub size: usize,
}

macros::from_impl!(StreamBufferConfig);
impl From<&StreamBufferConfigDto> for StreamBufferConfig {
    fn from(dto: &StreamBufferConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            size: dto.size,
        }
    }
}

impl From<&StreamBufferConfig> for StreamBufferConfigDto {
    fn from(dto: &StreamBufferConfig) -> Self {
        Self {
            enabled: dto.enabled,
            size: dto.size,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub retry: bool,
    pub buffer: Option<StreamBufferConfig>,
    pub grace_period_millis: u64,
    pub grace_period_timeout_secs: u64,
    pub forced_retry_interval_secs: u32,
    pub throttle_str: Option<String>,
    pub throttle_kbps: u64,
}

macros::from_impl!(StreamConfig);
impl From<&StreamConfigDto> for StreamConfig {
    fn from(dto: &StreamConfigDto) -> Self {
        Self {
            retry: dto.retry,
            buffer: dto.buffer.as_ref().map(Into::into),
            grace_period_millis: dto.grace_period_millis,
            grace_period_timeout_secs: dto.grace_period_timeout_secs,
            forced_retry_interval_secs: dto.forced_retry_interval_secs,
            throttle_str: dto.throttle.clone(),
            throttle_kbps: dto.throttle.as_ref().map_or(0u64, |throttle| parse_to_kbps(throttle).unwrap_or(0u64)),
        }
    }
}

impl From<&StreamConfig> for StreamConfigDto {
    fn from(instance: &StreamConfig) -> Self {
        Self {
            retry: instance.retry,
            buffer: instance.buffer.as_ref().map(Into::into),
            grace_period_millis: instance.grace_period_millis,
            grace_period_timeout_secs: instance.grace_period_timeout_secs,
            forced_retry_interval_secs: instance.forced_retry_interval_secs,
            throttle: instance.throttle_str.clone(),
            throttle_kbps: instance.throttle_kbps,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CustomStreamResponse {
    pub channel_unavailable: Option<TransportStreamBuffer>,
    pub user_connections_exhausted: Option<TransportStreamBuffer>, // user has no more connections
    pub provider_connections_exhausted: Option<TransportStreamBuffer>, // provider limit reached, has no more connections
    pub user_account_expired: Option<TransportStreamBuffer>,
}