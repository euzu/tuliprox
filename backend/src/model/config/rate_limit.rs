use shared::model::RateLimitConfigDto;
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub period_millis: u64,
    pub burst_size: u32,
}

macros::from_impl!(RateLimitConfig);
impl From<&RateLimitConfigDto> for RateLimitConfig {
    fn from(dto: &RateLimitConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            period_millis: dto.period_millis,
            burst_size: dto.burst_size,
        }
    }
}

impl From<&RateLimitConfig> for RateLimitConfigDto {
    fn from(instance: &RateLimitConfig) -> Self {
        Self {
            enabled: instance.enabled,
            period_millis: instance.period_millis,
            burst_size: instance.burst_size,
        }
    }
}