use shared::model::{AdmissionStrategyDto, StreamBufferConfigDto, StreamConfigDto};
use shared::utils::parse_to_kbps;
use crate::api::model::TransportStreamBuffer;
use crate::model::macros;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum AdmissionStrategy {
    EvictUserSameIpOldest,
    EvictUserSameIpLatest,
    EvictUserOldest,
    EvictUserLatest,
    GraceInstantStream,
    GraceHoldStream,
}

impl From<&AdmissionStrategyDto> for AdmissionStrategy {
    fn from(dto: &AdmissionStrategyDto) -> Self {
        match dto {
            AdmissionStrategyDto::EvictUserSameIpOldest => Self::EvictUserSameIpOldest,
            AdmissionStrategyDto::EvictUserSameIpLatest => Self::EvictUserSameIpLatest,
            AdmissionStrategyDto::EvictUserOldest => Self::EvictUserOldest,
            AdmissionStrategyDto::EvictUserLatest => Self::EvictUserLatest,
            AdmissionStrategyDto::GraceInstantStream => Self::GraceInstantStream,
            AdmissionStrategyDto::GraceHoldStream => Self::GraceHoldStream,
        }
    }
}

impl From<&AdmissionStrategy> for AdmissionStrategyDto {
    fn from(domain: &AdmissionStrategy) -> Self {
        match domain {
            AdmissionStrategy::EvictUserSameIpOldest => Self::EvictUserSameIpOldest,
            AdmissionStrategy::EvictUserSameIpLatest => Self::EvictUserSameIpLatest,
            AdmissionStrategy::EvictUserOldest => Self::EvictUserOldest,
            AdmissionStrategy::EvictUserLatest => Self::EvictUserLatest,
            AdmissionStrategy::GraceInstantStream => Self::GraceInstantStream,
            AdmissionStrategy::GraceHoldStream => Self::GraceHoldStream,
        }
    }
}

impl AdmissionStrategy {
    pub fn is_grace(&self) -> bool {
        matches!(self, Self::GraceInstantStream | Self::GraceHoldStream)
    }

    pub fn is_grace_hold(&self) -> bool {
        matches!(self, Self::GraceHoldStream)
    }
}

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
    pub metrics_enabled: bool,
    pub buffer: Option<StreamBufferConfig>,
    pub grace_period_millis: u64,
    pub grace_period_timeout_secs: u64,
    pub grace_period_hold_stream: bool,
    pub hls_session_ttl_secs: u64,
    pub catchup_session_ttl_secs: u64,
    pub throttle_str: Option<String>,
    pub throttle_kbps: u64,
    pub shared_burst_buffer_mb: u64,
    pub admission_strategies: Option<Vec<AdmissionStrategy>>,
}

macros::from_impl!(StreamConfig);
impl From<&StreamConfigDto> for StreamConfig {
    fn from(dto: &StreamConfigDto) -> Self {
        Self {
            retry: dto.retry,
            metrics_enabled: dto.metrics_enabled,
            buffer: dto.buffer.as_ref().map(Into::into),
            grace_period_millis: dto.grace_period_millis,
            grace_period_timeout_secs: dto.grace_period_timeout_secs,
            grace_period_hold_stream: dto.grace_period_hold_stream,
            hls_session_ttl_secs: dto.hls_session_ttl_secs,
            catchup_session_ttl_secs: dto.catchup_session_ttl_secs,
            throttle_str: dto.throttle.clone(),
            throttle_kbps: dto.throttle.as_ref().map_or(0u64, |throttle| parse_to_kbps(throttle).unwrap_or(0u64)),
            shared_burst_buffer_mb: dto.shared_burst_buffer_mb,
            admission_strategies: dto
                .admission_strategies
                .as_ref()
                .map(|entries| entries.iter().map(AdmissionStrategy::from).collect()),
        }
    }
}

impl From<&StreamConfig> for StreamConfigDto {
    fn from(instance: &StreamConfig) -> Self {
        Self {
            retry: instance.retry,
            metrics_enabled: instance.metrics_enabled,
            buffer: instance.buffer.as_ref().map(Into::into),
            grace_period_millis: instance.grace_period_millis,
            grace_period_timeout_secs: instance.grace_period_timeout_secs,
            grace_period_hold_stream: instance.grace_period_hold_stream,
            hls_session_ttl_secs: instance.hls_session_ttl_secs,
            catchup_session_ttl_secs: instance.catchup_session_ttl_secs,
            throttle: instance.throttle_str.clone(),
            throttle_kbps: instance.throttle_kbps,
            shared_burst_buffer_mb: instance.shared_burst_buffer_mb,
            admission_strategies: instance
                .admission_strategies
                .as_ref()
                .map(|entries| entries.iter().map(AdmissionStrategyDto::from).collect()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AdmissionStrategy, StreamConfig};
    use shared::model::{AdmissionStrategyDto, StreamConfigDto};

    #[test]
    fn stream_config_preserves_missing_admission_strategies() {
        let domain = StreamConfig::from(&StreamConfigDto::default());
        assert_eq!(domain.admission_strategies, None);
    }

    #[test]
    fn stream_config_preserves_explicit_empty_admission_strategies() {
        let domain = StreamConfig::from(&StreamConfigDto {
            admission_strategies: Some(vec![]),
            ..StreamConfigDto::default()
        });
        assert_eq!(domain.admission_strategies, Some(vec![]));
    }

    #[test]
    fn stream_config_roundtrips_admission_strategies() {
        let domain = StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 10,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 5,
            catchup_session_ttl_secs: 5,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserOldest,
                AdmissionStrategy::GraceHoldStream,
                AdmissionStrategy::EvictUserLatest,
            ]),
        };

        let dto = StreamConfigDto::from(&domain);
        assert_eq!(
            dto.admission_strategies,
            Some(vec![
                AdmissionStrategyDto::EvictUserOldest,
                AdmissionStrategyDto::GraceHoldStream,
                AdmissionStrategyDto::EvictUserLatest,
            ])
        );
    }
}

#[derive(Debug, Clone)]
pub struct CustomStreamResponse {
    pub channel_unavailable: Option<TransportStreamBuffer>,
    pub user_connections_exhausted: Option<TransportStreamBuffer>, // user has no more connections
    pub provider_connections_exhausted: Option<TransportStreamBuffer>, // provider limit reached, has no more connections
    pub low_priority_preempted: Option<TransportStreamBuffer>, // stream was preempted by a higher-priority user
    pub user_account_expired: Option<TransportStreamBuffer>,
    pub panel_api_provisioning: Option<TransportStreamBuffer>,
}
