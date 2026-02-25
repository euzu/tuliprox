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
        is_default_metadata_retry_delay, is_default_metadata_worker_idle_timeout, is_false, parse_size_base_2,
    },
};

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
    #[serde(skip)]
    pub queue_log_interval_secs: u64,
    #[serde(
        default = "default_metadata_progress_log_interval",
        skip_serializing_if = "is_default_metadata_progress_log_interval"
    )]
    pub progress_log_interval: String,
    #[serde(skip)]
    pub progress_log_interval_secs: u64,
    #[serde(
        default = "default_metadata_max_resolve_retry_backoff",
        skip_serializing_if = "is_default_metadata_max_resolve_retry_backoff"
    )]
    pub max_resolve_retry_backoff: String,
    #[serde(skip)]
    pub max_resolve_retry_backoff_secs: u64,
    #[serde(
        default = "default_metadata_resolve_min_retry_base",
        skip_serializing_if = "is_default_metadata_resolve_min_retry_base"
    )]
    pub resolve_min_retry_base: String,
    #[serde(skip)]
    pub resolve_min_retry_base_secs: u64,
    #[serde(
        default = "default_metadata_resolve_exhaustion_reset_gap",
        skip_serializing_if = "is_default_metadata_resolve_exhaustion_reset_gap"
    )]
    pub resolve_exhaustion_reset_gap: String,
    #[serde(skip)]
    pub resolve_exhaustion_reset_gap_secs: u64,
    #[serde(default = "default_metadata_probe_cooldown", skip_serializing_if = "is_default_metadata_probe_cooldown")]
    pub probe_cooldown: String,
    #[serde(skip)]
    pub probe_cooldown_secs: u64,
    #[serde(default = "default_metadata_retry_delay", skip_serializing_if = "is_default_metadata_retry_delay")]
    pub retry_delay: String,
    #[serde(skip)]
    pub t_retry_delay_secs: u64,
    #[serde(
        default = "default_metadata_probe_retry_load_retry_delay",
        skip_serializing_if = "is_default_metadata_probe_retry_load_retry_delay"
    )]
    pub probe_retry_load_retry_delay: String,
    #[serde(skip)]
    pub probe_retry_load_retry_delay_secs: u64,
    #[serde(
        default = "default_metadata_worker_idle_timeout",
        skip_serializing_if = "is_default_metadata_worker_idle_timeout"
    )]
    pub worker_idle_timeout: String,
    #[serde(skip)]
    pub worker_idle_timeout_secs: u64,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_1",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_1"
    )]
    pub probe_retry_backoff_step_1: String,
    #[serde(skip)]
    pub probe_retry_backoff_step_1_secs: u64,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_2",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_2"
    )]
    pub probe_retry_backoff_step_2: String,
    #[serde(skip)]
    pub probe_retry_backoff_step_2_secs: u64,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_3",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_3"
    )]
    pub probe_retry_backoff_step_3: String,
    #[serde(skip)]
    pub probe_retry_backoff_step_3_secs: u64,
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
    #[serde(skip)]
    pub t_ffprobe_analyze_duration_micros: u64,
    #[serde(
        default = "default_metadata_ffprobe_probe_size",
        skip_serializing_if = "is_default_metadata_ffprobe_probe_size",
        deserialize_with = "deserialize_as_string"
    )]
    pub ffprobe_probe_size: String,
    #[serde(skip)]
    pub t_ffprobe_probe_size_bytes: u64,
    #[serde(
        default = "default_metadata_ffprobe_live_analyze_duration",
        skip_serializing_if = "is_default_metadata_ffprobe_live_analyze_duration",
        deserialize_with = "deserialize_as_string"
    )]
    pub ffprobe_live_analyze_duration: String,
    #[serde(skip)]
    pub t_ffprobe_live_analyze_duration_micros: u64,
    #[serde(
        default = "default_metadata_ffprobe_live_probe_size",
        skip_serializing_if = "is_default_metadata_ffprobe_live_probe_size",
        deserialize_with = "deserialize_as_string"
    )]
    pub ffprobe_live_probe_size: String,
    #[serde(skip)]
    pub t_ffprobe_live_probe_size_bytes: u64,
}

