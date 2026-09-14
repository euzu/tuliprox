use crate::{defaults::is_zero_u8, error::TuliproxError, model::Prepare};

/// Per-cluster minimum quality thresholds for accepting input updates.
///
/// A value of zero disables the additional quality check for that cluster.
#[derive(Default, Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputUpdateQualityDto {
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub live: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub vod: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub series: u8,
}

impl ConfigInputUpdateQualityDto {
    #[must_use]
    pub const fn is_disabled(&self) -> bool { self.live == 0 && self.vod == 0 && self.series == 0 }

    #[must_use]
    pub const fn is_empty(&self) -> bool { self.is_disabled() }

    pub fn clean(&mut self) { *self = Self::default(); }
}

impl Prepare for ConfigInputUpdateQualityDto {
    type Ctx<'a> = ();

    fn prepare(&mut self, (): Self::Ctx<'_>) -> Result<(), TuliproxError> {
        for (field, value) in [("live", self.live), ("vod", self.vod), ("series", self.series)] {
            if value > 100 {
                return Err(TuliproxError::ConfigInput(format!(
                    "options.update_quality.{field} must be in the range 0..=100"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigInputUpdateQualityDto;
    use crate::model::Prepare;

    #[test]
    fn defaults_are_disabled_and_serialize_as_an_empty_map() -> Result<(), serde_json::Error> {
        let policy = ConfigInputUpdateQualityDto::default();

        assert!(policy.is_disabled());
        assert!(policy.is_empty());
        assert_eq!(serde_json::to_value(policy)?, serde_json::json!({}));
        Ok(())
    }

    #[test]
    fn serde_round_trip_omits_only_zero_thresholds() -> Result<(), serde_json::Error> {
        let policy = ConfigInputUpdateQualityDto { live: 95, vod: 0, series: 100 };
        let value = serde_json::to_value(policy)?;

        assert_eq!(value, serde_json::json!({ "live": 95, "series": 100 }));
        assert_eq!(serde_json::from_value::<ConfigInputUpdateQualityDto>(value)?, policy);
        Ok(())
    }

    #[test]
    fn deserialization_rejects_unknown_fields() {
        let result = serde_json::from_value::<ConfigInputUpdateQualityDto>(serde_json::json!({ "movie": 90 }));

        assert!(result.is_err());
    }

    #[test]
    fn prepare_validates_every_threshold() {
        let cases = [
            ("live", ConfigInputUpdateQualityDto { live: 101, ..ConfigInputUpdateQualityDto::default() }),
            ("vod", ConfigInputUpdateQualityDto { vod: 101, ..ConfigInputUpdateQualityDto::default() }),
            ("series", ConfigInputUpdateQualityDto { series: 101, ..ConfigInputUpdateQualityDto::default() }),
        ];

        for (field, mut policy) in cases {
            let error = policy.prepare(()).expect_err("threshold above 100 must be rejected");
            assert!(error.to_string().contains(field), "unexpected error for {field}: {error}");
        }

        for threshold in [0, 100] {
            let mut policy = ConfigInputUpdateQualityDto { live: threshold, vod: threshold, series: threshold };
            assert!(policy.prepare(()).is_ok(), "threshold {threshold} should be valid");
        }
    }

    #[test]
    fn clean_restores_disabled_defaults() {
        let mut policy = ConfigInputUpdateQualityDto { live: 95, vod: 90, series: 85 };

        policy.clean();

        assert_eq!(policy, ConfigInputUpdateQualityDto::default());
        assert!(policy.is_empty());
    }
}
