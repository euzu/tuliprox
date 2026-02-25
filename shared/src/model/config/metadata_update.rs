use crate::{
    error::TuliproxError,
    info_err_res,
    utils::{
        default_metadata_backoff_jitter_percent, default_metadata_ffprobe_analyze_duration,
        default_metadata_ffprobe_live_analyze_duration, default_metadata_ffprobe_live_probe_size,
        default_metadata_ffprobe_probe_size, default_metadata_max_attempts_probe,
        default_metadata_max_attempts_resolve, default_metadata_max_queue_size,
        default_metadata_max_resolve_retry_backoff, default_metadata_probe_cooldown,
        default_metadata_probe_retry_backoff_step_1, default_metadata_probe_retry_backoff_step_2,
        default_metadata_probe_retry_backoff_step_3, default_metadata_probe_retry_load_retry_delay,
        default_metadata_progress_log_interval, default_metadata_queue_log_interval,
        default_metadata_resolve_exhaustion_reset_gap, default_metadata_resolve_min_retry_base,
        default_metadata_retry_delay, default_metadata_worker_idle_timeout, deserialize_as_string,
        is_default_metadata_backoff_jitter_percent, is_default_metadata_ffprobe_analyze_duration,
        is_default_metadata_ffprobe_live_analyze_duration, is_default_metadata_ffprobe_live_probe_size,
        is_default_metadata_ffprobe_probe_size, is_default_metadata_max_attempts_probe,
        is_default_metadata_max_attempts_resolve, is_default_metadata_max_queue_size,
        is_default_metadata_max_resolve_retry_backoff, is_default_metadata_probe_cooldown,
        is_default_metadata_probe_retry_backoff_step_1, is_default_metadata_probe_retry_backoff_step_2,
        is_default_metadata_probe_retry_backoff_step_3, is_default_metadata_probe_retry_load_retry_delay,
        is_default_metadata_progress_log_interval, is_default_metadata_queue_log_interval,
        is_default_metadata_resolve_exhaustion_reset_gap, is_default_metadata_resolve_min_retry_base,
        is_default_metadata_retry_delay, is_default_metadata_worker_idle_timeout, is_false, parse_duration_seconds,
        parse_size_base_2,
    },
};
use std::sync::OnceLock;