impl Default for MetadataUpdateConfigDto {
    fn default() -> Self {
        Self {
            queue_log_interval: default_metadata_queue_log_interval(),
            queue_log_interval_secs: 30,
            progress_log_interval: default_metadata_progress_log_interval(),
            progress_log_interval_secs: 15,
            max_resolve_retry_backoff: default_metadata_max_resolve_retry_backoff(),
            max_resolve_retry_backoff_secs: 60 * 60,
            resolve_min_retry_base: default_metadata_resolve_min_retry_base(),
            resolve_min_retry_base_secs: 5,
            resolve_exhaustion_reset_gap: default_metadata_resolve_exhaustion_reset_gap(),
            resolve_exhaustion_reset_gap_secs: 60 * 60,
            probe_cooldown: default_metadata_probe_cooldown(),
            probe_cooldown_secs: 7 * 24 * 60 * 60,
            retry_delay: default_metadata_retry_delay(),
            t_retry_delay_secs: 2,
            probe_retry_load_retry_delay: default_metadata_probe_retry_load_retry_delay(),
            probe_retry_load_retry_delay_secs: 60,
            worker_idle_timeout: default_metadata_worker_idle_timeout(),
            worker_idle_timeout_secs: 60,
            probe_retry_backoff_step_1: default_metadata_probe_retry_backoff_step_1(),
            probe_retry_backoff_step_1_secs: 10 * 60,
            probe_retry_backoff_step_2: default_metadata_probe_retry_backoff_step_2(),
            probe_retry_backoff_step_2_secs: 30 * 60,
            probe_retry_backoff_step_3: default_metadata_probe_retry_backoff_step_3(),
            probe_retry_backoff_step_3_secs: 60 * 60,
            max_attempts_resolve: default_metadata_max_attempts_resolve(),
            max_attempts_probe: default_metadata_max_attempts_probe(),
            backoff_jitter_percent: default_metadata_backoff_jitter_percent(),
            max_queue_size: default_metadata_max_queue_size(),
            ffprobe_enabled: false,
            ffprobe_timeout: None,
            ffprobe_analyze_duration: default_metadata_ffprobe_analyze_duration(),
            t_ffprobe_analyze_duration_micros: 10_000_000,
            ffprobe_probe_size: default_metadata_ffprobe_probe_size(),
            t_ffprobe_probe_size_bytes: 10_000_000,
            ffprobe_live_analyze_duration: default_metadata_ffprobe_live_analyze_duration(),
            t_ffprobe_live_analyze_duration_micros: 5_000_000,
            ffprobe_live_probe_size: default_metadata_ffprobe_live_probe_size(),
            t_ffprobe_live_probe_size_bytes: 5_000_000,
        }
    }
}

impl MetadataUpdateConfigDto {
    pub fn is_empty(&self) -> bool {
        let defaults = Self::default();
        self.queue_log_interval == defaults.queue_log_interval
            && self.progress_log_interval == defaults.progress_log_interval
            && self.max_resolve_retry_backoff == defaults.max_resolve_retry_backoff
            && self.resolve_min_retry_base == defaults.resolve_min_retry_base
            && self.resolve_exhaustion_reset_gap == defaults.resolve_exhaustion_reset_gap
            && self.probe_cooldown == defaults.probe_cooldown
            && self.retry_delay == defaults.retry_delay
            && self.probe_retry_load_retry_delay == defaults.probe_retry_load_retry_delay
            && self.worker_idle_timeout == defaults.worker_idle_timeout
            && self.probe_retry_backoff_step_1 == defaults.probe_retry_backoff_step_1
            && self.probe_retry_backoff_step_2 == defaults.probe_retry_backoff_step_2
            && self.probe_retry_backoff_step_3 == defaults.probe_retry_backoff_step_3
            && self.max_attempts_resolve == defaults.max_attempts_resolve
            && self.max_attempts_probe == defaults.max_attempts_probe
            && self.backoff_jitter_percent == defaults.backoff_jitter_percent
            && self.max_queue_size == defaults.max_queue_size
            && self.ffprobe_enabled == defaults.ffprobe_enabled
            && self.ffprobe_timeout == defaults.ffprobe_timeout
            && self.ffprobe_analyze_duration == defaults.ffprobe_analyze_duration
            && self.ffprobe_probe_size == defaults.ffprobe_probe_size
            && self.ffprobe_live_analyze_duration == defaults.ffprobe_live_analyze_duration
            && self.ffprobe_live_probe_size == defaults.ffprobe_live_probe_size
    }

