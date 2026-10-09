use crate::{
    defaults::{
        default_hls_corrupt_segment_watchdog_max_parallel_jobs, default_hls_segment_repair_apply_to_first_segments,
        default_hls_segment_repair_high_size_increase_percent, default_hls_segment_repair_low_size_increase_percent,
        default_hls_segment_repair_max_parallel_repairs, default_hls_segment_repair_medium_size_increase_percent,
        default_hls_segment_repair_postprocess_timeout_ms,
    },
    error::TuliproxError,
    model::{HlsCorruptSegmentWatchdogMode, HlsSegmentRepairMode, Millis},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsSegmentRepairSizeIncreaseConfigDto {
    #[serde(default = "default_hls_segment_repair_low_size_increase_percent")]
    pub low_percent: u8,
    #[serde(default = "default_hls_segment_repair_medium_size_increase_percent")]
    pub medium_percent: u8,
    #[serde(default = "default_hls_segment_repair_high_size_increase_percent")]
    pub high_percent: u8,
}

impl Default for HlsSegmentRepairSizeIncreaseConfigDto {
    fn default() -> Self {
        Self {
            low_percent: default_hls_segment_repair_low_size_increase_percent(),
            medium_percent: default_hls_segment_repair_medium_size_increase_percent(),
            high_percent: default_hls_segment_repair_high_size_increase_percent(),
        }
    }
}

impl HlsSegmentRepairSizeIncreaseConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub const fn clean(&mut self) {}

    pub(super) fn validate(&self) -> Result<(), TuliproxError> {
        let fields = [
            ("low_percent", self.low_percent),
            ("medium_percent", self.medium_percent),
            ("high_percent", self.high_percent),
        ];
        for (field, value) in fields {
            if value > 100 {
                return Err(TuliproxError::ConfigReverseProxy(format!(
                    "hls_cache.segment_repair.size_increase.{field} must be <= 100"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsSegmentRepairConfigDto {
    #[serde(default)]
    pub max_level: HlsSegmentRepairMode,
    #[serde(default = "default_hls_segment_repair_apply_to_first_segments")]
    pub apply_to_first_segments: u8,
    #[serde(default = "default_hls_segment_repair_max_parallel_repairs")]
    pub max_parallel_repairs: usize,
    #[serde(default = "default_hls_segment_repair_postprocess_timeout_ms")]
    pub postprocess_timeout_ms: Millis,
    #[serde(default, skip_serializing_if = "HlsSegmentRepairSizeIncreaseConfigDto::is_empty")]
    pub size_increase: HlsSegmentRepairSizeIncreaseConfigDto,
    #[serde(default, skip_serializing_if = "HlsCorruptSegmentWatchdogConfigDto::is_empty")]
    pub corrupt_segment_watchdog: HlsCorruptSegmentWatchdogConfigDto,
}

impl Default for HlsSegmentRepairConfigDto {
    fn default() -> Self {
        Self {
            max_level: HlsSegmentRepairMode::Off,
            apply_to_first_segments: default_hls_segment_repair_apply_to_first_segments(),
            max_parallel_repairs: default_hls_segment_repair_max_parallel_repairs(),
            postprocess_timeout_ms: default_hls_segment_repair_postprocess_timeout_ms(),
            size_increase: HlsSegmentRepairSizeIncreaseConfigDto::default(),
            corrupt_segment_watchdog: HlsCorruptSegmentWatchdogConfigDto::default(),
        }
    }
}

impl HlsSegmentRepairConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub fn clean(&mut self) {
        self.size_increase.clean();
        self.corrupt_segment_watchdog.clean();
    }

    pub(super) fn validate(&self) -> Result<(), TuliproxError> {
        if self.postprocess_timeout_ms < Millis::new(100) {
            return Err(TuliproxError::ConfigReverseProxy(
                "hls_cache.segment_repair.postprocess_timeout_ms must be >= 100".to_string(),
            ));
        }
        self.size_increase.validate()?;
        self.corrupt_segment_watchdog.validate()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsCorruptSegmentWatchdogConfigDto {
    #[serde(default)]
    pub mode: HlsCorruptSegmentWatchdogMode,
    #[serde(default = "default_hls_corrupt_segment_watchdog_max_parallel_jobs")]
    pub max_parallel_jobs: usize,
}

impl Default for HlsCorruptSegmentWatchdogConfigDto {
    fn default() -> Self {
        Self {
            mode: HlsCorruptSegmentWatchdogMode::Off,
            max_parallel_jobs: default_hls_corrupt_segment_watchdog_max_parallel_jobs(),
        }
    }
}

impl HlsCorruptSegmentWatchdogConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub const fn clean(&mut self) {}

    pub(super) fn validate(&self) -> Result<(), TuliproxError> {
        if self.max_parallel_jobs == 0 {
            return Err(TuliproxError::ConfigReverseProxy(
                "hls_cache.segment_repair.corrupt_segment_watchdog.max_parallel_jobs must be >= 1".to_string(),
            ));
        }
        Ok(())
    }
}