const MIN_DURATION_SECS: u64 = 1;
const MIN_ATTEMPTS: u8 = 1;
const MAX_JITTER_PERCENT: u8 = 95;
const MIN_QUEUE_SIZE: usize = 1;
const DEFAULT_FFPROBE_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetadataUpdateConfigDto {
    #[serde(
        default = "default_metadata_queue_log_interval",
        skip_serializing_if = "is_default_metadata_queue_log_interval"
    )]
    pub queue_log_interval: String,
    #[serde(
        default = "default_metadata_progress_log_interval",
        skip_serializing_if = "is_default_metadata_progress_log_interval"
    )]
    pub progress_log_interval: String,
    #[serde(
        default = "default_metadata_max_resolve_retry_backoff",
        skip_serializing_if = "is_default_metadata_max_resolve_retry_backoff"
    )]
    pub max_resolve_retry_backoff: String,
    #[serde(
        default = "default_metadata_resolve_min_retry_base",
        skip_serializing_if = "is_default_metadata_resolve_min_retry_base"
    )]
    pub resolve_min_retry_base: String,
    #[serde(
        default = "default_metadata_resolve_exhaustion_reset_gap",
        skip_serializing_if = "is_default_metadata_resolve_exhaustion_reset_gap"
    )]
    pub resolve_exhaustion_reset_gap: String,
    #[serde(default = "default_metadata_probe_cooldown", skip_serializing_if = "is_default_metadata_probe_cooldown")]
    pub probe_cooldown: String,
    #[serde(default = "default_metadata_retry_delay", skip_serializing_if = "is_default_metadata_retry_delay")]
    pub retry_delay: String,
    #[serde(
        default = "default_metadata_probe_retry_load_retry_delay",
        skip_serializing_if = "is_default_metadata_probe_retry_load_retry_delay"
    )]
    pub probe_retry_load_retry_delay: String,
    #[serde(
        default = "default_metadata_worker_idle_timeout",
        skip_serializing_if = "is_default_metadata_worker_idle_timeout"
    )]
    pub worker_idle_timeout: String,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_1",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_1"
    )]
    pub probe_retry_backoff_step_1: String,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_2",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_2"
    )]
    pub probe_retry_backoff_step_2: String,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_3",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_3"
    )]
    pub probe_retry_backoff_step_3: String,
    #[serde(
        default = "default_metadata_max_attempts_resolve",
        skip_serializing_if = "is_default_metadata_max_attempts_resolve"
    )]
    pub max_attempts_resolve: u8,
    #[serde(
        default = "default_metadata_max_attempts_probe",
        skip_serializing_if = "is_default_metadata_max_attempts_probe"
    )]
    pub max_attempts_probe: u8,
    #[serde(
        default = "default_metadata_backoff_jitter_percent",
        skip_serializing_if = "is_default_metadata_backoff_jitter_percent"
    )]
    pub backoff_jitter_percent: u8,
    #[serde(default = "default_metadata_max_queue_size", skip_serializing_if = "is_default_metadata_max_queue_size")]
    pub max_queue_size: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ffprobe_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ffprobe_timeout: Option<u64>,
    #[serde(
        default = "default_metadata_ffprobe_analyze_duration",
        skip_serializing_if = "is_default_metadata_ffprobe_analyze_duration",
        deserialize_with = "deserialize_as_string"
    )]
    pub ffprobe_analyze_duration: String,
    #[serde(
        default = "default_metadata_ffprobe_probe_size",
        skip_serializing_if = "is_default_metadata_ffprobe_probe_size",
        deserialize_with = "deserialize_as_string"
    )]
    pub ffprobe_probe_size: String,
    #[serde(
        default = "default_metadata_ffprobe_live_analyze_duration",
        skip_serializing_if = "is_default_metadata_ffprobe_live_analyze_duration",
        deserialize_with = "deserialize_as_string"
    )]
    pub ffprobe_live_analyze_duration: String,
    #[serde(
        default = "default_metadata_ffprobe_live_probe_size",
        skip_serializing_if = "is_default_metadata_ffprobe_live_probe_size",
        deserialize_with = "deserialize_as_string"
    )]
    pub ffprobe_live_probe_size: String,
}

impl Default for MetadataUpdateConfigDto {
    fn default() -> Self {
        Self {
            queue_log_interval: default_metadata_queue_log_interval(),
            progress_log_interval: default_metadata_progress_log_interval(),
            max_resolve_retry_backoff: default_metadata_max_resolve_retry_backoff(),
            resolve_min_retry_base: default_metadata_resolve_min_retry_base(),
            resolve_exhaustion_reset_gap: default_metadata_resolve_exhaustion_reset_gap(),
            probe_cooldown: default_metadata_probe_cooldown(),
            retry_delay: default_metadata_retry_delay(),
            probe_retry_load_retry_delay: default_metadata_probe_retry_load_retry_delay(),
            worker_idle_timeout: default_metadata_worker_idle_timeout(),
            probe_retry_backoff_step_1: default_metadata_probe_retry_backoff_step_1(),
            probe_retry_backoff_step_2: default_metadata_probe_retry_backoff_step_2(),
            probe_retry_backoff_step_3: default_metadata_probe_retry_backoff_step_3(),
            max_attempts_resolve: default_metadata_max_attempts_resolve(),
            max_attempts_probe: default_metadata_max_attempts_probe(),
            backoff_jitter_percent: default_metadata_backoff_jitter_percent(),
            max_queue_size: default_metadata_max_queue_size(),
            ffprobe_enabled: false,
            ffprobe_timeout: None,
            ffprobe_analyze_duration: default_metadata_ffprobe_analyze_duration(),
            ffprobe_probe_size: default_metadata_ffprobe_probe_size(),
            ffprobe_live_analyze_duration: default_metadata_ffprobe_live_analyze_duration(),
            ffprobe_live_probe_size: default_metadata_ffprobe_live_probe_size(),
        }
    }
}

