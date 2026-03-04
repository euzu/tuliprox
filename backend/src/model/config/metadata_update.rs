use crate::model::macros;
use shared::model::{
    FfprobeConfigDto, MetadataLogConfigDto, MetadataUpdateConfigDto, ProbeConfigDto, ResolveConfigDto, TmdbConfigDto,
};
use shared::utils::{default_metadata_ffprobe_analyze_duration, default_metadata_ffprobe_live_analyze_duration, default_metadata_ffprobe_live_probe_size, default_metadata_ffprobe_probe_size, default_metadata_max_resolve_retry_backoff, default_metadata_probe_cooldown, default_metadata_probe_retry_backoff_step_1, default_metadata_probe_retry_backoff_step_2, default_metadata_probe_retry_backoff_step_3, default_metadata_probe_retry_load_retry_delay, default_metadata_progress_log_interval, default_metadata_queue_log_interval, default_metadata_resolve_exhaustion_reset_gap, default_metadata_resolve_min_retry_base, default_metadata_retry_delay, default_metadata_tmdb_cooldown, default_metadata_worker_idle_timeout, default_tmdb_cache_duration_days, default_tmdb_language, default_tmdb_match_threshold, default_tmdb_rate_limit_ms, parse_duration_seconds, parse_size_base_2};

#[derive(Debug, Clone)]
pub struct MetadataUpdateConfig {
    pub cache_path: String,
    pub log: MetadataLogConfig,
    pub resolve: ResolveConfig,
    pub probe: ProbeConfig,
    pub ffprobe: FfprobeConfig,
    pub tmdb: TmdbConfig,
    pub retry_delay: String,
    pub retry_delay_secs: u64,
    pub worker_idle_timeout: String,
    pub worker_idle_timeout_secs: u64,
    pub max_queue_size: usize,
}

#[derive(Debug, Clone)]
pub struct MetadataLogConfig {
    pub queue_interval: String,
    pub queue_interval_secs: u64,
    pub progress_interval: String,
    pub progress_interval_secs: u64,
}

#[derive(Debug, Clone)]
pub struct ResolveConfig {
    pub max_retry_backoff: String,
    pub max_retry_backoff_secs: u64,
    pub min_retry_base: String,
    pub min_retry_base_secs: u64,
    pub exhaustion_reset_gap: String,
    pub exhaustion_reset_gap_secs: u64,
    pub max_attempts: u8,
}

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub cooldown: String,
    pub cooldown_secs: u64,
    pub retry_load_retry_delay: String,
    pub retry_load_retry_delay_secs: u64,
    pub retry_backoff_step_1: String,
    pub retry_backoff_step_1_secs: u64,
    pub retry_backoff_step_2: String,
    pub retry_backoff_step_2_secs: u64,
    pub retry_backoff_step_3: String,
    pub retry_backoff_step_3_secs: u64,
    pub max_attempts: u8,
    pub backoff_jitter_percent: u8,
}

