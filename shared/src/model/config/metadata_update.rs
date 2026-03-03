use crate::{
    error::TuliproxError,
    info_err_res,
    utils::{
        default_metadata_backoff_jitter_percent, default_metadata_ffprobe_analyze_duration,
        default_metadata_ffprobe_live_analyze_duration, default_metadata_ffprobe_live_probe_size,
        default_metadata_ffprobe_probe_size, default_metadata_max_attempts_probe,
        default_metadata_max_attempts_resolve, default_metadata_max_queue_size,
        default_metadata_max_resolve_retry_backoff, default_metadata_path, default_metadata_probe_cooldown,
        default_metadata_probe_retry_backoff_step_1, default_metadata_probe_retry_backoff_step_2,
        default_metadata_probe_retry_backoff_step_3, default_metadata_probe_retry_load_retry_delay,
        default_metadata_progress_log_interval, default_metadata_queue_log_interval,
        default_metadata_resolve_exhaustion_reset_gap, default_metadata_resolve_min_retry_base,
        default_metadata_retry_delay, default_metadata_tmdb_cooldown, default_metadata_worker_idle_timeout,
        default_tmdb_api_key, default_tmdb_cache_duration_days, default_tmdb_language, default_tmdb_rate_limit_ms,
        deserialize_as_string, is_default_metadata_backoff_jitter_percent,
        is_default_metadata_ffprobe_analyze_duration, is_default_metadata_ffprobe_live_analyze_duration,
        is_default_metadata_ffprobe_live_probe_size, is_default_metadata_ffprobe_probe_size,
        is_default_metadata_max_attempts_probe, is_default_metadata_max_attempts_resolve,
        is_default_metadata_max_queue_size, is_default_metadata_max_resolve_retry_backoff, is_default_metadata_path,
        is_default_metadata_probe_cooldown, is_default_metadata_probe_retry_backoff_step_1,
        is_default_metadata_probe_retry_backoff_step_2, is_default_metadata_probe_retry_backoff_step_3,
        is_default_metadata_probe_retry_load_retry_delay, is_default_metadata_progress_log_interval,
        is_default_metadata_queue_log_interval, is_default_metadata_resolve_exhaustion_reset_gap,
        is_default_metadata_resolve_min_retry_base, is_default_metadata_retry_delay, is_default_metadata_tmdb_cooldown,
        is_default_metadata_worker_idle_timeout, is_default_tmdb_cache_duration_days, is_default_tmdb_language,
        is_default_tmdb_rate_limit_ms, is_false, is_tmdb_default_api_key, parse_duration_seconds, parse_size_base_2,
        TMDB_API_KEY,
    },
};

const MIN_DURATION_SECS: u64 = 1;
const MIN_ATTEMPTS: u8 = 1;
const MAX_JITTER_PERCENT: u8 = 95;
const MIN_QUEUE_SIZE: usize = 1;
const DEFAULT_FFPROBE_TIMEOUT_SECS: u64 = 60;

fn default_ffprobe_timeout_secs() -> Option<u64> { Some(DEFAULT_FFPROBE_TIMEOUT_SECS) }
fn is_default_ffprobe_timeout(timeout: &Option<u64>) -> bool {
    timeout.is_none_or(|value| value == DEFAULT_FFPROBE_TIMEOUT_SECS)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetadataUpdateConfigDto {
    #[serde(default = "default_metadata_path", skip_serializing_if = "is_default_metadata_path")]
    pub cache_path: String,
    #[serde(default, skip_serializing_if = "MetadataLogConfigDto::is_empty")]
    pub log: MetadataLogConfigDto,
    #[serde(default, skip_serializing_if = "ResolveConfigDto::is_empty")]
    pub resolve: ResolveConfigDto,
    #[serde(default, skip_serializing_if = "ProbeConfigDto::is_empty")]
    pub probe: ProbeConfigDto,
    #[serde(default, skip_serializing_if = "FfprobeConfigDto::is_empty")]
    pub ffprobe: FfprobeConfigDto,
    #[serde(default, skip_serializing_if = "TmdbConfigDto::is_empty")]
    pub tmdb: TmdbConfigDto,
    #[serde(default = "default_metadata_retry_delay", skip_serializing_if = "is_default_metadata_retry_delay")]
    pub retry_delay: String,
    #[serde(
        default = "default_metadata_worker_idle_timeout",
        skip_serializing_if = "is_default_metadata_worker_idle_timeout"
    )]
    pub worker_idle_timeout: String,
    #[serde(default = "default_metadata_max_queue_size", skip_serializing_if = "is_default_metadata_max_queue_size")]
    pub max_queue_size: usize,
}