    pub fn clean(&mut self) {
        if self.ffprobe_timeout.is_some_and(|v| v == DEFAULT_FFPROBE_TIMEOUT_SECS) {
            self.ffprobe_timeout = None;
        }
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.queue_log_interval_secs =
            Self::parse_and_clamp_duration(&self.queue_log_interval, MIN_DURATION_SECS, "queue_log_interval")?;
        self.queue_log_interval = Self::canonicalize_seconds(self.queue_log_interval_secs);

        self.progress_log_interval_secs =
            Self::parse_and_clamp_duration(&self.progress_log_interval, MIN_DURATION_SECS, "progress_log_interval")?;
        self.progress_log_interval = Self::canonicalize_seconds(self.progress_log_interval_secs);

        self.max_resolve_retry_backoff_secs = Self::parse_and_clamp_duration(
            &self.max_resolve_retry_backoff,
            MIN_DURATION_SECS,
            "max_resolve_retry_backoff",
        )?;
        self.max_resolve_retry_backoff = Self::canonicalize_seconds(self.max_resolve_retry_backoff_secs);

        self.resolve_min_retry_base_secs =
            Self::parse_and_clamp_duration(&self.resolve_min_retry_base, MIN_DURATION_SECS, "resolve_min_retry_base")?;
        self.resolve_min_retry_base = Self::canonicalize_seconds(self.resolve_min_retry_base_secs);

        self.resolve_exhaustion_reset_gap_secs = Self::parse_and_clamp_duration(
            &self.resolve_exhaustion_reset_gap,
            MIN_DURATION_SECS,
            "resolve_exhaustion_reset_gap",
        )?;
        self.resolve_exhaustion_reset_gap = Self::canonicalize_seconds(self.resolve_exhaustion_reset_gap_secs);

        self.probe_cooldown_secs =
            Self::parse_and_clamp_duration(&self.probe_cooldown, MIN_DURATION_SECS, "probe_cooldown")?;
        self.probe_cooldown = Self::canonicalize_seconds(self.probe_cooldown_secs);

        self.t_retry_delay_secs = Self::parse_and_clamp_duration(&self.retry_delay, MIN_DURATION_SECS, "retry_delay")?;
        self.retry_delay = Self::canonicalize_seconds(self.t_retry_delay_secs);

        self.probe_retry_load_retry_delay_secs = Self::parse_and_clamp_duration(
            &self.probe_retry_load_retry_delay,
            MIN_DURATION_SECS,
            "probe_retry_load_retry_delay",
        )?;
        self.probe_retry_load_retry_delay = Self::canonicalize_seconds(self.probe_retry_load_retry_delay_secs);

        self.worker_idle_timeout_secs =
            Self::parse_and_clamp_duration(&self.worker_idle_timeout, MIN_DURATION_SECS, "worker_idle_timeout")?;
        self.worker_idle_timeout = Self::canonicalize_seconds(self.worker_idle_timeout_secs);

        self.probe_retry_backoff_step_1_secs = Self::parse_and_clamp_duration(
            &self.probe_retry_backoff_step_1,
            MIN_DURATION_SECS,
            "probe_retry_backoff_step_1",
        )?;
        self.probe_retry_backoff_step_1 = Self::canonicalize_seconds(self.probe_retry_backoff_step_1_secs);

        self.probe_retry_backoff_step_2_secs = Self::parse_and_clamp_duration(
            &self.probe_retry_backoff_step_2,
            MIN_DURATION_SECS,
            "probe_retry_backoff_step_2",
        )?;
        self.probe_retry_backoff_step_2 = Self::canonicalize_seconds(self.probe_retry_backoff_step_2_secs);

        self.probe_retry_backoff_step_3_secs = Self::parse_and_clamp_duration(
            &self.probe_retry_backoff_step_3,
            MIN_DURATION_SECS,
            "probe_retry_backoff_step_3",
        )?;
        self.probe_retry_backoff_step_3 = Self::canonicalize_seconds(self.probe_retry_backoff_step_3_secs);

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
        self.t_ffprobe_analyze_duration_micros = ffprobe_analyze_duration_secs.saturating_mul(1_000_000);

        self.t_ffprobe_probe_size_bytes = parse_size_base_2(&self.ffprobe_probe_size)
            .map_err(|err| crate::error::info_err!("Invalid size for `ffprobe_probe_size`: {err}"))?
            .max(1);
        self.ffprobe_probe_size = Self::canonicalize_size_bytes(self.t_ffprobe_probe_size_bytes);

        let ffprobe_live_analyze_duration_secs = Self::parse_and_clamp_duration_with_required_unit(
            &self.ffprobe_live_analyze_duration,
            MIN_DURATION_SECS,
            "ffprobe_live_analyze_duration",
        )?;
        self.ffprobe_live_analyze_duration = Self::canonicalize_seconds(ffprobe_live_analyze_duration_secs);
        self.t_ffprobe_live_analyze_duration_micros = ffprobe_live_analyze_duration_secs.saturating_mul(1_000_000);

        self.t_ffprobe_live_probe_size_bytes = parse_size_base_2(&self.ffprobe_live_probe_size)
            .map_err(|err| crate::error::info_err!("Invalid size for `ffprobe_live_probe_size`: {err}"))?
            .max(1);
        self.ffprobe_live_probe_size = Self::canonicalize_size_bytes(self.t_ffprobe_live_probe_size_bytes);

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
        if let Ok(seconds) = value.parse::<u64>() {
            return Ok(seconds);
        }

        if value.len() <= 1 {
            return info_err_res!("Invalid duration format for `{field_name}`: {value}");
        }

        let (number_part, unit_part) = value.split_at(value.len() - 1);
        let number = number_part
            .parse::<u64>()
            .map_err(|_| crate::error::info_err!("Invalid duration value for `{field_name}`: {value}"))?;

        match unit_part {
            "s" => Ok(number),
            "m" => Ok(number.saturating_mul(60)),
            "h" => Ok(number.saturating_mul(60 * 60)),
            "d" => Ok(number.saturating_mul(24 * 60 * 60)),
            _ => info_err_res!("Invalid duration unit for `{field_name}`: {unit_part}"),
        }
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
        if bytes.is_multiple_of(1_099_511_628_000) {
            format!("{}TB", bytes / 1_099_511_628_000)
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

        assert_eq!(cfg.queue_log_interval_secs, 60);
        assert_eq!(cfg.progress_log_interval_secs, 7200);
        assert_eq!(cfg.probe_cooldown_secs, 86_400);
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

        assert_eq!(cfg.queue_log_interval_secs, 1);
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