#[derive(Debug, Clone)]
pub struct FfprobeConfig {
    pub enabled: bool,
    pub timeout: Option<u64>,
    pub analyze_duration: String,
    pub analyze_duration_micros: u64,
    pub probe_size: String,
    pub probe_size_bytes: u64,
    pub live_analyze_duration: String,
    pub live_analyze_duration_micros: u64,
    pub live_probe_size: String,
    pub live_probe_size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct TmdbConfig {
    pub enabled: bool,
    pub api_key: Option<String>,
    pub rate_limit_ms: u64,
    pub cache_duration_days: u32,
    pub language: String,
    pub cooldown: String,
    pub cooldown_secs: u64,
    pub match_threshold: u16,
}

impl Default for TmdbConfig {
    fn default() -> Self {
        let cooldown = default_metadata_tmdb_cooldown();
        Self {
            enabled: false,
            api_key: None,
            rate_limit_ms: default_tmdb_rate_limit_ms(),
            cache_duration_days: default_tmdb_cache_duration_days(),
            language: default_tmdb_language(),
            cooldown_secs: parse_duration_or_default(&cooldown, &default_metadata_tmdb_cooldown(), false),
            cooldown,
            match_threshold: default_tmdb_match_threshold(),
        }
    }
}

impl Default for MetadataUpdateConfig {
    fn default() -> Self {
        Self::from(&MetadataUpdateConfigDto::default())
    }
}

macros::from_impl!(MetadataUpdateConfig);

fn parse_duration_or_default(value: &str, default_value: &str, require_unit: bool) -> u64 {
    parse_duration_seconds(value, require_unit)
        .or_else(|| parse_duration_seconds(default_value, require_unit))
        .map_or(1, |v| v.max(1))
}

fn parse_size_or_default(value: &str, default_value: &str) -> u64 {
    parse_size_base_2(value)
        .ok()
        .map(|v| v.max(1))
        .or_else(|| parse_size_base_2(default_value).ok().map(|v| v.max(1)))
        .unwrap_or(1)
}

impl From<&MetadataUpdateConfigDto> for MetadataUpdateConfig {
    fn from(dto: &MetadataUpdateConfigDto) -> Self {
        // ConfigDto::prepare() should already normalize/validate, but conversion stays defensive.
        let mut normalized = dto.clone();
        if normalized.prepare().is_err() {
            normalized = MetadataUpdateConfigDto::default();
            let _ = normalized.prepare();
        }

        Self {
            cache_path: normalized.cache_path,
            log: MetadataLogConfig::from(&normalized.log),
            resolve: ResolveConfig::from(&normalized.resolve),
            probe: ProbeConfig::from(&normalized.probe),
            ffprobe: FfprobeConfig::from(&normalized.ffprobe),
            tmdb: TmdbConfig::from(&normalized.tmdb),
            retry_delay_secs: parse_duration_or_default(
                &normalized.retry_delay,
                &default_metadata_retry_delay(),
                false,
            ),
            retry_delay: normalized.retry_delay,
            worker_idle_timeout_secs: parse_duration_or_default(
                &normalized.worker_idle_timeout,
                &default_metadata_worker_idle_timeout(),
                false,
            ),
            worker_idle_timeout: normalized.worker_idle_timeout,
            max_queue_size: normalized.max_queue_size.max(1),
        }
    }
}

impl From<&MetadataUpdateConfig> for MetadataUpdateConfigDto {
    fn from(instance: &MetadataUpdateConfig) -> Self {
        Self {
            cache_path: instance.cache_path.clone(),
            log: MetadataLogConfigDto::from(&instance.log),
            resolve: ResolveConfigDto::from(&instance.resolve),
            probe: ProbeConfigDto::from(&instance.probe),
            ffprobe: FfprobeConfigDto::from(&instance.ffprobe),
            tmdb: TmdbConfigDto::from(&instance.tmdb),
            retry_delay: instance.retry_delay.clone(),
            worker_idle_timeout: instance.worker_idle_timeout.clone(),
            max_queue_size: instance.max_queue_size,
        }
    }
}

impl From<&MetadataLogConfigDto> for MetadataLogConfig {
    fn from(dto: &MetadataLogConfigDto) -> Self {
        Self {
            queue_interval_secs: parse_duration_or_default(
                &dto.queue_interval,
                &default_metadata_queue_log_interval(),
                false,
            ),
            queue_interval: dto.queue_interval.clone(),
            progress_interval_secs: parse_duration_or_default(
                &dto.progress_interval,
                &default_metadata_progress_log_interval(),
                false,
            ),
            progress_interval: dto.progress_interval.clone(),
        }
    }
}

impl From<&MetadataLogConfig> for MetadataLogConfigDto {
    fn from(instance: &MetadataLogConfig) -> Self {
        Self {
            queue_interval: instance.queue_interval.clone(),
            progress_interval: instance.progress_interval.clone(),
        }
    }
}

impl From<&ResolveConfigDto> for ResolveConfig {
    fn from(dto: &ResolveConfigDto) -> Self {
        Self {
            max_retry_backoff_secs: parse_duration_or_default(
                &dto.max_retry_backoff,
                &default_metadata_max_resolve_retry_backoff(),
                false,
            ),
            max_retry_backoff: dto.max_retry_backoff.clone(),
            min_retry_base_secs: parse_duration_or_default(
                &dto.min_retry_base,
                &default_metadata_resolve_min_retry_base(),
                false,
            ),
            min_retry_base: dto.min_retry_base.clone(),
            exhaustion_reset_gap_secs: parse_duration_or_default(
                &dto.exhaustion_reset_gap,
                &default_metadata_resolve_exhaustion_reset_gap(),
                false,
            ),
            exhaustion_reset_gap: dto.exhaustion_reset_gap.clone(),
            max_attempts: dto.max_attempts.max(1),
        }
    }
}

impl From<&ResolveConfig> for ResolveConfigDto {
    fn from(instance: &ResolveConfig) -> Self {
        Self {
            max_retry_backoff: instance.max_retry_backoff.clone(),
            min_retry_base: instance.min_retry_base.clone(),
            exhaustion_reset_gap: instance.exhaustion_reset_gap.clone(),
            max_attempts: instance.max_attempts,
        }
    }
}

impl From<&ProbeConfigDto> for ProbeConfig {
    fn from(dto: &ProbeConfigDto) -> Self {
        Self {
            cooldown_secs: parse_duration_or_default(&dto.cooldown, &default_metadata_probe_cooldown(), false),
            cooldown: dto.cooldown.clone(),
            retry_load_retry_delay_secs: parse_duration_or_default(
                &dto.retry_load_retry_delay,
                &default_metadata_probe_retry_load_retry_delay(),
                false,
            ),
            retry_load_retry_delay: dto.retry_load_retry_delay.clone(),
            retry_backoff_step_1_secs: parse_duration_or_default(
                &dto.retry_backoff_step_1,
                &default_metadata_probe_retry_backoff_step_1(),
                false,
            ),
            retry_backoff_step_1: dto.retry_backoff_step_1.clone(),
            retry_backoff_step_2_secs: parse_duration_or_default(
                &dto.retry_backoff_step_2,
                &default_metadata_probe_retry_backoff_step_2(),
                false,
            ),
            retry_backoff_step_2: dto.retry_backoff_step_2.clone(),
            retry_backoff_step_3_secs: parse_duration_or_default(
                &dto.retry_backoff_step_3,
                &default_metadata_probe_retry_backoff_step_3(),
                false,
            ),
            retry_backoff_step_3: dto.retry_backoff_step_3.clone(),
            max_attempts: dto.max_attempts.max(1),
            backoff_jitter_percent: dto.backoff_jitter_percent.min(95),
        }
    }
}

impl From<&ProbeConfig> for ProbeConfigDto {
    fn from(instance: &ProbeConfig) -> Self {
        Self {
            cooldown: instance.cooldown.clone(),
            retry_load_retry_delay: instance.retry_load_retry_delay.clone(),
            retry_backoff_step_1: instance.retry_backoff_step_1.clone(),
            retry_backoff_step_2: instance.retry_backoff_step_2.clone(),
            retry_backoff_step_3: instance.retry_backoff_step_3.clone(),
            max_attempts: instance.max_attempts,
            backoff_jitter_percent: instance.backoff_jitter_percent,
        }
    }
}

impl From<&FfprobeConfigDto> for FfprobeConfig {
    fn from(dto: &FfprobeConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            timeout: dto.timeout,
            analyze_duration_micros: parse_duration_or_default(
                &dto.analyze_duration,
                &default_metadata_ffprobe_analyze_duration(),
                true,
            )
            .saturating_mul(1_000_000),
            analyze_duration: dto.analyze_duration.clone(),
            probe_size_bytes: parse_size_or_default(&dto.probe_size, &default_metadata_ffprobe_probe_size()),
            probe_size: dto.probe_size.clone(),
            live_analyze_duration_micros: parse_duration_or_default(
                &dto.live_analyze_duration,
                &default_metadata_ffprobe_live_analyze_duration(),
                true,
            )
            .saturating_mul(1_000_000),
            live_analyze_duration: dto.live_analyze_duration.clone(),
            live_probe_size_bytes: parse_size_or_default(
                &dto.live_probe_size,
                &default_metadata_ffprobe_live_probe_size(),
            ),
            live_probe_size: dto.live_probe_size.clone(),
        }
    }
}