impl Default for MetadataUpdateConfigDto {
    fn default() -> Self {
        Self {
            cache_path: default_metadata_path(),
            log: MetadataLogConfigDto::default(),
            resolve: ResolveConfigDto::default(),
            probe: ProbeConfigDto::default(),
            ffprobe: FfprobeConfigDto::default(),
            tmdb: TmdbConfigDto::default(),
            retry_delay: default_metadata_retry_delay(),
            worker_idle_timeout: default_metadata_worker_idle_timeout(),
            max_queue_size: default_metadata_max_queue_size(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetadataLogConfigDto {
    #[serde(
        default = "default_metadata_queue_log_interval",
        skip_serializing_if = "is_default_metadata_queue_log_interval"
    )]
    pub queue_interval: String,
    #[serde(
        default = "default_metadata_progress_log_interval",
        skip_serializing_if = "is_default_metadata_progress_log_interval"
    )]
    pub progress_interval: String,
}

impl Default for MetadataLogConfigDto {
    fn default() -> Self {
        Self {
            queue_interval: default_metadata_queue_log_interval(),
            progress_interval: default_metadata_progress_log_interval(),
        }
    }
}

impl MetadataLogConfigDto {
    pub fn is_empty(&self) -> bool {
        self.queue_interval == default_metadata_queue_log_interval()
            && self.progress_interval == default_metadata_progress_log_interval()
    }

    fn prepare(&mut self) -> Result<(), TuliproxError> {
        let queue_interval_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.queue_interval,
            MIN_DURATION_SECS,
            "log.queue_interval",
        )?;
        self.queue_interval = MetadataUpdateConfigDto::canonicalize_seconds(queue_interval_secs);

        let progress_interval_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.progress_interval,
            MIN_DURATION_SECS,
            "log.progress_interval",
        )?;
        self.progress_interval = MetadataUpdateConfigDto::canonicalize_seconds(progress_interval_secs);

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResolveConfigDto {
    #[serde(
        default = "default_metadata_max_resolve_retry_backoff",
        skip_serializing_if = "is_default_metadata_max_resolve_retry_backoff"
    )]
    pub max_retry_backoff: String,
    #[serde(
        default = "default_metadata_resolve_min_retry_base",
        skip_serializing_if = "is_default_metadata_resolve_min_retry_base"
    )]
    pub min_retry_base: String,
    #[serde(
        default = "default_metadata_resolve_exhaustion_reset_gap",
        skip_serializing_if = "is_default_metadata_resolve_exhaustion_reset_gap"
    )]
    pub exhaustion_reset_gap: String,
    #[serde(
        default = "default_metadata_max_attempts_resolve",
        skip_serializing_if = "is_default_metadata_max_attempts_resolve"
    )]
    pub max_attempts: u8,
}

impl Default for ResolveConfigDto {
    fn default() -> Self {
        Self {
            max_retry_backoff: default_metadata_max_resolve_retry_backoff(),
            min_retry_base: default_metadata_resolve_min_retry_base(),
            exhaustion_reset_gap: default_metadata_resolve_exhaustion_reset_gap(),
            max_attempts: default_metadata_max_attempts_resolve(),
        }
    }
}

impl ResolveConfigDto {
    pub fn is_empty(&self) -> bool {
        self.max_retry_backoff == default_metadata_max_resolve_retry_backoff()
            && self.min_retry_base == default_metadata_resolve_min_retry_base()
            && self.exhaustion_reset_gap == default_metadata_resolve_exhaustion_reset_gap()
            && self.max_attempts == default_metadata_max_attempts_resolve()
    }

