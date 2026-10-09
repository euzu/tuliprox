use super::{MetadataUpdateConfigDto, MAX_JITTER_PERCENT, MIN_ATTEMPTS, MIN_DURATION_SECS};
use crate::{
    defaults::{
        default_metadata_backoff_jitter_percent, default_metadata_max_attempts_probe,
        default_metadata_max_attempts_resolve, default_metadata_max_resolve_retry_backoff,
        default_metadata_probe_cooldown, default_metadata_probe_retry_backoff_step_1,
        default_metadata_probe_retry_backoff_step_2, default_metadata_probe_retry_backoff_step_3,
        default_metadata_probe_retry_load_retry_delay, default_metadata_progress_log_interval,
        default_metadata_queue_log_interval, default_metadata_resolve_exhaustion_reset_gap,
        default_metadata_resolve_min_retry_base, default_probe_user_priority,
        is_default_metadata_backoff_jitter_percent, is_default_metadata_max_attempts_probe,
        is_default_metadata_max_attempts_resolve, is_default_metadata_max_resolve_retry_backoff,
        is_default_metadata_probe_cooldown, is_default_metadata_probe_retry_backoff_step_1,
        is_default_metadata_probe_retry_backoff_step_2, is_default_metadata_probe_retry_backoff_step_3,
        is_default_metadata_probe_retry_load_retry_delay, is_default_metadata_progress_log_interval,
        is_default_metadata_queue_log_interval, is_default_metadata_resolve_exhaustion_reset_gap,
        is_default_metadata_resolve_min_retry_base, is_default_probe_user_priority,
    },
    error::TuliproxError,
};

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

    pub(super) fn prepare(&mut self) -> Result<(), TuliproxError> {
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

    pub(super) fn prepare(&mut self) -> Result<(), TuliproxError> {
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
    #[serde(default = "default_probe_user_priority", skip_serializing_if = "is_default_probe_user_priority")]
    pub user_priority: i8,
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
            user_priority: default_probe_user_priority(),
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
            && self.user_priority == default_probe_user_priority()
    }

    pub(super) fn prepare(&mut self) -> Result<(), TuliproxError> {
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