impl MetadataUpdateConfigDto {
    fn defaults() -> &'static Self {
        static DEFAULTS: OnceLock<MetadataUpdateConfigDto> = OnceLock::new();
        DEFAULTS.get_or_init(Self::default)
    }

    pub fn is_empty(&self) -> bool { self == Self::defaults() }

    pub fn clean(&mut self) {
        if self.ffprobe_timeout.is_some_and(|v| v == DEFAULT_FFPROBE_TIMEOUT_SECS) {
            self.ffprobe_timeout = None;
        }
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        let queue_log_interval_secs =
            Self::parse_and_clamp_duration(&self.queue_log_interval, MIN_DURATION_SECS, "queue_log_interval")?;
        self.queue_log_interval = Self::canonicalize_seconds(queue_log_interval_secs);

        let progress_log_interval_secs =
            Self::parse_and_clamp_duration(&self.progress_log_interval, MIN_DURATION_SECS, "progress_log_interval")?;
        self.progress_log_interval = Self::canonicalize_seconds(progress_log_interval_secs);

        let max_resolve_retry_backoff_secs = Self::parse_and_clamp_duration(
            &self.max_resolve_retry_backoff,
            MIN_DURATION_SECS,
            "max_resolve_retry_backoff",
        )?;
        self.max_resolve_retry_backoff = Self::canonicalize_seconds(max_resolve_retry_backoff_secs);

        let resolve_min_retry_base_secs =
            Self::parse_and_clamp_duration(&self.resolve_min_retry_base, MIN_DURATION_SECS, "resolve_min_retry_base")?;
        self.resolve_min_retry_base = Self::canonicalize_seconds(resolve_min_retry_base_secs);

        let resolve_exhaustion_reset_gap_secs = Self::parse_and_clamp_duration(
            &self.resolve_exhaustion_reset_gap,
            MIN_DURATION_SECS,
            "resolve_exhaustion_reset_gap",
        )?;
        self.resolve_exhaustion_reset_gap = Self::canonicalize_seconds(resolve_exhaustion_reset_gap_secs);

        let probe_cooldown_secs =
            Self::parse_and_clamp_duration(&self.probe_cooldown, MIN_DURATION_SECS, "probe_cooldown")?;
        self.probe_cooldown = Self::canonicalize_seconds(probe_cooldown_secs);

        let retry_delay_secs = Self::parse_and_clamp_duration(&self.retry_delay, MIN_DURATION_SECS, "retry_delay")?;
        self.retry_delay = Self::canonicalize_seconds(retry_delay_secs);

        let probe_retry_load_retry_delay_secs = Self::parse_and_clamp_duration(
            &self.probe_retry_load_retry_delay,
            MIN_DURATION_SECS,
            "probe_retry_load_retry_delay",
        )?;
        self.probe_retry_load_retry_delay = Self::canonicalize_seconds(probe_retry_load_retry_delay_secs);

        let worker_idle_timeout_secs =
            Self::parse_and_clamp_duration(&self.worker_idle_timeout, MIN_DURATION_SECS, "worker_idle_timeout")?;
        self.worker_idle_timeout = Self::canonicalize_seconds(worker_idle_timeout_secs);

        let probe_retry_backoff_step_1_secs = Self::parse_and_clamp_duration(
            &self.probe_retry_backoff_step_1,
            MIN_DURATION_SECS,
            "probe_retry_backoff_step_1",
        )?;
        self.probe_retry_backoff_step_1 = Self::canonicalize_seconds(probe_retry_backoff_step_1_secs);

        let probe_retry_backoff_step_2_secs = Self::parse_and_clamp_duration(
            &self.probe_retry_backoff_step_2,
            MIN_DURATION_SECS,
            "probe_retry_backoff_step_2",
        )?;
        self.probe_retry_backoff_step_2 = Self::canonicalize_seconds(probe_retry_backoff_step_2_secs);

        let probe_retry_backoff_step_3_secs = Self::parse_and_clamp_duration(
            &self.probe_retry_backoff_step_3,
            MIN_DURATION_SECS,
            "probe_retry_backoff_step_3",
        )?;
        self.probe_retry_backoff_step_3 = Self::canonicalize_seconds(probe_retry_backoff_step_3_secs);

        self.max_attempts_resolve = self.max_attempts_resolve.max(MIN_ATTEMPTS);
        self.max_attempts_probe = self.max_attempts_probe.max(MIN_ATTEMPTS);
        self.backoff_jitter_percent = self.backoff_jitter_percent.min(MAX_JITTER_PERCENT);
        self.max_queue_size = self.max_queue_size.max(MIN_QUEUE_SIZE);
        self.ffprobe_timeout = self.ffprobe_timeout.map(|timeout| timeout.max(MIN_DURATION_SECS));

        let ffprobe_analyze_duration_secs = Self::parse_and_clamp_duration_with_required_unit(
            &self.ffprobe_analyze_duration,
            MIN_DURATION_SECS,
            "ffprobe_analyze_duration",
        )?;
        self.ffprobe_analyze_duration = Self::canonicalize_seconds(ffprobe_analyze_duration_secs);

        let ffprobe_probe_size_bytes = parse_size_base_2(&self.ffprobe_probe_size)
            .map_err(|err| crate::error::info_err!("Invalid size for `ffprobe_probe_size`: {err}"))?
            .max(1);
        self.ffprobe_probe_size = Self::canonicalize_size_bytes(ffprobe_probe_size_bytes);

        let ffprobe_live_analyze_duration_secs = Self::parse_and_clamp_duration_with_required_unit(
            &self.ffprobe_live_analyze_duration,
            MIN_DURATION_SECS,
            "ffprobe_live_analyze_duration",
        )?;
        self.ffprobe_live_analyze_duration = Self::canonicalize_seconds(ffprobe_live_analyze_duration_secs);

        let ffprobe_live_probe_size_bytes = parse_size_base_2(&self.ffprobe_live_probe_size)
            .map_err(|err| crate::error::info_err!("Invalid size for `ffprobe_live_probe_size`: {err}"))?
            .max(1);
        self.ffprobe_live_probe_size = Self::canonicalize_size_bytes(ffprobe_live_probe_size_bytes);

        self.clean();

        Ok(())
    }

    fn parse_and_clamp_duration(value: &str, min_seconds: u64, field_name: &str) -> Result<u64, TuliproxError> {
        let parsed = Self::parse_duration(value, field_name)?;
        Ok(parsed.max(min_seconds))
    }

    fn parse_and_clamp_duration_with_required_unit(
        value: &str,
        min_seconds: u64,
        field_name: &str,
    ) -> Result<u64, TuliproxError> {
        let parsed = Self::parse_duration_with_required_unit(value, field_name)?;
        Ok(parsed.max(min_seconds))
    }

    fn parse_duration_with_required_unit(value: &str, field_name: &str) -> Result<u64, TuliproxError> {
        if value.parse::<u64>().is_ok() {
            return info_err_res!(
                "Invalid duration format for `{field_name}`: {value}. Use explicit unit suffix (`s`, `m`, `h`, `d`), e.g. `10s`."
            );
        }
        Self::parse_duration(value, field_name)
    }

    fn parse_duration(value: &str, field_name: &str) -> Result<u64, TuliproxError> {
        parse_duration_seconds(value, false)
            .ok_or_else(|| crate::error::info_err!("Invalid duration format for `{field_name}`: {value}"))
    }

    fn canonicalize_seconds(seconds: u64) -> String {
        if seconds.is_multiple_of(24 * 60 * 60) {
            format!("{}d", seconds / (24 * 60 * 60))
        } else if seconds.is_multiple_of(60 * 60) {
            format!("{}h", seconds / (60 * 60))
        } else if seconds.is_multiple_of(60) {
            format!("{}m", seconds / 60)
        } else {
            format!("{seconds}s")
        }
    }

    fn canonicalize_size_bytes(bytes: u64) -> String {
        if bytes.is_multiple_of(1_099_511_627_776) {
            format!("{}TB", bytes / 1_099_511_627_776)
        } else if bytes.is_multiple_of(1_073_741_824) {
            format!("{}GB", bytes / 1_073_741_824)
        } else if bytes.is_multiple_of(1_048_576) {
            format!("{}MB", bytes / 1_048_576)
        } else if bytes.is_multiple_of(1_024) {
            format!("{}KB", bytes / 1_024)
        } else {
            format!("{bytes}B")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MetadataUpdateConfigDto;

    #[test]
    fn default_config_is_empty() {
        let cfg = MetadataUpdateConfigDto::default();
        assert!(cfg.is_empty());
    }

    #[test]
    fn prepare_parses_duration_suffixes() {
        let mut cfg = MetadataUpdateConfigDto {
            queue_log_interval: "1m".to_string(),
            progress_log_interval: "2h".to_string(),
            probe_cooldown: "1d".to_string(),
            ..MetadataUpdateConfigDto::default()
        };

        cfg.prepare().expect("metadata update config should parse duration values");

        assert_eq!(cfg.queue_log_interval, "1m");
        assert_eq!(cfg.progress_log_interval, "2h");
        assert_eq!(cfg.probe_cooldown, "1d");
    }

    #[test]
    fn prepare_clamps_minimum_values() {
        let mut cfg = MetadataUpdateConfigDto {
            queue_log_interval: "0".to_string(),
            max_attempts_resolve: 0,
            max_attempts_probe: 0,
            max_queue_size: 0,
            ffprobe_timeout: Some(0),
            ffprobe_analyze_duration: "0s".to_string(),
            ffprobe_probe_size: "0".to_string(),
            ..MetadataUpdateConfigDto::default()
        };

        cfg.prepare().expect("metadata update config should clamp minimum values");

        assert_eq!(cfg.queue_log_interval, "1s");
        assert_eq!(cfg.max_attempts_resolve, 1);
        assert_eq!(cfg.max_attempts_probe, 1);
        assert_eq!(cfg.max_queue_size, 1);
        assert_eq!(cfg.ffprobe_timeout, Some(1));
        assert_eq!(cfg.ffprobe_analyze_duration, "1s");
        assert_eq!(cfg.ffprobe_probe_size, "1B");
    }

    #[test]
    fn prepare_rejects_invalid_duration_unit() {
        let mut cfg =
            MetadataUpdateConfigDto { queue_log_interval: "1w".to_string(), ..MetadataUpdateConfigDto::default() };

        let result = cfg.prepare();
        assert!(result.is_err(), "invalid duration unit must fail");
    }

    #[test]
    fn prepare_canonicalizes_to_larger_units() {
        let mut cfg = MetadataUpdateConfigDto {
            probe_cooldown: "604800".to_string(),
            worker_idle_timeout: "60".to_string(),
            probe_retry_backoff_step_3: "3600".to_string(),
            ffprobe_analyze_duration: "10s".to_string(),
            ffprobe_probe_size: "10485760".to_string(),
            ffprobe_live_analyze_duration: "5s".to_string(),
            ffprobe_live_probe_size: "5242880".to_string(),
            ..MetadataUpdateConfigDto::default()
        };

        cfg.prepare().expect("metadata update config should canonicalize durations");

        assert_eq!(cfg.probe_cooldown, "7d");
        assert_eq!(cfg.worker_idle_timeout, "1m");
        assert_eq!(cfg.probe_retry_backoff_step_3, "1h");
        assert_eq!(cfg.ffprobe_analyze_duration, "10s");
        assert_eq!(cfg.ffprobe_probe_size, "10MB");
        assert_eq!(cfg.ffprobe_live_analyze_duration, "5s");
        assert_eq!(cfg.ffprobe_live_probe_size, "5MB");
    }

    #[test]
    fn prepare_rejects_ffprobe_duration_without_unit() {
        let mut cfg = MetadataUpdateConfigDto {
            ffprobe_analyze_duration: "10000000".to_string(),
            ..MetadataUpdateConfigDto::default()
        };

        let result = cfg.prepare();
        assert!(result.is_err(), "numeric ffprobe analyze duration without unit must fail");
        let err_text = result.unwrap_err().to_string();
        assert!(err_text.contains("ffprobe_analyze_duration"));
    }
}