    fn prepare(&mut self) -> Result<(), TuliproxError> {
        let max_retry_backoff_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.max_retry_backoff,
            MIN_DURATION_SECS,
            "resolve.max_retry_backoff",
        )?;
        self.max_retry_backoff = MetadataUpdateConfigDto::canonicalize_seconds(max_retry_backoff_secs);

        let min_retry_base_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.min_retry_base,
            MIN_DURATION_SECS,
            "resolve.min_retry_base",
        )?;
        self.min_retry_base = MetadataUpdateConfigDto::canonicalize_seconds(min_retry_base_secs);

        let exhaustion_reset_gap_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.exhaustion_reset_gap,
            MIN_DURATION_SECS,
            "resolve.exhaustion_reset_gap",
        )?;
        self.exhaustion_reset_gap = MetadataUpdateConfigDto::canonicalize_seconds(exhaustion_reset_gap_secs);

        self.max_attempts = self.max_attempts.max(MIN_ATTEMPTS);

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfigDto {
    #[serde(default = "default_metadata_probe_cooldown", skip_serializing_if = "is_default_metadata_probe_cooldown")]
    pub cooldown: String,
    #[serde(
        default = "default_metadata_probe_retry_load_retry_delay",
        skip_serializing_if = "is_default_metadata_probe_retry_load_retry_delay"
    )]
    pub retry_load_retry_delay: String,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_1",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_1"
    )]
    pub retry_backoff_step_1: String,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_2",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_2"
    )]
    pub retry_backoff_step_2: String,
    #[serde(
        default = "default_metadata_probe_retry_backoff_step_3",
        skip_serializing_if = "is_default_metadata_probe_retry_backoff_step_3"
    )]
    pub retry_backoff_step_3: String,
    #[serde(
        default = "default_metadata_max_attempts_probe",
        skip_serializing_if = "is_default_metadata_max_attempts_probe"
    )]
    pub max_attempts: u8,
    #[serde(
        default = "default_metadata_backoff_jitter_percent",
        skip_serializing_if = "is_default_metadata_backoff_jitter_percent"
    )]
    pub backoff_jitter_percent: u8,
}

impl Default for ProbeConfigDto {
    fn default() -> Self {
        Self {
            cooldown: default_metadata_probe_cooldown(),
            retry_load_retry_delay: default_metadata_probe_retry_load_retry_delay(),
            retry_backoff_step_1: default_metadata_probe_retry_backoff_step_1(),
            retry_backoff_step_2: default_metadata_probe_retry_backoff_step_2(),
            retry_backoff_step_3: default_metadata_probe_retry_backoff_step_3(),
            max_attempts: default_metadata_max_attempts_probe(),
            backoff_jitter_percent: default_metadata_backoff_jitter_percent(),
        }
    }
}

impl ProbeConfigDto {
    pub fn is_empty(&self) -> bool {
        self.cooldown == default_metadata_probe_cooldown()
            && self.retry_load_retry_delay == default_metadata_probe_retry_load_retry_delay()
            && self.retry_backoff_step_1 == default_metadata_probe_retry_backoff_step_1()
            && self.retry_backoff_step_2 == default_metadata_probe_retry_backoff_step_2()
            && self.retry_backoff_step_3 == default_metadata_probe_retry_backoff_step_3()
            && self.max_attempts == default_metadata_max_attempts_probe()
            && self.backoff_jitter_percent == default_metadata_backoff_jitter_percent()
    }

    fn prepare(&mut self) -> Result<(), TuliproxError> {
        let cooldown_secs =
            MetadataUpdateConfigDto::parse_and_clamp_duration(&self.cooldown, MIN_DURATION_SECS, "probe.cooldown")?;
        self.cooldown = MetadataUpdateConfigDto::canonicalize_seconds(cooldown_secs);

        let retry_load_retry_delay_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.retry_load_retry_delay,
            MIN_DURATION_SECS,
            "probe.retry_load_retry_delay",
        )?;
        self.retry_load_retry_delay = MetadataUpdateConfigDto::canonicalize_seconds(retry_load_retry_delay_secs);

