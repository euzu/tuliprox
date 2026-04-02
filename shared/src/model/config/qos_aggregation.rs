use crate::{
    error::{TuliproxError, TuliproxErrorKind},
    utils::is_false,
};

const fn default_qos_aggregation_interval_secs() -> u64 { 300 }
const fn is_default_qos_aggregation_interval_secs(value: &u64) -> bool {
    *value == default_qos_aggregation_interval_secs()
}

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
}

impl Default for QosAggregationConfigDto {
    fn default() -> Self { Self { enabled: false, interval_secs: default_qos_aggregation_interval_secs() } }
}

impl QosAggregationConfigDto {
    pub fn is_empty(&self) -> bool { !self.enabled && self.interval_secs == default_qos_aggregation_interval_secs() }

    pub(crate) fn prepare(&mut self, stream_history_enabled: bool) -> Result<(), TuliproxError> {
        if !stream_history_enabled {
            self.enabled = false;
            return Ok(());
        }
        if self.enabled && self.interval_secs == 0 {
            return Err(TuliproxError::new(
                TuliproxErrorKind::Info,
                "`qos_aggregation.interval_secs` must be > 0 when qos_aggregation is enabled".to_string(),
            ));
        }
        Ok(())
    }
}
