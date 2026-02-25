use crate::model::macros;
use shared::model::MetadataUpdateConfigDto;
use shared::utils::{
    default_metadata_ffprobe_analyze_duration, default_metadata_ffprobe_live_analyze_duration,
    default_metadata_ffprobe_live_probe_size, default_metadata_ffprobe_probe_size,
    default_metadata_max_resolve_retry_backoff, default_metadata_probe_cooldown,
    default_metadata_probe_retry_backoff_step_1, default_metadata_probe_retry_backoff_step_2,
    default_metadata_probe_retry_backoff_step_3, default_metadata_probe_retry_load_retry_delay,
    default_metadata_progress_log_interval, default_metadata_queue_log_interval,
    default_metadata_resolve_exhaustion_reset_gap, default_metadata_resolve_min_retry_base,
    default_metadata_retry_delay, default_metadata_worker_idle_timeout, parse_duration_seconds, parse_size_base_2,
};

#[derive(Debug, Clone)]
pub struct MetadataUpdateConfig {
    pub queue_log_interval: String,
    pub queue_log_interval_secs: u64,
    pub progress_log_interval: String,
    pub progress_log_interval_secs: u64,
    pub max_resolve_retry_backoff: String,
    pub max_resolve_retry_backoff_secs: u64,
    pub resolve_min_retry_base: String,
    pub resolve_min_retry_base_secs: u64,
    pub resolve_exhaustion_reset_gap: String,
    pub resolve_exhaustion_reset_gap_secs: u64,
    pub probe_cooldown: String,
    pub probe_cooldown_secs: u64,
    pub retry_delay: String,
    pub retry_delay_secs: u64,
    pub probe_retry_load_retry_delay: String,
    pub probe_retry_load_retry_delay_secs: u64,
    pub worker_idle_timeout: String,
    pub worker_idle_timeout_secs: u64,
    pub probe_retry_backoff_step_1: String,
    pub probe_retry_backoff_step_1_secs: u64,
    pub probe_retry_backoff_step_2: String,
    pub probe_retry_backoff_step_2_secs: u64,
    pub probe_retry_backoff_step_3: String,
    pub probe_retry_backoff_step_3_secs: u64,
    pub max_attempts_resolve: u8,
    pub max_attempts_probe: u8,
    pub backoff_jitter_percent: u8,
    pub max_queue_size: usize,
    pub ffprobe_enabled: bool,
    pub ffprobe_timeout: Option<u64>,
    pub ffprobe_analyze_duration: String,
    pub ffprobe_analyze_duration_micros: u64,
    pub ffprobe_probe_size: String,
    pub ffprobe_probe_size_bytes: u64,
    pub ffprobe_live_analyze_duration: String,
    pub ffprobe_live_analyze_duration_micros: u64,
    pub ffprobe_live_probe_size: String,
    pub ffprobe_live_probe_size_bytes: u64,
}

