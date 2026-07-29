use crate::{
    defaults::{
        default_qos_aggregation_compaction_interval_secs, default_qos_aggregation_interval_secs,
        is_default_qos_aggregation_compaction_interval_secs, is_default_qos_aggregation_interval_secs, is_false,
    },
    error::TuliproxError,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QosAggregationConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(
        default = "default_qos_aggregation_interval_secs",
        skip_serializing_if = "is_default_qos_aggregation_interval_secs"
    )]
    pub interval_secs: u64,
    #[serde(
        default = "default_qos_aggregation_compaction_interval_secs",
        skip_serializing_if = "is_default_qos_aggregation_compaction_interval_secs"
    )]
    pub compaction_interval_secs: u64,
}

impl Default for QosAggregationConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_qos_aggregation_interval_secs(),
            compaction_interval_secs: default_qos_aggregation_compaction_interval_secs(),
        }
    }
}

impl QosAggregationConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && self.interval_secs == default_qos_aggregation_interval_secs()
            && self.compaction_interval_secs == default_qos_aggregation_compaction_interval_secs()
    }

    pub(crate) fn prepare(&mut self, stream_history_enabled: bool) -> Result<(), TuliproxError> {
        if !stream_history_enabled {
            self.enabled = false;
            return Ok(());
        }
        if self.enabled && self.interval_secs == 0 {
            return Err(TuliproxError::ConfigQosAggregation(
                "`qos_aggregation.interval_secs` must be > 0 when qos_aggregation is enabled".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::QosAggregationConfigDto;

    #[test]
    fn qos_aggregation_defaults_to_daily_snapshot_compaction() {
        assert_eq!(QosAggregationConfigDto::default().compaction_interval_secs, 86_400);
    }

    #[test]
    fn qos_aggregation_allows_disabling_automatic_snapshot_compaction() {
        let config: QosAggregationConfigDto =
            serde_json::from_str(r#"{"enabled":true,"interval_secs":300,"compaction_interval_secs":0}"#)
                .expect("configuration should deserialize");

        assert_eq!(config.compaction_interval_secs, 0);
    }
}
