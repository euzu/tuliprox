use crate::{
    defaults::{
        default_hls_cache_bytes, default_hls_cache_bytes_per_session, default_hls_cache_duration,
        default_hls_initial_manifest_wait_timeout_secs, default_hls_max_concurrent_segment_fetches_global,
        default_hls_max_concurrent_segment_fetches_per_session, default_hls_max_segments_prefetch,
        default_hls_origin_manifest_timeout_ms, default_hls_origin_segment_timeout_ms,
        default_hls_session_idle_timeout, DEFAULT_HLS_CACHE_BYTES, DEFAULT_HLS_CACHE_BYTES_PER_SESSION,
    },
    error::TuliproxError,
    model::{
        ByteSize, HlsCorruptSegmentWatchdogMode, HlsManifestRecoveryBurstLevel, HlsSegmentRepairMode, HlsStripMode,
        Millis, Secs,
    },
    utils::is_blank_optional_string,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsStripConfigDto {
    #[serde(default)]
    pub mode: HlsStripMode,
    #[serde(default)]
    pub value: u64,
}

impl HlsStripConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub const fn clean(&mut self) {}
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsManifestRecoveryBurstConfigDto {
    #[serde(default)]
    pub level: HlsManifestRecoveryBurstLevel,
}

impl HlsManifestRecoveryBurstConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub const fn clean(&mut self) {}
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsCacheConfigDto {
    #[serde(default, skip_serializing_if = "HlsStartupConfigDto::is_empty")]
    pub startup: HlsStartupConfigDto,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub cache_path: Option<String>,
    #[serde(default)]
    pub strip: HlsStripConfigDto,
    #[serde(default = "default_hls_cache_duration")]
    pub cache_duration: Secs,
    #[serde(default = "default_hls_cache_bytes")]
    pub cache_bytes: ByteSize,
    #[serde(default = "default_hls_cache_bytes_per_session")]
    pub cache_bytes_per_session: ByteSize,
    #[serde(default = "default_hls_max_segments_prefetch")]
    pub max_segments_prefetch: usize,
    #[serde(default = "default_hls_max_concurrent_segment_fetches_per_session")]
    pub max_concurrent_segment_fetches_per_session: usize,
    #[serde(default = "default_hls_max_concurrent_segment_fetches_global")]
    pub max_concurrent_segment_fetches_global: usize,
    #[serde(default = "default_hls_origin_manifest_timeout_ms")]
    pub origin_manifest_timeout_ms: Millis,
    #[serde(default = "default_hls_origin_segment_timeout_ms")]
    pub origin_segment_timeout_ms: Millis,
    /// How long a client may wait for the initial manifest decision before the session bootstraps time out.
    #[serde(default = "default_hls_initial_manifest_wait_timeout_secs")]
    pub initial_manifest_wait_timeout_secs: Secs,
    #[serde(default = "default_hls_session_idle_timeout")]
    pub session_idle_timeout: Secs,
    #[serde(default, skip_serializing_if = "HlsManifestRecoveryBurstConfigDto::is_empty")]
    pub manifest_recovery_burst: HlsManifestRecoveryBurstConfigDto,
    #[serde(default, skip_serializing_if = "HlsSegmentRepairConfigDto::is_empty")]
    pub segment_repair: HlsSegmentRepairConfigDto,
}

impl Default for HlsCacheConfigDto {
    fn default() -> Self {
        Self {
            startup: HlsStartupConfigDto::default(),
            cache_path: None,
            strip: HlsStripConfigDto::default(),
            cache_duration: default_hls_cache_duration(),
            cache_bytes: default_hls_cache_bytes(),
            cache_bytes_per_session: default_hls_cache_bytes_per_session(),
            max_segments_prefetch: default_hls_max_segments_prefetch(),
            max_concurrent_segment_fetches_per_session: default_hls_max_concurrent_segment_fetches_per_session(),
            max_concurrent_segment_fetches_global: default_hls_max_concurrent_segment_fetches_global(),
            origin_manifest_timeout_ms: default_hls_origin_manifest_timeout_ms(),
            origin_segment_timeout_ms: default_hls_origin_segment_timeout_ms(),
            initial_manifest_wait_timeout_secs: default_hls_initial_manifest_wait_timeout_secs(),
            session_idle_timeout: default_hls_session_idle_timeout(),
            manifest_recovery_burst: HlsManifestRecoveryBurstConfigDto::default(),
            segment_repair: HlsSegmentRepairConfigDto::default(),
        }
    }
}

impl HlsCacheConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub fn clean(&mut self) {
        self.strip.clean();
        self.manifest_recovery_burst.clean();
        self.segment_repair.clean();
    }

    fn ensure_min_millis(field_name: &str, value: Millis, min_value: Millis) -> Result<(), TuliproxError> {
        if value < min_value {
            return Err(TuliproxError::ConfigReverseProxy(format!(
                "hls_cache.{field_name} must be >= {}",
                min_value.get()
            )));
        }
        Ok(())
    }

    fn ensure_min_secs(field_name: &str, value: Secs, min_value: Secs) -> Result<(), TuliproxError> {
        if value < min_value {
            return Err(TuliproxError::ConfigReverseProxy(format!(
                "hls_cache.{field_name} must be >= {}",
                min_value.get()
            )));
        }
        Ok(())
    }

    fn ensure_min_usize(field_name: &str, value: usize, min_value: usize) -> Result<(), TuliproxError> {
        if value < min_value {
            return Err(TuliproxError::ConfigReverseProxy(format!("hls_cache.{field_name} must be >= {min_value}")));
        }
        Ok(())
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.startup.prepare()?;
        if let Some(cache_path) = &self.cache_path {
            if cache_path.is_empty() {
                self.cache_path = None;
            }
        }

        self.cache_bytes.clean_or_default(DEFAULT_HLS_CACHE_BYTES);
        self.cache_bytes_per_session.clean_or_default(DEFAULT_HLS_CACHE_BYTES_PER_SESSION);

        self.cache_bytes.parse_bytes().map_err(TuliproxError::ConfigReverseProxy)?;
        self.cache_bytes_per_session.parse_bytes().map_err(TuliproxError::ConfigReverseProxy)?;

        Self::ensure_min_secs("cache_duration", self.cache_duration, Secs::new(1))?;
        Self::ensure_min_usize(
            "max_concurrent_segment_fetches_per_session",
            self.max_concurrent_segment_fetches_per_session,
            1,
        )?;
        Self::ensure_min_usize("max_concurrent_segment_fetches_global", self.max_concurrent_segment_fetches_global, 1)?;
        Self::ensure_min_millis("origin_manifest_timeout_ms", self.origin_manifest_timeout_ms, Millis::new(1))?;
        Self::ensure_min_millis("origin_segment_timeout_ms", self.origin_segment_timeout_ms, Millis::new(1))?;
        Self::ensure_min_secs(
            "initial_manifest_wait_timeout_secs",
            self.initial_manifest_wait_timeout_secs,
            Secs::new(1),
        )?;
        Self::ensure_min_secs("session_idle_timeout", self.session_idle_timeout, Secs::new(1))?;
        if self.segment_repair.apply_to_first_segments > 6 {
            return Err(TuliproxError::ConfigReverseProxy(
                "hls_cache.segment_repair.apply_to_first_segments must be <= 6".to_string(),
            ));
        }
        self.segment_repair.validate()?;
        if self.segment_repair.max_level != HlsSegmentRepairMode::Off && self.segment_repair.max_parallel_repairs == 0 {
            return Err(TuliproxError::ConfigReverseProxy(
                "hls_cache.segment_repair.max_parallel_repairs must be >= 1 when segment repair is enabled".to_string(),
            ));
        }
        if self.segment_repair.max_level != HlsSegmentRepairMode::Off
            && self.segment_repair.max_parallel_repairs > self.max_segments_prefetch
        {
            return Err(TuliproxError::ConfigReverseProxy(format!(
                "hls_cache.segment_repair.max_parallel_repairs must be <= max_segments_prefetch ({})",
                self.max_segments_prefetch
            )));
        }
        if self.segment_repair.corrupt_segment_watchdog.mode != HlsCorruptSegmentWatchdogMode::Off
            && self.segment_repair.corrupt_segment_watchdog.max_parallel_jobs > self.max_segments_prefetch
        {
            return Err(TuliproxError::ConfigReverseProxy(format!(
                "hls_cache.segment_repair.corrupt_segment_watchdog.max_parallel_jobs must be <= max_segments_prefetch ({})",
                self.max_segments_prefetch
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod startup_config_tests;

mod repair;
mod startup;
pub use repair::{
    HlsCorruptSegmentWatchdogConfigDto, HlsSegmentRepairConfigDto, HlsSegmentRepairSizeIncreaseConfigDto,
};
pub use startup::{
    HlsStartupConfigDto, HlsStartupMode, DEFAULT_HLS_PROGRESSIVE_BYTES_PER_SEGMENT, DEFAULT_HLS_PROGRESSIVE_BYTES_TOTAL,
};
