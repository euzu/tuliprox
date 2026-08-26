use shared::model::QosAggregationConfigDto;

#[derive(Debug, Clone)]
pub struct QosAggregationConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub compaction_interval_secs: u64,
}

impl From<&QosAggregationConfigDto> for QosAggregationConfig {
    fn from(dto: &QosAggregationConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            interval_secs: dto.interval_secs,
            compaction_interval_secs: dto.compaction_interval_secs,
        }
    }
}

impl From<&QosAggregationConfig> for QosAggregationConfigDto {
    fn from(config: &QosAggregationConfig) -> Self {
        Self {
            enabled: config.enabled,
            interval_secs: config.interval_secs,
            compaction_interval_secs: config.compaction_interval_secs,
        }
    }
}
