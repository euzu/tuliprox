use shared::model::QosAggregationConfigDto;

#[derive(Debug, Clone)]
pub struct QosAggregationConfig {
    pub enabled: bool,
    pub interval_secs: u64,
}

impl From<&QosAggregationConfigDto> for QosAggregationConfig {
    fn from(dto: &QosAggregationConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            interval_secs: dto.interval_secs,
        }
    }
}

impl From<&QosAggregationConfig> for QosAggregationConfigDto {
    fn from(config: &QosAggregationConfig) -> Self {
        Self {
            enabled: config.enabled,
            interval_secs: config.interval_secs,
        }
    }
}
