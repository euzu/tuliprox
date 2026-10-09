use super::{MetadataUpdateConfigDto, MIN_DURATION_SECS, MIN_QUEUE_SIZE};
use crate::{
    defaults::{
        default_metadata_max_queue_size, default_metadata_no_change_cache_ttl_secs, default_metadata_path,
        default_metadata_probe_fairness_resolve_burst, default_metadata_retry_delay,
        default_metadata_worker_idle_timeout, is_default_metadata_path,
    },
    error::TuliproxError,
    utils::parse_duration_seconds,
};

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
            && self.no_change_cache_ttl_secs == default_metadata_no_change_cache_ttl_secs()
            && self.probe_fairness_resolve_burst == default_metadata_probe_fairness_resolve_burst()
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
            return Err(TuliproxError::ConfigMetadataUpdate("metadata_update.cache_path cannot be empty".to_string()));
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
        self.no_change_cache_ttl_secs = self.no_change_cache_ttl_secs.max(MIN_DURATION_SECS);
        self.probe_fairness_resolve_burst = self.probe_fairness_resolve_burst.max(MIN_QUEUE_SIZE);

        self.clean();

        Ok(())
    }

    pub(super) fn parse_and_clamp_duration(
        value: &str,
        min_seconds: u64,
        field_name: &str,
    ) -> Result<u64, TuliproxError> {
        let parsed = Self::parse_duration(value, field_name)?;
        Ok(parsed.max(min_seconds))
    }

    pub(super) fn parse_and_clamp_duration_with_required_unit(
        value: &str,
        min_seconds: u64,
        field_name: &str,
    ) -> Result<u64, TuliproxError> {
        let parsed = Self::parse_duration_with_required_unit(value, field_name)?;
        Ok(parsed.max(min_seconds))
    }

    pub(super) fn parse_duration_with_required_unit(value: &str, field_name: &str) -> Result<u64, TuliproxError> {
        if value.parse::<u64>().is_ok() {
            return Err(TuliproxError::ConfigMetadataUpdate(format!(
                "Invalid duration format for `{field_name}`: {value}. Use explicit unit suffix (`s`, `m`, `h`, `d`), e.g. `10s`."
            )));
        }
        Self::parse_duration(value, field_name)
    }

    pub(super) fn parse_duration(value: &str, field_name: &str) -> Result<u64, TuliproxError> {
        parse_duration_seconds(value, false).ok_or_else(|| {
            crate::error::TuliproxError::ConfigMetadataUpdate(format!(
                "Invalid duration format for `{field_name}`: {value}"
            ))
        })
    }

    pub(super) fn canonicalize_seconds(seconds: u64) -> String {
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

    pub(super) fn canonicalize_size_bytes(bytes: u64) -> String {
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