        let retry_backoff_step_1_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.retry_backoff_step_1,
            MIN_DURATION_SECS,
            "probe.retry_backoff_step_1",
        )?;
        self.retry_backoff_step_1 = MetadataUpdateConfigDto::canonicalize_seconds(retry_backoff_step_1_secs);

        let retry_backoff_step_2_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.retry_backoff_step_2,
            MIN_DURATION_SECS,
            "probe.retry_backoff_step_2",
        )?;
        self.retry_backoff_step_2 = MetadataUpdateConfigDto::canonicalize_seconds(retry_backoff_step_2_secs);

        let retry_backoff_step_3_secs = MetadataUpdateConfigDto::parse_and_clamp_duration(
            &self.retry_backoff_step_3,
            MIN_DURATION_SECS,
            "probe.retry_backoff_step_3",
        )?;
        self.retry_backoff_step_3 = MetadataUpdateConfigDto::canonicalize_seconds(retry_backoff_step_3_secs);

        self.max_attempts = self.max_attempts.max(MIN_ATTEMPTS);
        self.backoff_jitter_percent = self.backoff_jitter_percent.min(MAX_JITTER_PERCENT);

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FfprobeConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(default = "default_ffprobe_timeout_secs", skip_serializing_if = "is_default_ffprobe_timeout")]
    pub timeout: Option<u64>,
    #[serde(
        default = "default_metadata_ffprobe_analyze_duration",
        skip_serializing_if = "is_default_metadata_ffprobe_analyze_duration",
        deserialize_with = "deserialize_as_string"
    )]
    pub analyze_duration: String,
    #[serde(
        default = "default_metadata_ffprobe_probe_size",
        skip_serializing_if = "is_default_metadata_ffprobe_probe_size",
        deserialize_with = "deserialize_as_string"
    )]
    pub probe_size: String,
    #[serde(
        default = "default_metadata_ffprobe_live_analyze_duration",
        skip_serializing_if = "is_default_metadata_ffprobe_live_analyze_duration",
        deserialize_with = "deserialize_as_string"
    )]
    pub live_analyze_duration: String,
    #[serde(
        default = "default_metadata_ffprobe_live_probe_size",
        skip_serializing_if = "is_default_metadata_ffprobe_live_probe_size",
        deserialize_with = "deserialize_as_string"
    )]
    pub live_probe_size: String,
}

impl Default for FfprobeConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: default_ffprobe_timeout_secs(),
            analyze_duration: default_metadata_ffprobe_analyze_duration(),
            probe_size: default_metadata_ffprobe_probe_size(),
            live_analyze_duration: default_metadata_ffprobe_live_analyze_duration(),
            live_probe_size: default_metadata_ffprobe_live_probe_size(),
        }
    }
}

impl FfprobeConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && is_default_ffprobe_timeout(&self.timeout)
            && self.analyze_duration == default_metadata_ffprobe_analyze_duration()
            && self.probe_size == default_metadata_ffprobe_probe_size()
            && self.live_analyze_duration == default_metadata_ffprobe_live_analyze_duration()
            && self.live_probe_size == default_metadata_ffprobe_live_probe_size()
    }

    pub fn clean(&mut self) {
        self.enabled = false;
        self.timeout = default_ffprobe_timeout_secs();
        self.analyze_duration = default_metadata_ffprobe_analyze_duration();
        self.probe_size = default_metadata_ffprobe_probe_size();
        self.live_analyze_duration = default_metadata_ffprobe_live_analyze_duration();
        self.live_probe_size = default_metadata_ffprobe_live_probe_size();
    }

    fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.timeout = self.timeout.map(|timeout| timeout.max(MIN_DURATION_SECS));

        let analyze_duration_secs = MetadataUpdateConfigDto::parse_and_clamp_duration_with_required_unit(
            &self.analyze_duration,
            MIN_DURATION_SECS,
            "ffprobe.analyze_duration",
        )?;
        self.analyze_duration = MetadataUpdateConfigDto::canonicalize_seconds(analyze_duration_secs);

        let probe_size_bytes = parse_size_base_2(&self.probe_size)
            .map_err(|err| crate::error::info_err!("Invalid size for `ffprobe.probe_size`: {err}"))?
            .max(1);
        self.probe_size = MetadataUpdateConfigDto::canonicalize_size_bytes(probe_size_bytes);

        let live_analyze_duration_secs = MetadataUpdateConfigDto::parse_and_clamp_duration_with_required_unit(
            &self.live_analyze_duration,
            MIN_DURATION_SECS,
            "ffprobe.live_analyze_duration",
        )?;
        self.live_analyze_duration = MetadataUpdateConfigDto::canonicalize_seconds(live_analyze_duration_secs);

        let live_probe_size_bytes = parse_size_base_2(&self.live_probe_size)
            .map_err(|err| crate::error::info_err!("Invalid size for `ffprobe.live_probe_size`: {err}"))?
            .max(1);
        self.live_probe_size = MetadataUpdateConfigDto::canonicalize_size_bytes(live_probe_size_bytes);

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TmdbConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tmdb_api_key", skip_serializing_if = "is_tmdb_default_api_key")]
    pub api_key: Option<String>,
    #[serde(default = "default_tmdb_rate_limit_ms", skip_serializing_if = "is_default_tmdb_rate_limit_ms")]
    pub rate_limit_ms: u64,
    #[serde(default = "default_tmdb_cache_duration_days", skip_serializing_if = "is_default_tmdb_cache_duration_days")]
    pub cache_duration_days: u32,
    #[serde(default = "default_tmdb_language", skip_serializing_if = "is_default_tmdb_language")]
    pub language: String,
    #[serde(default = "default_metadata_tmdb_cooldown", skip_serializing_if = "is_default_metadata_tmdb_cooldown")]
    pub cooldown: String,
}