impl From<&FfprobeConfig> for FfprobeConfigDto {
    fn from(instance: &FfprobeConfig) -> Self {
        Self {
            enabled: instance.enabled,
            timeout: instance.timeout,
            analyze_duration: instance.analyze_duration.clone(),
            probe_size: instance.probe_size.clone(),
            live_analyze_duration: instance.live_analyze_duration.clone(),
            live_probe_size: instance.live_probe_size.clone(),
        }
    }
}

impl From<&TmdbConfigDto> for TmdbConfig {
    fn from(dto: &TmdbConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            api_key: dto.api_key.clone(),
            rate_limit_ms: dto.rate_limit_ms,
            cache_duration_days: dto.cache_duration_days,
            language: dto.language.clone(),
            cooldown_secs: parse_duration_or_default(&dto.cooldown, &default_metadata_tmdb_cooldown(), false),
            cooldown: dto.cooldown.clone(),
            match_threshold: dto.match_threshold,
        }
    }
}

impl From<&TmdbConfig> for TmdbConfigDto {
    fn from(instance: &TmdbConfig) -> Self {
        Self {
            enabled: instance.enabled,
            api_key: instance.api_key.clone(),
            rate_limit_ms: instance.rate_limit_ms,
            cache_duration_days: instance.cache_duration_days,
            language: instance.language.clone(),
            cooldown: instance.cooldown.clone(),
            match_threshold: instance.match_threshold,
        }
    }
}
