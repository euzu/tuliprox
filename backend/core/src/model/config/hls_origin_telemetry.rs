use crate::model::macros;
use shared::model::HlsOriginTelemetryConfigDto;

#[derive(Debug, Clone)]
pub struct HlsOriginTelemetryConfig {
    pub enabled: bool,
    pub queue_capacity: usize,
    pub freshness_secs: u64,
    pub window_secs: u64,
}

impl From<&HlsOriginTelemetryConfigDto> for HlsOriginTelemetryConfig {
    fn from(dto: &HlsOriginTelemetryConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            queue_capacity: dto.queue_capacity,
            freshness_secs: dto.freshness_secs,
            window_secs: dto.window_secs,
        }
    }
}

impl From<&HlsOriginTelemetryConfig> for HlsOriginTelemetryConfigDto {
    fn from(config: &HlsOriginTelemetryConfig) -> Self {
        Self {
            enabled: config.enabled,
            queue_capacity: config.queue_capacity,
            freshness_secs: config.freshness_secs,
            window_secs: config.window_secs,
        }
    }
}

macros::from_impl!(HlsOriginTelemetryConfig);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_roundtrips_every_field() {
        let dto =
            HlsOriginTelemetryConfigDto { enabled: true, queue_capacity: 512, freshness_secs: 45, window_secs: 180 };
        let config = HlsOriginTelemetryConfig::from(dto.clone());
        assert!(config.enabled);
        assert_eq!((config.queue_capacity, config.freshness_secs, config.window_secs), (512, 45, 180));
        assert_eq!(HlsOriginTelemetryConfigDto::from(&config), dto);
    }
}