impl Default for MetadataUpdateConfig {
    fn default() -> Self { Self::from(&MetadataUpdateConfigDto::default()) }
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

struct ParsedMetadataUpdateNumbers {
    queue_log_interval_secs: u64,
    progress_log_interval_secs: u64,
    max_resolve_retry_backoff_secs: u64,
    resolve_min_retry_base_secs: u64,
    resolve_exhaustion_reset_gap_secs: u64,
    probe_cooldown_secs: u64,
    retry_delay_secs: u64,
    probe_retry_load_retry_delay_secs: u64,
    worker_idle_timeout_secs: u64,
    probe_retry_backoff_step_1_secs: u64,
    probe_retry_backoff_step_2_secs: u64,
    probe_retry_backoff_step_3_secs: u64,
    ffprobe_analyze_duration_micros: u64,
    ffprobe_probe_size_bytes: u64,
    ffprobe_live_analyze_duration_micros: u64,
    ffprobe_live_probe_size_bytes: u64,
}

fn parse_numeric_fields(cfg: &MetadataUpdateConfigDto) -> ParsedMetadataUpdateNumbers {
    let queue_log_interval_secs =
        parse_duration_or_default(&cfg.queue_log_interval, &default_metadata_queue_log_interval(), false);
    let progress_log_interval_secs =
        parse_duration_or_default(&cfg.progress_log_interval, &default_metadata_progress_log_interval(), false);
    let max_resolve_retry_backoff_secs = parse_duration_or_default(
        &cfg.max_resolve_retry_backoff,
        &default_metadata_max_resolve_retry_backoff(),
        false,
    );
    let resolve_min_retry_base_secs =
        parse_duration_or_default(&cfg.resolve_min_retry_base, &default_metadata_resolve_min_retry_base(), false);
    let resolve_exhaustion_reset_gap_secs = parse_duration_or_default(
        &cfg.resolve_exhaustion_reset_gap,
        &default_metadata_resolve_exhaustion_reset_gap(),
        false,
    );
    let probe_cooldown_secs = parse_duration_or_default(&cfg.probe_cooldown, &default_metadata_probe_cooldown(), false);
    let retry_delay_secs = parse_duration_or_default(&cfg.retry_delay, &default_metadata_retry_delay(), false);
    let probe_retry_load_retry_delay_secs = parse_duration_or_default(
        &cfg.probe_retry_load_retry_delay,
        &default_metadata_probe_retry_load_retry_delay(),
        false,
    );
    let worker_idle_timeout_secs =
        parse_duration_or_default(&cfg.worker_idle_timeout, &default_metadata_worker_idle_timeout(), false);
    let probe_retry_backoff_step_1_secs = parse_duration_or_default(
        &cfg.probe_retry_backoff_step_1,
        &default_metadata_probe_retry_backoff_step_1(),
        false,
    );
    let probe_retry_backoff_step_2_secs = parse_duration_or_default(
        &cfg.probe_retry_backoff_step_2,
        &default_metadata_probe_retry_backoff_step_2(),
        false,
    );
    let probe_retry_backoff_step_3_secs = parse_duration_or_default(
        &cfg.probe_retry_backoff_step_3,
        &default_metadata_probe_retry_backoff_step_3(),
        false,
    );
    let ffprobe_analyze_duration_micros = parse_duration_or_default(
        &cfg.ffprobe_analyze_duration,
        &default_metadata_ffprobe_analyze_duration(),
        true,
    )
    .saturating_mul(1_000_000);
    let ffprobe_probe_size_bytes =
        parse_size_or_default(&cfg.ffprobe_probe_size, &default_metadata_ffprobe_probe_size());
    let ffprobe_live_analyze_duration_micros = parse_duration_or_default(
        &cfg.ffprobe_live_analyze_duration,
        &default_metadata_ffprobe_live_analyze_duration(),
        true,
    )
    .saturating_mul(1_000_000);
    let ffprobe_live_probe_size_bytes =
        parse_size_or_default(&cfg.ffprobe_live_probe_size, &default_metadata_ffprobe_live_probe_size());

    ParsedMetadataUpdateNumbers {
        queue_log_interval_secs,
        progress_log_interval_secs,
        max_resolve_retry_backoff_secs,
        resolve_min_retry_base_secs,
        resolve_exhaustion_reset_gap_secs,
        probe_cooldown_secs,
        retry_delay_secs,
        probe_retry_load_retry_delay_secs,
        worker_idle_timeout_secs,
        probe_retry_backoff_step_1_secs,
        probe_retry_backoff_step_2_secs,
        probe_retry_backoff_step_3_secs,
        ffprobe_analyze_duration_micros,
        ffprobe_probe_size_bytes,
        ffprobe_live_analyze_duration_micros,
        ffprobe_live_probe_size_bytes,
    }
}

impl From<&MetadataUpdateConfigDto> for MetadataUpdateConfig {
    fn from(dto: &MetadataUpdateConfigDto) -> Self {
        // ConfigDto::prepare() should already normalize/validate, but conversion stays defensive.
        let mut normalized = dto.clone();
        if normalized.prepare().is_err() {
            normalized = MetadataUpdateConfigDto::default();
            let _ = normalized.prepare();
        }
        let parsed = parse_numeric_fields(&normalized);

        Self {
            queue_log_interval: normalized.queue_log_interval,
            queue_log_interval_secs: parsed.queue_log_interval_secs,
            progress_log_interval: normalized.progress_log_interval,
            progress_log_interval_secs: parsed.progress_log_interval_secs,
            max_resolve_retry_backoff: normalized.max_resolve_retry_backoff,
            max_resolve_retry_backoff_secs: parsed.max_resolve_retry_backoff_secs,
            resolve_min_retry_base: normalized.resolve_min_retry_base,
            resolve_min_retry_base_secs: parsed.resolve_min_retry_base_secs,
            resolve_exhaustion_reset_gap: normalized.resolve_exhaustion_reset_gap,
            resolve_exhaustion_reset_gap_secs: parsed.resolve_exhaustion_reset_gap_secs,
            probe_cooldown: normalized.probe_cooldown,
            probe_cooldown_secs: parsed.probe_cooldown_secs,
            retry_delay: normalized.retry_delay,
            retry_delay_secs: parsed.retry_delay_secs,
            probe_retry_load_retry_delay: normalized.probe_retry_load_retry_delay,
            probe_retry_load_retry_delay_secs: parsed.probe_retry_load_retry_delay_secs,
            worker_idle_timeout: normalized.worker_idle_timeout,
            worker_idle_timeout_secs: parsed.worker_idle_timeout_secs,
            probe_retry_backoff_step_1: normalized.probe_retry_backoff_step_1,
            probe_retry_backoff_step_1_secs: parsed.probe_retry_backoff_step_1_secs,
            probe_retry_backoff_step_2: normalized.probe_retry_backoff_step_2,
            probe_retry_backoff_step_2_secs: parsed.probe_retry_backoff_step_2_secs,
            probe_retry_backoff_step_3: normalized.probe_retry_backoff_step_3,
            probe_retry_backoff_step_3_secs: parsed.probe_retry_backoff_step_3_secs,
            max_attempts_resolve: normalized.max_attempts_resolve,
            max_attempts_probe: normalized.max_attempts_probe,
            backoff_jitter_percent: normalized.backoff_jitter_percent,
            max_queue_size: normalized.max_queue_size,
            ffprobe_enabled: normalized.ffprobe_enabled,
            ffprobe_timeout: normalized.ffprobe_timeout,
            ffprobe_analyze_duration: normalized.ffprobe_analyze_duration,
            ffprobe_analyze_duration_micros: parsed.ffprobe_analyze_duration_micros,
            ffprobe_probe_size: normalized.ffprobe_probe_size,
            ffprobe_probe_size_bytes: parsed.ffprobe_probe_size_bytes,
            ffprobe_live_analyze_duration: normalized.ffprobe_live_analyze_duration,
            ffprobe_live_analyze_duration_micros: parsed.ffprobe_live_analyze_duration_micros,
            ffprobe_live_probe_size: normalized.ffprobe_live_probe_size,
            ffprobe_live_probe_size_bytes: parsed.ffprobe_live_probe_size_bytes,
        }
    }
}

impl From<&MetadataUpdateConfig> for MetadataUpdateConfigDto {
    fn from(instance: &MetadataUpdateConfig) -> Self {
        Self {
            queue_log_interval: instance.queue_log_interval.clone(),
            progress_log_interval: instance.progress_log_interval.clone(),
            max_resolve_retry_backoff: instance.max_resolve_retry_backoff.clone(),
            resolve_min_retry_base: instance.resolve_min_retry_base.clone(),
            resolve_exhaustion_reset_gap: instance.resolve_exhaustion_reset_gap.clone(),
            probe_cooldown: instance.probe_cooldown.clone(),
            retry_delay: instance.retry_delay.clone(),
            probe_retry_load_retry_delay: instance.probe_retry_load_retry_delay.clone(),
            worker_idle_timeout: instance.worker_idle_timeout.clone(),
            probe_retry_backoff_step_1: instance.probe_retry_backoff_step_1.clone(),
            probe_retry_backoff_step_2: instance.probe_retry_backoff_step_2.clone(),
            probe_retry_backoff_step_3: instance.probe_retry_backoff_step_3.clone(),
            max_attempts_resolve: instance.max_attempts_resolve,
            max_attempts_probe: instance.max_attempts_probe,
            backoff_jitter_percent: instance.backoff_jitter_percent,
            max_queue_size: instance.max_queue_size,
            ffprobe_enabled: instance.ffprobe_enabled,
            ffprobe_timeout: instance.ffprobe_timeout,
            ffprobe_analyze_duration: instance.ffprobe_analyze_duration.clone(),
            ffprobe_probe_size: instance.ffprobe_probe_size.clone(),
            ffprobe_live_analyze_duration: instance.ffprobe_live_analyze_duration.clone(),
            ffprobe_live_probe_size: instance.ffprobe_live_probe_size.clone(),
        }
    }
}
