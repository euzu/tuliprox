use shared::model::{StreamBufferConfigDto, StreamConfigDto};
use shared::utils::parse_to_kbps;
use crate::api::model::TransportStreamBuffer;
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
    pub grace_period_hold_stream: bool,
    pub throttle_str: Option<String>,
    pub throttle_kbps: u64,
    pub shared_burst_buffer_mb: u64,
}

macros::from_impl!(StreamConfig);
impl From<&StreamConfigDto> for StreamConfig {
    fn from(dto: &StreamConfigDto) -> Self {
        Self {
            retry: dto.retry,
            buffer: dto.buffer.as_ref().map(Into::into),
            grace_period_millis: dto.grace_period_millis,
            grace_period_timeout_secs: dto.grace_period_timeout_secs,
            throttle_str: dto.throttle.clone(),
            throttle_kbps: dto.throttle.as_ref().map_or(0u64, |throttle| parse_to_kbps(throttle).unwrap_or(0u64)),
            shared_burst_buffer_mb: dto.shared_burst_buffer_mb,
            grace_period_hold_stream: dto.grace_period_hold_stream,
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
            throttle: instance.throttle_str.clone(),
            throttle_kbps: instance.throttle_kbps,
            shared_burst_buffer_mb: instance.shared_burst_buffer_mb,
            grace_period_hold_stream: instance.grace_period_hold_stream,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CustomStreamResponse {
    pub channel_unavailable: Option<TransportStreamBuffer>,
    pub user_connections_exhausted: Option<TransportStreamBuffer>, // user has no more connections
    pub provider_connections_exhausted: Option<TransportStreamBuffer>, // provider limit reached, has no more connections
    pub user_account_expired: Option<TransportStreamBuffer>,
    pub panel_api_provisioning: Option<TransportStreamBuffer>,
}