impl Default for TmdbConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: default_tmdb_api_key(),
            rate_limit_ms: default_tmdb_rate_limit_ms(),
            cache_duration_days: default_tmdb_cache_duration_days(),
            language: default_tmdb_language(),
            cooldown: default_metadata_tmdb_cooldown(),
        }
    }
}

impl TmdbConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && self.api_key.as_ref().is_none_or(|api_key| api_key == TMDB_API_KEY)
            && self.rate_limit_ms == default_tmdb_rate_limit_ms()
            && self.cache_duration_days == default_tmdb_cache_duration_days()
            && self.language == default_tmdb_language()
            && self.cooldown == default_metadata_tmdb_cooldown()
    }

    fn clean(&mut self) {
        self.api_key = self.api_key.take().and_then(|api_key| {
            let trimmed = api_key.trim();
            if trimmed.is_empty() || trimmed == TMDB_API_KEY {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
    }

    fn prepare(&mut self) -> Result<(), TuliproxError> {
        let cooldown_secs =
            MetadataUpdateConfigDto::parse_and_clamp_duration(&self.cooldown, MIN_DURATION_SECS, "tmdb.cooldown")?;
        self.cooldown = MetadataUpdateConfigDto::canonicalize_seconds(cooldown_secs);
        Ok(())
    }
}

impl MetadataUpdateConfigDto {
    pub fn is_empty(&self) -> bool {
        is_default_metadata_path(&self.cache_path)
            && self.log.is_empty()
            && self.resolve.is_empty()
            && self.probe.is_empty()
            && self.ffprobe.is_empty()
            && self.tmdb.is_empty()
            && self.retry_delay == default_metadata_retry_delay()
            && self.worker_idle_timeout == default_metadata_worker_idle_timeout()
            && self.max_queue_size == default_metadata_max_queue_size()
    }

    pub fn clean(&mut self) {
        if self.cache_path.trim().is_empty() {
            self.cache_path = default_metadata_path();
        }
        self.ffprobe.clean();
        self.tmdb.clean();
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if self.cache_path.trim().is_empty() {
            return info_err_res!("metadata_update.cache_path cannot be empty");
        }
        self.log.prepare()?;
        self.resolve.prepare()?;
        self.probe.prepare()?;
        self.ffprobe.prepare()?;
        self.tmdb.prepare()?;

        let retry_delay_secs = Self::parse_and_clamp_duration(&self.retry_delay, MIN_DURATION_SECS, "retry_delay")?;
        self.retry_delay = Self::canonicalize_seconds(retry_delay_secs);

        let worker_idle_timeout_secs =
            Self::parse_and_clamp_duration(&self.worker_idle_timeout, MIN_DURATION_SECS, "worker_idle_timeout")?;
        self.worker_idle_timeout = Self::canonicalize_seconds(worker_idle_timeout_secs);

        self.max_queue_size = self.max_queue_size.max(MIN_QUEUE_SIZE);

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
    fn prepare_keeps_default_config_empty() {
        let mut cfg = MetadataUpdateConfigDto::default();
        cfg.prepare().expect("metadata update config defaults should be valid");
        assert!(cfg.is_empty());
    }

    #[test]
    fn prepare_parses_duration_suffixes() {
        let mut cfg = MetadataUpdateConfigDto::default();
        cfg.log.queue_interval = "1m".to_string();
        cfg.log.progress_interval = "2h".to_string();
        cfg.probe.cooldown = "1d".to_string();
        cfg.tmdb.cooldown = "2d".to_string();

        cfg.prepare().expect("metadata update config should parse duration values");

        assert_eq!(cfg.log.queue_interval, "1m");
        assert_eq!(cfg.log.progress_interval, "2h");
        assert_eq!(cfg.probe.cooldown, "1d");
        assert_eq!(cfg.tmdb.cooldown, "2d");
    }

    #[test]
    fn prepare_clamps_minimum_values() {
        let mut cfg = MetadataUpdateConfigDto::default();
        cfg.log.queue_interval = "0".to_string();
        cfg.resolve.max_attempts = 0;
        cfg.probe.max_attempts = 0;
        cfg.max_queue_size = 0;
        cfg.ffprobe.timeout = Some(0);
        cfg.ffprobe.analyze_duration = "0s".to_string();
        cfg.ffprobe.probe_size = "0".to_string();

        cfg.prepare().expect("metadata update config should clamp minimum values");

        assert_eq!(cfg.log.queue_interval, "1s");
        assert_eq!(cfg.resolve.max_attempts, 1);
        assert_eq!(cfg.probe.max_attempts, 1);
        assert_eq!(cfg.max_queue_size, 1);
        assert_eq!(cfg.ffprobe.timeout, Some(1));
        assert_eq!(cfg.ffprobe.analyze_duration, "1s");
        assert_eq!(cfg.ffprobe.probe_size, "1B");
    }

    #[test]
    fn prepare_rejects_invalid_duration_unit() {
        let mut cfg = MetadataUpdateConfigDto::default();
        cfg.log.queue_interval = "1w".to_string();

        let result = cfg.prepare();
        assert!(result.is_err(), "invalid duration unit must fail");
    }

    #[test]
    fn prepare_canonicalizes_to_larger_units() {
        let mut cfg = MetadataUpdateConfigDto::default();
        cfg.probe.cooldown = "604800".to_string();
        cfg.tmdb.cooldown = "259200".to_string();
        cfg.worker_idle_timeout = "60".to_string();
        cfg.probe.retry_backoff_step_3 = "3600".to_string();
        cfg.ffprobe.analyze_duration = "10s".to_string();
        cfg.ffprobe.probe_size = "10485760".to_string();
        cfg.ffprobe.live_analyze_duration = "5s".to_string();
        cfg.ffprobe.live_probe_size = "5242880".to_string();

        cfg.prepare().expect("metadata update config should canonicalize durations");

        assert_eq!(cfg.probe.cooldown, "7d");
        assert_eq!(cfg.tmdb.cooldown, "3d");
        assert_eq!(cfg.worker_idle_timeout, "1m");
        assert_eq!(cfg.probe.retry_backoff_step_3, "1h");
        assert_eq!(cfg.ffprobe.analyze_duration, "10s");
        assert_eq!(cfg.ffprobe.probe_size, "10MB");
        assert_eq!(cfg.ffprobe.live_analyze_duration, "5s");
        assert_eq!(cfg.ffprobe.live_probe_size, "5MB");
    }

    #[test]
    fn prepare_rejects_ffprobe_duration_without_unit() {
        let mut cfg = MetadataUpdateConfigDto::default();
        cfg.ffprobe.analyze_duration = "10000000".to_string();

        let result = cfg.prepare();
        assert!(result.is_err(), "numeric ffprobe analyze duration without unit must fail");
        let err_text = result.expect_err("validation should fail").to_string();
        assert!(err_text.contains("ffprobe.analyze_duration"));
    }
}
